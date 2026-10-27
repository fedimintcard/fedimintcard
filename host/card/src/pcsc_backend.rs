//! PC/SC backend (feature `pcsc`).

use anyhow::{Context, Result, bail};

use crate::Transport;

pub struct PcscTransport {
    card: pcsc::Card,
}

impl PcscTransport {
    /// Connect to the reader whose name contains `reader` (first reader if `None`).
    pub fn connect(reader: Option<&str>) -> Result<Self> {
        let ctx = pcsc::Context::establish(pcsc::Scope::User).context("PC/SC context")?;
        let readers = ctx.list_readers_owned().context("listing readers")?;
        let chosen = match reader {
            Some(name) => readers.iter().find(|r| r.to_string_lossy().contains(name)),
            None => readers.first(),
        };
        let Some(chosen) = chosen else {
            bail!("no matching PC/SC reader (found {readers:?})");
        };
        let card = ctx
            .connect(chosen, pcsc::ShareMode::Shared, pcsc::Protocols::ANY)
            .context("connecting to card")?;
        Ok(Self { card })
    }
}

impl Transport for PcscTransport {
    fn transmit(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
        let mut buf = [0u8; pcsc::MAX_BUFFER_SIZE];
        Ok(self.card.transmit(apdu, &mut buf)?.to_vec())
    }
}
