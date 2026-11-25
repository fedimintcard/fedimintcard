//! The guardian fixture only exposes a zero/standard fee switch. Override the trusted
//! dealer's generated fee so all four real mint modules run the historical 10 msat schedule.
use fedimint_api_client::api::DynModuleApi;
use fedimint_core::config::{ClientModuleConfig, ServerModuleConfig, ServerModuleConsensusConfig, TypedServerModuleConfig};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::{Database, DatabaseTransaction, DatabaseVersion};
use fedimint_core::module::{IDynCommonModuleInit, ModuleInit};
use fedimint_core::task::TaskGroup;
use fedimint_core::{NumPeers, PeerId};
use fedimint_mintv2_common::{MintCommonInit, config::MintConfig};
use fedimint_mintv2_server::MintInit;
use fedimint_server_core::{
    ConfigGenModuleArgs, DynServerModule, EnvVarDoc, IServerModuleInit, bitcoin_rpc::ServerBitcoinRpcMonitor, config::PeerHandleOps,
    migration::DynServerDbMigrationFn,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
pub struct TenMsatMint;

impl ModuleInit for TenMsatMint {
    type Common = MintCommonInit;

    async fn dump_database(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        prefixes: Vec<String>,
    ) -> Box<dyn Iterator<Item = (String, Box<dyn erased_serde::Serialize + Send>)> + '_> {
        ModuleInit::dump_database(&MintInit, dbtx, prefixes).await
    }
}

#[async_trait::async_trait]
impl IServerModuleInit for TenMsatMint {
    fn as_common(&self) -> &(dyn IDynCommonModuleInit + Send + Sync + 'static) {
        &MintInit
    }

    async fn init(
        &self,
        peer_num: NumPeers,
        cfg: ServerModuleConfig,
        db: Database,
        task_group: &TaskGroup,
        our_peer_id: PeerId,
        module_api: DynModuleApi,
        monitor: ServerBitcoinRpcMonitor,
    ) -> anyhow::Result<DynServerModule> {
        IServerModuleInit::init(&MintInit, peer_num, cfg, db, task_group, our_peer_id, module_api, monitor).await
    }

    fn trusted_dealer_gen(&self, peers: &[PeerId], args: &ConfigGenModuleArgs) -> BTreeMap<PeerId, ServerModuleConfig> {
        let mut configs = IServerModuleInit::trusted_dealer_gen(&MintInit, peers, args);
        for config in configs.values_mut() {
            let mut mint: MintConfig = config.to_typed().expect("test dealer mint config");
            mint.consensus.fee_consensus =
                serde_json::from_value(serde_json::json!({"base":10, "parts_per_million":0})).expect("historical Bitcoin mint fee");
            *config = mint.to_erased();
        }
        configs
    }

    async fn distributed_gen(
        &self,
        _peers: &(dyn PeerHandleOps + Send + Sync),
        _args: &ConfigGenModuleArgs,
    ) -> anyhow::Result<ServerModuleConfig> {
        anyhow::bail!("fixture uses trusted-dealer test generation only")
    }
    fn validate_config(&self, ident: &PeerId, cfg: ServerModuleConfig) -> anyhow::Result<()> {
        IServerModuleInit::validate_config(&MintInit, ident, cfg)
    }
    fn get_client_config(&self, id: ModuleInstanceId, cfg: &ServerModuleConsensusConfig) -> anyhow::Result<ClientModuleConfig> {
        IServerModuleInit::get_client_config(&MintInit, id, cfg)
    }
    fn get_database_migrations(&self) -> BTreeMap<DatabaseVersion, DynServerDbMigrationFn> {
        IServerModuleInit::get_database_migrations(&MintInit)
    }
    fn used_db_prefixes(&self) -> Option<BTreeSet<u8>> {
        IServerModuleInit::used_db_prefixes(&MintInit)
    }
    fn is_enabled_by_default(&self) -> bool {
        IServerModuleInit::is_enabled_by_default(&MintInit)
    }
    fn get_documented_env_vars(&self) -> Vec<EnvVarDoc> {
        IServerModuleInit::get_documented_env_vars(&MintInit)
    }
}
