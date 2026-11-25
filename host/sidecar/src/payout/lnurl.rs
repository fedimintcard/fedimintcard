//! Lightning addresses (LUD-16) and LNURL-pay (LUD-06), as strict as a payout needs.
//!
//! - The address's well-known document must be a `payRequest` whose metadata names this very
//!   address (`text/identifier` or `text/email`), and whose callback is HTTPS on the address's
//!   domain or a subdomain of it.
//! - The invoice the callback answers is accepted only if it is for exactly the amount asked, on
//!   the wallet's network, commits to the metadata by its description hash, and has time left.
//!   The caller also refuses a payment hash it has seen before.
//!
//! Every fetch goes through [`super::net::Fetch`].

use fedimint_core::bitcoin::hashes::{Hash as _, sha256};
use lightning_invoice::{Bolt11InvoiceDescriptionRef, Currency};
use reqwest::Url;
use serde_json::Value;

use super::net::{Fetch, FetchError, check_domain, check_url};
use crate::till::Reply;

const MAX_USER: usize = 64;
const MAX_METADATA: usize = 16 * 1024;
const MAX_REASON: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LnAddress {
    pub user: String,
    pub domain: String,
}

impl LnAddress {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let s = s.strip_prefix("lightning:").or_else(|| s.strip_prefix("LIGHTNING:")).unwrap_or(s);
        let (user, domain) = s.split_once('@').ok_or("a Lightning address looks like name@domain")?;
        let user = user.to_ascii_lowercase();
        if user.is_empty() || user.len() > MAX_USER || !user.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(&b)) {
            return Err("the name part may hold a-z, 0-9, '-', '_', '.' and '+' only".into());
        }
        let domain = check_domain(domain)?;
        Ok(Self { user, domain })
    }

    pub fn to_text(&self) -> String {
        format!("{}@{}", self.user, self.domain)
    }

    pub fn well_known(&self) -> Url {
        Url::parse(&format!("https://{}/.well-known/lnurlp/{}", self.domain, self.user)).expect("checked parts")
    }
}

/// A `payRequest`, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayParams {
    pub callback: Url,
    pub callback_host: String,
    pub min_msat: u64,
    pub max_msat: u64,
    pub description: String,
    /// sha256 of the metadata string: what the invoice's description hash must be.
    pub metadata_hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LnurlError {
    Fetch(FetchError),
    /// The service answered `ERROR`, or something that is not a usable answer.
    Refused(String),
    /// The metadata does not name this address.
    Identity(String),
}

impl LnurlError {
    pub fn reply(&self) -> Reply {
        match self {
            LnurlError::Fetch(FetchError::Unsafe(d)) => Reply::fail(400, "lnurl_unsafe", d),
            LnurlError::Fetch(FetchError::Unreachable(d)) => Reply::fail(502, "lnurl_unreachable", d),
            LnurlError::Fetch(FetchError::Refused(d)) | LnurlError::Refused(d) => Reply::fail(502, "lnurl_refused", d),
            LnurlError::Identity(d) => Reply::fail(400, "lnurl_identity", d),
        }
    }
}

impl std::fmt::Display for LnurlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LnurlError::Fetch(e) => write!(f, "{e}"),
            LnurlError::Refused(d) | LnurlError::Identity(d) => f.write_str(d),
        }
    }
}

/// A service's own words, cut short and stripped of control characters.
fn reason_of(v: &Value) -> String {
    let r = v.get("reason").and_then(Value::as_str).unwrap_or("no reason given");
    r.chars().filter(|c| !c.is_control()).take(MAX_REASON).collect()
}

fn refused_if_error(v: &Value) -> Result<(), LnurlError> {
    if v.get("status").and_then(Value::as_str).is_some_and(|s| s.eq_ignore_ascii_case("ERROR")) {
        return Err(LnurlError::Refused(format!("the service refused: {}", reason_of(v))));
    }
    Ok(())
}

/// Whether `host` is `domain` or a subdomain of it.
fn on_domain(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Check a well-known document for `addr` (see the module docs).
pub fn parse_params(addr: &LnAddress, v: &Value) -> Result<PayParams, LnurlError> {
    refused_if_error(v)?;
    let bad = |what: &str| LnurlError::Refused(format!("the service's answer is not a pay request: {what}"));
    if v.get("tag").and_then(Value::as_str) != Some("payRequest") {
        return Err(bad("tag"));
    }
    let callback = v.get("callback").and_then(Value::as_str).ok_or_else(|| bad("callback"))?;
    let callback = Url::parse(callback).map_err(|_| bad("callback"))?;
    let callback_host = check_url(&callback).map_err(LnurlError::Fetch)?;
    if !on_domain(&callback_host, &addr.domain) {
        return Err(LnurlError::Fetch(FetchError::Unsafe(format!(
            "the callback is on {callback_host}, not on {} or a subdomain of it",
            addr.domain
        ))));
    }
    if callback.query_pairs().any(|(k, _)| k == "amount") {
        return Err(bad("the callback already names an amount"));
    }
    let min_msat = v.get("minSendable").and_then(Value::as_u64).ok_or_else(|| bad("minSendable"))?;
    let max_msat = v.get("maxSendable").and_then(Value::as_u64).ok_or_else(|| bad("maxSendable"))?;
    if min_msat == 0 || min_msat > max_msat {
        return Err(bad("minSendable/maxSendable"));
    }
    let metadata = v.get("metadata").and_then(Value::as_str).ok_or_else(|| bad("metadata"))?;
    if metadata.len() > MAX_METADATA {
        return Err(bad("metadata too large"));
    }
    let entries: Vec<Vec<Value>> = serde_json::from_str(metadata).map_err(|_| bad("metadata"))?;
    let text = |kind: &str| {
        entries.iter().find_map(|e| match (e.first().and_then(Value::as_str), e.get(1).and_then(Value::as_str)) {
            (Some(k), Some(t)) if k == kind => Some(t.to_owned()),
            _ => None,
        })
    };
    let me = addr.to_text();
    let named = ["text/identifier", "text/email"].iter().any(|k| text(k).is_some_and(|t| t.trim().eq_ignore_ascii_case(&me)));
    if !named {
        return Err(LnurlError::Identity(format!("the service's metadata does not name {me}")));
    }
    let description: String = text("text/plain").unwrap_or_default().chars().filter(|c| !c.is_control()).take(MAX_REASON).collect();
    Ok(PayParams {
        callback,
        callback_host,
        min_msat,
        max_msat,
        description,
        metadata_hash: sha256::Hash::hash(metadata.as_bytes()).to_byte_array(),
    })
}

pub async fn pay_params(fetch: &dyn Fetch, addr: &LnAddress) -> Result<PayParams, LnurlError> {
    let v = fetch.get_json(addr.well_known()).await.map_err(LnurlError::Fetch)?;
    parse_params(addr, &v)
}

/// The callback asked for `amount_msat`.
pub fn callback_url(params: &PayParams, amount_msat: u64) -> Url {
    let mut u = params.callback.clone();
    u.query_pairs_mut().append_pair("amount", &amount_msat.to_string());
    u
}

/// Ask the callback for an invoice of `amount_msat`: the `pr` it answers, unchecked.
pub async fn request_invoice(fetch: &dyn Fetch, params: &PayParams, amount_msat: u64) -> Result<String, LnurlError> {
    let v = fetch.get_json(callback_url(params, amount_msat)).await.map_err(LnurlError::Fetch)?;
    refused_if_error(&v)?;
    v.get("pr").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| LnurlError::Refused("the service answered no invoice".into()))
}

/// What the invoice must be.
#[derive(Debug, Clone)]
pub struct Expect {
    pub amount_msat: u64,
    pub currency: Currency,
    pub description_hash: [u8; 32],
    pub now_ms: u64,
    pub min_expiry_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedInvoice {
    pub bolt11: String,
    pub payment_hash: String,
    pub payee: String,
    pub expires_at_ms: u64,
}

/// Accept `pr` only if it is exactly the invoice asked for (see the module docs). Its signature is
/// checked by the parse.
pub fn check_invoice(pr: &str, e: &Expect) -> Result<CheckedInvoice, String> {
    let inv = crate::wallet::parse_bolt11(pr).map_err(|err| format!("{err:#}"))?;
    let amount = inv.amount_milli_satoshis().unwrap_or(0);
    if amount != e.amount_msat {
        return Err(format!("the invoice is for {amount} msat, not the {} msat asked", e.amount_msat));
    }
    if inv.currency() != e.currency {
        return Err(format!("the invoice is for {:?}, not {:?}", inv.currency(), e.currency));
    }
    match inv.description() {
        Bolt11InvoiceDescriptionRef::Hash(h) if h.0.to_byte_array() == e.description_hash => {}
        _ => return Err("the invoice's description hash does not commit to the address's metadata".into()),
    }
    let expires_at_ms = inv.expires_at().map_or(u64::MAX, |d| d.as_millis().min(u64::MAX as u128) as u64);
    if expires_at_ms <= e.now_ms.saturating_add(e.min_expiry_secs.saturating_mul(1000)) {
        return Err("the invoice expires too soon".into());
    }
    Ok(CheckedInvoice {
        bolt11: inv.to_string(),
        payment_hash: inv.payment_hash().to_string(),
        payee: inv.get_payee_pub_key().to_string(),
        expires_at_ms,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use fedimint_core::secp256k1::{SECP256K1, SecretKey};
    use lightning_invoice::{InvoiceBuilder, PaymentSecret};
    use serde_json::json;

    use super::*;

    pub(crate) const META: &str = r#"[["text/plain","Pay to shop"],["text/identifier","shop@example.com"]]"#;

    pub(crate) fn invoice_with(currency: Currency, msat: u64, hash: Option<[u8; 32]>, expiry: Duration, preimage: [u8; 32]) -> String {
        let key = SecretKey::from_slice(&[9u8; 32]).unwrap();
        let b = InvoiceBuilder::new(currency);
        let b = match hash {
            Some(h) => b.description_hash(sha256::Hash::from_byte_array(h)),
            None => b.description("direct".into()),
        };
        b.payment_hash(sha256::Hash::hash(&preimage))
            .payment_secret(PaymentSecret([42u8; 32]))
            .current_timestamp()
            .min_final_cltv_expiry_delta(144)
            .expiry_time(expiry)
            .amount_milli_satoshis(msat)
            .build_signed(|m| SECP256K1.sign_ecdsa_recoverable(m, &key))
            .unwrap()
            .to_string()
    }

    pub(crate) fn meta_hash() -> [u8; 32] {
        sha256::Hash::hash(META.as_bytes()).to_byte_array()
    }

    pub(crate) fn well_known(callback: &str) -> Value {
        json!({"tag": "payRequest", "callback": callback, "minSendable": 1000, "maxSendable": 100_000_000_000u64, "metadata": META})
    }

    fn addr() -> LnAddress {
        LnAddress::parse("Shop@Example.com").unwrap()
    }

    #[test]
    fn addresses_parse_strictly() {
        assert_eq!(addr(), LnAddress { user: "shop".into(), domain: "example.com".into() });
        assert_eq!(addr().well_known().as_str(), "https://example.com/.well-known/lnurlp/shop");
        assert!(LnAddress::parse("lightning:shop@example.com").is_ok());
        for bad in [
            "shop",
            "@example.com",
            "shop@localhost",
            "shop@10.0.0.1",
            "shop@[::1]",
            "sh op@example.com",
            "shop@ex/ample.com",
            "a@b@example.com",
        ] {
            assert!(LnAddress::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_pay_request_must_name_the_address_and_call_back_on_its_domain() {
        let p = parse_params(&addr(), &well_known("https://pay.example.com/cb/1?k=v")).unwrap();
        assert_eq!(p.callback_host, "pay.example.com");
        assert_eq!(p.description, "Pay to shop");
        assert_eq!(p.metadata_hash, meta_hash());
        assert_eq!(callback_url(&p, 5_000).as_str(), "https://pay.example.com/cb/1?k=v&amount=5000");

        let unsafe_cb = |cb: &str| matches!(parse_params(&addr(), &well_known(cb)), Err(LnurlError::Fetch(FetchError::Unsafe(_))));
        assert!(unsafe_cb("https://evil.com/cb"), "another domain");
        assert!(unsafe_cb("https://notexample.com/cb"), "a suffix that is not a subdomain");
        assert!(unsafe_cb("http://example.com/cb"), "not HTTPS");
        assert!(unsafe_cb("https://127.0.0.1/cb"), "an IP literal");
        assert!(unsafe_cb("https://example.com:8080/cb"), "another port");

        let mut other = well_known("https://example.com/cb");
        other["metadata"] = json!(r#"[["text/plain","x"],["text/identifier","thief@example.com"]]"#);
        assert!(matches!(parse_params(&addr(), &other), Err(LnurlError::Identity(_))));
        let mut none = well_known("https://example.com/cb");
        none["metadata"] = json!(r#"[["text/plain","x"]]"#);
        assert!(matches!(parse_params(&addr(), &none), Err(LnurlError::Identity(_))));

        let err = json!({"status": "ERROR", "reason": "no such user\u{7}"});
        assert_eq!(parse_params(&addr(), &err), Err(LnurlError::Refused("the service refused: no such user".into())));
        let mut bounds = well_known("https://example.com/cb");
        bounds["minSendable"] = json!(5000);
        bounds["maxSendable"] = json!(1000);
        assert!(matches!(parse_params(&addr(), &bounds), Err(LnurlError::Refused(_))));
        assert!(matches!(parse_params(&addr(), &well_known("https://example.com/cb?amount=1")), Err(LnurlError::Refused(_))));
    }

    #[test]
    fn only_the_exact_invoice_is_accepted() {
        let now = crate::topup::now_ms();
        let e =
            Expect { amount_msat: 21_000, currency: Currency::Regtest, description_hash: meta_hash(), now_ms: now, min_expiry_secs: 60 };
        let good = invoice_with(Currency::Regtest, 21_000, Some(meta_hash()), Duration::from_secs(600), [1; 32]);
        let c = check_invoice(&good, &e).unwrap();
        assert_eq!(c.payment_hash, sha256::Hash::hash(&[1u8; 32]).to_string());

        let wrong_amount = invoice_with(Currency::Regtest, 21_001, Some(meta_hash()), Duration::from_secs(600), [2; 32]);
        assert!(check_invoice(&wrong_amount, &e).unwrap_err().contains("msat"));
        let wrong_net = invoice_with(Currency::Bitcoin, 21_000, Some(meta_hash()), Duration::from_secs(600), [3; 32]);
        assert!(check_invoice(&wrong_net, &e).is_err());
        let wrong_hash = invoice_with(Currency::Regtest, 21_000, Some([7; 32]), Duration::from_secs(600), [4; 32]);
        assert!(check_invoice(&wrong_hash, &e).unwrap_err().contains("description hash"));
        let direct = invoice_with(Currency::Regtest, 21_000, None, Duration::from_secs(600), [5; 32]);
        assert!(check_invoice(&direct, &e).unwrap_err().contains("description hash"));
        let short = invoice_with(Currency::Regtest, 21_000, Some(meta_hash()), Duration::from_secs(30), [6; 32]);
        assert!(check_invoice(&short, &e).unwrap_err().contains("expires"));
        assert!(check_invoice("lnbcrt1garbage", &e).is_err());
    }
}
