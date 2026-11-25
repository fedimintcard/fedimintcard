//! The card's clock (docs/OFFLINE-PROTOCOL.md § 4.3): the till keeps the newest Bitcoin block
//! header it has seen and shows it to a 3.0 card (`SET_HEADER`) whenever the card's clock is
//! older. The card's day, and so its daily offline limit, moves only with real headers.
//!
//! Headers come from an Esplora API (`GET /blocks/tip/hash`, then `GET /block/:hash/header`),
//! every [`REFRESH_EVERY`] in the background, and are kept in `<data_dir>/block-header.json` so a
//! till that restarts offline still has the last one. Nothing here ever blocks or fails a card
//! job: an unreachable Esplora only means the card's clock does not move this time.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use fedimint_card_host_card::v3::{FLOOR_BITS, HEADER_LEN, check_header_at, header_has_work, header_hash_hex, header_time};
use serde::{Deserialize, Serialize};

pub const DEFAULT_ESPLORA: &str = "https://mempool.space/api";
pub const REFRESH_EVERY: Duration = Duration::from_secs(600);
pub const HEADER_FILE: &str = "block-header.json";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize, Deserialize)]
struct HeaderFile {
    header: String,
    time: u32,
}

/// The newest header this till knows, and where it comes from.
pub struct Headers {
    url: Option<String>,
    path: PathBuf,
    latest: Mutex<Option<[u8; HEADER_LEN]>>,
    /// The `bits` of the hardest header taken, as a card keeps its own: a header weak against it
    /// is dropped here and never reaches a card.
    hardest: Mutex<Option<[u8; 4]>>,
    http: reqwest::Client,
}

fn bits_of(h: &[u8; HEADER_LEN]) -> [u8; 4] {
    h[72..76].try_into().expect("4 bytes")
}

impl Headers {
    /// `url`: the Esplora base URL; `None` keeps only what the data directory already has.
    pub fn new(url: Option<String>, data_dir: &Path) -> Arc<Self> {
        let path = data_dir.join(HEADER_FILE);
        let latest = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<HeaderFile>(&b).ok())
            .and_then(|f| hex::decode(f.header).ok())
            .and_then(|b| <[u8; HEADER_LEN]>::try_from(b).ok())
            .filter(header_has_work);
        let http = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build().expect("an HTTP client");
        Arc::new(Self {
            url: url.map(|u| u.trim_end_matches('/').to_owned()).filter(|u| !u.is_empty()),
            path,
            latest: Mutex::new(latest),
            hardest: Mutex::new(latest.as_ref().map(bits_of)),
            http,
        })
    }

    /// The newest header kept.
    pub fn latest(&self) -> Option<[u8; HEADER_LEN]> {
        *self.latest.lock().unwrap()
    }

    /// Keep `h` if a card would take it ([`check_header`](fedimint_card_host_card::v3::check_header):
    /// its own work, the floor, and not weak against the hardest this till has taken) and it is
    /// later than the one kept. A header that fails is an error and changes nothing. Answers
    /// whether it was kept.
    pub fn offer(&self, h: [u8; HEADER_LEN]) -> Result<bool> {
        self.offer_at(h, &FLOOR_BITS)
    }

    /// [`offer`](Self::offer) against another floor (tests).
    pub fn offer_at(&self, h: [u8; HEADER_LEN], floor: &[u8; 4]) -> Result<bool> {
        {
            let mut hardest = self.hardest.lock().unwrap();
            let harder = check_header_at(&h, hardest.as_ref(), floor).map_err(|sw| {
                anyhow!("a card would refuse this header ({sw:04X}): it lacks the work its bits claim or the floor's, or it is weak against the hardest kept")
            })?;
            if harder {
                *hardest = Some(bits_of(&h));
            }
        }
        let mut g = self.latest.lock().unwrap();
        if g.is_some_and(|old| header_time(&old) >= header_time(&h)) {
            return Ok(false);
        }
        *g = Some(h);
        drop(g);
        crate::state::write_private_json(&self.path, &HeaderFile { header: hex::encode(h), time: header_time(&h) })?;
        Ok(true)
    }

    /// Fetch the tip's header from Esplora and keep it if it is newer.
    pub async fn refresh(&self) -> Result<[u8; HEADER_LEN]> {
        let url = self.url.as_deref().context("no Esplora URL configured")?;
        let hash = self.get(&format!("{url}/blocks/tip/hash")).await?;
        let hash = hash.trim().to_ascii_lowercase();
        ensure!(hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()), "Esplora answered a bad tip hash");
        let hdr = self.get(&format!("{url}/block/{hash}/header")).await?;
        let bytes = hex::decode(hdr.trim()).context("Esplora answered a header that is not hex")?;
        let h: [u8; HEADER_LEN] = bytes.try_into().map_err(|_| anyhow::anyhow!("Esplora answered a header that is not 80 bytes"))?;
        ensure!(header_hash_hex(&h) == hash, "the header Esplora answered is not the tip it named");
        self.offer(h)?;
        Ok(h)
    }

    async fn get(&self, url: &str) -> Result<String> {
        let r = self.http.get(url).send().await.with_context(|| format!("GET {url}"))?;
        ensure!(r.status().is_success(), "GET {url}: {}", r.status());
        Ok(r.text().await?)
    }

    /// Refresh now and every `every` after, for as long as the till runs. Failures are logged.
    pub fn spawn(self: &Arc<Self>, every: Duration) -> Option<tokio::task::JoinHandle<()>> {
        self.url.as_ref()?;
        let me = self.clone();
        Some(tokio::spawn(async move {
            loop {
                if let Err(e) = me.refresh().await {
                    tracing::warn!("block header: {e:#}");
                }
                tokio::time::sleep(every).await;
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use fedimint_card_host_card::v3::{header_work_hash, target_of};

    use super::*;

    /// A header with `bits` (as the u32 a header holds, little-endian) and `time`, its nonce
    /// searched until it has its own work.
    fn mined(bits: u32, time: u32) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[68..72].copy_from_slice(&time.to_le_bytes());
        h[72..76].copy_from_slice(&bits.to_le_bytes());
        let target = target_of(&bits.to_le_bytes()).unwrap();
        for nonce in 0u32.. {
            h[76..80].copy_from_slice(&nonce.to_le_bytes());
            if header_work_hash(&h) <= target {
                return h;
            }
        }
        unreachable!()
    }

    #[test]
    fn a_header_weak_against_the_hardest_kept_is_dropped_before_any_card() {
        let dir = tempfile::tempdir().unwrap();
        let hs = Headers::new(None, dir.path());
        let floor = 0x207F_FFFF_u32.to_le_bytes();
        // Before any hardest: the floor's own work is weak; twice it is taken.
        assert!(hs.offer_at(mined(0x207F_FFFF, 100), &floor).is_err());
        let a = mined(0x203F_FFFF, 100);
        assert!(hs.offer_at(a, &floor).unwrap());
        // A later header whose target is over twice the hardest's: dropped, nothing changed.
        assert!(hs.offer_at(mined(0x207F_FFFF, 200), &floor).is_err());
        assert_eq!(hs.latest(), Some(a));
        // A harder one raises the hardest, even when it is not later and so not kept.
        assert!(!hs.offer_at(mined(0x201F_FFFF, 50), &floor).unwrap());
        assert_eq!(hs.latest(), Some(a));
        // Against the raised hardest: exactly twice its target is taken, a hair over is weak.
        assert!(hs.offer_at(mined(0x203F_FFFF, 400), &floor).is_err(), "a hair over twice the hardest's target");
        let b = mined(0x203F_FFFE, 300);
        assert!(hs.offer_at(b, &floor).unwrap());
        assert_eq!(hs.latest(), Some(b));
    }
}
