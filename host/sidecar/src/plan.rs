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

use std::collections::HashSet;

/// Largest amount (msat) by which the till may be paid more than the price.
pub const MAX_TILL_OVERPAY: u64 = 2_048;
/// Largest denomination exponent the planner will use (2^50 msat is about 11,000 BTC).
pub const MAX_EXP: u32 = 50;
/// Cap on notes in a cashout.
pub const MAX_CASHOUT_NOTES: usize = 40;
/// Work cap for the exhaustive search, in fee evaluations / DFS nodes.
const BUDGET: u64 = 6_000_000;

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// Live notes cannot cover the price plus fees.
    Insufficient { have: u64, need: u64 },
    /// Funds exist, but no exact-balance layout fits the free slots and the overpay limit.
    NoExactPlan { price: u64, free_slots: usize },
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::Insufficient { have, need } => {
                write!(f, "insufficient funds on the card: have {have} msat, need at least {need} msat")
            }
            PlanError::NoExactPlan { price, free_slots } => write!(
                f,
                "no exact-balance spend of {price} msat with {free_slots} free slot(s); \
                 top up to get more free slots, or spend a different amount"
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
pub fn min_notes(target: u64, max_notes: usize, fee: FeeFn) -> Option<Vec<u64>> {
    if target == 0 {
        return Some(vec![]);
    }
    if let Some(c) = flat_fee(fee) {
        for k in 1..=max_notes {
            let Some(total) = target.checked_sub(c.checked_mul(k as u64)?) else { break };
            if total == 0 || total < k as u64 {
                continue;
            }
            if (total.count_ones() as usize) <= k {
                return Some(split_to(binary(total), k));
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
        if dfs(top, k, target, fee, &mut failed, &mut out, &mut nodes) {
            out.sort_unstable_by(|a, b| b.cmp(a));
            return Some(out);
        }
    }
    None
}

/// Split the largest splittable note in half until there are exactly `k` notes.
fn split_to(mut notes: Vec<u64>, k: usize) -> Vec<u64> {
    while notes.len() < k {
        notes.sort_unstable_by(|a, b| b.cmp(a));
        let big = notes[0];
        debug_assert!(big > 1, "caller guarantees total >= k");
        notes[0] = big / 2;
        notes.push(big / 2);
    }
    notes.sort_unstable_by(|a, b| b.cmp(a));
    notes
}

fn dfs(
    e: i32,
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
    if e < 0 {
        return false;
    }
    *nodes += 1;
    if *nodes > BUDGET || failed.contains(&(e, items, rem)) {
        return false;
    }
    let w_hi = weight(1u64 << e, fee);
    let w_lo = weight(1, fee);
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
        if dfs(e - 1, items - c, rem - used, fee, failed, out, nodes) {
            return true;
        }
        out.truncate(out.len() - c);
    }
    failed.insert((e, items, rem));
    false
}

/// Plan a cashout of one card note: host-owned notes whose creation cost is exactly
/// `note_amount - fee(note_amount)`. `None` if the note cannot even pay its own fee.
pub fn cashout(note_amount: u64, fee: FeeFn) -> Option<Vec<u64>> {
    let target = note_amount.checked_sub(fee(note_amount))?;
    if target == 0 {
        return None;
    }
    min_notes(target, MAX_CASHOUT_NOTES, fee)
}

/// Choose inputs, till notes and change notes for a card spend of `price` msat.
///
/// `notes` are the amounts of the card's live notes. `free_slots` bounds the number of change
/// notes. Objective, in order: fewest change notes, fewest inputs (each costs one card
/// signature, about a second on silicon), smallest overpay.
pub fn solve(notes: &[u64], price: u64, free_slots: usize, fee: FeeFn) -> Result<Plan, PlanError> {
    if price == 0 {
        return Err(PlanError::NoExactPlan { price, free_slots });
    }
    let have: u64 = notes.iter().sum();
    // Cheapest conceivable cost of the price: the price itself paid with one-note-per-bit fees.
    let min_need = price + fee_sum(&binary(price), fee);
    if have < min_need {
        return Err(PlanError::Insufficient { have, need: min_need });
    }

    // Till cost table: for each overpay 0..=MAX, what the till outputs cost in total.
    let till_cost: Vec<u64> = (0..=MAX_TILL_OVERPAY)
        .map(|o| {
            let t = price + o;
            t + fee_sum(&binary(t), fee)
        })
        .collect();
    let min_till = till_cost.iter().copied().min().expect("non-empty");

    let n = notes.len();
    assert!(n <= 20, "a card has at most 16 slots");
    let flat = flat_fee(fee);
    let mut budget = BUDGET;
    let mut best: Option<(usize, usize, u64, Vec<usize>)> = None; // (change notes, inputs, overpay, indices)
    let mut first_feasible_size: Option<usize> = None;

    'sizes: for size in 1..=n {
        if let Some(s) = first_feasible_size {
            // One size beyond the first feasible one is enough: more inputs rarely help.
            if size > s + 1 {
                break;
            }
        }
        // Enumerate combinations of `size` notes with Gosper's hack.
        let mut mask: u32 = (1u32 << size) - 1;
        while mask < (1u32 << n) {
            let idx: Vec<usize> = (0..n).filter(|i| mask >> i & 1 == 1).collect();
            let gross: u64 = idx.iter().map(|&i| notes[i]).sum();
            let in_fees: u64 = idx.iter().map(|&i| fee(notes[i])).sum();
            if let Some(avail) = gross.checked_sub(in_fees) {
                if avail >= min_till {
                    for (o, tc) in till_cost.iter().enumerate() {
                        let Some(r) = avail.checked_sub(*tc) else { continue };
                        if budget == 0 {
                            break 'sizes;
                        }
                        let cap = match &best {
                            Some((cc, ..)) => (*cc).min(free_slots),
                            None => free_slots,
                        };
                        budget = budget.saturating_sub(1 + if flat.is_some() { 0 } else { 64 });
                        let Some(change) = min_notes(r, cap, fee) else { continue };
                        let cand = (change.len(), size, o as u64);
                        let better = match &best {
                            None => true,
                            Some((cc, ni, ov, _)) => cand < (*cc, *ni, *ov),
                        };
                        if better {
                            first_feasible_size.get_or_insert(size);
                            best = Some((change.len(), size, o as u64, idx.clone()));
                        }
                    }
                }
            }
            // next combination
            let c = mask & mask.wrapping_neg();
            let r = mask + c;
            mask = (((r ^ mask) >> 2) / c) | r;
        }
    }

    let Some((_, _, overpay, idx)) = best else {
        return Err(PlanError::NoExactPlan { price, free_slots });
    };
    let gross: u64 = idx.iter().map(|&i| notes[i]).sum();
    let in_fees: u64 = idx.iter().map(|&i| fee(notes[i])).sum();
    let till = binary(price + overpay);
    let r = gross - in_fees - (price + overpay) - fee_sum(&till, fee);
    let change = min_notes(r, free_slots, fee).expect("found during search");
    Ok(Plan { inputs: idx, till, change, overpay })
}

impl Plan {
    /// Check the exact-balance invariant.
    pub fn balances(&self, notes: &[u64], fee: FeeFn) -> bool {
        let ins: Vec<u64> = self.inputs.iter().map(|&i| notes[i]).collect();
        let outs: Vec<u64> = self.till.iter().chain(&self.change).copied().collect();
        ins.iter().sum::<u64>() == outs.iter().sum::<u64>() + fee_sum(&ins, fee) + fee_sum(&outs, fee)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn check(notes: &[u64], price: u64, free: usize, fee: FeeFn) -> Plan {
        let p = solve(notes, price, free, fee).expect("plan");
        assert!(p.balances(notes, fee), "unbalanced: {p:?}");
        assert!(p.change.len() <= free);
        assert!(p.overpay <= MAX_TILL_OVERPAY);
        assert_eq!(p.till.iter().sum::<u64>(), price + p.overpay);
        assert!(p.till.iter().chain(&p.change).all(|a| a.is_power_of_two()));
        p
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
        let r = solve(&notes, 100_000, 1, &zero);
        assert!(matches!(r, Err(PlanError::NoExactPlan { .. })), "{r:?}");
        // No slots, no change: only an exact match works.
        let r = solve(&notes, 100_000, 0, &zero);
        assert!(matches!(r, Err(PlanError::NoExactPlan { .. })), "{r:?}");
        assert_eq!(check(&[1 << 20], 1 << 20, 0, &zero).change.len(), 0);
    }

    #[test]
    fn small_change_is_absorbed_by_the_till() {
        // Change of under 2048 msat is cheaper to hand to the till than to mint.
        let notes = [1 << 20];
        let price = (1u64 << 20) - 100 - 100 - 1_500;
        let p = check(&notes, price, 4, &base100);
        assert!(p.change.len() <= 1, "{p:?}");
    }

    #[test]
    fn insufficient_funds() {
        let r = solve(&[1024], 5000, 4, &base100);
        assert!(matches!(r, Err(PlanError::Insufficient { .. })));
        // Enough for the price but not for the fees.
        let r = solve(&[1024], 1000, 4, &base100);
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
                let outs = cashout(a, fee).expect("cashout");
                assert_eq!(a, outs.iter().sum::<u64>() + fee(a) + fee_sum(&outs, fee), "a={a}");
                assert!(outs.iter().all(|x| x.is_power_of_two()));
            }
        }
        // zero fee: the whole note comes back as one note
        assert_eq!(cashout(1 << 20, &zero), Some(vec![1 << 20]));
        // a note that cannot pay its fee
        assert_eq!(cashout(64, &base100), None);
    }

    #[test]
    fn min_notes_edge_cases() {
        assert_eq!(min_notes(0, 4, &base100), Some(vec![]));
        assert_eq!(min_notes(100, 4, &base100), None); // one note would be 0 msat
        assert_eq!(min_notes(101, 4, &base100), Some(vec![1]));
        assert_eq!(min_notes(7, 3, &zero), Some(vec![4, 2, 1]));
        assert_eq!(min_notes(7, 2, &zero), None);
        // splitting: 8 msat in 4 notes
        assert_eq!(min_notes(8, 4, &zero).map(|v| v.len()), Some(1));
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
                if let Ok(p) = solve(&notes, price, 16, fee) {
                    assert!(p.balances(&notes, fee), "{notes:?} {price} {p:?}");
                }
            }
        }
    }
}
