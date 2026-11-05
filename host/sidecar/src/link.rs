//! The card link (`docs/POS-LINK.md`): a POS phone holds a WebSocket open to the till while a tap
//! is in progress, and the till sends the card's APDUs down it.
//!
//! The socket is served by its own (`Send`) task, [`run`]. A job's card is a [`CardClient`] over a
//! [`LinkTransport`], which is blocking like every other transport: each `transmit` hands the APDU
//! to the socket task and waits for the answer on a std channel. The actor calls the card through
//! `on_card` (`block_in_place`), so that wait never stalls the socket task.
//!
//! Every way a link can fail mid-job (the phone sent `lost`, closed, went quiet for 10 s, or the
//! link outlived 120 s) is a [`BridgeError::NoCard`]: the same "card moved away" class, `no_card`,
//! as a relay with no card in the field. APDU bytes are never logged, only INS and SW.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use fedimint_card_host_card::{BridgeError, Transport};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::Instant;

/// How long the till waits for each answer.
pub const APDU_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest a link lives.
pub const LINK_LIFE: Duration = Duration::from_secs(120);

/// Why a link could not answer an APDU. All of them mean the card is gone for this job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkFault {
    /// The phone said `lost`: the card left the field before answering.
    Lost,
    /// No answer within [`APDU_TIMEOUT`].
    Timeout,
    /// The link was closed (by the phone, by the till, or because it outlived [`LINK_LIFE`]).
    Closed,
}

impl LinkFault {
    fn into_error(self) -> anyhow::Error {
        let why = match self {
            LinkFault::Lost => "the card moved away (the phone lost it)",
            LinkFault::Timeout => "the card did not answer within 10 s",
            LinkFault::Closed => "the card link closed: the card moved away",
        };
        BridgeError::NoCard(format!("card link: {why}")).into()
    }
}

enum Cmd {
    Apdu { apdu: Vec<u8>, reply: std::sync::mpsc::SyncSender<Result<Vec<u8>, LinkFault>> },
    Close,
}

/// A live link, as a job holds it. Cheap to clone, `Send`.
#[derive(Clone)]
pub struct Link {
    pub id: String,
    tx: mpsc::UnboundedSender<Cmd>,
}

impl Link {
    /// Ask the socket task to close the link. Harmless if it already has.
    pub fn close(&self) {
        let _ = self.tx.send(Cmd::Close);
    }

    pub fn is_open(&self) -> bool {
        !self.tx.is_closed()
    }

    pub fn transport(&self) -> LinkTransport {
        LinkTransport { link: self.clone() }
    }
}

/// Links by id. A link is removed when its socket task ends.
#[derive(Default)]
pub struct Links {
    map: Mutex<HashMap<String, Link>>,
}

impl Links {
    /// The open link with this id, if there is one.
    pub fn get(&self, id: &str) -> Option<Link> {
        let map = self.map.lock().unwrap();
        map.get(id).filter(|l| l.is_open()).cloned()
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn insert(&self, link: Link) {
        self.map.lock().unwrap().insert(link.id.clone(), link);
    }

    fn remove(&self, id: &str) {
        self.map.lock().unwrap().remove(id);
    }
}

/// The blocking side of a link: one APDU at a time, answered by the phone.
pub struct LinkTransport {
    link: Link,
}

impl Transport for LinkTransport {
    fn transmit(&mut self, apdu: &[u8]) -> Result<Vec<u8>> {
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        if self.link.tx.send(Cmd::Apdu { apdu: apdu.to_vec(), reply }).is_err() {
            return Err(LinkFault::Closed.into_error());
        }
        // The socket task enforces the 10 s itself; the margin only covers a task that died
        // without answering (the dropped sender ends the wait at once in that case).
        match answer.recv_timeout(APDU_TIMEOUT + Duration::from_secs(2)) {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(f)) => Err(f.into_error()),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(LinkFault::Timeout.into_error()),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(LinkFault::Closed.into_error()),
        }
    }
}

fn new_id() -> String {
    hex::encode(rand::random::<[u8; 8]>())
}

fn ins_of(apdu: &[u8]) -> String {
    apdu.get(1).map_or_else(|| "--".into(), |i| format!("{i:02X}"))
}

fn sw_of(r: &[u8]) -> String {
    if r.len() >= 2 { hex::encode_upper(&r[r.len() - 2..]) } else { "----".into() }
}

/// What a text frame from the phone says.
#[derive(Debug, PartialEq, Eq)]
enum FromPhone {
    Rapdu { id: u64, rapdu: Vec<u8> },
    Lost { id: u64 },
    Other,
}

fn parse(text: &str) -> FromPhone {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return FromPhone::Other };
    let id = v["id"].as_u64();
    match (v["type"].as_str(), id) {
        (Some("rapdu"), Some(id)) => match v["rapdu"].as_str().map(hex::decode) {
            Some(Ok(rapdu)) => FromPhone::Rapdu { id, rapdu },
            _ => FromPhone::Other,
        },
        (Some("lost"), Some(id)) => FromPhone::Lost { id },
        _ => FromPhone::Other,
    }
}

/// Serve one link until the phone closes it, the job closes it, or it outlives [`LINK_LIFE`].
pub async fn run(mut socket: WebSocket, links: Arc<Links>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let link = Link { id: new_id(), tx };
    let id = link.id.clone();
    let deadline = Instant::now() + LINK_LIFE;
    let ready = json!({"type": "ready", "link": id}).to_string();
    if socket.send(Message::Text(ready.into())).await.is_err() {
        return;
    }
    links.insert(link);
    tracing::info!("card link {id} open");
    let mut next = 0u64;
    let why = 'link: loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                None | Some(Cmd::Close) => break 'link "closed by the till",
                Some(Cmd::Apdu { apdu, reply }) => {
                    next += 1;
                    let n = next;
                    let ins = ins_of(&apdu);
                    let frame = json!({"type": "apdu", "id": n, "apdu": hex::encode(&apdu)}).to_string();
                    if socket.send(Message::Text(frame.into())).await.is_err() {
                        let _ = reply.send(Err(LinkFault::Closed));
                        break 'link "send failed";
                    }
                    let wait_until = (Instant::now() + APDU_TIMEOUT).min(deadline);
                    loop {
                        let msg = tokio::select! {
                            m = socket.recv() => m,
                            _ = tokio::time::sleep_until(wait_until) => {
                                let fault = if Instant::now() >= deadline { LinkFault::Closed } else { LinkFault::Timeout };
                                tracing::warn!("card link {id}: INS {ins} unanswered");
                                let _ = reply.send(Err(fault));
                                break 'link if fault == LinkFault::Closed { "outlived 120 s" } else { "APDU timeout" };
                            }
                        };
                        match msg {
                            Some(Ok(Message::Text(t))) => match parse(t.as_str()) {
                                FromPhone::Rapdu { id: got, rapdu } if got == n => {
                                    tracing::debug!("card link {id}: INS {ins} -> SW {}", sw_of(&rapdu));
                                    let _ = reply.send(Ok(rapdu));
                                    break;
                                }
                                FromPhone::Lost { id: got } if got == n => {
                                    tracing::info!("card link {id}: card lost at INS {ins}");
                                    let _ = reply.send(Err(LinkFault::Lost));
                                    break 'link "card lost";
                                }
                                // A stale or malformed frame: keep waiting for this APDU's answer.
                                _ => {}
                            },
                            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                                let _ = reply.send(Err(LinkFault::Closed));
                                break 'link "closed by the phone";
                            }
                            Some(Ok(_)) => {}
                        }
                    }
                }
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break 'link "closed by the phone",
                // Nothing is asked: a `lost` here just ends the link early, the rest is ignored.
                Some(Ok(Message::Text(t))) if matches!(parse(t.as_str()), FromPhone::Lost { .. }) => break 'link "card lost",
                Some(Ok(_)) => {}
            },
            _ = tokio::time::sleep_until(deadline) => break 'link "outlived 120 s",
        }
    };
    links.remove(&id);
    rx.close();
    // Anything queued behind the close is answered, so no transmit waits out its timeout.
    while let Ok(cmd) = rx.try_recv() {
        if let Cmd::Apdu { reply, .. } = cmd {
            let _ = reply.send(Err(LinkFault::Closed));
        }
    }
    let _ = socket.send(Message::Close(None)).await;
    tracing::info!("card link {id} ended: {why}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_from_the_phone() {
        assert_eq!(parse(r#"{"type":"rapdu","id":3,"rapdu":"9000"}"#), FromPhone::Rapdu { id: 3, rapdu: vec![0x90, 0] });
        assert_eq!(parse(r#"{"type":"lost","id":4}"#), FromPhone::Lost { id: 4 });
        assert_eq!(parse(r#"{"type":"rapdu","id":3,"rapdu":"zz"}"#), FromPhone::Other);
        assert_eq!(parse(r#"{"type":"rapdu","rapdu":"9000"}"#), FromPhone::Other);
        assert_eq!(parse("not json"), FromPhone::Other);
        assert_eq!(new_id().len(), 16);
        assert_eq!((ins_of(&[0xB0, 0x12]), sw_of(&[1, 0x90, 0x00])), ("12".into(), "9000".into()));
    }
}
