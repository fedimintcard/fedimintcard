//! Run with: cargo run --example pcsc --features pcsc
use fedimintcard::Reader;
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let context = pcsc::Context::establish(pcsc::Scope::User)?;
    let readers = context.list_readers_owned()?;
    let reader = readers.first().ok_or("connect a PC/SC reader")?;
    let card = context.connect(reader, pcsc::ShareMode::Shared, pcsc::Protocols::ANY)?;
    let mut session = Reader::select(|command: &[u8]| {
        let mut buffer = [0; 258];
        card.transmit(command, &mut buffer)
            .map(|data| data.to_vec())
    })?;
    let amounts = session.balance()?;
    println!(
        "Stored {} msat, owed {} msat",
        amounts.stored_msat, amounts.owed_msat
    );
    Ok(())
}
