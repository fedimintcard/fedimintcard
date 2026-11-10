//! Fixtures for the core's tests, with no federation and no network: a 3.0 SoftCard the registry
//! certified, loaded by a one-guardian mint (`softkit`), and an offline kit for it.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use fedimint_card_host_card::softkit::{SoftMint, SoftTransport};
use fedimint_card_host_card::{Card, Certificate, SlotState, SoftCard, TEST_PIN, Transport, open_pin_session_with};
use fedimint_card_pos_core::kit::{KIT_VERSION, Kit, PoolTier};
use fedimint_card_pos_core::plan::{self, FeeHeadroom};
use fedimint_core::encoding::Encodable;
use fedimint_mint_common::config::FeeConsensus;

pub const DAY_MS: u64 = 86_400_000;
/// The kit's pool tiers stop here, as a till's do under its default caps (2^26 msat).
pub const TOP_TIER: u64 = 1 << 26;

pub struct Fx {
    pub mint: SoftMint,
    pub registry: p256::ecdsa::SigningKey,
    pub fee: FeeConsensus,
    pub surplus_allowed: bool,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

impl Fx {
    /// fedimint's default mint fees (100 msat per input and output), or none.
    pub fn new(fees: bool) -> Self {
        Self {
            mint: SoftMint::powers_of_two(32),
            registry: p256::ecdsa::SigningKey::random(&mut rand::thread_rng()),
            fee: if fees { FeeConsensus::new(0).unwrap() } else { FeeConsensus::zero() },
            surplus_allowed: true,
        }
    }

    pub fn cert_key_hex(&self) -> String {
        hex::encode(self.registry.verifying_key().to_encoded_point(false).as_bytes())
    }

    pub fn min_denom(&self) -> u64 {
        self.fee.min_economical_denomination().msats
    }

    /// A 3.0 card set up as the Setup app does it, under this fixture's registry key: `keyed`
    /// slots with keys, the attestation key certified, the registry key, the PIN `1234`; then
    /// `notes` (msat) loaded into slots 0, 1, ...
    pub fn card(&self, keyed: u8, notes: &[u64]) -> SoftTransport {
        let mut c = SoftCard::v3();
        let keys: Vec<_> = (0..keyed).map(|s| c.gen_key(s).unwrap()).collect();
        let w = c.attest_init().unwrap();
        let cert = Certificate::sign(&self.registry, &keys[0].serialize(), &w).unwrap();
        c.set_cert(&cert.to_bytes()).unwrap();
        c.set_registry_key(&self.registry.verifying_key().to_encoded_point(false).as_bytes().try_into().unwrap()).unwrap();
        open_pin_session_with(&mut c, TEST_PIN).unwrap();
        for (slot, amount) in notes.iter().enumerate() {
            self.mint.load(&mut c, slot as u8, *amount).unwrap();
        }
        SoftTransport::new(c)
    }

    /// A kit for `phone_id`: `depth` pool entries per tier, from the lowest the planner gives the
    /// till to [`TOP_TIER`], issued `issued_at_ms` and good for `ttl_ms`.
    pub fn kit(&self, phone_id: &str, depth: usize, issued_at_ms: u64, ttl_ms: u64) -> Kit {
        let fee = self.fee.clone();
        let fee_fn = move |a: u64| fee.fee(fedimint_core::Amount::from_msats(a)).msats;
        let lowest = plan::lowest_till_note(&fee_fn, self.min_denom());
        let mut pool = vec![];
        let mut t = lowest;
        while t <= TOP_TIER {
            let blinded = (0..depth)
                .map(|_| {
                    let m = tbs::Message::from_bytes(&rand::random::<[u8; 32]>());
                    tbs::blind_message(m, tbs::BlindingKey::random()).consensus_encode_to_hex()
                })
                .collect();
            pool.push(PoolTier { amount_msat: t, blinded });
            t <<= 1;
        }
        Kit {
            version: KIT_VERSION,
            kit_id: hex::encode(rand::random::<[u8; 16]>()),
            phone_id: phone_id.to_owned(),
            federation_id: "00".repeat(32),
            mint_instance_id: 1,
            tbs_pks: self.mint.tbs_pks().into_iter().map(|(t, pk)| (t.to_string(), pk.consensus_encode_to_hex())).collect(),
            fee_consensus: self.fee.consensus_encode_to_hex(),
            fee_headroom: FeeHeadroom::DEFAULT,
            surplus_allowed: self.surplus_allowed,
            cert_key: self.cert_key_hex(),
            cap_msat: 100_000_000,
            cap_count: 200,
            issued_at_ms,
            expires_at_ms: issued_at_ms + ttl_ms,
            block_header: None,
            pool,
        }
    }

    pub fn kit_json(&self, phone_id: &str) -> String {
        serde_json::to_string(&self.kit(phone_id, 40, now_ms(), 7 * DAY_MS)).unwrap()
    }
}

/// A transport that counts the APDUs it carried.
pub struct Counted<T> {
    pub inner: T,
    pub n: Arc<Mutex<usize>>,
}

impl<T: Transport> Transport for Counted<T> {
    fn transmit(&mut self, apdu: &[u8]) -> anyhow::Result<Vec<u8>> {
        *self.n.lock().unwrap() += 1;
        self.inner.transmit(apdu)
    }
}

/// The card's slots in `state`.
pub fn slots_in(card: &SoftTransport, state: SlotState) -> Vec<u8> {
    let c = card.0.lock().unwrap();
    (0..c.slot_len()).filter(|s| c.slot_state(*s) == Some(state)).collect()
}

/// The hash each SPENT slot signed.
pub fn spent_hashes(card: &SoftTransport) -> Vec<(u8, [u8; 32])> {
    let mut c = card.0.lock().unwrap();
    let spent: Vec<u8> = (0..c.slot_len()).filter(|s| c.slot_state(*s) == Some(SlotState::Spent)).collect();
    spent.into_iter().map(|s| (s, c.get_slot(s).unwrap().spent_hash)).collect()
}
