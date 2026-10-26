use fedimint_client::transaction::TransactionBuilder;
use fedimint_core::Amount;
use fedimint_core::core::OperationId;
use fedimint_dummy_client::{DummyClientInit, DummyClientModule};
use fedimint_dummy_server::DummyInit;
use fedimint_mintv2_client::MintClientInit;
use fedimint_mintv2_server::MintInit;
use fedimint_testing::fixtures::Fixtures;

#[tokio::test(flavor = "multi_thread")]
async fn federation_starts_and_client_is_funded() -> anyhow::Result<()> {
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit).with_module(DummyClientInit, DummyInit);
    let fed = fixtures.new_fed_not_degraded().await;
    let client = fed.new_client().await;
    let dummy = client.get_first_module::<DummyClientModule>()?;
    let op = OperationId::new_random();
    let range = client
        .finalize_and_submit_transaction(op, "Issue".into(), |_| (), TransactionBuilder::new().with_inputs(dummy.create_input(Amount::from_sats(10_000))))
        .await?;
    client.await_primary_bitcoin_module_outputs(op, range.into_iter().collect()).await?;
    let bal = client.get_balance_for_btc().await?;
    assert!(bal >= Amount::from_sats(9_900), "balance {bal}");
    Ok(())
}
