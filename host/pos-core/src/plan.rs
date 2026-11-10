//! Exact-balance spend planning.
//!
//! A mint v1 transaction must balance to the msat on federations older than core consensus 2.1
//! (they refuse overpayment), so the planner chooses inputs and outputs such that
//!
//! ```text
//! sum(inputs) == sum(outputs) + sum(fee(input)) + sum(fee(output))
//! ```
//!
//! exactly. Fees come from a caller-supplied function (the federation's `FeeConsensus::fee`),
//! so the planner knows nothing about fedimint types and is unit-testable.
//!
//! Notes are powers of two msat. The till (merchant) outputs are the binary decomposition of the
//! amount the till receives; the card's change is the *fewest* notes that make the balance exact,
//! because every change note needs a free card slot and every extra note costs a fee.
//!
//! The till may be overpaid by at most [`MAX_TILL_OVERPAY`] msat. That slack is what lets small
//! change be absorbed instead of minted as a dust note.
//!
//! From core consensus 2.1 a federation also takes a transaction that pays more than its outputs
//! and fees (`fedimint-server/src/consensus/transaction.rs`, `FundingVerifier::verify_funding`:
//! inputs below outputs plus fees are refused always, inputs above them only before 2.1); the
//! surplus is burned. [`solve_with_margin`] plans such a transaction for an offline charge, so a
//! fee rise between signing and sending is paid out of the surplus (a *fee margin*):
//!
//! ```text
//! sum(inputs) == sum(outputs) + sum(fee(input)) + sum(fee(output)) + surplus
//! ```

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// Largest amount (msat) by which the till may be paid more than the price.
pub const MAX_TILL_OVERPAY: u64 = 2_048;
/// Largest denomination exponent the planner will use (2^50 msat is about 11,000 BTC).
pub const MAX_EXP: u32 = 50;
/// Cap on notes in a cashout.
pub const MAX_CASHOUT_NOTES: usize = 40;
/// Work cap for the exhaustive search, in fee evaluations / DFS nodes.
const BUDGET: u64 = 6_000_000;
/// Free slots a charge tries to leave on the card: room for a top-up (`topup::MAX_TOPUP_NOTES`).
/// When the cheapest plan would leave fewer, the planner spends more of the card's notes so the
/// change comes back as fewer, larger notes. Costs a card signature (about a second) per extra
/// input, and only when the card is nearly full.
pub const ROOM_AFTER_CHARGE: usize = 3;
/// Most live notes a card holds: one per slot on the 32-slot applet. More is an inconsistent state.
pub const MAX_CARD_NOTES: usize = 32;
/// Most card notes one charge spends. Each input is one card signature (about 850 ms on a
/// J3R180), and a tap has to stay short.
pub const MAX_INPUTS: usize = 3;

/// How far a plan leaves the card short of [`ROOM_AFTER_CHARGE`] free slots, assuming the spent
/// inputs' slots are renewed after the charge (`glue.rs` renews until a slot is free).
fn room_short(free_slots: usize, inputs: usize, change: usize) -> usize {
    ROOM_AFTER_CHARGE.saturating_sub((free_slots + inputs).saturating_sub(change))
}

/// Fee in msat for one input or output of the given amount.
pub type FeeFn<'a> = &'a dyn Fn(u64) -> u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Indices into the `notes` slice given to [`solve`].
    pub inputs: Vec<usize>,
    /// Amounts the till receives, as notes (descending powers of two).
    pub till: Vec<u64>,
    /// Amounts of change notes for the card (descending).
    pub change: Vec<u64>,
    /// `sum(till) - price`.
    pub overpay: u64,
    /// msat the inputs pay beyond the outputs and their fees: burned by the federation (a fee
    /// margin, [`solve_with_margin`]). 0 for an exact balance.
    pub surplus: u64,
    /// True if some output is below the federation's smallest economical denomination, which
    /// the planner only allows when no dust-free exact balance exists.
    pub dust: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// Live notes cannot cover the price plus fees.
    Insufficient { have: u64, need: u64 },
    /// More live notes than a card can hold.
    TooManyNotes(usize),
    /// No [`MAX_INPUTS`] of the card's notes cover the price and fees; more would make the tap
    /// too long.
    TooManyInputs { price: u64, max_inputs: usize },
    /// Funds exist, but no exact-balance layout fits the free slots and the overpay limit.
    NoExactPlan { price: u64, free_slots: usize },
    /// A topup needs more empty slots than the card has.
    NoFreeSlots { need: usize, free: usize },
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::Insufficient { have, need } => {
                write!(f, "insufficient funds on the card: have {have} msat, need at least {need} msat")
            }
            PlanError::TooManyNotes(n) => write!(f, "{n} live notes is more than a card can hold; state file is inconsistent"),
            PlanError::TooManyInputs { price, max_inputs } => write!(
                f,
                "a spend of {price} msat needs more than {max_inputs} of the card's notes; \
                 spend a smaller amount, or cash out and reload to merge notes"
            ),
            PlanError::NoExactPlan { price, free_slots } => write!(
                f,
                "no exact-balance spend of {price} msat with {free_slots} free slot(s); \
                 top up to get more free slots, or spend a different amount"
            ),
            PlanError::NoFreeSlots { need, free } => write!(
                f,
                "{need} note(s) need {need} free slot(s) but only {free} are free \
                 (run `card setup`, or spend/cash out to free slots)"
            ),
        }
    }
}
impl std::error::Error for PlanError {}

/// `amount` as descending powers of two.
pub fn binary(amount: u64) -> Vec<u64> {
    (0..64).rev().filter(|b| amount >> b & 1 == 1).map(|b| 1u64 << b).collect()
}

/// Input fee plus output fees for a set of amounts.
pub fn fee_sum(amounts: &[u64], fee: FeeFn) -> u64 {
    amounts.iter().map(|a| fee(*a)).sum()
}

/// `amount + fee(amount)`: what one note of `amount` costs to create.
fn weight(amount: u64, fee: FeeFn) -> u64 {
    amount + fee(amount)
}

/// True if every denomination pays the same fee, which allows a closed-form search.
fn flat_fee(fee: FeeFn) -> Option<u64> {
    let c = fee(1);
    (0..=MAX_EXP).all(|e| fee(1u64 << e) == c).then_some(c)
}

/// Fewest notes (at most `max_notes`) whose creation cost `sum(a + fee(a))` is exactly `target`.
/// Returns amounts descending. `Some(vec![])` for `target == 0`.
pub fn min_notes(target: u64, max_notes: usize, fee: FeeFn, min_denom: u64) -> Option<Vec<u64>> {
    min_notes_with(target, max_notes, fee, min_denom, flat_fee(fee))
}

/// [`min_notes`] with `flat_fee(fee)` already worked out: the planner calls this once per
/// candidate, and `flat_fee` alone is 51 fee evaluations.
fn min_notes_with(target: u64, max_notes: usize, fee: FeeFn, min_denom: u64, flat: Option<u64>) -> Option<Vec<u64>> {
    debug_assert!(min_denom.is_power_of_two());
    if target == 0 {
        return Some(vec![]);
    }
    if let Some(c) = flat {
        for k in 1..=max_notes {
            let Some(total) = target.checked_sub(c.checked_mul(k as u64)?) else { break };
            if total % min_denom != 0 {
                continue;
            }
            let units = total / min_denom;
            if units == 0 || units < k as u64 {
                continue;
            }
            if (units.count_ones() as usize) <= k {
                let notes = binary(units).into_iter().map(|u| u * min_denom).collect();
                return Some(split_to(notes, k, min_denom));
            }
        }
        return None;
    }
    // General fee schedule: depth-first over exponents with memoised failures.
    for k in 1..=max_notes {
        let mut failed = HashSet::new();
        let mut out = Vec::new();
        let mut nodes = 0u64;
        let top = (63 - target.leading_zeros() as i32).min(MAX_EXP as i32);
        let floor = min_denom.trailing_zeros() as i32;
        if dfs(top, floor, k, target, fee, &mut failed, &mut out, &mut nodes) {
            out.sort_unstable_by(|a, b| b.cmp(a));
            return Some(out);
        }
    }
    None
}

/// Split the largest splittable note in half until there are exactly `k` notes.
fn split_to(mut notes: Vec<u64>, k: usize, min_denom: u64) -> Vec<u64> {
    while notes.len() < k {
        notes.sort_unstable_by(|a, b| b.cmp(a));
        let big = notes[0];
        debug_assert!(big > min_denom, "caller guarantees units >= k");
        notes[0] = big / 2;
        notes.push(big / 2);
    }
    notes.sort_unstable_by(|a, b| b.cmp(a));
    notes
}

fn dfs(
    e: i32,
    floor: i32,
    items: usize,
    rem: u64,
    fee: FeeFn,
    failed: &mut HashSet<(i32, usize, u64)>,
    out: &mut Vec<u64>,
    nodes: &mut u64,
) -> bool {
    if items == 0 {
        return rem == 0;
    }
    if e < floor {
        return false;
    }
    *nodes += 1;
    if *nodes > BUDGET || failed.contains(&(e, items, rem)) {
        return false;
    }
    let w_hi = weight(1u64 << e, fee);
    let w_lo = weight(1u64 << floor, fee);
    if rem > items as u64 * w_hi || rem < items as u64 * w_lo {
        failed.insert((e, items, rem));
        return false;
    }
    let max_c = (rem / w_hi).min(items as u64) as usize;
    for c in (0..=max_c).rev() {
        let used = c as u64 * w_hi;
        for _ in 0..c {
            out.push(1u64 << e);
        }
        if dfs(e - 1, floor, items - c, rem - used, fee, failed, out, nodes) {
            return true;
        }
        out.truncate(out.len() - c);
    }
    failed.insert((e, items, rem));
    false
}

/// Plan a cashout of one card note: host-owned notes whose creation cost is exactly
/// `note_amount - fee(note_amount)`, using only tiers of at least `min_denom` when that can be
/// balanced exactly and otherwise (second element `true`) allowing smaller ones.
/// `None` if the note cannot even pay its own fee.
pub fn cashout(note_amount: u64, fee: FeeFn, min_denom: u64) -> Option<(Vec<u64>, bool)> {
    let target = note_amount.checked_sub(fee(note_amount))?;
    if target == 0 {
        return None;
    }
    for last_resort in [false, true] {
        let mut best: Option<(Vec<u64>, bool)> = None;
        for floor in floors(min_denom, last_resort) {
            if let Some(v) = min_notes(target, MAX_CASHOUT_NOTES, fee, floor) {
                // Fewer notes means fewer fees for everyone who touches them afterwards.
                if best.as_ref().is_none_or(|(b, _)| v.len() < b.len()) {
                    best = Some((v, floor < min_denom));
                }
            }
        }
        if best.is_some() {
            return best;
        }
    }
    None
}

/// Floors to try, highest first: `min_denom`, halving down to 4 msat (every multiple-of-4 fee
/// schedule balances there). With `last_resort`, only 1 msat.
fn floors(min_denom: u64, last_resort: bool) -> Vec<u64> {
    if last_resort {
        return vec![1];
    }
    let mut v = vec![];
    let mut f = min_denom.max(1);
    while f > 4 {
        v.push(f);
        f /= 2;
    }
    v.push(f);
    v
}

/// Choose inputs, till notes and change notes for a card spend of `price` msat.
///
/// `notes` are the amounts of the card's live notes. `free_slots` bounds the number of change
/// notes. Output notes are at least `min_denom` (the federation's smallest economical
/// denomination) when an exact balance exists that way; since fees are not multiples of
/// `min_denom` that is rare, and the planner then retries allowing any tier and sets `dust`.
/// Objective, in order: leave [`ROOM_AFTER_CHARGE`] free slots if any plan can (else get as close
/// as possible), fewest change notes, fewest output notes, fewest inputs (each costs one card
/// signature, about a second on silicon; at most [`MAX_INPUTS`]), smallest overpay.
pub fn solve(
    notes: &[u64],
    price: u64,
    free_slots: usize,
    fee: FeeFn,
    min_denom: u64,
) -> Result<Plan, PlanError> {
    solve_surplus(notes, price, free_slots, fee, min_denom, 0, 0)
}

/// [`solve`] for a transaction whose inputs pay exactly `surplus` msat more than its outputs and
/// their fees, overpaying the till by at least `min_overpay`. Only for a federation at core
/// consensus 2.1 or later; the objective is [`solve`]'s.
fn solve_surplus(
    notes: &[u64],
    price: u64,
    free_slots: usize,
    fee: FeeFn,
    min_denom: u64,
    surplus: u64,
    min_overpay: u64,
) -> Result<Plan, PlanError> {
    // Every floor that balances is a candidate. Card slots are scarce (8 or 32), so the fewest change
    // notes wins, then the fewest output notes (each costs a fee), fewest inputs, smallest overpay. A high floor can force dozens of
    // notes when fees are not multiples of it, so "no dust" is not allowed to beat "few notes".
    let mut best: Option<Plan> = None;
    let mut first_err = None;
    let mut tried = Vec::new();
    for floor in floors(min_denom, false).into_iter().chain(floors(min_denom, true)) {
        if tried.contains(&floor) {
            continue; // `min_denom == 1`: the last-resort floor is the one already searched
        }
        tried.push(floor);
        if floor == 1 && min_denom > 1 && best.is_some() {
            break; // 1 msat notes only when nothing larger balances
        }
        match solve_with(notes, price, free_slots, fee, floor, surplus, min_overpay) {
            Ok(mut p) => {
                p.dust = floor < min_denom;
                let key = |p: &Plan| {
                    (
                        room_short(free_slots, p.inputs.len(), p.change.len()),
                        p.change.len(),
                        p.till.len() + p.change.len(),
                        p.inputs.len(),
                        p.overpay,
                    )
                };
                if best.as_ref().is_none_or(|b| key(&p) < key(b)) {
                    best = Some(p);
                }
            }
            Err(e @ PlanError::NoExactPlan { .. }) => {
                first_err.get_or_insert(e);
            }
            Err(e) => return Err(e),
        }
    }
    best.ok_or_else(|| first_err.unwrap_or(PlanError::NoExactPlan { price, free_slots }))
}

fn solve_with(
    notes: &[u64],
    price: u64,
    free_slots: usize,
    fee: FeeFn,
    min_denom: u64,
    surplus: u64,
    min_overpay: u64,
) -> Result<Plan, PlanError> {
    if price == 0 {
        return Err(PlanError::NoExactPlan { price, free_slots });
    }
    let n = notes.len();
    if n > MAX_CARD_NOTES {
        return Err(PlanError::TooManyNotes(n));
    }
    let have: u64 = notes.iter().sum();
    let min_need = price + fee_sum(&binary(price), fee) + surplus;
    if have < min_need {
        return Err(PlanError::Insufficient { have, need: min_need });
    }
    // Cheapest till side at any floor: no input set worth less (after its own fees) can pay.
    let min_till_any = (price..=price + MAX_TILL_OVERPAY).map(|t| t + fee_sum(&binary(t), fee)).min().unwrap_or(u64::MAX);
    let mut nets: Vec<u64> = notes.iter().map(|&a| a.saturating_sub(fee(a))).collect();
    nets.sort_unstable_by(|a, b| b.cmp(a));
    if nets.iter().take(MAX_INPUTS).sum::<u64>() < min_till_any.saturating_add(surplus) {
        return Err(PlanError::TooManyInputs { price, max_inputs: MAX_INPUTS });
    }

    // Till candidates: totals that are multiples of `min_denom`, price <= t <= price + MAX.
    let first = price.div_ceil(min_denom) * min_denom;
    let till_cost: Vec<(u64, u64)> = (0..)
        .map(|i| first + i * min_denom)
        .take_while(|t| *t <= price + MAX_TILL_OVERPAY)
        .filter(|t| t - price >= min_overpay)
        .map(|t| (t - price, t + fee_sum(&binary(t), fee)))
        .collect();
    let Some(min_till) = till_cost.iter().map(|c| c.1).min() else {
        return Err(PlanError::NoExactPlan { price, free_slots });
    };

    // Notes of equal amount are interchangeable, so the search runs over how many notes of each
    // amount go in (a multiset of at most MAX_INPUTS amounts), not over subsets of notes: at most
    // C(d + 2, 3) candidates for d distinct amounts, about 4,000 for a full 32-slot card, against
    // 2^32 subsets. Input sets with the same size and the same value after fees are the same plan
    // to every term of the objective, so only the first of each is searched.
    let mut groups: Vec<(u64, Vec<usize>)> = Vec::new();
    for (i, &a) in notes.iter().enumerate() {
        match groups.iter_mut().find(|g| g.0 == a) {
            Some(g) => g.1.push(i),
            None => groups.push((a, vec![i])),
        }
    }
    let mut picks = Vec::new();
    multisets(&groups, 0, MAX_INPUTS, &mut Vec::new(), &mut picks);
    picks.sort_by_key(|p| p.len()); // stable: fewest inputs first, as before
    let mut seen = HashSet::new();
    // Without a flat fee, change is not closed-form. Rather than solve for the change of every
    // (input set, overpay) pair, the search lists, per input set and change count k, the k-note
    // change amounts whose cost falls in the window the till candidates leave, and looks each one
    // up by the till cost it implies. The first k with a hit is the fewest change notes for that
    // input set; among its hits the smallest overpay wins.
    let mut by_cost: HashMap<u64, u64> = HashMap::new(); // till cost -> smallest overpay
    for (o, tc) in &till_cost {
        by_cost.entry(*tc).or_insert(*o);
    }
    let max_till = till_cost.iter().map(|c| c.1).max().unwrap_or(min_till);
    let weights: Vec<u64> = (0..=MAX_EXP).map(|e| weight(1u64 << e, fee)).collect();
    let floor_exp = min_denom.trailing_zeros() as i32;

    let flat = flat_fee(fee);
    let mut budget = BUDGET;
    // The objective of `solve`, in its order: (room short, change notes, output notes, inputs,
    // overpay). Output notes are the till's `binary(price + overpay)` plus the change.
    let key = |size: usize, change: usize, o: u64| {
        (room_short(free_slots, size, change), change, (price + o).count_ones() as usize + change, size, o)
    };
    let mut best: Option<(Key, Vec<usize>, Vec<u64>)> = None; // (key, input indices, change)
    // Every input size up to MAX_INPUTS is searched, not just the sizes up to the first fitted
    // one: a price the small sizes cannot pay must still find its plan among the larger ones, and
    // a sale refused for lack of a plan is a customer turned away at the counter. `budget` bounds
    // the work.
    let mut sets: Vec<(usize, u64, Vec<usize>)> = Vec::new(); // (inputs, value after fees, indices)
    for pick in &picks {
        let size = pick.len();
        let mut idx: Vec<usize> = Vec::with_capacity(size);
        for (k, &g) in pick.iter().enumerate() {
            // The j-th use of group g takes its j-th note.
            let j = pick[..k].iter().filter(|&&h| h == g).count();
            idx.push(groups[g].1[j]);
        }
        idx.sort_unstable();
        let gross: u64 = idx.iter().map(|&i| notes[i]).sum();
        let in_fees: u64 = idx.iter().map(|&i| fee(notes[i])).sum();
        // The surplus is paid like a fee: what is left after it goes to the till and the change.
        let Some(avail) = gross.checked_sub(in_fees).and_then(|a| a.checked_sub(surplus)) else { continue };
        if avail >= min_till && seen.insert((size, avail)) {
            sets.push((size, avail, idx));
        }
    }

    // `free_slots` is a hard ceiling, never a tuning knob: change needing more slots than the card
    // has cannot be signed at all. Below it, `change_cap` is the most change notes a plan with a
    // given number of inputs can have and not lose to the best plan so far on the first two terms;
    // both grow with the change count, so nothing past it is searched.
    if flat.is_some() {
        'sets: for (size, avail, idx) in &sets {
            let mut cap = change_cap(free_slots, *size, best.as_ref().map(|b| &b.0));
            for (o, tc) in &till_cost {
                let Some(cap_now) = cap else { break };
                let Some(r) = avail.checked_sub(*tc) else { continue };
                if budget == 0 {
                    break 'sets;
                }
                budget -= 1;
                let Some(change) = min_notes_with(r, cap_now, fee, min_denom, flat) else { continue };
                let cand = key(*size, change.len(), *o);
                if best.as_ref().is_none_or(|b| cand < b.0) {
                    best = Some((cand, idx.clone(), change));
                    cap = change_cap(free_slots, *size, best.as_ref().map(|b| &b.0));
                }
            }
        }
    } else {
        // Change count by change count across every input set, so no set is searched for more
        // change notes than the best plan so far needs.
        'levels: for k in 0..=free_slots {
            let mut alive = false;
            for (size, avail, idx) in &sets {
                if change_cap(free_slots, *size, best.as_ref().map(|b| &b.0)).is_none_or(|c| c < k) {
                    continue;
                }
                alive = true;
                let (lo, hi) = (avail.saturating_sub(max_till), avail - min_till);
                let top = (63 - hi.max(1).leading_zeros() as i32).min(MAX_EXP as i32);
                let mut hit: Option<(Key, Vec<u64>)> = None;
                let mut nodes = 0u64;
                let mut out = Vec::new();
                let bound = best.as_ref().map(|b| b.0);
                change_in_window(top, floor_exp, k, 0, lo, hi, &weights, &mut out, &mut nodes, &mut |sum, notes| {
                    let Some(&o) = by_cost.get(&(avail - sum)) else { return };
                    let cand = key(*size, k, o);
                    if bound.is_none_or(|b| cand < b) && hit.as_ref().is_none_or(|h| cand < h.0) {
                        hit = Some((cand, notes.to_vec()));
                    }
                });
                budget = budget.saturating_sub(nodes);
                if let Some((cand, change)) = hit {
                    best = Some((cand, idx.clone(), change));
                }
                if budget == 0 {
                    break 'levels;
                }
            }
            if !alive {
                break;
            }
        }
    }

    let Some(((.., overpay), idx, mut change)) = best else {
        return Err(PlanError::NoExactPlan { price, free_slots });
    };
    let till = binary(price + overpay);
    change.sort_unstable_by(|a, b| b.cmp(a));
    Ok(Plan { inputs: idx, till, change, overpay, surplus, dust: false })
}

/// Every multiset of exactly `items` notes of exponents `floor..=e` whose creation cost
/// `sum + sum(weights)` lies in `lo..=hi`; `visit` gets each cost and its notes. Weights must not
/// fall as the exponent rises. `nodes` counts the work.
#[allow(clippy::too_many_arguments)]
fn change_in_window(
    e: i32,
    floor: i32,
    items: usize,
    sum: u64,
    lo: u64,
    hi: u64,
    weights: &[u64],
    out: &mut Vec<u64>,
    nodes: &mut u64,
    visit: &mut dyn FnMut(u64, &[u64]),
) {
    *nodes += 1;
    if items == 0 {
        if (lo..=hi).contains(&sum) {
            visit(sum, out);
        }
        return;
    }
    if e < floor || *nodes > BUDGET {
        return;
    }
    let (w_hi, w_lo) = (weights[e as usize], weights[floor as usize]);
    if sum + items as u64 * w_lo > hi || sum + items as u64 * w_hi < lo {
        return;
    }
    let max_c = ((hi - sum) / w_hi).min(items as u64) as usize;
    for c in (0..=max_c).rev() {
        out.extend(std::iter::repeat_n(1u64 << e, c));
        change_in_window(e - 1, floor, items - c, sum + c as u64 * w_hi, lo, hi, weights, out, nodes, visit);
        out.truncate(out.len() - c);
    }
}

/// A plan's rank in `solve_with`: (room short, change notes, output notes, inputs, overpay).
type Key = (usize, usize, usize, usize, u64);

/// Most change notes a plan with `size` inputs may have and not lose to `best` on room short and
/// change notes. `None` when no change count can.
fn change_cap(free_slots: usize, size: usize, best: Option<&Key>) -> Option<usize> {
    let Some(&(rs, cc, ..)) = best else { return Some(free_slots) };
    (0..=free_slots).rev().find(|&c| (room_short(free_slots, size, c), c) <= (rs, cc))
}

/// Every multiset of at most `left` group indices, starting at group `from`, that no group
/// supplies more notes to than it has. Each is a non-decreasing list of group indices.
fn multisets(groups: &[(u64, Vec<usize>)], from: usize, left: usize, cur: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
    if left == 0 {
        return;
    }
    for g in from..groups.len() {
        if cur.iter().filter(|&&h| h == g).count() >= groups[g].1.len() {
            continue;
        }
        cur.push(g);
        out.push(cur.clone());
        multisets(groups, g, left - 1, cur, out);
        cur.pop();
    }
}

impl Plan {
    /// Check the exact-balance invariant.
    pub fn balances(&self, notes: &[u64], fee: FeeFn) -> bool {
        let ins: Vec<u64> = self.inputs.iter().map(|&i| notes[i]).collect();
        let outs: Vec<u64> = self.till.iter().chain(&self.change).copied().collect();
        ins.iter().sum::<u64>() == outs.iter().sum::<u64>() + fee_sum(&ins, fee) + fee_sum(&outs, fee) + self.surplus
    }

    /// The amounts of the plan's inputs and outputs (till, then change).
    fn items(&self, notes: &[u64]) -> (Vec<u64>, Vec<u64>) {
        (self.inputs.iter().map(|&i| notes[i]).collect(), self.till.iter().chain(&self.change).copied().collect())
    }
}

/// A fee schedule an offline charge keeps room for: `base_msat + amount * ppm / 1_000_000` per
/// input and per output, as fedimint's mint v1 `FeeConsensus::fee`
/// (`modules/fedimint-mint-common/src/config.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeHeadroom {
    pub base_msat: u64,
    pub ppm: u64,
}

impl FeeHeadroom {
    /// Fedimint's default mint fees: what `fedimint-mint-server` configures unless base fees are
    /// disabled, `FeeConsensus::new(0)` (`modules/fedimint-mint-server/src/lib.rs`), a 100 msat
    /// base fee and no relative fee.
    pub const DEFAULT: FeeHeadroom = FeeHeadroom { base_msat: 100, ppm: 0 };
    /// No margin.
    pub const OFF: FeeHeadroom = FeeHeadroom { base_msat: 0, ppm: 0 };

    pub fn fee(&self, amount: u64) -> u64 {
        amount.saturating_mul(self.ppm) / 1_000_000 + self.base_msat
    }

    pub fn is_off(&self) -> bool {
        self.base_msat == 0 && self.ppm == 0
    }
}

/// The fee margin a transaction with these inputs and outputs needs: its fees under `headroom`
/// less its fees under the current schedule `fee`, or 0 when the current fees are the higher.
pub fn fee_margin(inputs: &[u64], outputs: &[u64], fee: FeeFn, headroom: FeeHeadroom) -> u64 {
    let items = || inputs.iter().chain(outputs);
    let at_headroom: u64 = items().map(|a| headroom.fee(*a)).sum();
    let now: u64 = items().map(|a| fee(*a)).sum();
    at_headroom.saturating_sub(now)
}

/// Rounds [`solve_with_margin`] re-plans for a margin that a plan's own shape changed.
const MARGIN_ROUNDS: usize = 4;

/// [`solve`] with a fee margin: the plan's inputs also pay a surplus of at least
/// [`fee_margin`] of its own inputs and outputs under `headroom`, so the transaction still
/// balances if the federation's fees rise to `headroom` before it is sent.
///
/// The margin comes out of the change, or out of the till's overpay (its rounding surplus over
/// the price), the till always getting at least `price`. With notes in powers of two, trimming a
/// change note by a few hundred msat usually takes more notes than a card has slots for, so in the
/// second e2e run (2026-11-07) every margin was dropped until the overpay could pay it too. A
/// margin plan is
/// taken only when it is no worse than the exact plan on room left, change notes and inputs (card
/// slots and signatures are what a tap is short of); otherwise, or when the notes cannot pay it,
/// the exact plan is returned with `surplus == 0`. Only for a federation at core consensus 2.1 or
/// later: older ones refuse any surplus.
pub fn solve_with_margin(
    notes: &[u64],
    price: u64,
    free_slots: usize,
    fee: FeeFn,
    min_denom: u64,
    headroom: FeeHeadroom,
) -> Result<Plan, PlanError> {
    let exact = solve(notes, price, free_slots, fee, min_denom)?;
    if headroom.is_off() {
        return Ok(exact);
    }
    let rank = |p: &Plan| (room_short(free_slots, p.inputs.len(), p.change.len()), p.change.len());
    let need = |p: &Plan| {
        let (ins, outs) = p.items(notes);
        fee_margin(&ins, &outs, fee, headroom)
    };
    // The till keeps at least the price; the change or its overpay pays the margin.
    let min_overpay = 0;
    let mut margin = need(&exact);
    for _ in 0..MARGIN_ROUNDS {
        if margin == 0 {
            break;
        }
        let Ok(p) = solve_surplus(notes, price, free_slots, fee, min_denom, margin, min_overpay) else { break };
        if rank(&p) > rank(&exact) || p.inputs.len() > exact.inputs.len() {
            break;
        }
        let more = need(&p);
        if more <= margin {
            return Ok(p);
        }
        margin = more; // the margin plan has more items than the exact one: plan for them too
    }
    Ok(exact)
}

/// The card amount nearest `msat` (docs/TOPUP.md): at most `max_notes` notes, each a power of two
/// at least `min_denom` for which `is_denom` holds. The two candidates are the largest such value
/// at or below `msat` and the smallest at or above it; the nearer wins, a tie goes to the lower.
/// `None` when neither exists (e.g. `msat` below the smallest note).
pub fn nearest_card_amount(msat: u64, max_notes: u32, min_denom: u64, is_denom: impl Fn(u64) -> bool) -> Option<u64> {
    if msat == 0 || max_notes == 0 {
        return None;
    }
    // Work in units of the smallest note (a power of two), so no candidate has a note below it.
    let shift = min_denom.max(1).next_power_of_two().trailing_zeros();
    let valid = |units: u64| {
        let v = units.checked_shl(shift).filter(|v| v >> shift == units)?;
        (units > 0 && units.count_ones() <= max_notes && binary(v).into_iter().all(&is_denom)).then_some(v)
    };
    // Largest value at or below: the top `max_notes` set bits.
    let floor = msat >> shift;
    let mut down = 0u64;
    for b in (0..64).rev() {
        if down.count_ones() == max_notes {
            break;
        }
        if floor >> b & 1 == 1 {
            down |= 1 << b;
        }
    }
    // Smallest value at or above: add the lowest set bit until few enough remain.
    let mut up = Some(floor + u64::from(floor << shift != msat));
    while let Some(v) = up {
        if v.count_ones() <= max_notes {
            break;
        }
        up = v.checked_add(v & v.wrapping_neg());
    }
    let down = valid(down);
    let up = up.and_then(valid);
    match (down, up) {
        (Some(d), Some(u)) => Some(if msat - d <= u - msat { d } else { u }),
        (d, u) => d.or(u),
    }
}

/// Most prices [`exact_prices_near`] checks with [`solve`] on each side of the asked one.
const NEAR_TRIES: usize = 8;

/// For a price `notes` cannot pay within `free_slots` change notes: the nearest prices, in
/// multiples of `step` msat (1,000 for whole sats), that they can, as `(below, above)`. Kept cheap
/// for a refusal at the counter: every set of at most [`MAX_INPUTS`] notes gives the price it would
/// pay with no change (its value after the input fees, less the till's output fees, rounded down
/// to `step`), and the nearest of those on each side are checked with [`solve`], at most
/// [`NEAR_TRIES`] per side. `None` on a side where none checks.
pub fn exact_prices_near(notes: &[u64], price: u64, free_slots: usize, fee: FeeFn, min_denom: u64, step: u64) -> (Option<u64>, Option<u64>) {
    let step = step.max(1);
    let n = notes.len().min(MAX_CARD_NOTES);
    let net = |a: u64| a.saturating_sub(fee(a));
    let mut cands: Vec<u64> = vec![];
    let mut add = |avail: u64| {
        // The till's notes cost their own fees: t + fee_sum(binary(t)) == avail, near enough.
        let mut t = avail;
        for _ in 0..4 {
            t = avail.saturating_sub(fee_sum(&binary(t), fee));
        }
        let p = t / step * step;
        // The overpay slack lets a slightly lower price pay too; the step below is the next one.
        for c in [p, p.saturating_sub(step)] {
            if c > 0 && c != price {
                cands.push(c);
            }
        }
    };
    for i in 0..n {
        add(net(notes[i]));
        for j in i + 1..n {
            add(net(notes[i]) + net(notes[j]));
            for k in j + 1..n {
                add(net(notes[i]) + net(notes[j]) + net(notes[k]));
            }
        }
    }
    cands.sort_unstable();
    cands.dedup();
    let pays = |p: u64| solve(notes, p, free_slots, fee, min_denom).is_ok();
    let below = cands.iter().rev().filter(|c| **c < price).take(NEAR_TRIES).copied().find(|c| pays(*c));
    let above = cands.iter().filter(|c| **c > price).take(NEAR_TRIES).copied().find(|c| pays(*c));
    (below, above)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The drill's card (F6): ten notes, no slot for change. A price that needs change gets the
    /// nearest prices the card pays exactly, in whole sats, and each of them does pay.
    #[test]
    fn prices_near_a_refused_one_pay_with_no_change() {
        let notes = binary(30_000_000);
        for fee in [&zero as FeeFn, &base100, &ppm1000] {
            let m = min_econ(fee);
            for price in [10_000u64, 777_000, 5_000_000] {
                if solve(&notes, price, 0, fee, m).is_ok() {
                    continue;
                }
                let (below, above) = exact_prices_near(&notes, price, 0, fee, m, 1_000);
                assert!(below.is_some() || above.is_some(), "price {price}: no suggestion");
                for p in below.into_iter().chain(above) {
                    assert_eq!(p % 1_000, 0);
                    let plan = solve(&notes, p, 0, fee, m).expect("a suggestion pays");
                    assert!(plan.change.is_empty(), "{plan:?}");
                }
                assert!(below.is_none_or(|b| b < price) && above.is_none_or(|a| a > price));
            }
        }
        // Nothing to suggest from a card with no notes.
        assert_eq!(exact_prices_near(&[], 10_000, 0, &base100, 512, 1_000), (None, None));
    }

    fn zero(_: u64) -> u64 {
        0
    }
    /// `FeeConsensus::new(0)`: 100 msat base, no relative fee.
    fn base100(_: u64) -> u64 {
        100
    }
    /// `FeeConsensus::new(1000)`: 100 msat base plus 0.1 percent.
    fn ppm1000(a: u64) -> u64 {
        100 + a * 1000 / 1_000_000
    }

    /// What `FeeConsensus::min_economical_denomination` returns for these schedules.
    fn min_econ(fee: FeeFn) -> u64 {
        (fee(0) * 4).next_power_of_two()
    }

    fn check(notes: &[u64], price: u64, free: usize, fee: FeeFn) -> Plan {
        let p = solve(notes, price, free, fee, min_econ(fee)).expect("plan");
        assert!(p.balances(notes, fee), "unbalanced: {p:?}");
        assert!(p.change.len() <= free);
        assert!(p.overpay <= MAX_TILL_OVERPAY);
        assert_eq!(p.till.iter().sum::<u64>(), price + p.overpay);
        assert!(p.till.iter().chain(&p.change).all(|a| a.is_power_of_two()));
        p
    }

    /// A card filled with change (5 notes, 2 free slots): the charge spends more notes so the
    /// card keeps room for a 3-note top-up afterwards.
    #[test]
    fn nearly_full_card_is_tidied() {
        let notes = [65_536, 32_768, 16_384, 8_192, 2_048];
        let p = check(&notes, 5_000, 2, &zero);
        let free_after = 2 + p.inputs.len() - p.change.len();
        assert!(free_after >= ROOM_AFTER_CHARGE, "{p:?} leaves {free_after} free");
        assert!(p.inputs.len() > 1);
    }

    /// With room to spare the planner keeps the fastest plan: one input.
    #[test]
    fn roomy_card_is_not_tidied() {
        let notes = [65_536, 32_768, 16_384, 8_192, 2_048];
        let p = check(&notes, 5_000, 6, &zero);
        assert_eq!(p.inputs.len(), 1, "{p:?}");
    }

    #[test]
    fn fees_off_exact_split() {
        let notes = [1 << 20];
        let p = check(&notes, 1 << 19, 4, &zero);
        assert_eq!(p.till, vec![1 << 19]);
        assert_eq!(p.change, vec![1 << 19]);
        assert_eq!(p.overpay, 0);
    }

    #[test]
    fn fees_off_change_is_binary_decomposition() {
        let notes = [1 << 20];
        let p = check(&notes, 100_000, 16, &zero);
        // 1_048_576 - 100_000 = 948_576 has this many set bits; the solver may overpay to cut it.
        assert!(p.change.len() <= 948_576u64.count_ones() as usize);
    }

    #[test]
    fn fees_on_exact_balance() {
        let notes = [1 << 20];
        for price in [1u64, 999, 50_000, 100_000, 524_288, 900_000] {
            let p = check(&notes, price, 16, &base100);
            assert_eq!(p.inputs, vec![0]);
        }
    }

    #[test]
    fn ppm_fee_schedule_is_exact_too() {
        let notes = [1 << 20, 1 << 18, 1 << 16];
        for price in [1_000u64, 77_777, 300_000, 1_000_000] {
            check(&notes, price, 16, &ppm1000);
        }
    }

    #[test]
    fn free_slots_bound_the_change() {
        let notes = [1 << 20];
        // Half the note back: one change note is enough.
        let p = check(&notes, 1 << 19, 1, &zero);
        assert_eq!(p.change.len(), 1);
        // 948_576 msat of change is not one note and the till can absorb at most 2048.
        let r = solve(&notes, 100_000, 1, &zero, 1);
        assert!(matches!(r, Err(PlanError::NoExactPlan { .. })), "{r:?}");
        // No slots, no change: only an exact match works.
        let r = solve(&notes, 100_000, 0, &zero, 1);
        assert!(matches!(r, Err(PlanError::NoExactPlan { .. })), "{r:?}");
        assert_eq!(check(&[1 << 20], 1 << 20, 0, &zero).change.len(), 0);
    }

    #[test]
    fn small_change_is_absorbed_by_the_till() {
        // Change of under 2048 msat is cheaper to hand to the till than to mint.
        let notes = [1 << 20];
        let price = (1u64 << 20) - 100 - 100 - 1_500;
        let p = check(&notes, price, 4, &base100);
        assert!(p.change.len() <= 3 && p.overpay <= MAX_TILL_OVERPAY, "{p:?}");
    }

    #[test]
    fn insufficient_funds() {
        let r = solve(&[1024], 5000, 4, &base100, 512);
        assert!(matches!(r, Err(PlanError::Insufficient { .. })));
        // Enough for the price but not for the fees.
        let r = solve(&[1024], 1000, 4, &base100, 512);
        assert!(matches!(r, Err(PlanError::Insufficient { .. })), "{r:?}");
    }

    #[test]
    fn multiple_inputs_are_used_when_needed() {
        let notes = [1 << 16, 1 << 16, 1 << 16];
        let p = check(&notes, 150_000, 8, &base100);
        assert!(p.inputs.len() >= 3);
    }

    #[test]
    fn prefers_fewer_change_notes() {
        let notes = [1 << 20, 1 << 10];
        let p = check(&notes, 1 << 10, 16, &zero);
        // Spending the small note alone leaves no change.
        assert_eq!(p.change.len(), 0);
        assert_eq!(p.inputs, vec![1]);
    }

    #[test]
    fn cashout_is_exact_and_minimal() {
        for fee in [&zero as FeeFn, &base100, &ppm1000] {
            for a in [1u64 << 20, 1 << 17, 1 << 9, 1 << 12] {
                let (outs, _dust) = cashout(a, fee, min_econ(fee)).expect("cashout");
                assert_eq!(a, outs.iter().sum::<u64>() + fee(a) + fee_sum(&outs, fee), "a={a}");
                assert!(outs.iter().all(|x| x.is_power_of_two()));
            }
        }
        // zero fee: the whole note comes back as one note
        assert_eq!(cashout(1 << 20, &zero, 1), Some((vec![1 << 20], false)));
        // a note that cannot pay its fee
        assert_eq!(cashout(64, &base100, 512), None);
    }

    #[test]
    fn min_notes_edge_cases() {
        assert_eq!(min_notes(0, 4, &base100, 1), Some(vec![]));
        assert_eq!(min_notes(100, 4, &base100, 1), None); // one note would be 0 msat
        assert_eq!(min_notes(101, 4, &base100, 1), Some(vec![1]));
        assert_eq!(min_notes(7, 3, &zero, 1), Some(vec![4, 2, 1]));
        assert_eq!(min_notes(7, 2, &zero, 1), None);
        // splitting: 8 msat in 4 notes
        assert_eq!(min_notes(8, 4, &zero, 1).map(|v| v.len()), Some(1));
    }

    #[test]
    fn randomised_plans_always_balance() {
        // xorshift, no extra deps
        let mut s = 0x9E3779B97F4A7C15u64;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for fee in [&zero as FeeFn, &base100, &ppm1000] {
            for _ in 0..40 {
                let n = 1 + (next() % 6) as usize;
                let notes: Vec<u64> = (0..n).map(|_| 1u64 << (10 + next() % 12)).collect();
                let total: u64 = notes.iter().sum();
                let price = 1 + next() % (total / 2);
                if let Ok(p) = solve(&notes, price, 16, fee, min_econ(fee)) {
                    assert!(p.balances(&notes, fee), "{notes:?} {price} {p:?}");
                }
            }
        }
    }

    #[test]
    fn dust_free_plan_when_the_fee_allows_it() {
        // Zero fees but a 512 msat floor: every output is a multiple of 512, and the overpay
        // absorbs the 352 msat that the price is short of one.
        let p = solve(&[1 << 20], 100_000, 16, &zero, 512).unwrap();
        assert!(!p.dust);
        assert!(p.till.iter().chain(&p.change).all(|a| *a >= 512), "{p:?}");
        assert_eq!(p.overpay, 352);
        assert!(p.balances(&[1 << 20], &zero));
    }

    #[test]
    fn base_fee_forces_dust_and_says_so() {
        // 100 msat per item is never a multiple of 512, so a dust-free exact balance is
        // arithmetically impossible; the planner falls back to small tiers and flags it.
        let notes = [1u64 << 20];
        let p = solve(&notes, 100_000, 16, &base100, 512).unwrap();
        assert!(p.dust);
        assert!(p.balances(&notes, &base100));
        // The descending floors stop at 4 msat here: 100 msat fees balance on multiples of 4.
        assert!(p.till.iter().chain(&p.change).all(|a| *a >= 4), "{p:?}");
        // And the strict search alone finds nothing.
        assert!(matches!(solve_with(&notes, 100_000, 16, &base100, 512, 0, 0), Err(PlanError::NoExactPlan { .. })));
    }

    #[test]
    fn too_many_notes_is_an_error_not_a_panic() {
        let notes = vec![1024u64; MAX_CARD_NOTES + 1];
        assert!(matches!(solve(&notes, 1000, 4, &zero, 1), Err(PlanError::TooManyNotes(33))));
    }

    #[test]
    fn more_than_max_inputs_is_refused_by_name() {
        // Four 1024 msat notes are needed for 3500 msat; three may be spent.
        let notes = [1024u64; 8];
        assert_eq!(solve(&notes, 3500, 4, &zero, 1), Err(PlanError::TooManyInputs { price: 3500, max_inputs: MAX_INPUTS }));
        // Three cover 3000 msat.
        let p = check(&notes, 3000, 4, &zero);
        assert_eq!(p.inputs.len(), 3);
    }

    #[test]
    fn plans_never_exceed_max_inputs() {
        let notes = [1u64 << 16; 6];
        for price in [10_000u64, 70_000, 150_000, 190_000] {
            for fee in [&zero as FeeFn, &base100, &ppm1000] {
                if let Ok(p) = solve(&notes, price, 8, fee, min_econ(fee)) {
                    assert!(p.inputs.len() <= MAX_INPUTS, "{p:?}");
                    assert!(p.balances(&notes, fee));
                }
            }
        }
    }

    /// A full 32-slot card (a 131,072 sat load plus change of every size), and the same card with
    /// slots free, plan fast. The time limit is for release builds; debug builds only check the
    /// plans.
    #[test]
    fn full_card_of_32_notes_plans_quickly() {
        let mut notes: Vec<u64> = binary(131_072_000);
        let mut e = 10;
        while notes.len() < MAX_CARD_NOTES {
            notes.push(1 << e);
            e = if e == 26 { 10 } else { e + 1 };
        }
        let mut planned = 0;
        for fee in [&zero as FeeFn, &base100, &ppm1000] {
            for (held, free) in [(32usize, 0usize), (30, 2), (24, 8)] {
                let notes = &notes[..held];
                // Prices a few inputs pay exactly, and arbitrary ones.
                let prices = [notes[0] - 1_000, notes[1] + notes[5] - 3_000, notes[2] + notes[7] + notes[20] - 1_500, 777_000, 12_345_678, 29_999_000];
                for price in prices {
                    let t = std::time::Instant::now();
                    let r = solve(notes, price, free, fee, min_econ(fee));
                    let ms = t.elapsed().as_secs_f64() * 1e3;
                    if let Ok(p) = &r {
                        assert!(p.balances(notes, fee) && p.inputs.len() <= MAX_INPUTS && p.change.len() <= free, "{p:?}");
                        planned += 1;
                    }
                    if !cfg!(debug_assertions) {
                        assert!(ms < 50.0, "{price} msat on {held} notes took {ms:.1} ms ({r:?})");
                    }
                }
            }
        }
        assert!(planned >= 20, "only {planned} of 54 charges planned");
    }

    // ---- nearest_card_amount (docs/TOPUP.md)

    fn any(_: u64) -> bool {
        true
    }

    #[test]
    fn nearest_card_amount_matches_the_doc() {
        assert_eq!(nearest_card_amount(100_000, 3, 1, any), Some(100_352));
        assert_eq!(nearest_card_amount(1_000_000, 3, 1, any), Some(1_048_576));
        assert_eq!(nearest_card_amount(21_000, 3, 1, any), Some(20_992));
        assert_eq!(nearest_card_amount(10_000_000, 3, 1, any), Some(9_961_472));
    }

    #[test]
    fn nearest_card_amount_edges() {
        // Already a card amount: unchanged.
        assert_eq!(nearest_card_amount(1 << 16, 3, 1, any), Some(1 << 16));
        assert_eq!(nearest_card_amount((1 << 16) + (1 << 10) + 1, 3, 1, any), Some((1 << 16) + (1 << 10) + 1));
        // A tie goes to the lower: 6 msat in one note is 4 or 8.
        assert_eq!(nearest_card_amount(6, 1, 1, any), Some(4));
        // Nothing to round.
        assert_eq!(nearest_card_amount(0, 3, 1, any), None);
        assert_eq!(nearest_card_amount(5, 0, 1, any), None);
        // Below the smallest note: only the upward candidate is valid.
        assert_eq!(nearest_card_amount(700, 3, 1024, any), Some(1024));
        // A candidate with a note below the smallest note is not a card amount.
        assert_eq!(nearest_card_amount(1024 + 3, 3, 1024, any), Some(1024));
        assert_eq!(nearest_card_amount(3 * 1024 - 1, 3, 1024, any), Some(3 * 1024));
        // Notes the federation does not issue: only notes up to 2^20 here.
        let small = |n: u64| n <= 1 << 20;
        assert_eq!(nearest_card_amount((1 << 21) + 5, 3, 1, small), None);
        assert_eq!(nearest_card_amount((1 << 20) + (1 << 19) + (1 << 18) + (1 << 17), 3, 1, small), Some((1 << 20) + (1 << 19) + (1 << 18)));
        // Never more than max_notes notes, never far off.
        for msat in [1_234_567u64, 999_999, 50_000, 5_000_000, 77_777_777] {
            let v = nearest_card_amount(msat, 3, 1, any).unwrap();
            assert!(v.count_ones() <= 3, "{msat} -> {v}");
            assert!((v as f64 - msat as f64).abs() / (msat as f64) < 0.15, "{msat} -> {v}");
        }
        // Overflow at the top is not a candidate.
        assert_eq!(nearest_card_amount(u64::MAX, 3, 1, any), Some(u64::MAX - (u64::MAX >> 3)));
    }

    // ------------------------------------------------------------------ fee margin

    /// Fees of a plan's inputs and outputs under `headroom`.
    fn headroom_fees(p: &Plan, notes: &[u64], h: FeeHeadroom) -> u64 {
        let (ins, outs) = p.items(notes);
        ins.iter().chain(&outs).map(|a| h.fee(*a)).sum()
    }

    #[test]
    fn the_margin_is_headroom_fees_less_current_fees() {
        let (ins, outs) = ([65_536u64, 16_384], [32_768u64, 16_384, 1_024]);
        // Five items at 100 msat each, against no fees now.
        assert_eq!(fee_margin(&ins, &outs, &zero, FeeHeadroom::DEFAULT), 500);
        // Already at the default: nothing to keep.
        assert_eq!(fee_margin(&ins, &outs, &base100, FeeHeadroom::DEFAULT), 0);
        // Current fees above the headroom: floored at 0.
        assert_eq!(fee_margin(&ins, &outs, &ppm1000, FeeHeadroom::DEFAULT), 0);
        // The relative part of a ppm headroom: 0.1 % of 131,072 msat in, of 50,176 out.
        let h = FeeHeadroom { base_msat: 100, ppm: 1_000 };
        assert_eq!(fee_margin(&ins, &outs, &base100, h), 65 + 16 + 32 + 16 + 1);
        assert_eq!(fee_margin(&ins, &outs, &zero, FeeHeadroom::OFF), 0);
    }

    /// Zero fees, change to give: the change or the till's overpay pays the margin, the till gets
    /// at least the price, and the transaction still balances once fees rise to the headroom.
    #[test]
    fn the_margin_comes_out_of_the_change() {
        let notes = [65_536u64, 16_384];
        let exact = check(&notes, 50_000, 6, &zero);
        assert!(!exact.change.is_empty());
        let p = solve_with_margin(&notes, 50_000, 6, &zero, 1, FeeHeadroom::DEFAULT).unwrap();
        let (ins, outs) = p.items(&notes);
        assert!(p.surplus > 0 && p.surplus >= fee_margin(&ins, &outs, &zero, FeeHeadroom::DEFAULT), "{p:?}");
        assert!(p.balances(&notes, &zero), "{p:?}");
        assert_eq!(p.till.iter().sum::<u64>(), 50_000 + p.overpay);
        let in_sum: u64 = ins.iter().sum();
        assert_eq!(p.change.iter().sum::<u64>(), in_sum - 50_000 - p.overpay - p.surplus, "it balances: change, till, surplus");
        assert_eq!(in_sum, exact.inputs.iter().map(|&i| notes[i]).sum::<u64>());
        // Under the default fees the inputs still cover outputs and fees.
        assert!(in_sum >= outs.iter().sum::<u64>() + headroom_fees(&p, &notes, FeeHeadroom::DEFAULT), "{p:?}");
        assert!(p.change.len() <= exact.change.len() && p.inputs.len() <= exact.inputs.len());
    }

    /// The second e2e run's 30 sat charge (2026-11-07): one 32,768 msat note, 2,048 msat change,
    /// 720 msat overpay. Trimming the change note would take more notes; the overpay pays instead.
    #[test]
    fn with_change_the_overpay_pays_the_margin_when_the_change_cannot() {
        let notes = [32_768u64];
        let exact = check(&notes, 30_000, 6, &zero);
        assert_eq!((exact.change.clone(), exact.overpay), (vec![2_048], 720));
        let p = solve_with_margin(&notes, 30_000, 6, &zero, 1, FeeHeadroom::DEFAULT).unwrap();
        let (ins, outs) = p.items(&notes);
        assert!(p.surplus > 0 && p.surplus >= fee_margin(&ins, &outs, &zero, FeeHeadroom::DEFAULT), "{p:?}");
        assert!(p.balances(&notes, &zero), "{p:?}");
        assert!(p.till.iter().sum::<u64>() >= 30_000, "the till still gets the price: {p:?}");
        assert!(p.change.len() <= exact.change.len(), "{p:?}");
    }

    /// No change to give: the margin may come out of the till's overpay (the price is still paid).
    #[test]
    fn with_no_change_the_margin_comes_out_of_the_overpay() {
        let notes = [65_536u64];
        let exact = check(&notes, 65_536 - 2_000, 0, &zero);
        assert_eq!((exact.change.len(), exact.overpay), (0, 2_000));
        let p = solve_with_margin(&notes, 65_536 - 2_000, 0, &zero, 1, FeeHeadroom::DEFAULT).unwrap();
        assert!(p.change.is_empty() && p.surplus > 0, "{p:?}");
        assert!(p.balances(&notes, &zero));
        assert!(p.till.iter().sum::<u64>() >= 65_536 - 2_000);
        assert!(65_536 >= p.till.iter().sum::<u64>() + headroom_fees(&p, &notes, FeeHeadroom::DEFAULT), "{p:?}");
    }

    /// Neither change nor overpay covers a margin: the exact plan, with no margin.
    #[test]
    fn without_room_for_a_margin_the_charge_is_exact() {
        let notes = [65_536u64];
        let price = 65_536 - 100;
        let exact = check(&notes, price, 0, &zero);
        let p = solve_with_margin(&notes, price, 0, &zero, 1, FeeHeadroom::DEFAULT).unwrap();
        assert_eq!((p.surplus, &p), (0, &exact));
        // A margin that would need a second change note, with one slot free: not taken either.
        let notes = [65_536u64];
        let exact = check(&notes, 32_768, 1, &zero);
        assert_eq!(exact.change, vec![32_768]);
        assert_eq!(solve_with_margin(&notes, 32_768, 1, &zero, 1, FeeHeadroom::DEFAULT).unwrap(), exact);
    }

    /// The margin off, or fees already at the headroom: exactly [`solve`].
    #[test]
    fn no_margin_is_the_exact_plan() {
        let notes = [65_536u64, 32_768, 16_384, 8_192, 2_048];
        for price in [5_000u64, 50_000, 100_000] {
            let exact = solve(&notes, price, 6, &zero, 1).unwrap();
            assert_eq!(solve_with_margin(&notes, price, 6, &zero, 1, FeeHeadroom::OFF).unwrap(), exact);
            let m = min_econ(&base100);
            let exact = solve(&notes, price, 6, &base100, m).unwrap();
            assert_eq!(solve_with_margin(&notes, price, 6, &base100, m, FeeHeadroom::DEFAULT).unwrap(), exact);
        }
    }

    #[test]
    fn randomised_margin_plans_balance_and_cover_the_headroom() {
        let mut s = 0x2545F4914F6CDD1Du64;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let wide = FeeHeadroom { base_msat: 100, ppm: 1_000 };
        let mut with_margin = 0;
        for (fee, h) in [(&zero as FeeFn, FeeHeadroom::DEFAULT), (&zero, wide), (&base100, wide)] {
            for _ in 0..40 {
                let n = 1 + (next() % 6) as usize;
                let notes: Vec<u64> = (0..n).map(|_| 1u64 << (10 + next() % 12)).collect();
                let total: u64 = notes.iter().sum();
                let price = 1 + next() % (total / 2);
                let free = (next() % 8) as usize;
                let m = min_econ(fee);
                let Ok(exact) = solve(&notes, price, free, fee, m) else {
                    assert!(solve_with_margin(&notes, price, free, fee, m, h).is_err());
                    continue;
                };
                let p = solve_with_margin(&notes, price, free, fee, m, h).unwrap();
                assert!(p.balances(&notes, fee), "{notes:?} {price} {p:?}");
                assert!(p.till.iter().sum::<u64>() >= price && p.change.len() <= free);
                assert!(p.change.len() <= exact.change.len() && p.inputs.len() <= exact.inputs.len(), "{p:?} {exact:?}");
                if p.surplus > 0 {
                    with_margin += 1;
                    let (ins, outs) = p.items(&notes);
                    assert!(ins.iter().sum::<u64>() >= outs.iter().sum::<u64>() + headroom_fees(&p, &notes, h), "{notes:?} {price} {p:?}");
                } else {
                    assert_eq!(p, exact);
                }
            }
        }
        assert!(with_margin > 20, "{with_margin}");
    }
}
