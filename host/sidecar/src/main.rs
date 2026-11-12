//! `card-sidecar`: the card wallet CLI.
//!
//! The backend comes from `CARD_BACKEND` (soft | bridge | pcsc), exactly as in the test suite.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fedimint_card_host_card::open_from_env;
use fedimint_card_sidecar::glue::{CardWallet, Recovered};
use fedimint_card_sidecar::sales::SalesStore;
use fedimint_card_sidecar::state::{NoteStatus, StateStore};
use fedimint_card_sidecar::wallet::{Wallet, sales_path, state_path};

#[derive(Parser)]
#[command(name = "card-sidecar", about = "Fedimint ecash on a JavaCard")]
struct Cli {
    /// Directory holding the wallet database, the mnemonic and card_state.json.
    #[arg(long, env = "CARD_DATA_DIR", default_value = "./card-data", global = true)]
    data_dir: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Join a federation by invite code (first run).
    Join { invite: String },
    /// Show the federation, the wallet balance and the card state file.
    Info,
    /// Ecash in and out of the wallet.
    #[command(subcommand)]
    Ecash(Ecash),
    /// Operate on the card.
    #[command(subcommand)]
    Card(CardCmd),
    /// The point-of-sale HTTP service (see docs/TILL-API.md).
    #[command(subcommand)]
    Till(TillCmd),
    /// Lightning through the federation's gateways.
    #[command(subcommand)]
    Ln(LnCmd),
    /// Serve the USB (PC/SC) reader's card over the bridge protocol, for `till serve --relay`:
    /// the till then charges, checks and loads the card on the reader. Loopback only; needs a
    /// build with `--features pcsc`. Takes no wallet, so it runs beside `till serve`.
    Relay {
        #[arg(long, default_value = "127.0.0.1:9876")]
        listen: std::net::SocketAddr,
        /// A reader whose name contains this (the first reader otherwise).
        #[arg(long, env = "CARD_PCSC_READER")]
        reader: Option<String>,
    },
}

#[derive(Subcommand)]
enum LnCmd {
    /// The gateways registered with the federation, with their fees.
    Gateways,
    /// Make an invoice for `amount_msat`, print it, and wait (up to 10 minutes) until it is paid
    /// and the eCash is in the wallet.
    Receive {
        amount_msat: u64,
        /// Use this gateway (its id, as `ln gateways` prints it) instead of letting the client pick.
        #[arg(long)]
        gateway: Option<String>,
    },
    /// Pay a BOLT11 invoice (with an amount) from the wallet's balance through a gateway, follow
    /// it for up to 90 s, and print the outcome as JSON: `status` paid, pending (still in flight:
    /// keep the operation id), refunded, failed or refused (nothing spent). Exits 1 unless paid or
    /// pending.
    Pay {
        bolt11: String,
        /// Refuse, before spending anything, a gateway whose fee for this invoice is above this.
        #[arg(long)]
        max_fee_msat: Option<u64>,
    },
}

#[derive(Subcommand)]
enum TillCmd {
    /// Serve the till API. Owns the data directory: do not run other commands meanwhile.
    Serve {
        /// Address to listen on. Anything but loopback needs the POS token (`till pair`), and
        /// every request not from loopback must then carry it (docs/POS-LINK.md).
        #[arg(long, env = "TILL_LISTEN", default_value = fedimint_card_sidecar::till::DEFAULT_LISTEN)]
        listen: std::net::SocketAddr,
        /// The phone's NFC relay (reach it with `adb forward tcp:9876 tcp:9876`).
        #[arg(long, env = "CARD_BRIDGE_URL", default_value = "http://127.0.0.1:9876")]
        relay: String,
        /// The ops registry (docs/OPS-PLAN.md § 6). With it, each card's state comes from ops,
        /// keyed by the card's short id, instead of card_state.json. The token is --ops-token, or
        /// else the one an approved `till claim` left in <data_dir>/till-ops.json; with neither,
        /// the till waits for a claim and refuses card jobs (docs/TILL-CLAIM.md).
        #[arg(long, env = "OPS_URL")]
        ops_url: Option<String>,
        /// This till's token (`till_…`, from `POST /tills`). Needs --ops-url. Takes precedence
        /// over <data_dir>/till-ops.json.
        #[arg(long, env = "OPS_TILL_TOKEN", hide_env_values = true, requires = "ops_url")]
        ops_token: Option<String>,
        /// Require the POS token on every request, loopback included, and make the token now if
        /// there is none. For a till behind a proxy (StartOS), where a phone's request can
        /// arrive from a local address.
        #[arg(long, env = "TILL_REQUIRE_POS_TOKEN", value_parser = clap::builder::BoolishValueParser::new())]
        require_pos_token: bool,
        /// Esplora API for Bitcoin block headers, the clock of a 3.0 card's daily offline limit
        /// (fetched every 10 minutes; an empty value turns it off).
        #[arg(long, env = "TILL_ESPLORA_URL", default_value = fedimint_card_sidecar::clock::DEFAULT_ESPLORA)]
        esplora_url: String,
        /// Most value, in msat, this till holds in offline charges not yet settled.
        #[arg(long, env = "TILL_OFFLINE_CAP_MSAT", default_value_t = fedimint_card_sidecar::offline::DEFAULT_CAP_MSAT)]
        offline_cap_msat: u64,
        /// Most offline charges this till holds not yet settled.
        #[arg(long, env = "TILL_OFFLINE_CAP_COUNT", default_value_t = fedimint_card_sidecar::offline::DEFAULT_CAP_COUNT)]
        offline_cap_count: usize,
        /// The fee margin of offline charges: they keep room for a fee schedule of this base fee
        /// (msat per input and per output) plus --offline-fee-headroom-ppm, against a fee rise
        /// before they are sent. Only on a federation at core consensus 2.1 or later; 0 and 0
        /// turn the margin off. Default: fedimint's default mint fees (100 msat, 0 ppm).
        #[arg(long, env = "TILL_OFFLINE_FEE_BASE_MSAT", default_value_t = fedimint_card_sidecar::plan::FeeHeadroom::DEFAULT.base_msat)]
        offline_fee_base_msat: u64,
        /// The relative part of that fee schedule, parts per million of each note (fedimint
        /// allows at most 1,000).
        #[arg(long, env = "TILL_OFFLINE_FEE_HEADROOM_PPM", default_value_t = fedimint_card_sidecar::plan::FeeHeadroom::DEFAULT.ppm)]
        offline_fee_headroom_ppm: u64,
        /// Fresh keyed slots an online tap keeps an applet 3.0 card at, so it can take change
        /// offline (0 turns the reserve off).
        #[arg(long, env = "TILL_SPARE_TARGET", default_value_t = fedimint_card_sidecar::ops::SPARE_TARGET)]
        spare_target: usize,
        /// Most slots one tap keys for that reserve (a key takes about 320 ms on the card).
        #[arg(long, env = "TILL_SPARE_KEYS_PER_TAP", default_value_t = fedimint_card_sidecar::ops::MAX_KEY_PER_TAP)]
        spare_keys_per_tap: usize,
        /// How long a POS phone's offline kit lasts from the fetch that answered it, in hours
        /// (`GET /v1/offline/kit`).
        #[arg(long, env = "TILL_OFFLINE_KIT_TTL_HOURS", default_value_t = 168)]
        offline_kit_ttl_hours: u64,
        /// Days after a kit expires that the charges a phone took with it are still taken; then
        /// its unused pool entries are dropped with their secrets.
        #[arg(long, env = "TILL_OFFLINE_KIT_GRACE_DAYS", default_value_t = 30)]
        offline_kit_grace_days: u64,
        /// Pay Lightning invoices with a card (`POST /v1/pay`): the till charges the card for the
        /// invoice, its fee reserve and margin, then pays the invoice from its wallet. Off by default.
        #[arg(long, env = "TILL_PAY_ENABLED", value_parser = clap::builder::BoolishValueParser::new())]
        pay_enabled: bool,
        /// Largest invoice paid with a card, in msat.
        #[arg(long, env = "TILL_PAY_MAX_INVOICE_MSAT", default_value_t = fedimint_card_sidecar::pay::PayConfig::DEFAULT_MAX_INVOICE_MSAT)]
        pay_max_invoice_msat: u64,
        /// Cap on a pay's fee reserve, in msat: a larger `max_fee_msat` is lowered to it.
        #[arg(long, env = "TILL_PAY_MAX_FEE_MSAT", default_value_t = fedimint_card_sidecar::pay::PayConfig::DEFAULT_MAX_FEE_MSAT)]
        pay_max_fee_msat: u64,
        /// The till's margin on each pay, in msat (added to --pay-margin-bps).
        #[arg(long, env = "TILL_PAY_MARGIN_MSAT", default_value_t = 0)]
        pay_margin_msat: u64,
        /// The till's margin on each pay, in basis points of the invoice.
        #[arg(long, env = "TILL_PAY_MARGIN_BPS", default_value_t = 0)]
        pay_margin_bps: u64,
        /// Most value, in msat, the till charges cards for pays in any 24 hours.
        #[arg(long, env = "TILL_PAY_DAILY_CAP_MSAT", default_value_t = fedimint_card_sidecar::pay::PayConfig::DEFAULT_DAILY_CAP_MSAT)]
        pay_daily_cap_msat: u64,
        /// Least time, in seconds, an invoice must have left before it expires.
        #[arg(long, env = "TILL_PAY_MIN_EXPIRY_SECS", default_value_t = fedimint_card_sidecar::pay::PayConfig::DEFAULT_MIN_EXPIRY_SECS)]
        pay_min_expiry_secs: u64,
    },
    /// Ask the registry for this till's token (docs/TILL-CLAIM.md): print the QR code an operator
    /// scans in the provisioner app (Settings, Tills, Add a till), write the claim to
    /// <data_dir>/till-claim.json and exit. `till serve` polls it and goes on ops once approved.
    Claim {
        /// The registry, e.g. https://registry.fedimintcard.org.
        #[arg(long, env = "OPS_URL")]
        ops_url: String,
        /// What the operator sees before approving, and what the registry lists: 1 to 64
        /// characters.
        #[arg(long)]
        name: String,
        /// Print only the QR text and, on a second line, the code: no QR code, no notes.
        #[arg(long)]
        text: bool,
        /// Ask even though this till already has a token. The current token keeps working until
        /// an operator approves the new claim.
        #[arg(long)]
        replace: bool,
    },
    /// Pair a POS phone: make the POS token if there is none, and print it as a QR code to scan
    /// in the POS app's settings (docs/POS-LINK.md).
    Pair {
        /// The till's URL as the phone reaches it: http:// or https:// (behind a proxy that
        /// terminates TLS; the phone then uses wss:// for the card link), host and port only.
        /// Default: http://<first LAN IPv4>:9877.
        #[arg(long)]
        url: Option<String>,
        /// Print only the pairing text on stdout: no QR code, no notes on stderr.
        #[arg(long)]
        text: bool,
        /// Replace the POS token first. Every paired phone must pair again, and a running till
        /// keeps the old token until it restarts.
        #[arg(long)]
        new_token: bool,
    },
}

#[derive(Subcommand)]
enum Ecash {
    /// Reissue an ecash string into the wallet.
    Import { notes: String },
    /// Print an ecash string worth exactly `amount_msat`.
    /// Reissue an ecash string this wallet exported earlier and nobody redeemed. Fails if the
    /// recipient already did.
    Reclaim { notes: String },
    Export {
        amount_msat: u64,
        /// Also draw the string as a QR code.
        #[arg(long)]
        qr: bool,
        /// Accept an amount that is not a multiple of the federation's smallest economical note
        /// and let the wallet round it up.
        #[arg(long)]
        round_up: bool,
        /// Put the federation invite code in the string so the recipient can join.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set, value_name = "BOOL")]
        include_invite: bool,
    },
}

#[derive(Subcommand)]
enum CardCmd {
    /// Generate keys in every empty slot and cache the public keys.
    Setup {
        /// New wallet on a card that was used before: replace every key and treat all slots as
        /// free. Refused while the state file has live or pending notes.
        #[arg(long)]
        fresh: bool,
    },
    /// Move wallet funds onto the card, one note per set bit of each amount.
    Topup {
        #[arg(required = true)]
        amount_msat: Vec<u64>,
    },
    /// Load the card from this wallet through ops, as a holder's wallet would (OPS-PLAN § 6):
    /// one note per set bit of each amount, into the free slots ops reports. Needs no PIN. With
    /// this till's claimed token for the same registry, it keys unkeyed slots the load needs.
    Load {
        #[arg(required = true)]
        amount_msat: Vec<u64>,
        /// The ops registry. Holder routes take no token.
        #[arg(long, env = "OPS_URL")]
        ops_url: String,
    },
    /// Reconcile and list the card's notes (asks the federation about each live note).
    Balance,
    /// Pay the till from the card.
    Spend {
        #[arg(long)]
        price: u64,
    },
    /// Turn the note in a slot into an ecash string.
    Cashout {
        slot: u8,
        #[arg(long)]
        qr: bool,
    },
}

fn qr(data: &str) -> Result<()> {
    use qrcode::render::unicode::Dense1x2;
    let code = qrcode::QrCode::new(data.as_bytes()).context("ecash string too long for a QR code")?;
    println!("{}", code.render::<Dense1x2>().quiet_zone(true).build());
    Ok(())
}

fn sats(msat: u64) -> String {
    format!("{msat} msat ({:.3} sat)", msat as f64 / 1000.0)
}

async fn card_wallet(cli: &Cli) -> Result<CardWallet> {
    let wallet = Wallet::open(&cli.data_dir).await?;
    let store = StateStore::open(state_path(&cli.data_dir))?;
    let card = open_from_env().context("opening the card")?;
    let mut cw = CardWallet::new(wallet, store, card);
    // The applet drops its PIN session on every SELECT (`select()` calls `pin.reset()`), and
    // `open_from_env` has just done the one SELECT this connection will make, so verifying here
    // covers every later command on it. A card whose PIN nobody named answers `CARD_PIN` or the
    // test PIN; a wrong one costs a retry, so this is reported rather than fatal.
    if let Err(e) = cw.verify_pin_session() {
        tracing::warn!("could not open the PIN session: {e:#}");
    }
    // Any command that resolves a till sale must record it where the till service looks.
    let sales = std::sync::Arc::new(std::sync::Mutex::new(SalesStore::open(sales_path(&cli.data_dir))?));
    fedimint_card_sidecar::till::ensure_hook(&mut cw, &sales);
    Ok(cw)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::WARN).with_writer(std::io::stderr).init();
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Relay { listen, reader } => {
            fedimint_card_sidecar::relay::serve(*listen, reader.clone()).await?;
        }
        Cmd::Join { invite } => {
            let w = Wallet::join(&cli.data_dir, invite).await?;
            println!("joined federation {}", w.federation_id());
            println!("data dir: {}", cli.data_dir.display());
            println!("back up {}/mnemonic: it is the wallet's root secret", cli.data_dir.display());
        }
        Cmd::Ln(LnCmd::Gateways) => {
            let w = Wallet::open(&cli.data_dir).await?;
            let ln = w.lightning()?;
            ln.update_gateway_cache().await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
            let gws = ln.list_gateways().await;
            println!("{} gateway(s)", gws.len());
            for g in gws {
                println!(
                    "{} vetted={} fees base {} msat + {} ppm  {}",
                    g.info.gateway_id, g.vetted, g.info.fees.base_msat, g.info.fees.proportional_millionths, g.info.api
                );
            }
        }
        Cmd::Ln(LnCmd::Receive { amount_msat, gateway }) => {
            use futures::StreamExt as _;
            let w = Wallet::open(&cli.data_dir).await?;
            let ln = w.lightning()?;
            ln.update_gateway_cache().await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
            let want = match gateway {
                Some(id) => Some(
                    ln.list_gateways()
                        .await
                        .into_iter()
                        .map(|g| g.info)
                        .find(|g| g.gateway_id.to_string() == *id)
                        .with_context(|| format!("no gateway {id} in the federation (see `ln gateways`)"))?,
                ),
                None => None,
            };
            let gw = ln.select_available_gateway(want, None).await.map_err(|e| anyhow::anyhow!("no gateway: {e:?}"))?;
            eprintln!("gateway {} ({})", gw.gateway_id, gw.api);
            let desc =
                lightning_invoice::Bolt11InvoiceDescription::Direct(lightning_invoice::Description::new("fedimintcard top-up".into())?);
            let (op, invoice, _) = ln
                .create_bolt11_invoice(fedimint_core::Amount::from_msats(*amount_msat), desc, Some(600), (), Some(gw))
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            println!("{invoice}");
            let mut updates = ln.subscribe_ln_receive(op).await?.into_stream();
            while let Some(st) = updates.next().await {
                eprintln!("{st:?}");
                if matches!(st, fedimint_ln_client::LnReceiveState::Claimed | fedimint_ln_client::LnReceiveState::Canceled { .. }) {
                    break;
                }
            }
            println!("wallet balance now {} msat", w.balance_msat().await?);
        }
        Cmd::Ln(LnCmd::Pay { bolt11, max_fee_msat }) => {
            use fedimint_card_sidecar::wallet::LnPayOutcome;
            let w = Wallet::open(&cli.data_dir).await?;
            let outcome = w.pay_bolt11(bolt11, *max_fee_msat).await?;
            println!("{}", serde_json::to_string(&outcome)?);
            if !matches!(outcome, LnPayOutcome::Paid { .. } | LnPayOutcome::Pending { .. }) {
                std::process::exit(1);
            }
        }
        Cmd::Info => {
            let w = Wallet::open(&cli.data_dir).await?;
            let st = StateStore::open(state_path(&cli.data_dir))?;
            println!("federation: {}", w.federation_id());
            let cfg = w.client.config().await;
            println!(
                "name: {}; guardians: {}; core consensus: {}.{}",
                cfg.global.meta.get("federation_name").map_or("?", String::as_str),
                cfg.global.api_endpoints.len(),
                cfg.global.consensus_version.major,
                cfg.global.consensus_version.minor
            );
            for (id, m) in &cfg.modules {
                println!("module {id}: {} v{}.{}", m.kind, m.version.major, m.version.minor);
            }
            println!("wallet balance: {}", sats(w.balance_msat().await?));
            println!("mint fee: {} msat for a 1,048,576 msat note (per input and per output)", w.fee(1 << 20));
            let s = &st.state;
            println!(
                "card state: {} slot(s) cached, {} live note(s) worth {}, {} free slot(s), {} pending overwrite(s), {} in-flight host output(s)",
                s.slots.len(),
                s.live_notes().len(),
                sats(s.live_total_msat()),
                s.free_slots().len(),
                s.pending_overwrites.len(),
                s.host_pending.len()
            );
        }
        Cmd::Ecash(Ecash::Import { notes }) => {
            let w = Wallet::open(&cli.data_dir).await?;
            let imp = w.import(notes).await?;
            if imp.already {
                println!("these notes were already reissued into this wallet");
            } else {
                println!("reissued {} (fees {} msat)", sats(imp.face.msats), imp.fee_msat);
            }
            println!("wallet balance now {}", sats(w.balance_msat().await?));
        }
        Cmd::Ecash(Ecash::Reclaim { notes }) => {
            let w = Wallet::open(&cli.data_dir).await?;
            match w.import(notes).await {
                Ok(imp) if imp.already => println!("already reissued into this wallet"),
                Ok(imp) => println!("reclaimed {} (fees {} msat)", sats(imp.face.msats), imp.fee_msat),
                Err(e) => return Err(e.context("could not reclaim: the recipient probably redeemed it already")),
            }
            println!("wallet balance now {}", sats(w.balance_msat().await?));
        }
        Cmd::Ecash(Ecash::Export { amount_msat, qr: want_qr, round_up, include_invite }) => {
            let w = Wallet::open(&cli.data_dir).await?;
            let (notes, face) = w.export(*amount_msat, *round_up, *include_invite).await?;
            // The log comes before the screen: this string is the only copy of the value.
            StateStore::open(state_path(&cli.data_dir))?.append_cashout_log(&notes.to_string())?;
            println!("{notes}");
            eprintln!("exported {}", sats(face.msats));
            if *want_qr {
                qr(&notes.to_string())?;
            }
        }
        Cmd::Till(TillCmd::Serve {
            listen,
            relay,
            ops_url,
            ops_token,
            require_pos_token,
            esplora_url,
            offline_cap_msat,
            offline_cap_count,
            offline_fee_base_msat,
            offline_fee_headroom_ppm,
            spare_target,
            spare_keys_per_tap,
            offline_kit_ttl_hours,
            offline_kit_grace_days,
            pay_enabled,
            pay_max_invoice_msat,
            pay_max_fee_msat,
            pay_margin_msat,
            pay_margin_bps,
            pay_daily_cap_msat,
            pay_min_expiry_secs,
        }) => {
            let wallet = Wallet::open(&cli.data_dir).await?;
            let store = StateStore::open(state_path(&cli.data_dir))?;
            let cw = CardWallet::new(wallet, store, Box::new(fedimint_card_sidecar::till::NoCard));
            let opts = fedimint_card_sidecar::till::ServeOptions {
                esplora_url: Some(esplora_url.clone()).filter(|u| !u.trim().is_empty()),
                offline: fedimint_card_sidecar::offline::OfflineConfig {
                    cap_msat: *offline_cap_msat,
                    cap_count: *offline_cap_count,
                    fee_headroom: fedimint_card_sidecar::plan::FeeHeadroom {
                        base_msat: *offline_fee_base_msat,
                        ppm: *offline_fee_headroom_ppm,
                    },
                },
                spares: fedimint_card_sidecar::ops::SpareConfig { target: *spare_target, max_per_tap: *spare_keys_per_tap },
                kits: fedimint_card_sidecar::pool::KitConfig {
                    ttl_ms: offline_kit_ttl_hours.saturating_mul(3_600_000),
                    grace_ms: offline_kit_grace_days.saturating_mul(86_400_000),
                },
                pay: fedimint_card_sidecar::pay::PayConfig {
                    enabled: *pay_enabled,
                    max_invoice_msat: *pay_max_invoice_msat,
                    max_fee_msat: *pay_max_fee_msat,
                    margin_msat: *pay_margin_msat,
                    margin_bps: *pay_margin_bps,
                    daily_cap_msat: *pay_daily_cap_msat,
                    min_expiry_secs: *pay_min_expiry_secs,
                    ..Default::default()
                },
            };
            fedimint_card_sidecar::till::serve_opts(
                cw,
                *listen,
                relay.clone(),
                ops_url.clone(),
                ops_token.clone(),
                *require_pos_token,
                opts,
            )
            .await?;
        }
        Cmd::Till(TillCmd::Claim { ops_url, name, text: text_only, replace }) => {
            use fedimint_card_sidecar::claim;
            let c = claim::start(&cli.data_dir, ops_url, name, *replace).await?;
            let text = c.qr_text()?;
            if *text_only {
                println!("{text}");
                println!("{}", c.code);
                return Ok(());
            }
            qr(&text)?;
            println!("{text}");
            println!("code: {}", c.code);
            eprintln!(
                "in the provisioner app open Settings, Tills, Add a till, and scan this or type the code. It expires in {} minutes; `till serve` picks up the token once it is approved.",
                c.expires_at.saturating_sub(claim::now_ms()) / 60_000
            );
        }
        Cmd::Till(TillCmd::Pair { url, text: text_only, new_token }) => {
            use fedimint_card_sidecar::pos;
            // Check the URL before touching the token, so a typo changes nothing.
            let url = match url {
                Some(u) => pos::check_till_url(u)?,
                None => format!("http://{}:9877", pos::lan_ipv4()?),
            };
            let (token, made) = if *new_token { (pos::rotate_token(&cli.data_dir)?, true) } else { pos::ensure_token(&cli.data_dir)? };
            let text = pos::pair_text(&url, &token);
            if *text_only {
                println!("{text}");
                return Ok(());
            }
            if *new_token {
                eprintln!("replaced the POS token in {}: every phone must pair again", pos::token_path(&cli.data_dir).display());
            } else if made {
                eprintln!("made a POS token in {}", pos::token_path(&cli.data_dir).display());
            }
            qr(&text)?;
            println!("{text}");
            eprintln!("scan it in the POS app: Settings -> Pair with till. Restart the till to listen on the LAN.");
        }
        Cmd::Card(CardCmd::Load { amount_msat, ops_url }) => {
            // No `card_wallet`: a load never signs, so no PIN session is opened (a guessed PIN
            // would cost the holder a try).
            let w = Wallet::open(&cli.data_dir).await?;
            let mut card = open_from_env().context("opening the card")?;
            let mut loader = fedimint_card_sidecar::holder::Loader::new(&w, ops_url, &cli.data_dir)?;
            // A till's own wallet loading a split-keyed card can key its unkeyed slots with the
            // token an approved claim left, when it is for this registry (`holder.rs`).
            if let Some(cfg) = fedimint_card_sidecar::claim::read_ops(&cli.data_dir)?
                && cfg.url.trim_end_matches('/') == ops_url.trim_end_matches('/')
            {
                loader = loader.with_keyer(fedimint_card_sidecar::ops::OpsClient::new(&cfg)?);
            }
            let r = loader.load(card.as_mut(), amount_msat).await?;
            if r.resumed > 0 {
                println!("finished {} load(s) an earlier run left", r.resumed);
            }
            println!("card {}: load {} accepted and recorded in ops", r.short_id, r.txid);
            for (slot, amount) in &r.notes {
                println!("slot {slot:2}  +{}", sats(*amount));
            }
        }
        Cmd::Card(c) => {
            let mut cw = card_wallet(&cli).await?;
            let res = run_card(&mut cw, c).await;
            // Whatever happened, ecash recovered from legacy records must reach the user. It is
            // also in cashouts.log next to the state file.
            for ecash in cw.recovered_cashouts.drain(..) {
                println!("recovered ecash (also saved in cashouts.log):\n{ecash}");
            }
            res?;
        }
    }
    Ok(())
}

fn report_recovered(r: &Recovered) {
    if r.notes_live > 0 || r.till_received > 0 || r.abandoned > 0 {
        println!(
            "recovered: {} card note(s) made live, {} reissued into the wallet (fees {} msat), {} unsent/rejected transaction(s) abandoned",
            r.notes_live,
            sats(r.till_received),
            r.till_fee,
            r.abandoned
        );
    }
    for e in &r.errors {
        eprintln!("warning: {e}");
    }
}

async fn run_card(cw: &mut CardWallet, cmd: &CardCmd) -> Result<()> {
    match cmd {
        CardCmd::Setup { fresh } => {
            for (slot, pk) in cw.card_setup(*fresh)? {
                println!("slot {slot:2}  {pk}");
            }
        }
        CardCmd::Topup { amount_msat } => {
            for (slot, amount) in cw.topup(amount_msat).await? {
                println!("slot {slot:2}  +{}", sats(amount));
            }
        }
        CardCmd::Load { .. } => unreachable!("handled in main without a PIN session"),
        CardCmd::Balance => {
            let r = cw.balance().await?;
            report_recovered(&r.recovered);
            for t in &r.unresolved {
                println!("unresolved transaction {t} (run `card balance` again)");
            }
            for n in &r.notes {
                let status = match n.status {
                    NoteStatus::Live => "live",
                    NoteStatus::Pending => "pending",
                    NoteStatus::Spent => "spent",
                    NoteStatus::Dead => "dead",
                };
                println!("slot {:2}  {:>14} msat  {status}", n.slot, n.amount_msat);
            }
            println!("live total: {}", sats(r.live_msat));
            println!("free slots: {:?}  to renew: {:?}", r.free_slots, r.overwritable_slots);
        }
        CardCmd::Spend { price } => {
            let r = cw.spend(*price).await?;
            println!("submitted, accepted by the federation: {}", r.txid);
            println!(
                "inputs {:?}; till received {} (price {}, overpaid {} msat); fees {} msat",
                r.inputs,
                sats(r.received.msats),
                sats(r.price_msat),
                r.overpay_msat,
                r.fees_msat
            );
            println!("change notes {:?}; renewed slots {:?}", r.change, r.renewed_slots);
            if r.dust {
                println!("note: no exact balance exists with notes of the economical size or more, so some outputs are small");
            }
            if r.collection_pending {
                println!("PAID, but collecting the till notes is still pending: run `card balance`");
            }
            for w in &r.warnings {
                eprintln!("warning: {w}");
            }
        }
        CardCmd::Cashout { slot, qr: want_qr } => {
            let r = cw.cashout(*slot).await?;
            println!("submitted, accepted by the federation: {}", r.txid);
            println!(
                "slot {} ({} msat note): {} reached the wallet after fees{}",
                r.slot,
                r.note_msat,
                sats(r.gained_msat),
                if r.dust { " (small outputs were needed for an exact balance)" } else { "" }
            );
            for w in &r.warnings {
                eprintln!("warning: {w}");
            }
            if let Some(ecash) = &r.ecash {
                println!("{ecash}");
                eprintln!("exported {}", sats(r.exported_msat));
                if *want_qr {
                    qr(ecash)?;
                }
            }
        }
    }
    Ok(())
}
