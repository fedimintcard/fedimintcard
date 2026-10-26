//! `card-sidecar`: the card wallet CLI.
//!
//! The backend comes from `CARD_BACKEND` (soft | bridge | pcsc), exactly as in the test suite.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fedimint_card_host_card::open_from_env;
use fedimint_card_sidecar::glue::CardWallet;
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
    /// Print an ecash string worth `amount_msat` (at least that much if no exact set exists).
    Export {
        amount_msat: u64,
        /// Also draw the string as a QR code.
        #[arg(long)]
        qr: bool,
    },
}

#[derive(Subcommand)]
enum CardCmd {
    /// Generate keys in every empty slot and cache the public keys.
    Setup,
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
            let face = w.import(notes).await?;
            println!("reissued {}; wallet balance now {}", sats(face.msats), sats(w.balance_msat().await?));
        }
        Cmd::Ecash(Ecash::Export { amount_msat, qr: want_qr }) => {
            let w = Wallet::open(&cli.data_dir).await?;
            let (notes, face) = w.export(*amount_msat).await?;
            println!("{notes}");
            eprintln!("exported {}", sats(face.msats));
            if *want_qr {
                qr(&notes.to_string())?;
            }
        }
        Cmd::Card(CardCmd::Setup) => {
            let mut cw = card_wallet(&cli).await?;
            for (slot, pk) in cw.card_setup()? {
                println!("slot {slot:2}  {pk}");
            }
        }
        Cmd::Card(CardCmd::Topup { amount_msat }) => {
            let mut cw = card_wallet(&cli).await?;
            for (slot, amount) in cw.topup(amount_msat).await? {
                println!("slot {slot:2}  +{}", sats(amount));
            }
        }
        Cmd::Card(CardCmd::Balance) => {
            let mut cw = card_wallet(&cli).await?;
            let r = cw.balance().await?;
            for ecash in &r.recovered_cashouts {
                println!("recovered cashout ecash:\n{ecash}");
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
        Cmd::Card(CardCmd::Spend { price }) => {
            let mut cw = card_wallet(&cli).await?;
            let r = cw.spend(*price).await?;
            println!("accepted {}", r.txid);
            println!(
                "inputs {:?}; till received {} (price {}, overpaid {} msat); fees {} msat",
                r.inputs, sats(r.received.msats), sats(r.price_msat), r.overpay_msat, r.fees_msat
            );
            println!("change notes {:?}; renewed slots {:?}", r.change, r.renewed_slots);
        }
        Cmd::Card(CardCmd::Cashout { slot, qr: want_qr }) => {
            let mut cw = card_wallet(&cli).await?;
            let r = cw.cashout(*slot).await?;
            println!("{}", r.ecash);
            eprintln!("cashed out slot {} ({} msat note) as {}", r.slot, r.note_msat, sats(r.face_msat));
            if *want_qr {
                qr(&r.ecash)?;
            }
        }
    }
    Ok(())
}
