//! Payout eCash against the in-process federation (as `topup.rs` builds it):
//!
//! - the tagged spend is found again in the wallet's own operation log, which is what settles an
//!   export that a crash cut short between the spend and the payout record's save; an untagged
//!   spend is not mistaken for one;
//! - a reclaim is the wallet's tagged reissue, settled by that operation's own state, and found
//!   by its tag after a crash; a second reissue of the same notes is refused by the client;
//! - the engine does all of that over the till's own wallet ([`TillBackend`]).
//!
//! Nothing here reaches a real federation or service.

use std::time::Duration;

use fedimint_card_sidecar::payout::backend::{
    ReclaimStart, ReclaimState, Tagged, TillBackend, export_tagged, find_tagged, reclaim_start, reclaim_status,
};
use fedimint_card_sidecar::payout::engine::Engine;
use fedimint_card_sidecar::payout::net::{Fetch, FetchError};
use fedimint_card_sidecar::payout::store::{Kind, Origin, PayoutStore, Record, State};
use fedimint_card_sidecar::wallet::Wallet;
use fedimint_client::ClientHandleArc;
use fedimint_client::transaction::TransactionBuilder;
use fedimint_core::Amount;
use fedimint_core::core::OperationId;
use fedimint_dummy_client::{DummyClientInit, DummyClientModule};
use fedimint_dummy_server::DummyInit;
use fedimint_mint_client::MintClientInit;
use fedimint_mint_server::MintInit;
use fedimint_testing::federation::FederationTest;
use fedimint_testing::fixtures::Fixtures;
use futures::future::BoxFuture;

const WAIT: Duration = Duration::from_secs(60);

async fn fund(client: &ClientHandleArc, amount: Amount) -> anyhow::Result<()> {
    let dummy = client.get_first_module::<DummyClientModule>()?;
    let op = OperationId::new_random();
    let range = client
        .finalize_and_submit_transaction(op, "fund", |_| (), TransactionBuilder::new().with_inputs(dummy.create_input(amount)))
        .await?;
    client.await_primary_bitcoin_module_outputs(op, range.into_iter().collect()).await?;
    Ok(())
}

async fn wallet(sats: u64) -> anyhow::Result<(FederationTest, Wallet)> {
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit).with_module(DummyClientInit, DummyInit);
    let fed = tokio::time::timeout(Duration::from_secs(60), fixtures.new_fed_builder(0).disable_mint_fees().build()).await?;
    let client = fed.new_client().await;
    fund(&client, Amount::from_sats(sats)).await?;
    let w = Wallet::from_client(client).await?;
    Ok((fed, w))
}

/// No Lightning address is used by eCash payouts.
struct NoNetwork;

impl Fetch for NoNetwork {
    fn get_json(&self, _url: reqwest::Url) -> BoxFuture<'_, Result<serde_json::Value, FetchError>> {
        Box::pin(async { Err(FetchError::Unsafe("no network in this test".into())) })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tagged_export_and_its_reclaim_are_found_by_their_tags_and_settled_by_the_client() -> anyhow::Result<()> {
    let (_fed, w) = wallet(100_000).await?;
    let before = w.balance_msat().await?;

    let tags = vec!["fedimintcard-payout:po_a".to_owned(), "fedimintcard-payout:po_b".to_owned()];
    assert!(find_tagged(&w, &tags).await?.is_empty(), "nothing exported yet");

    let (notes, face) = export_tagged(&w, 10_000_000, &tags[0]).await?;
    assert_eq!(face, 10_000_000);
    assert_eq!(w.balance_msat().await?, before - 10_000_000);

    let found = find_tagged(&w, &tags).await?;
    assert_eq!(found.len(), 1);
    assert_eq!(found[&tags[0]], Tagged::Export { notes: notes.clone() });

    // A spend without the tag (the operator's `ecash export`) is not taken for a payout's.
    w.export(5_000_000, false, false).await?;
    assert_eq!(find_tagged(&w, &tags).await?.len(), 1);

    // The reclaim: a tagged reissue, found by its tag, settled by its own state.
    let rtag = "fedimintcard-payout-reclaim:po_a".to_owned();
    let ReclaimStart::Started(op) = reclaim_start(&w, &notes, &rtag).await? else { panic!("a first reissue starts") };
    assert_eq!(find_tagged(&w, std::slice::from_ref(&rtag)).await?[&rtag], Tagged::Reclaim { operation_id: op.clone() });
    assert_eq!(reclaim_status(&w, &op, WAIT).await?, ReclaimState::Done);
    assert_eq!(w.balance_msat().await?, before - 5_000_000);
    // The client refuses to reissue the same notes twice: nothing more comes back.
    assert_eq!(reclaim_start(&w, &notes, &rtag).await?, ReclaimStart::AlreadyReissued);
    assert_eq!(w.balance_msat().await?, before - 5_000_000);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_engine_exports_recovers_a_cut_export_and_reclaims_on_the_tills_wallet() -> anyhow::Result<()> {
    let (_fed, w) = wallet(100_000).await?;
    let dir = tempfile::tempdir()?;
    let backend = TillBackend { wallet: w.clone(), topups: None, pays: None, dir: dir.path().to_owned(), till: None };
    let e = Engine { dir: dir.path(), backend: &backend, fetch: &NoNetwork };
    let before = w.balance_msat().await?;

    // 100,000 - 10,000 reserve: an export above what is free is refused, nothing spent.
    assert_eq!(e.ecash("big".into(), 95_000, WAIT).await.unwrap_err().body["error"], "insufficient_available");
    assert_eq!(w.balance_msat().await?, before);

    let rec = e.ecash("one".into(), 20_000, WAIT).await.map_err(|r| anyhow::anyhow!("{}", r.body))?;
    assert_eq!(rec.state, State::Exported);
    assert_eq!(w.balance_msat().await?, before - 20_000_000);

    // The process stopped after the wallet spent, before `exported` was saved.
    let mut cut = Record::new(Kind::Ecash, Origin::Manual, Some("cut".into()), 7_000_000, 1);
    let (notes, _) = export_tagged(&w, 7_000_000, &cut.export_tag()).await?;
    cut.state = State::Exporting;
    PayoutStore::open(dir.path())?.insert(cut.clone())?;
    e.sweep(Duration::from_secs(5)).await;
    let found = PayoutStore::open(dir.path())?.get(&cut.id).cloned().unwrap();
    assert_eq!((found.state, found.notes.as_deref()), (State::Exported, Some(notes.as_str())));

    // Its reclaim brings the value back; the notes are wiped only then.
    let back = e.reclaim(&cut.id, WAIT).await.map_err(|r| anyhow::anyhow!("{}", r.body))?;
    assert_eq!((back.state, back.notes.is_none()), (State::Reclaimed, true));
    assert_eq!(w.balance_msat().await?, before - 20_000_000);
    assert_eq!(e.reclaim(&cut.id, WAIT).await.unwrap_err().body["error"], "not_exported");
    Ok(())
}
