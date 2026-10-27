//! Time GEN_KEY, GET_PUB and SIGN on whatever `CARD_BACKEND` points at.
//!
//! ```sh
//! CARD_BACKEND=pcsc cargo run --locked -p fedimint-card-host-card --features pcsc --example bench -- [slot] [iterations]
//! ```
//! Every signature is verified against the card's public key before it counts.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use fedimint_card_host_card::open_from_env;
use secp256k1::{Message, SECP256K1};

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let slot: u8 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(0);
    let iterations: usize = args.next().map(|s| s.parse()).transpose()?.unwrap_or(10);

    let mut card = open_from_env()?;

    let t = Instant::now();
    let generated = match card.gen_key(slot) {
        Ok(_) => true,
        Err(e) if e.to_string().contains("6985") => false,
        Err(e) => return Err(e),
    };
    let gen_time = t.elapsed();

    let t = Instant::now();
    let pubkey = card.get_pub(slot)?;
    let get_pub_time = t.elapsed();

    let mut times = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let mut msg = [0u8; 32];
        msg[0] = i as u8;
        msg[31] = 0xA5;
        let t = Instant::now();
        let sig = card.sign(slot, &msg)?;
        times.push(t.elapsed());
        if SECP256K1
            .verify_schnorr(&sig, &Message::from_digest(msg), &pubkey)
            .is_err()
        {
            bail!("signature {i} did not verify");
        }
    }

    println!("backend: {}", std::env::var("CARD_BACKEND").unwrap_or_else(|_| "soft".into()));
    println!("slot {slot}: pubkey {}", hex::encode(pubkey.serialize()));
    if generated {
        println!("GEN_KEY: {gen_time:?}");
    } else {
        println!("GEN_KEY: slot already held a key (6985), skipped");
    }
    println!("GET_PUB: {get_pub_time:?}");
    println!(
        "SIGN x{iterations}: median {:?}, min {:?}, max {:?}, all verified",
        median(times.clone()),
        times.iter().min().unwrap(),
        times.iter().max().unwrap()
    );
    Ok(())
}
