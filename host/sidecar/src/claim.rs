//! Till claims (`docs/TILL-CLAIM.md`): the till asks the registry for its own till token and an
//! operator approves it in the provisioner app.
//!
//! `till claim` starts a claim (`POST /till-claims`) and writes it to `<data_dir>/till-claim.json`
//! (0600). `till serve`, while it has no token, polls the claim (`POST /till-claims/:id/poll`); on
//! approval it writes the token and the registry's URL to `<data_dir>/till-ops.json` (0600),
//! deletes the claim file, and goes on ops without a restart. The poll secret and the token are
//! never printed or logged.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ops::OpsConfig;

pub const CLAIM_FILE: &str = "till-claim.json";
pub const OPS_FILE: &str = "till-ops.json";
pub const QR_SCHEME: &str = "fedimintcard-till-claim:";
/// How often `serve` asks about a live claim.
pub const POLL_EVERY: Duration = Duration::from_secs(3);
const CLAIM_TIMEOUT: Duration = Duration::from_secs(15);

pub fn claim_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CLAIM_FILE)
}

pub fn ops_path(data_dir: &Path) -> PathBuf {
    data_dir.join(OPS_FILE)
}

/// A claim as `till claim` left it.
#[derive(Clone, Serialize, Deserialize)]
pub struct Claim {
    pub claim_id: String,
    pub poll_secret: String,
    /// `XXXX-XXXX`, as shown.
    pub code: String,
    /// Unix ms, the registry's.
    pub expires_at: u64,
    pub ops_url: String,
    pub name: String,
}

impl std::fmt::Debug for Claim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Claim")
            .field("claim_id", &self.claim_id)
            .field("poll_secret", &"<redacted>")
            .field("code", &self.code)
            .field("expires_at", &self.expires_at)
            .field("ops_url", &self.ops_url)
            .finish()
    }
}

impl Claim {
    pub fn expired(&self, now_ms: u64) -> bool {
        now_ms >= self.expires_at
    }

    /// What the provisioner app scans: `fedimintcard-till-claim:<registry origin>?c=<code>`.
    pub fn qr_text(&self) -> Result<String> {
        Ok(format!("{QR_SCHEME}{}?c={}", origin(&self.ops_url)?, self.code))
    }
}

/// The token file `serve` reads when no `--ops-token` is given.
#[derive(Clone, Default, Serialize, Deserialize)]
struct OpsFile {
    #[serde(default)]
    url: String,
    #[serde(default)]
    token: String,
    /// The registry's certificate key (docs/OFFLINE-PROTOCOL.md § 3), 65 bytes uncompressed, hex:
    /// fetched from `GET /registry/cert-key` while online, used to check cards offline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cert_key: Option<String>,
}

/// `https://registry.example.org` from any URL on it; only http and https.
pub fn origin(url: &str) -> Result<String> {
    let u = reqwest::Url::parse(url.trim()).with_context(|| format!("{url} is not a URL"))?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
        bail!("the registry URL must be http:// or https:// with a host, not {url}");
    }
    Ok(u.origin().ascii_serialization())
}

/// 1 to 64 printable characters, trimmed: what the operator sees before approving.
pub fn check_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 64 || n.chars().any(char::is_control) {
        bail!("the till's name must be 1 to 64 printable characters");
    }
    Ok(n.to_owned())
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn http() -> Result<reqwest::Client> {
    reqwest::Client::builder().timeout(CLAIM_TIMEOUT).build().context("building the HTTP client")
}

async fn post(url: &str, body: &Value) -> Result<(u16, Value)> {
    let resp = http()?.post(url).json(body).send().await.with_context(|| format!("no answer from {url}"))?;
    let status = resp.status().as_u16();
    Ok((status, resp.json().await.unwrap_or(Value::Null)))
}

/// `till claim`: start a claim at `ops_url` and write it to the data directory, replacing any
/// earlier claim. A till that already has a token is refused unless `replace`: its token keeps
/// working until the new claim is approved.
pub async fn start(data_dir: &Path, ops_url: &str, name: &str, replace: bool) -> Result<Claim> {
    let name = check_name(name)?;
    let base = origin(ops_url)?;
    if !replace {
        if let Some(cfg) = read_ops(data_dir)? {
            bail!(
                "this till is already connected to {}; pass --replace to ask for a new token (the current one works until it is approved)",
                cfg.url
            );
        }
    }
    let (status, v) = post(&format!("{base}/till-claims"), &json!({"name": name})).await?;
    if status != 200 {
        bail!("the registry refused the claim ({status}): {}", v["reason"].as_str().unwrap_or("no reason given"));
    }
    let field = |k: &str| v[k].as_str().map(str::to_owned).with_context(|| format!("the registry's answer has no {k}"));
    let claim = Claim {
        claim_id: field("claim_id")?,
        poll_secret: field("poll_secret")?,
        code: field("code")?,
        expires_at: v["expires_at"].as_u64().context("the registry's answer has no expires_at")?,
        ops_url: base,
        name,
    };
    write_private(&claim_path(data_dir), &serde_json::to_vec_pretty(&claim)?)?;
    Ok(claim)
}

/// The claim file, if there is one.
pub fn read_claim(data_dir: &Path) -> Result<Option<Claim>> {
    read_json(&claim_path(data_dir))
}

/// The till token `serve` keeps (`till-ops.json`), if there is one.
pub fn read_ops(data_dir: &Path) -> Result<Option<OpsConfig>> {
    // A file holding only the certificate key (a till given its token by flag) has no token.
    Ok(read_json::<OpsFile>(&ops_path(data_dir))?
        .filter(|f| !f.token.is_empty())
        .map(|f| OpsConfig { url: f.url, token: f.token }))
}

pub fn write_ops(data_dir: &Path, cfg: &OpsConfig) -> Result<()> {
    let old = read_json::<OpsFile>(&ops_path(data_dir)).ok().flatten().unwrap_or_default();
    let f = OpsFile { url: cfg.url.clone(), token: cfg.token.clone(), cert_key: old.cert_key };
    write_private(&ops_path(data_dir), &serde_json::to_vec_pretty(&f)?)
}

/// The registry's certificate key this till keeps (`cert_key` in `till-ops.json`), hex.
pub fn read_cert_key(data_dir: &Path) -> Result<Option<String>> {
    Ok(read_json::<OpsFile>(&ops_path(data_dir))?.and_then(|f| f.cert_key))
}

/// Keep the registry's certificate key in `till-ops.json`, beside the token (a file is made
/// for it when the token came by flag).
pub fn write_cert_key(data_dir: &Path, key_hex: &str) -> Result<()> {
    let mut f = read_json::<OpsFile>(&ops_path(data_dir))?.unwrap_or_default();
    if f.cert_key.as_deref() == Some(key_hex) {
        return Ok(());
    }
    f.cert_key = Some(key_hex.to_owned());
    write_private(&ops_path(data_dir), &serde_json::to_vec_pretty(&f)?)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(serde_json::from_slice(&b).with_context(|| format!("reading {}", path.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// `bytes` at `path`, mode 0600, written whole to a private temporary file first and renamed.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("a file path with no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.{}", hex::encode(rand::random::<[u8; 8]>())));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// What the registry says about a claim.
#[derive(Debug, PartialEq, Eq)]
pub enum Poll {
    Pending,
    Approved { till_id: u64, token: String },
    /// 404: unknown, expired, or never this secret's. It will not come back.
    Gone,
}

pub async fn poll(claim: &Claim) -> Result<Poll> {
    let url = format!("{}/till-claims/{}/poll", claim.ops_url, claim.claim_id);
    let (status, v) = post(&url, &json!({"poll_secret": claim.poll_secret})).await?;
    match (status, v["status"].as_str()) {
        (404, _) => Ok(Poll::Gone),
        (200, Some("pending")) => Ok(Poll::Pending),
        (200, Some("approved")) => Ok(Poll::Approved {
            till_id: v["till_id"].as_u64().context("an approved claim with no till_id")?,
            token: v["token"].as_str().context("an approved claim with no token")?.to_owned(),
        }),
        (s, _) => bail!("the registry answered the poll with {s}: {}", v["reason"].as_str().unwrap_or("no reason given")),
    }
}

/// Keep the token an approved poll answered: write `till-ops.json`, then remove the claim file.
pub fn adopt(data_dir: &Path, claim: &Claim, token: String) -> Result<OpsConfig> {
    let cfg = OpsConfig { url: claim.ops_url.clone(), token };
    write_ops(data_dir, &cfg)?;
    match std::fs::remove_file(claim_path(data_dir)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("removing the claim file"),
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_and_qr_text() {
        assert_eq!(origin("https://registry.fedimintcard.org/").unwrap(), "https://registry.fedimintcard.org");
        assert_eq!(origin("http://127.0.0.1:8787/x?y").unwrap(), "http://127.0.0.1:8787");
        assert!(origin("ftp://x").is_err());
        assert!(origin("registry.fedimintcard.org").is_err());
        let c = Claim {
            claim_id: "a".repeat(32),
            poll_secret: "b".repeat(64),
            code: "K7QM-3XRP".into(),
            expires_at: 0,
            ops_url: "https://registry.fedimintcard.org".into(),
            name: "t".into(),
        };
        assert_eq!(c.qr_text().unwrap(), "fedimintcard-till-claim:https://registry.fedimintcard.org?c=K7QM-3XRP");
        assert!(!format!("{c:?}").contains(&"b".repeat(64)));
    }

    #[test]
    fn the_cert_key_lives_beside_the_token() {
        let dir = tempfile::tempdir().unwrap();
        write_cert_key(dir.path(), "04ab").unwrap();
        assert!(read_ops(dir.path()).unwrap().is_none(), "a key alone is no token");
        assert_eq!(read_cert_key(dir.path()).unwrap().as_deref(), Some("04ab"));
        write_ops(dir.path(), &OpsConfig { url: "http://x".into(), token: "till_t".into() }).unwrap();
        assert_eq!(read_cert_key(dir.path()).unwrap().as_deref(), Some("04ab"), "kept across a new token");
        write_cert_key(dir.path(), "04cd").unwrap();
        assert_eq!(read_ops(dir.path()).unwrap().unwrap().token, "till_t");
        assert_eq!(read_cert_key(dir.path()).unwrap().as_deref(), Some("04cd"));
    }

    #[test]
    fn names() {
        assert_eq!(check_name("  StartOS till ").unwrap(), "StartOS till");
        assert!(check_name("").is_err());
        assert!(check_name("a\nb").is_err());
        assert!(check_name(&"x".repeat(65)).is_err());
        assert!(check_name(&"é".repeat(64)).is_ok());
    }
}
