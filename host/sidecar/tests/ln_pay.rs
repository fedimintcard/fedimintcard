//! `ln pay` (`Wallet::pay_bolt11`) without a gateway: invoice parsing, the refusals that come
//! before anything is spent, and that wallets joined by invite code open again with the Lightning
//! modules the till registers (ln always; lnv2 only on a federation without ln). The in-process
//! federations have no gateway, so a payment through one is not covered here.

use std::time::Duration;

use fedimint_card_sidecar::wallet::{LnPayOutcome, Wallet, parse_bolt11};
use fedimint_core::bitcoin::hashes::{Hash as _, sha256};
use fedimint_core::secp256k1::{SECP256K1, SecretKey};
use fedimint_mint_client::MintClientInit;
use fedimint_mint_server::MintInit;
use fedimint_testing::federation::FederationTest;
use fedimint_testing::fixtures::Fixtures;
use lightning_invoice::{Bolt11Invoice, Currency, InvoiceBuilder, PaymentSecret};

fn invoice(amount_msat: Option<u64>) -> Bolt11Invoice {
    invoice_on(Currency::Regtest, amount_msat)
}

fn invoice_on(currency: Currency, amount_msat: Option<u64>) -> Bolt11Invoice {
    let key = SecretKey::from_slice(&[7u8; 32]).unwrap();
    let b = InvoiceBuilder::new(currency)
        .description("ln pay test".into())
        .payment_hash(sha256::Hash::hash(&rand::random::<[u8; 32]>()))
        .payment_secret(PaymentSecret([42u8; 32]))
        .current_timestamp()
        .min_final_cltv_expiry_delta(144);
    match amount_msat {
        Some(a) => b.amount_milli_satoshis(a).build_signed(|m| SECP256K1.sign_ecdsa_recoverable(m, &key)),
        None => b.build_signed(|m| SECP256K1.sign_ecdsa_recoverable(m, &key)),
    }
    .unwrap()
}

async fn fed(fixtures: Fixtures) -> anyhow::Result<FederationTest> {
    assert!(
        std::env::var_os("FM_TEST_USE_REAL_DAEMONS").is_none(),
        "FM_TEST_USE_REAL_DAEMONS is set: these tests need the in-process fake bitcoin backend"
    );
    Ok(tokio::time::timeout(Duration::from_secs(60), fixtures.new_fed_builder(0).build()).await?)
}

/// Shut the client down so its database can be opened again.
async fn close(w: Wallet) {
    let client = w.client.clone();
    drop(w);
    if let Ok(handle) = std::sync::Arc::try_unwrap(client) {
        handle.shutdown().await;
    }
}

#[test]
fn invoice_text_is_parsed_as_a_wallet_would_take_it() {
    let inv = invoice(Some(21_000));
    let text = inv.to_string();
    assert_eq!(parse_bolt11(&text).unwrap(), inv);
    assert_eq!(parse_bolt11(&format!("  lightning:{text}\n")).unwrap(), inv);
    assert_eq!(parse_bolt11(&format!("LIGHTNING:{}", text.to_uppercase())).unwrap(), inv);

    let amountless = parse_bolt11(&invoice(None).to_string()).unwrap_err().to_string();
    assert!(amountless.contains("no amount"), "{amountless}");
    // One character off breaks the checksum.
    let mut broken = text.clone();
    let last = broken.pop().unwrap();
    broken.push(if last == 'q' { 'p' } else { 'q' });
    assert!(parse_bolt11(&broken).unwrap_err().to_string().contains("not a BOLT11 invoice"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mint_only_federation_refuses_ln_pay_and_the_wallet_reopens() -> anyhow::Result<()> {
    let fed = fed(Fixtures::new_primary(MintClientInit, MintInit)).await?;
    let dir = tempfile::tempdir()?;
    let w = Wallet::join(dir.path(), &fed.invite_code().to_string()).await?;
    assert!(w.lightning().is_err() && w.lightning_v2().is_err());

    let err = w.pay_bolt11("lnbc1garbage", None).await.unwrap_err().to_string();
    assert!(err.contains("not a BOLT11 invoice"), "{err}");
    let out = w.pay_bolt11(&invoice(Some(10_000)).to_string(), None).await?;
    let LnPayOutcome::Refused { reason, insufficient_funds: false, retryable: false } = &out else { panic!("{out:?}") };
    assert!(reason.contains("no Lightning module"), "{reason}");
    let quote = w.quote_invoice(&invoice(Some(10_000)).to_string()).await.unwrap_err().to_string();
    assert!(quote.contains("no Lightning module"), "{quote}");
    assert_eq!(serde_json::to_value(&out)?["status"], "refused");
    close(w).await;

    // Reopened as it was joined, then as a wallet from before either marker existed.
    let w = Wallet::open(dir.path()).await?;
    assert_eq!(w.federation_id(), fed.id());
    close(w).await;
    for m in ["api-versions-ln", "api-versions-lnv2"] {
        std::fs::remove_file(dir.path().join(m))?;
    }
    let w = Wallet::open(dir.path()).await?;
    assert!(w.mint().is_ok() && w.lightning_v2().is_err());
    assert!(dir.path().join("api-versions-ln").exists());
    close(w).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn lnv2_only_federation_registers_lnv2_and_refuses_without_a_gateway() -> anyhow::Result<()> {
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit).with_server_only_module(fedimint_lnv2_server::LightningInit);
    let fed = fed(fixtures).await?;
    let dir = tempfile::tempdir()?;
    let w = Wallet::join(dir.path(), &fed.invite_code().to_string()).await?;
    assert!(w.lightning().is_err());
    assert!(w.lightning_v2().is_ok(), "a federation without ln gets the lnv2 client");

    let out = w.pay_bolt11(&invoice(Some(10_000)).to_string(), Some(5_000)).await?;
    // No gateway answers now: refused with nothing funded, and it may go through later.
    let LnPayOutcome::Refused { reason, insufficient_funds: false, retryable: true } = &out else { panic!("{out:?}") };
    assert!(reason.contains("no Lightning gateway"), "{reason}");
    // The quote asked before a card is charged says so too.
    let quote = w.quote_invoice(&invoice(Some(10_000)).to_string()).await.unwrap_err().to_string();
    assert!(quote.contains("no Lightning gateway"), "{quote}");
    close(w).await;

    // A wallet that joined this federation before lnv2 was registered: its cached API versions
    // leave lnv2 out until the lnv2 marker makes `open` refetch them.
    std::fs::remove_file(dir.path().join("api-versions-lnv2"))?;
    let w = Wallet::open(dir.path()).await?;
    assert!(w.lightning_v2().is_ok());
    assert!(dir.path().join("api-versions-lnv2").exists());
    close(w).await;
    Ok(())
}

/// Against a real federation, when `LN_PAY_LIVE_INVITE` names one (skipped otherwise): join a new,
/// empty wallet, check which Lightning modules the till registers there, and pay an invoice nobody
/// can settle. The empty wallet is refused after a real gateway quoted its fee, so nothing is spent.
#[tokio::test(flavor = "multi_thread")]
async fn live_federation_when_configured() -> anyhow::Result<()> {
    let Ok(invite) = std::env::var("LN_PAY_LIVE_INVITE") else { return Ok(()) };
    let dir = tempfile::tempdir()?;
    let w = Wallet::join(dir.path(), invite.trim()).await?;
    let (v1, v2) = (w.lightning().is_ok(), w.lightning_v2().is_ok());
    eprintln!("ln registered: {v1}, lnv2 registered: {v2}");
    assert!(v1 != v2, "exactly one Lightning client per federation");
    assert_eq!(w.balance_msat().await?, 0);
    let out = w.pay_bolt11(&invoice_on(Currency::Bitcoin, Some(1_000_000)).to_string(), None).await?;
    eprintln!("{}", serde_json::to_string(&out)?);
    assert!(matches!(out, LnPayOutcome::Refused { insufficient_funds: true, .. }), "{out:?}");
    close(w).await;
    let w = Wallet::open(dir.path()).await?;
    assert_eq!((w.lightning().is_ok(), w.lightning_v2().is_ok()), (v1, v2));
    close(w).await;
    Ok(())
}
