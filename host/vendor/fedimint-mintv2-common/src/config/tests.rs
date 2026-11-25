use fedimint_core::encoding::Decodable as _;
use fedimint_core::module::serde_json;
use fedimint_core::Amount;

use super::FeeConsensus;

#[test]
fn decoded_fee_consensus_is_validated() {
    use fedimint_core::encoding::Encodable as _;
    use fedimint_core::module::registry::ModuleDecoderRegistry;

    let modules = ModuleDecoderRegistry::default();
    for fee in
        [FeeConsensus::zero(), FeeConsensus::new(0).expect("zero ppm is valid"), FeeConsensus::new(1_000).expect("maximum ppm is valid")]
    {
        assert_eq!(
            serde_json::from_str::<FeeConsensus>(&serde_json::to_string(&fee).expect("fee consensus can be encoded"))
                .expect("valid JSON fee consensus can be decoded"),
            fee
        );
        assert_eq!(
            FeeConsensus::consensus_decode_whole(&fee.consensus_encode_to_vec(), &modules).expect("valid consensus fee can be decoded"),
            fee
        );
    }

    for malformed in [
        (Amount::ZERO, 1),
        (Amount::from_msats(9), 0),
        (Amount::from_msats(10), 1),
        (Amount::from_msats(11), 0),
        (Amount::from_msats(99), 0),
        (Amount::from_msats(100), 1_001),
        (Amount::from_msats(u64::MAX), u64::MAX),
    ] {
        let json = serde_json::json!({
            "base": malformed.0,
            "parts_per_million": malformed.1,
        });
        assert!(serde_json::from_value::<FeeConsensus>(json).is_err());

        let encoded = malformed.consensus_encode_to_vec();
        assert!(FeeConsensus::consensus_decode_whole(&encoded, &modules).is_err());
    }
}

#[test]
fn deployed_ten_msat_fee_round_trips_and_charges_exactly_ten() {
    use fedimint_core::encoding::Encodable as _;
    use fedimint_core::module::registry::ModuleDecoderRegistry;

    let json = serde_json::json!({ "base": 10, "parts_per_million": 0 });
    let fee: FeeConsensus = serde_json::from_value(json.clone()).unwrap();
    let encoded = (Amount::from_msats(10), 0u64).consensus_encode_to_vec();
    let decoded = FeeConsensus::consensus_decode_whole(&encoded, &ModuleDecoderRegistry::default()).unwrap();
    assert_eq!(decoded, fee);
    assert_eq!(decoded.consensus_encode_to_vec(), encoded);
    assert_eq!(serde_json::to_value(&decoded).unwrap(), json);
    for amount in [0, 512, 1_000_000, u64::MAX] {
        assert_eq!(fee.fee(Amount::from_msats(amount)), Amount::from_msats(10));
    }
}
