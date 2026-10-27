//! Steps 3 and 4: the federation starts inside `cargo test`, and a note is issued to a card key.

use fedimint_card_fed_tests::{Env, issue_to_card, nonce_for, on_card, setup};
use fedimint_card_host_card::open_from_env;
use fedimint_client::transaction::TransactionBuilder;
use fedimint_core::Amount;
use fedimint_core::core::OperationId;
use fedimint_dummy_client::{DummyClientInit, DummyClientModule};
use fedimint_dummy_server::DummyInit;
use fedimint_mintv2_client::MintClientInit;
use fedimint_mintv2_common::{Denomination, verify_note};
use fedimint_mintv2_server::MintInit;
use fedimint_testing::fixtures::Fixtures;

/// Step 3: 4 guardians start in-process, and a client is issued ecash through the stock path.
#[tokio::test(flavor = "multi_thread")]
async fn federation_starts_and_client_is_funded() -> anyhow::Result<()> {
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit).with_module(DummyClientInit, DummyInit);
    let fed = fixtures.new_fed_not_degraded().await;
    let client = fed.new_client().await;
    let dummy = client.get_first_module::<DummyClientModule>()?;
    let op = OperationId::new_random();
    let range = client
        .finalize_and_submit_transaction(
            op,
            "Issue".into(),
            |_| (),
            TransactionBuilder::new().with_inputs(dummy.create_input(Amount::from_sats(10_000))),
        )
        .await?;
    client.await_primary_bitcoin_module_outputs(op, range.into_iter().collect()).await?;
    let bal = client.get_balance_for_btc().await?;
    assert!(bal >= Amount::from_sats(9_900), "balance {bal}");
    Ok(())
}

/// Step 4: the issued note verifies against the federation key and its nonce is the card's key.
#[tokio::test(flavor = "multi_thread")]
async fn note_issued_to_card_key_verifies() -> anyhow::Result<()> {
    let env: Env = setup(true).await;
    let mut card = open_from_env()?;
    let key = on_card(|| card.get_or_gen(0))?;

    let denom = Denomination(20);
    let note = issue_to_card(&env, key, denom).await?;

    assert_eq!(note.nonce.x_only_public_key().0, key, "nonce is the card key");
    assert_eq!(note.nonce, nonce_for(key));
    assert_eq!(note.denomination, denom);
    let pk = env.mint_cfg.tbs_agg_pks[&denom];
    assert!(verify_note(note, pk));

    // The check can fail: wrong denomination key, different nonce, or another note's signature.
    assert!(!verify_note(fedimint_mintv2_common::Note { denomination: Denomination(19), ..note }, env.mint_cfg.tbs_agg_pks[&Denomination(19)]));
    let other = on_card(|| card.get_or_gen(1))?;
    assert!(!verify_note(fedimint_mintv2_common::Note { nonce: nonce_for(other), ..note }, pk));
    Ok(())
}
