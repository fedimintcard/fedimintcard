//! Upper bounds, before anything is funded, on the federation's own fees for one transaction
//! from the till's wallet, derived from the pinned client (fedimint v0.13) rather than estimated:
//!
//! - **Items.** A transaction the client builds is refused before submission when it encodes to
//!   more than `Transaction::MAX_TX_SIZE` bytes (`fedimint-client/src/client.rs`,
//!   `finalize_and_submit_transaction`): nothing is funded. Every mint input carries a 48-byte
//!   BLS signature (and a 33-byte nonce), every mint output a 48-byte blinded nonce
//!   (`fedimint-mint-common`), so a transaction holds at most `MAX_TX_SIZE / 48` mint items,
//!   whatever the wallet's inventory. This includes the up to 20 notes `consolidate_notes` adds.
//!   The wallet's note count cannot tighten it: notes credited while the funding runs (a top-up's
//!   claim, a reissue landing) are not under the wallet's admission guard.
//! - **Per item.** The mint charges `base + floor(ppm × face / 1e6)` per input and per output
//!   (`MintClientModule::input_fee`/`output_fee`, `FeeConsensus::fee_msats`). The sum of floors is
//!   at most the floor of the sum, so a transaction's mint fees are at most
//!   `base × items + ppm × (inputs + change) / 1e6`.
//! - **Face.** The inputs are at most the balance `B`; the change is what the inputs leave after
//!   the spend and the fees, so `inputs + change ≤ 2B − spend`. A note credited during the funding
//!   adds its whole value to the balance and at most `2 × ppm / 1e6` of it (0.2 %) to the fees:
//!   it pays for itself.
//! - **Dust.** The change layout (`represent_amount`) leaves less than `fee(1 msat) = base`.
//! - **Module.** ln (v1) charges a fixed `contract_output` on the outgoing contract; lnv2 charges
//!   `base + floor(ppm × contract / 1e6)`. The contract is the invoice plus the gateway's fee,
//!   which the wallet refuses above the payout's cap.
//!
//! A schedule that is not `base + floor(ppm × amount / 1e6)` (checked against the mint's own fee
//! function) cannot be bounded this way: Lightning funding is refused for it, with the reason.
//! The client's exact quote (`send_fee_quote`, a dry run over the current inventory) is checked
//! against the bound as a second opinion; a quote above it refuses funding too.

use fedimint_core::transaction::Transaction;

/// The fewest bytes any mint input or output encodes to (one G1 point).
pub const MIN_ITEM_BYTES: u64 = 48;

/// The most mint inputs and outputs one transaction can hold.
pub fn max_items() -> u64 {
    Transaction::MAX_TX_SIZE as u64 / MIN_ITEM_BYTES
}

/// A fee of `base + floor(ppm × amount / 1e6)` per item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Linear {
    pub base_msat: u64,
    pub ppm: u64,
}

impl Linear {
    pub const ZERO: Self = Self { base_msat: 0, ppm: 0 };

    /// The schedule behind `fee`, a module's own fee function, or why it is not one this module
    /// bounds.
    pub fn from_fee_fn(fee: impl Fn(u64) -> u64) -> Result<Self, String> {
        let base_msat = fee(0);
        let ppm = fee(1_000_000).checked_sub(base_msat).ok_or("the fee schedule falls with the amount")?;
        if ppm > 1_000_000 {
            return Err(format!("the fee schedule charges {ppm} parts per million, more than the amount itself"));
        }
        let s = Self { base_msat, ppm };
        for x in [1u64, 999, 1_000, 65_536, 999_999, 1_000_001, 123_456_789, 1 << 32, 1 << 40] {
            if fee(x) != s.fee(x) {
                return Err(format!("the fee schedule is not a base fee plus a proportional part (at {x} msat)"));
            }
        }
        Ok(s)
    }

    pub fn fee(&self, amount_msat: u64) -> u64 {
        self.base_msat.saturating_add((amount_msat as u128 * self.ppm as u128 / 1_000_000) as u64)
    }

    /// `ppm × value / 1e6`, rounded up.
    fn proportional_ceil(&self, value_msat: u128) -> u64 {
        (value_msat * self.ppm as u128).div_ceil(1_000_000).min(u64::MAX as u128) as u64
    }
}

/// The Lightning module's fees on a contract: as an output (paying), as an input (a top-up's
/// claim).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LnSchedule {
    /// ln (v1): `contract_input` / `contract_output`, whatever the amount.
    V1 { contract_input_msat: u64, contract_output_msat: u64 },
    /// lnv2: per input and per output, on the contract amount.
    V2(Linear),
}

impl LnSchedule {
    pub fn output_fee(&self, contract_msat: u64) -> u64 {
        match self {
            LnSchedule::V1 { contract_output_msat, .. } => *contract_output_msat,
            LnSchedule::V2(l) => l.fee(contract_msat),
        }
    }

    pub fn input_fee(&self, contract_msat: u64) -> u64 {
        match self {
            LnSchedule::V1 { contract_input_msat, .. } => *contract_input_msat,
            LnSchedule::V2(l) => l.fee(contract_msat),
        }
    }
}

/// The wallet's fee schedules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedules {
    pub mint: Linear,
    /// `None` without a Lightning module, or with one whose fees cannot be bounded.
    pub ln: Option<LnSchedule>,
    /// Why the Lightning module's fees cannot be bounded, when it has some.
    pub ln_problem: Option<String>,
}

impl Schedules {
    /// Fees off everywhere (tests, federations with zero fees).
    pub fn zero() -> Self {
        Self { mint: Linear::ZERO, ln: Some(LnSchedule::V1 { contract_input_msat: 0, contract_output_msat: 0 }), ln_problem: None }
    }

    fn ln(&self) -> Result<LnSchedule, String> {
        self.ln.ok_or_else(|| self.ln_problem.clone().unwrap_or_else(|| "this federation has no Lightning module the till can use".into()))
    }
}

/// An upper bound on the mint fees (inputs, change outputs, dust) of one transaction from a wallet
/// holding `balance_msat` whose explicit non-mint outputs are worth `spend_msat`.
pub fn mint_fee_bound(mint: &Linear, balance_msat: u64, spend_msat: u64) -> u64 {
    let value = (2 * balance_msat as u128).saturating_sub(spend_msat as u128);
    mint.base_msat
        .saturating_mul(max_items())
        .saturating_add(mint.proportional_ceil(value))
        // Dust: less than `fee(1 msat)`, which is the base.
        .saturating_add(mint.base_msat)
}

/// An upper bound on the federation's fees for funding an outgoing contract worth `contract_msat`:
/// the mint's and the Lightning module's. `Err` without a Lightning module.
pub fn ln_send_fee_bound(s: &Schedules, balance_msat: u64, contract_msat: u64) -> Result<u64, String> {
    let ln = s.ln()?;
    Ok(mint_fee_bound(&s.mint, balance_msat, contract_msat).saturating_add(ln.output_fee(contract_msat)))
}

/// [`ln_send_fee_bound`] for every contract amount in `lo..=hi`: the bound is affine in the
/// contract (the mint part falls, the module part rises), so its largest value is at an end.
pub fn ln_send_fee_bound_over(s: &Schedules, balance_msat: u64, lo: u64, hi: u64) -> Result<u64, String> {
    Ok(ln_send_fee_bound(s, balance_msat, lo)?.max(ln_send_fee_bound(s, balance_msat, hi)?))
}

/// An upper bound on the fees of a spend the wallet will make later for an obligation of
/// `amount_msat` (a top-up's claim and its load, a pay's funding, a refund's load): two
/// transactions, each bounded as above, and the Lightning module's fees on one contract as an
/// input and as an output. `Err` when the Lightning module's fees cannot be bounded: what such an
/// obligation will cost is unknown.
pub fn obligation_fee_bound(s: &Schedules, balance_msat: u64, amount_msat: u64) -> Result<u64, String> {
    if amount_msat == 0 {
        return Ok(0);
    }
    if let Some(p) = &s.ln_problem {
        return Err(p.clone());
    }
    let ln = s.ln.map_or(0, |l| l.output_fee(amount_msat).saturating_add(l.input_fee(amount_msat)));
    Ok(mint_fee_bound(&s.mint, balance_msat, 0).saturating_mul(2).saturating_add(ln))
}

#[cfg(test)]
mod tests {
    use fedimint_core::Amount;
    use fedimint_mint_common::config::FeeConsensus;

    use super::*;

    fn mint_of(f: &FeeConsensus) -> Linear {
        Linear::from_fee_fn(|x| f.fee(Amount::from_msats(x)).msats).unwrap()
    }

    #[test]
    fn a_transaction_holds_at_most_a_thousand_and_forty_one_mint_items() {
        assert_eq!(Transaction::MAX_TX_SIZE, 49_968);
        assert_eq!(max_items(), 1_041);
    }

    #[test]
    fn the_mints_own_schedules_are_read_exactly_and_others_are_refused() {
        assert_eq!(mint_of(&FeeConsensus::zero()), Linear::ZERO);
        assert_eq!(mint_of(&FeeConsensus::new(0).unwrap()), Linear { base_msat: 100, ppm: 0 });
        assert_eq!(mint_of(&FeeConsensus::new(1_000).unwrap()), Linear { base_msat: 100, ppm: 1_000 });
        // A stepped or capped schedule is not one this bound holds for.
        assert!(Linear::from_fee_fn(|x| if x > 1_000_000 { 500 } else { 100 }).is_err());
        assert!(Linear::from_fee_fn(|x| 100 + x / 2_000 + u64::from(x % 7 == 0)).is_err());
        assert!(Linear::from_fee_fn(|x| 100u64.saturating_sub(x)).is_err());
    }

    /// Every way the client can lay out a funding of `contract` from a wallet of `balance`, within
    /// the transaction's size, costs no more than the bound, on every schedule the mint allows.
    #[test]
    fn the_bound_covers_the_worst_layout_on_mixed_fee_schedules() {
        let schedules = [
            FeeConsensus::zero(),
            FeeConsensus::new(0).unwrap(),
            FeeConsensus::new(1).unwrap(),
            FeeConsensus::new(250).unwrap(),
            FeeConsensus::new(1_000).unwrap(),
        ];
        let lns = [
            LnSchedule::V1 { contract_input_msat: 0, contract_output_msat: 0 },
            LnSchedule::V1 { contract_input_msat: 500, contract_output_msat: 1_000 },
            LnSchedule::V2(Linear { base_msat: 1_000, ppm: 1_000 }),
        ];
        for fc in &schedules {
            let mint = mint_of(fc);
            for ln in lns {
                let s = Schedules { mint, ln: Some(ln), ln_problem: None };
                for (balance, contract) in [(10_000_000u64, 1_000_000u64), (100_000_000, 99_000_000), (1 << 40, 1 << 20), (5_000, 1_000)] {
                    let bound = ln_send_fee_bound(&s, balance, contract).unwrap();
                    // Worst layout: the most items the transaction holds, spending the whole
                    // balance as inputs, returning everything left as change.
                    let items = max_items();
                    let n_in = items / 2;
                    let n_out = items - n_in;
                    let per_in = balance / n_in;
                    let input_fees: u64 = (0..n_in).map(|_| fc.fee(Amount::from_msats(per_in)).msats).sum();
                    let change = balance.saturating_sub(contract).saturating_sub(input_fees);
                    let per_out = change / n_out;
                    let output_fees: u64 = (0..n_out).map(|_| fc.fee(Amount::from_msats(per_out)).msats).sum();
                    let worst = input_fees + output_fees + mint.base_msat + ln.output_fee(contract);
                    assert!(worst <= bound, "{fc:?} {ln:?} {balance} {contract}: {worst} > {bound}");
                }
            }
        }
    }

    #[test]
    fn typical_schedules_hold_back_little() {
        // 100 msat per item, no proportional part: about 104 sat, whatever the amount.
        let v1 = LnSchedule::V1 { contract_input_msat: 0, contract_output_msat: 0 };
        let s = Schedules { mint: Linear { base_msat: 100, ppm: 0 }, ln: Some(v1), ln_problem: None };
        assert_eq!(ln_send_fee_bound(&s, 1 << 40, 1 << 30).unwrap(), 100 * 1_041 + 100);
        assert_eq!(obligation_fee_bound(&s, 1 << 40, 5_000).unwrap(), 2 * (100 * 1_041 + 100));
        // Fees off: nothing.
        let off = Schedules::zero();
        assert_eq!(ln_send_fee_bound(&off, 1 << 40, 1 << 30).unwrap(), 0);
        assert_eq!(obligation_fee_bound(&off, 1 << 40, 5_000).unwrap(), 0);
        // No Lightning module: no Lightning funding. One whose fees cannot be bounded: refused
        // with its reason, and what its obligations will cost is unknown.
        assert!(ln_send_fee_bound(&Schedules { mint: Linear::ZERO, ln: None, ln_problem: None }, 1, 1).is_err());
        let bad = Schedules { mint: Linear::ZERO, ln: None, ln_problem: Some("stepped lnv2 fees".into()) };
        assert_eq!(ln_send_fee_bound(&bad, 1, 1).unwrap_err(), "stepped lnv2 fees");
        assert!(obligation_fee_bound(&bad, 1, 1).is_err());
        assert_eq!(obligation_fee_bound(&bad, 1, 0).unwrap(), 0);
    }

    #[test]
    fn the_bound_over_a_range_is_its_largest_end() {
        let s = Schedules {
            mint: Linear { base_msat: 100, ppm: 1_000 },
            ln: Some(LnSchedule::V2(Linear { base_msat: 1_000, ppm: 1_000 })),
            ln_problem: None,
        };
        let b = 50_000_000;
        let over = ln_send_fee_bound_over(&s, b, 1_000, 40_000_000).unwrap();
        for c in [1_000, 1_000_000, 20_000_000, 40_000_000] {
            assert!(ln_send_fee_bound(&s, b, c).unwrap() <= over);
        }
    }
}
