//! Mint v2 exposes spent notes through its quorum-agreed recovery stream. Index
//! each confirmed input hash once in the wallet database. The cursor advances
//! atomically with the page, so an interrupted query can resume for any card.
use std::future::Future;

use anyhow::{Context, Result, ensure};
use fedimint_api_client::api::{DynModuleApi, FederationApiExt as _};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::{Database, IDatabaseTransactionOpsCore as _};
use fedimint_core::encoding::Encodable as _;
use fedimint_core::module::ApiRequestErased;
use fedimint_core::secp256k1::{
    PublicKey,
    hashes::{Hash as _, hash160},
};
use fedimint_mintv2_common::RecoveryItem;
use fedimint_mintv2_common::endpoint_constants::{RECOVERY_COUNT_ENDPOINT, RECOVERY_SLICE_ENDPOINT};
use futures::{StreamExt as _, stream};
use tokio::sync::Mutex;

const PAGE: u64 = 1024;
const PARALLEL: usize = 4;
// Fedimint reserves database prefixes 0xb1..=0xcf for downstream applications.
// The wallet database belongs to one federation. Isolate our schema and its mint instance.
const PREFIX: &[u8] = b"\xb1fedimintcard/spent/v1\0";
const CURSOR: &[u8] = b"through";

pub(crate) struct SpentHistory {
    db: Database,
    sync: Mutex<()>,
}

impl SpentHistory {
    pub(crate) fn new(db: &Database, mint: ModuleInstanceId) -> Self {
        let mut prefix = PREFIX.to_vec();
        prefix.extend_from_slice(&mint.to_be_bytes());
        Self { db: db.with_prefix(prefix), sync: Mutex::new(()) }
    }

    pub(crate) async fn is_spent(&self, api: DynModuleApi, nonce: PublicKey) -> Result<bool> {
        let target: hash160::Hash = nonce.consensus_hash();
        // A spend is permanent. This answer needs neither a refresh nor the updater lock.
        if self.contains(target).await? {
            return Ok(true);
        }
        let _updater = self.sync.lock().await;
        let count: u64 = api.request_current_consensus(RECOVERY_COUNT_ENDPOINT.to_owned(), ApiRequestErased::default()).await?;
        let api = &api;
        self.refresh(count, |start, end| async move {
            Ok(api.request_current_consensus(RECOVERY_SLICE_ENDPOINT.to_owned(), ApiRequestErased::new((start, end))).await?)
        })
        .await?;
        self.contains(target).await
    }

    async fn contains(&self, target: hash160::Hash) -> Result<bool> {
        Ok(self.db.begin_transaction_nc().await.raw_get_bytes(target.as_byte_array()).await?.is_some())
    }

    async fn cursor(&self) -> Result<u64> {
        Self::decode_cursor(self.db.begin_transaction_nc().await.raw_get_bytes(CURSOR).await?)
    }

    fn decode_cursor(bytes: Option<Vec<u8>>) -> Result<u64> {
        let Some(bytes) = bytes else {
            return Ok(0);
        };
        Ok(u64::from_be_bytes(bytes.try_into().ok().context("invalid mint recovery cursor")?))
    }

    async fn refresh<F: Future<Output = Result<Vec<RecoveryItem>>>>(&self, count: u64, page: impl Fn(u64, u64) -> F) -> Result<()> {
        let start = self.cursor().await?;
        ensure!(count >= start, "mint v2 recovery history has moved backwards");
        // Bounded in-flight pages, delivered in cursor order even when a later page returns first.
        let mut pages = stream::iter((start..count).step_by(PAGE as usize))
            .map(|from| {
                let end = from.saturating_add(PAGE).min(count);
                let response = page(from, end);
                async move { Ok::<_, anyhow::Error>((from, end, response.await?)) }
            })
            .buffered(PARALLEL);
        while let Some(next) = pages.next().await {
            let (from, end, items) = next?;
            ensure!(items.len() as u64 == end - from, "incomplete mint v2 recovery page");
            let mut tx = self.db.begin_transaction().await;
            let committed = Self::decode_cursor(tx.raw_get_bytes(CURSOR).await?)?;
            if committed >= end {
                continue;
            }
            ensure!(committed >= from, "gap in the local mint recovery index");
            for item in items {
                if let RecoveryItem::Input { nonce_hash } = item {
                    tx.raw_insert_bytes(nonce_hash.as_byte_array(), &[]).await?;
                }
            }
            tx.raw_insert_bytes(CURSOR, &end.to_be_bytes()).await?;
            tx.commit_tx_result().await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fedimint_core::db::mem_impl::MemDatabase;
    use fedimint_core::module::registry::ModuleRegistry;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn items(from: u64, end: u64) -> Vec<RecoveryItem> {
        (from..end).map(|i| RecoveryItem::Input { nonce_hash: hash160::Hash::hash(&i.to_be_bytes()) }).collect()
    }

    #[tokio::test]
    async fn interrupted_pages_resume_for_every_card_without_rescanning() -> Result<()> {
        let db = Database::new(MemDatabase::new(), ModuleRegistry::default());
        let seen = |n: u64| hash160::Hash::hash(&n.to_be_bytes());
        let index = SpentHistory::new(&db, 3);
        let torn = index.refresh(7238, |from, end| async move { Ok(if from == 2 * PAGE { Vec::new() } else { items(from, end) }) }).await;
        assert!(torn.is_err());
        assert_eq!(index.cursor().await?, 2 * PAGE);
        let fresh = SpentHistory::new(&db, 3);
        let calls = AtomicUsize::new(0);
        let active = AtomicUsize::new(0);
        let high = AtomicUsize::new(0);
        fresh
            .refresh(7238, |from, end| {
                let (calls, active, high) = (&calls, &active, &high);
                async move {
                    assert!(from >= 2 * PAGE);
                    calls.fetch_add(1, Ordering::Relaxed);
                    let n = active.fetch_add(1, Ordering::Relaxed) + 1;
                    high.fetch_max(n, Ordering::Relaxed);
                    tokio::task::yield_now().await;
                    active.fetch_sub(1, Ordering::Relaxed);
                    Ok(items(from, end))
                }
            })
            .await?;
        assert_eq!(calls.load(Ordering::Relaxed), 6);
        assert_eq!(high.load(Ordering::Relaxed), PARALLEL);
        assert!(fresh.contains(seen(0)).await? && fresh.contains(seen(7237)).await?);
        assert!(!fresh.contains(seen(7238)).await?);
        fresh.refresh(7238, |_, _| async { anyhow::bail!("already indexed") }).await?;
        assert!(fresh.refresh(7237, |_, _| async { unreachable!() }).await.is_err());
        assert_eq!(SpentHistory::new(&db, 4).cursor().await?, 0);
        Ok(())
    }
}
