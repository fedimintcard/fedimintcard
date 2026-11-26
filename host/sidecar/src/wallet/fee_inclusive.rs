//! Price the entire receipt and card load before issuing the one customer invoice.
//! The wallet can receive other notes between quote, payment and tap. The bound
//! therefore depends on the mint configuration and transaction format, never on
//! an assumption that today's note inventory or balance will still be there.

use anyhow::{Context, Result, anyhow, bail, ensure};
use fedimint_core::Amount;
use fedimint_core::transaction::Transaction;
use fedimint_lnv2_client::common::gateway_api::PaymentFee;

use super::{MintVersion, Wallet};
use crate::payout::fees::{Linear, LnSchedule};
use crate::topup::{MAX_TOPUP_MSAT, MAX_TOPUP_NOTES, TopupInvoice, WalletLightning};

/// A mint input has a 33-byte spend key, 48-byte blind signature and a separate
/// 64-byte spend signature in the client's NaiveMultisig transaction. A mint
/// output has at least its 48-byte blinded nonce in both protocol versions.
const INPUT_BYTES: u64 = 33 + 48 + 64;
const OUTPUT_BYTES: u64 = 48;

struct MintLayout {
    fee: Linear,
    smallest: u64,
    largest: u64,
    /// Maximum refill outputs plus greedy outputs strictly below the largest tier.
    change_allowance: u64,
    /// Face of newly issued wallet change that v1 could decline to spend.
    uneconomical_change: u64,
    v1: bool,
}

impl MintLayout {
    fn new(fee: Linear, tiers: &[u64], version: MintVersion) -> Result<Self> {
        let &smallest = tiers.first().context("the mint has no change denominations")?;
        let largest = *tiers.last().unwrap();
        ensure!(version != MintVersion::V1 || smallest == 1, "the v1 mint has no 1-msat change denomination");
        ensure!(smallest > 0 && largest > fee.fee(largest), "the mint has no spendable change denomination");
        let refill = match version {
            MintVersion::V1 => 2,
            MintVersion::V2 => 3,
        };
        // Both pinned clients first refill their target count per tier, then make
        // change greedily from largest to smallest, including each output's own
        // fee. Below tier j+1 the greedy remainder is less than cost(j+1).
        let mut change_allowance = refill * tiers.len() as u64;
        let mut uneconomical_change = 0u64;
        for pair in tiers.windows(2) {
            let (lower, upper) = (pair[0], pair[1]);
            ensure!(lower < upper, "invalid mint change denominations");
            let cost = lower.checked_add(fee.fee(lower)).context("mint denomination cost is too large")?;
            let next = upper.checked_add(fee.fee(upper)).context("mint denomination cost is too large")?;
            let greedy = (next - 1) / cost;
            change_allowance = change_allowance.checked_add(greedy).context("too many mint denominations")?;
            if lower <= fee.fee(lower) {
                // Mint v1 may issue a small note it cannot select as a funding
                // input later. Reserve its entire face on each transaction.
                let stuck = (refill + greedy).checked_mul(lower).context("mint denomination reserve is too large")?;
                uneconomical_change = uneconomical_change.checked_add(stuck).context("mint denomination reserve is too large")?;
            }
        }
        Ok(Self { fee, smallest, largest, change_allowance, uneconomical_change, v1: version == MintVersion::V1 })
    }

    /// If I mint inputs fund the transaction, the mint's largest change notes
    /// can number at most I + floor(external / largest): every input is at most
    /// that denomination. The SDK change algorithm bounds all remaining
    /// outputs by change_allowance. With O <= I + K and 145I + 48O <= max_bytes,
    /// I + O <= (2*max_bytes + (145-48)*K)/(145+48).
    fn item_limit(&self, external: u64, card_notes: u64, input_cap: u64) -> u64 {
        let bytes = Transaction::MAX_TX_SIZE as u64;
        let k = self.change_allowance.saturating_add(external / self.largest).saturating_add(card_notes).min(bytes / OUTPUT_BYTES);
        let sized = (2 * bytes + (INPUT_BYTES - OUTPUT_BYTES) * k) / (INPUT_BYTES + OUTPUT_BYTES);
        sized.min(bytes / OUTPUT_BYTES).min(2 * input_cap + k)
    }

    fn tx_cost(&self, external: u64, ln_input_fee: u64, card_notes: u64) -> Result<u64> {
        if self.fee == Linear::ZERO {
            // Every wallet input and card output is a multiple of the smallest
            // tier. With no mint fees only the incoming contract can leave dust.
            return Ok(external.saturating_sub(ln_input_fee) % self.smallest);
        }
        let mut input_cap = Transaction::MAX_TX_SIZE as u64 / INPUT_BYTES;
        if self.v1 && external != 0 && external.saturating_sub(ln_input_fee) >= self.fee.base_msat.saturating_mul(20) {
            // A v1 claim consolidates at most 20 notes. Even if every one is
            // uneconomical, the contract covers its shortfall; create_sufficient_input
            // requests zero and does not draw additional funding notes.
            input_cap = 20;
        }
        let items = self.item_limit(external, card_notes, input_cap);
        let faces = 2 * u128::from(input_cap) * u128::from(self.largest) + u128::from(external);
        let proportional = (faces * u128::from(self.fee.ppm)) / 1_000_000;
        // After the greedy pass the wallet can overpay at most one smallest
        // note's cost less one msat, independently of how many notes it held.
        let dust = u128::from(self.smallest) + u128::from(self.fee.fee(self.smallest)) - 1;
        (u128::from(self.fee.base_msat) * u128::from(items) + proportional + dust)
            .try_into()
            .context("this federation's top-up fee bound is too large")
    }
}

struct Pricing {
    mint: MintLayout,
    ln: LnSchedule,
    face: u64,
    load: u64,
}

#[derive(Debug)]
struct Quote {
    gross: u64,
    contract: u64,
    claim_fee: u64,
    load_fee: u64,
}

impl Pricing {
    fn new(mint: MintLayout, ln: LnSchedule, face: u64) -> Result<Self> {
        ensure!(face > 0 && face <= MAX_TOPUP_MSAT && face.count_ones() <= MAX_TOPUP_NOTES, "unsupported card top-up amount");
        let card_notes = crate::plan::binary(face);
        let mut holder_margin = 0u64;
        for tier in &card_notes {
            let margin = mint.fee.fee(*tier).checked_mul(2).context("card load fee overflow")?;
            holder_margin = holder_margin.checked_add(margin).context("card load fee overflow")?;
        }
        // The holder checks twice the exact card-note fees. The wallet may
        // also have to pay for funding inputs, change outputs and rounding.
        let load = mint
            .tx_cost(0, 0, card_notes.len() as u64)?
            .max(holder_margin)
            .checked_add(mint.uneconomical_change)
            .context("card load fee overflow")?;
        Ok(Self { mint, ln, face, load })
    }

    fn solve(&self, gateway: Linear) -> Result<Quote> {
        let mut gross = self.face;
        for _ in 0..64 {
            let gateway_fee = gateway.fee(gross);
            let contract = gross.saturating_sub(gateway_fee);
            let ln_input = self.ln.input_fee(contract);
            let claim_fee = ln_input
                .checked_add(self.mint.tx_cost(contract, ln_input, 0)?)
                .and_then(|cost| cost.checked_add(self.mint.uneconomical_change))
                .context("advance fee bound overflow")?;
            // With no mint fee the wallet rounds the net claim down to its
            // smallest tier. The promised card face is already a multiple of
            // that tier: covering its exact face and LN fee avoids charging
            // dust from a smaller trial amount that will never be invoiced.
            let required_claim = if self.mint.fee == Linear::ZERO { ln_input } else { claim_fee };
            let allowance =
                gateway_fee.checked_add(required_claim).and_then(|f| f.checked_add(self.load)).context("advance fee bound overflow")?;
            // A fee larger than the entire supported range of top-ups is not
            // a practical quote. This limit caps the fee itself, not card
            // principal plus fee, including on the largest supported card load.
            ensure!(allowance <= MAX_TOPUP_MSAT, "the federation's safe fee bound exceeds the supported top-up fee limit");
            let needed = self.face.checked_add(allowance).context("invoice amount overflow")?;
            if needed <= gross {
                ensure!(
                    contract.checked_sub(claim_fee).is_some_and(|net| net >= self.face + self.load),
                    "the claim cannot fund the card and its load"
                );
                return Ok(Quote { gross, contract, claim_fee, load_fee: self.load });
            }
            gross = needed;
        }
        bail!("the federation fees do not allow a bounded card top-up")
    }
}

impl Wallet {
    fn topup_mint_layout(&self, fee: Linear) -> Result<MintLayout> {
        let tiers = match self.mint_version {
            MintVersion::V1 => self.mint_cfg.tbs_pks.tiers().map(|a| a.msats).collect::<Vec<_>>(),
            MintVersion::V2 => fedimint_mintv2_common::config::client_denominations().map(|d| d.amount().msats).collect(),
        };
        ensure!(
            tiers.iter().all(|tier| self.mint_cfg.tbs_pks.get(Amount::from_msats(*tier)).is_some()),
            "the mint is missing a wallet denomination"
        );
        MintLayout::new(fee, &tiers, self.mint_version)
    }

    /// Largest face an accepted wallet transaction can return as new notes
    /// the SDK cannot spend. Other customers' spendable cover must survive
    /// this amount as well as the mint's explicit fees and rounding.
    pub fn uneconomical_change_bound_msat(&self) -> Result<u64> {
        if self.mint_version == MintVersion::V2 {
            return Ok(0);
        }
        let mint_fee = Linear::from_fee_fn(|x| self.fee(x)).map_err(anyhow::Error::msg)?;
        Ok(self.topup_mint_layout(mint_fee)?.uneconomical_change)
    }

    /// Nominal mint balance and face of notes the funding selector can use.
    /// Read both from the same database snapshot. All v2 wallet change tiers
    /// are economical; v1 can hold notes worth no more than their spend fee.
    pub async fn mint_balances_msat(&self) -> Result<(u64, u64)> {
        if self.mint_version == MintVersion::V2 {
            let balance = self.balance_msat().await?;
            return Ok((balance, balance));
        }
        let mint = self.mint()?;
        let mut dbtx = mint.db.begin_transaction_nc().await;
        let counts = mint.get_note_counts_by_denomination(&mut dbtx).await;
        let spendable =
            counts.iter().filter(|(tier, _)| tier.msats > self.fee(tier.msats)).map(|(tier, count)| tier * count as u64).sum::<Amount>();
        Ok((counts.total_amount().msats, spendable.msats))
    }

    async fn card_topup_pricing(&self, face: u64) -> Result<Pricing> {
        let s = self.fee_schedules().await.map_err(anyhow::Error::msg)?;
        let ln = s.ln.context(s.ln_problem.unwrap_or_else(|| "the till needs a usable Lightning fee schedule".into()))?;
        ensure!(
            crate::plan::binary(face).iter().all(|a| *a >= self.min_denom() && self.mint_cfg.tbs_pks.get(Amount::from_msats(*a)).is_some()),
            "the mint cannot issue the card denominations"
        );
        ensure!(
            s.mint == Linear::ZERO || self.surplus_allowed(),
            "this federation does not accept the rounding required for a fee-bearing card load"
        );
        Pricing::new(self.topup_mint_layout(s.mint)?, ln, face)
    }
}

impl WalletLightning {
    pub(crate) async fn fee_inclusive_invoice(&self, face_msat: u64, expiry_secs: u64) -> Result<TopupInvoice> {
        let pricing = self.wallet.card_topup_pricing(face_msat).await?;
        let (quote, invoice, op, contract_expiry) = if let Ok(ln) = self.wallet.lightning() {
            // LN v1's incoming offer requires the entire advertised amount in
            // the contract. The gateway's outgoing routing fees do not apply.
            ln.update_gateway_cache().await.map_err(|e| anyhow!("reading the federation's gateways: {e:?}"))?;
            let gw = ln.select_available_gateway(None, None).await.map_err(|e| anyhow!("no Lightning gateway answers: {e:?}"))?;
            let q = pricing.solve(Linear::ZERO)?;
            let amount = Amount::from_msats(q.contract);
            let observed = ln.receive_fee_quote(amount).await?.total().get_bitcoin().msats;
            ensure!(observed <= q.claim_fee, "the federation quoted a claim fee above the top-up bound");
            let desc =
                lightning_invoice::Bolt11InvoiceDescription::Direct(lightning_invoice::Description::new("fedimintcard top-up".into())?);
            let (op, invoice, _) = ln
                .create_bolt11_invoice(Amount::from_msats(q.gross), desc, Some(expiry_secs), (), Some(gw))
                .await
                .map_err(|e| anyhow!("the gateway made no invoice: {e:?}"))?;
            (q, invoice, op, None)
        } else {
            let ln = self.wallet.lightning_v2()?;
            let mut selected = None;
            for gateway in ln.list_gateways(None).await? {
                if let Ok(Some(info)) = ln.routing_info(&gateway).await
                    && info.receive_enabled
                    && info.receive_fee.is_within(&PaymentFee::RECEIVE_FEE_LIMIT)
                {
                    selected = Some((gateway, info.receive_fee));
                    break;
                }
            }
            let (gateway, terms) = selected.context("no Lightning gateway is accepting receives within the fee limit")?;
            let q = pricing.solve(Linear { base_msat: terms.base.msats, ppm: terms.parts_per_million })?;
            ensure!(terms.subtract_from(q.gross).msats == q.contract, "the gateway fee does not match the quoted contract");
            let observed = ln.receive_fee_quote(Amount::from_msats(q.contract)).await?.total().get_bitcoin().msats;
            ensure!(observed <= q.claim_fee, "the federation quoted a claim fee above the top-up bound");
            let (invoice, op) = ln
                .receive_with_terms(
                    Amount::from_msats(q.gross),
                    expiry_secs.try_into()?,
                    fedimint_lnv2_client::common::Bolt11InvoiceDescription::Direct("fedimintcard top-up".into()),
                    gateway,
                    terms,
                    serde_json::Value::Null,
                )
                .await?;
            let entry = self.wallet.client.operation_log().get_operation(op).await.context("missing Lightning receive operation")?;
            let fedimint_lnv2_client::LightningOperationMeta::Receive(receive) = entry.try_meta()? else {
                bail!("the Lightning operation is not an incoming invoice")
            };
            (q, invoice, op, Some(receive.contract.commitment.expiration_or_fee))
        };
        ensure!(invoice.amount_milli_satoshis() == Some(quote.gross), "the gateway returned an invoice for a different amount");
        let signed_deadline = invoice.expires_at().context("the Lightning invoice expiry is out of range")?.as_secs();
        let deadline = contract_expiry.map_or(signed_deadline, |contract| signed_deadline.min(contract));
        let expires_at_ms = deadline.checked_mul(1000).context("the Lightning invoice expiry is out of range")?;
        Ok(TopupInvoice {
            expires_at_ms,
            operation_id: hex::encode(op.0),
            invoice: invoice.to_string(),
            invoice_amount_msat: quote.gross,
            topup_fee_msat: quote.gross - face_msat,
            load_fee_bound_msat: quote.load_fee,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2_tiers() -> Vec<u64> {
        fedimint_mintv2_common::config::client_denominations().map(|d| d.amount().msats).collect()
    }

    fn fixed_ln(fee: u64) -> LnSchedule {
        LnSchedule::V1 { contract_input_msat: fee, contract_output_msat: 0 }
    }

    #[test]
    fn mint_v2_fixed_fee_claim_and_load_are_covered_after_gross_gateway_deduction() {
        let face = 2_100_224;
        let pricing =
            Pricing::new(MintLayout::new(Linear { base_msat: 10, ppm: 0 }, &v2_tiers(), MintVersion::V2).unwrap(), fixed_ln(1_000), face)
                .unwrap();
        let gateway = Linear { base_msat: 0, ppm: 1_000 };
        let q = pricing.solve(gateway).unwrap();
        assert_eq!(q.gross, face + gateway.fee(q.gross) + q.claim_fee + q.load_fee);
        assert_eq!(q.contract, q.gross - gateway.fee(q.gross));
        assert!(q.claim_fee >= 1_000 + 511);
        assert!(q.load_fee >= 60);
        assert!(q.contract - q.claim_fee >= face + q.load_fee);
        assert!(q.gross - face < 20_000, "fee should reflect the tighter count derived from the change algorithm");
        assert!(2_096_640 < face + 60);
    }

    #[test]
    fn zero_mint_rounding_and_both_lightning_modules_charge_only_the_needed_total() {
        let face = 1 << 21;
        for (tiers, version) in [(vec![1, 2, 4, 8, 1 << 22], MintVersion::V1), (v2_tiers(), MintVersion::V2)] {
            for ln in [fixed_ln(0), fixed_ln(37), LnSchedule::V2(Linear { base_msat: 123, ppm: 400 })] {
                let p = Pricing::new(MintLayout::new(Linear::ZERO, &tiers, version).unwrap(), ln, face).unwrap();
                for gateway in [Linear::ZERO, Linear { base_msat: 2_000, ppm: 3_000 }] {
                    let q = p.solve(gateway).unwrap();
                    let net = |gross| {
                        let contract = gross - gateway.fee(gross);
                        let after_ln = contract - ln.input_fee(contract);
                        after_ln - after_ln % tiers[0]
                    };
                    assert_eq!(q.load_fee, 0);
                    assert_eq!(net(q.gross), face);
                    assert!(net(q.gross - 1) < face);
                }
            }
        }
    }

    #[test]
    fn change_count_bound_covers_mixed_wallet_inputs_and_outputs() {
        for (version, tiers, fee) in [
            (MintVersion::V2, v2_tiers(), Linear { base_msat: 10, ppm: 0 }),
            (MintVersion::V1, vec![1, 2, 4, 64, 256, 8_192, 262_144], Linear { base_msat: 100, ppm: 250 }),
        ] {
            let layout = MintLayout::new(fee, &tiers, version).unwrap();
            for (external, card_notes) in [(0, 3), (2_100_224, 0)] {
                let bytes = Transaction::MAX_TX_SIZE as u64;
                let cap = bytes / INPUT_BYTES;
                let bound = layout.item_limit(external, card_notes, cap);
                assert!(bound < bytes / OUTPUT_BYTES);
                // Consider every possible input count, with the largest number
                // of physically and algorithmically possible change outputs.
                for inputs in 0..=cap {
                    let output_cap = inputs + external / layout.largest + layout.change_allowance + card_notes;
                    let outputs = output_cap.min((bytes - INPUT_BYTES * inputs) / OUTPUT_BYTES);
                    assert!(inputs + outputs <= bound);
                }
            }
        }
    }

    #[test]
    fn historical_v1_small_notes_and_proportional_fee_limits_are_accounted_for() {
        let basic = Linear { base_msat: 100, ppm: 0 };
        assert!(MintLayout::new(basic, &[128, 256], MintVersion::V1).is_err());
        let tiers: Vec<_> = (0..=33).map(|x| 1u64 << x).collect();
        let layout = MintLayout::new(basic, &tiers, MintVersion::V1).unwrap();
        // The v1 change algorithm may put notes at or below the 100-msat
        // spend fee back into the wallet. None is needed to fund the card.
        assert!(layout.uneconomical_change >= 2 * (1 + 2 + 4 + 8 + 16 + 32 + 64));
        let q = Pricing::new(layout, fixed_ln(0), 1 << 21).unwrap().solve(Linear::ZERO).unwrap();
        assert!(q.claim_fee > 0 && q.load_fee > 0);
        assert_eq!(q.contract - q.claim_fee, (1 << 21) + q.load_fee);
        let mixed = Linear { base_msat: 100, ppm: 1_000 };
        let huge = MintLayout::new(mixed, &v2_tiers(), MintVersion::V2).unwrap();
        assert!(Pricing::new(huge, fixed_ln(0), 1 << 20).unwrap().solve(Linear::ZERO).unwrap_err().to_string().contains("fee limit"));
        let small = MintLayout::new(mixed, &[1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1_024, 2_048], MintVersion::V1).unwrap();
        let q = Pricing::new(small, fixed_ln(0), 1_024).unwrap().solve(Linear::ZERO).unwrap();
        assert!(q.contract - q.claim_fee >= 1_024 + q.load_fee);
    }

    #[test]
    fn fee_limit_is_separate_from_principal_and_unbounded_fees_are_refused() {
        let face = (1 << 26) + (1 << 24) + (1 << 23);
        let simple = || MintLayout::new(Linear::ZERO, &v2_tiers(), MintVersion::V2).unwrap();
        let p = Pricing::new(simple(), fixed_ln(12_000_000), face).unwrap();
        assert!(p.solve(Linear::ZERO).unwrap().gross > MAX_TOPUP_MSAT);
        let p = Pricing::new(simple(), fixed_ln(MAX_TOPUP_MSAT + 1), face).unwrap();
        assert!(p.solve(Linear::ZERO).unwrap_err().to_string().contains("fee limit"));
    }
}
