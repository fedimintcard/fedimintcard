//! `card-sidecar`: the card wallet CLI.
//!
//! The backend comes from `CARD_BACKEND` (soft | bridge | pcsc), exactly as in the test suite.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fedimint_card_host_card::open_from_env;
use fedimint_card_sidecar::glue::{CardWallet, Recovered};
use fedimint_card_sidecar::state::{NoteStatus, StateStore};
use fedimint_card_sidecar::wallet::{Wallet, state_path};

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
    Ok(CardWallet::new(wallet, store, card))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::WARN).with_writer(std::io::stderr).init();
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Join { invite } => {
            let w = Wallet::join(&cli.data_dir, invite).await?;
            println!("joined federation {}", w.federation_id());
            println!("data dir: {}", cli.data_dir.display());
            println!("back up {}/mnemonic: it is the wallet's root secret", cli.data_dir.display());
        }
        Cmd::Info => {
            let w = Wallet::open(&cli.data_dir).await?;
            let st = StateStore::open(state_path(&cli.data_dir))?;
            println!("federation: {}", w.federation_id());
            println!("wallet balance: {}", sats(w.balance_msat().await?));
            println!(
                "mint fee: {} msat for a 1,048,576 msat note (per input and per output)",
                w.fee(1 << 20)
            );
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
                r.inputs, sats(r.received.msats), sats(r.price_msat), r.overpay_msat, r.fees_msat
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
