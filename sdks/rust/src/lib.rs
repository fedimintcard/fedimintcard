//! Read the public records of a Fedimint Card 4.x applet.
//! Provide one ISO 7816 session, with responses including SW1 and SW2.
use secp256k1::XOnlyPublicKey;
use std::{error::Error, fmt};

pub const SELECT: [u8; 14] = [
    0, 0xa4, 4, 0, 8, 0xf0, 0x46, 0x4d, 0x43, 0x41, 0x52, 0x44, 1, 0,
];
pub const SLOT_COUNT: usize = 32;

#[derive(Debug)]
pub enum ReaderError {
    Transport(Box<dyn Error + Send + Sync>),
    Status(u16),
    UnsupportedVersion { major: u8, minor: u8 },
    InvalidResponse(&'static str),
    InvalidSlot(u8),
    SessionEnded,
}

impl fmt::Display for ReaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "card transport failed: {e}"),
            Self::Status(sw) => write!(f, "card returned {sw:04X}"),
            Self::UnsupportedVersion { major, minor } => {
                write!(f, "unsupported applet {major}.{minor}")
            }
            Self::InvalidResponse(reason) => write!(f, "invalid card response: {reason}"),
            Self::InvalidSlot(slot) => write!(f, "slot {slot} is outside 0..32"),
            Self::SessionEnded => write!(f, "select the card in a new session"),
        }
    }
}

impl Error for ReaderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

type Result<T> = std::result::Result<T, ReaderError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    Empty,
    Keyed,
    Note,
    Change,
    Spent,
}

impl SlotState {
    fn decode(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Empty),
            1 => Ok(Self::Keyed),
            2 => Ok(Self::Note),
            3 => Ok(Self::Change),
            4 => Ok(Self::Spent),
            _ => Err(ReaderError::InvalidResponse("unknown slot state")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardState {
    pub slots: [SlotState; SLOT_COUNT],
    pub now: u32,
    pub window: u32,
    pub limit_msat: u64,
    pub spent_msat: u64,
    pub certificate_length: u8,
    pub has_attestation_key: bool,
    pub has_registry_key: bool,
    pub pin_set: bool,
    pub wrong_tries: u8,
    pub owed: u16,
    pub anchored: bool,
    pub chip_multiply: bool,
    pub nonce_key_in_ram: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeOutput {
    pub transaction_id: [u8; 32],
    pub output_index: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub state: SlotState,
    pub public_key: Option<XOnlyPublicKey>,
    pub amount_msat: u64,
    pub change_output: Option<ChangeOutput>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Balance {
    pub stored_msat: u128,
    pub owed_msat: u128,
}

/// A session with one selected card. Drop it and select again if the card changes.
/// The transport must report a disconnect instead of silently selecting another card.
pub struct Reader<T> {
    transmit: T,
    version: (u8, u8),
    active: bool,
}

impl<T, E> Reader<T>
where
    T: FnMut(&[u8]) -> std::result::Result<Vec<u8>, E>,
    E: Error + Send + Sync + 'static,
{
    pub fn select(transmit: T) -> Result<Self> {
        let mut reader = Self {
            transmit,
            version: (0, 0),
            active: true,
        };
        match reader.exchange(&SELECT)?.as_slice() {
            [4, minor] => reader.version = (4, *minor),
            [major, minor] => {
                return Err(ReaderError::UnsupportedVersion {
                    major: *major,
                    minor: *minor,
                })
            }
            _ => {
                return Err(ReaderError::InvalidResponse(
                    "SELECT requires two version bytes",
                ))
            }
        }
        Ok(reader)
    }

    pub fn version(&self) -> (u8, u8) {
        self.version
    }

    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>> {
        if !self.active {
            return Err(ReaderError::SessionEnded);
        }
        let mut command = request.to_vec();
        let mut data = Vec::new();
        for _ in 0..16 {
            let response = (self.transmit)(&command).map_err(|e| {
                self.active = false;
                ReaderError::Transport(Box::new(e))
            })?;
            if response.len() < 2 {
                return Err(ReaderError::InvalidResponse("missing status word"));
            }
            let (body, sw) = response.split_at(response.len() - 2);
            match sw {
                [0x90, 0] | [0x61, _] => {
                    if data.len() + body.len() > 256 {
                        return Err(ReaderError::InvalidResponse("oversize response"));
                    }
                    data.extend_from_slice(body);
                    if sw[0] == 0x90 {
                        return Ok(data);
                    }
                    command = vec![0, 0xc0, 0, 0, sw[1]];
                }
                [0x6c, le] => *command.last_mut().expect("read commands have Le") = *le,
                _ => return Err(ReaderError::Status(u16::from_be_bytes([sw[0], sw[1]]))),
            }
        }
        Err(ReaderError::InvalidResponse("response chain limit reached"))
    }

    pub fn state(&mut self) -> Result<CardState> {
        let b: [u8; 65] = self
            .exchange(&[0xb0, 0x16, 0, 0, 0])?
            .try_into()
            .map_err(|_| ReaderError::InvalidResponse("GET_STATE requires 65 bytes"))?;
        let mut slots = [SlotState::Empty; SLOT_COUNT];
        for (slot, byte) in slots.iter_mut().zip(&b) {
            *slot = SlotState::decode(*byte)?;
        }
        Ok(CardState {
            slots,
            now: u32::from_be_bytes(b[32..36].try_into().unwrap()),
            window: u32::from_be_bytes(b[36..40].try_into().unwrap()),
            limit_msat: u64::from_be_bytes(b[40..48].try_into().unwrap()),
            spent_msat: u64::from_be_bytes(b[48..56].try_into().unwrap()),
            certificate_length: b[56],
            has_attestation_key: b[57] != 0,
            has_registry_key: b[58] != 0,
            pin_set: b[59] != 0,
            wrong_tries: b[60],
            owed: u16::from_be_bytes([b[61], b[62]]),
            anchored: b[63] != 0,
            chip_multiply: b[64] & 1 != 0,
            nonce_key_in_ram: b[64] & 2 != 0,
        })
    }

    /// Read the x-only key for one occupied slot. Slot zero identifies the card.
    pub fn public_key(&mut self, slot: u8) -> Result<XOnlyPublicKey> {
        check_slot(slot)?;
        decode_key(&self.exchange(&[0xb0, 0x11, slot, 0, 0])?)
    }

    pub fn slot(&mut self, slot: u8) -> Result<Slot> {
        check_slot(slot)?;
        let b = self.exchange(&[0xb0, 0x15, slot, 0, 0])?;
        if b.len() != 187 {
            return Err(ReaderError::InvalidResponse("GET_SLOT requires 187 bytes"));
        }
        let state = SlotState::decode(b[0])?;
        let public_key = match state {
            SlotState::Empty => None,
            _ => Some(decode_key(&b[1..33])?),
        };
        Ok(Slot {
            state,
            public_key,
            amount_msat: u64::from_be_bytes(b[33..41].try_into().unwrap()),
            change_output: (state == SlotState::Change).then(|| ChangeOutput {
                transaction_id: b[121..153].try_into().unwrap(),
                output_index: u16::from_be_bytes([b[153], b[154]]),
            }),
        })
    }

    /// Read the unverified version-one certificate. An unprovisioned card returns None.
    pub fn certificate(&mut self) -> Result<Option<[u8; 130]>> {
        let data = match self.exchange(&[0xb0, 0x32, 0, 0, 0]) {
            Err(ReaderError::Status(0x6a83)) => return Ok(None),
            other => other?,
        };
        let cert: [u8; 130] = data
            .try_into()
            .map_err(|_| ReaderError::InvalidResponse("certificate requires 130 bytes"))?;
        if cert[0] != 1 {
            return Err(ReaderError::InvalidResponse("unknown certificate version"));
        }
        Ok(Some(cert))
    }

    /// Sum notes and change separately. These are card-reported amounts in millisatoshis.
    pub fn balance(&mut self) -> Result<Balance> {
        let mut total = Balance::default();
        for (slot, expected) in self.state()?.slots.into_iter().enumerate() {
            if matches!(expected, SlotState::Note | SlotState::Change) {
                let actual = self.slot(slot as u8)?;
                if actual.state != expected {
                    return Err(ReaderError::InvalidResponse("slot changed during read"));
                }
                if expected == SlotState::Note {
                    total.stored_msat += u128::from(actual.amount_msat);
                } else {
                    total.owed_msat += u128::from(actual.amount_msat);
                }
            }
        }
        Ok(total)
    }
}

fn check_slot(slot: u8) -> Result<()> {
    if usize::from(slot) < SLOT_COUNT {
        Ok(())
    } else {
        Err(ReaderError::InvalidSlot(slot))
    }
}

fn decode_key(value: &[u8]) -> Result<XOnlyPublicKey> {
    XOnlyPublicKey::from_slice(value)
        .map_err(|_| ReaderError::InvalidResponse("invalid secp256k1 x-only key"))
}

/// Verify the registry signature and bind the certificate to the identity read from slot zero.
/// Supply an independently trusted, uncompressed P-256 registry public key.
/// This checks certificate issuance. A copy of a certificate does not prove possession.
pub fn verify_certificate(
    certificate: &[u8],
    registry_key: &[u8],
    card_identity: &XOnlyPublicKey,
) -> bool {
    use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
    if certificate.len() != 130
        || certificate[0] != 1
        || certificate[1..33] != card_identity.serialize()
        || registry_key.len() != 65
        || registry_key[0] != 4
        || p256::PublicKey::from_sec1_bytes(&certificate[33..66]).is_err()
    {
        return false;
    }
    let Ok(key) = VerifyingKey::from_sec1_bytes(registry_key) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&certificate[66..]) else {
        return false;
    };
    let mut message = b"fedimintcard/cert1".to_vec();
    message.extend_from_slice(&certificate[1..66]);
    key.verify(&message, &signature).is_ok()
}
