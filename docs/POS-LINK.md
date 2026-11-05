# POS link: a POS phone and the till over the network

Status: agreed 2026-11-04, being built. Replaces the two `adb` tunnels for any POS phone, and is the
only way an iPhone reaches the till. The USB path (`CARD_BRIDGE_URL` + `adb forward`/`reverse`) keeps
working as a fallback.

## Directions

The phone always connects to the till; the till never connects to a phone. While a tap is in
progress the POS app holds a WebSocket open to the till, and the till sends the card's APDUs down
it. Whichever phone started a charge or a balance read supplies that job's card.

## Pairing and auth

- `card-sidecar till pair` (and `host/scripts/till-service.sh pair`) creates the POS token if there
  is none: 32 random bytes, hex, in `<data_dir>/till-pos-token` (mode 0600). It prints a QR code
  and the same text:
  `fedimintcard-till:http://<mac-lan-ip>:9877?t=<token>`
  The address is the first non-loopback IPv4 address, overridable with `--url`.
- `till serve --listen 0.0.0.0:9877` is refused unless the token file exists. With a token, every
  `/v1/*` request not from loopback must carry `Authorization: Bearer <token>`; wrong or missing ->
  `401 {"ok":false,"error":"unauthorized"}`. Loopback stays open, so the CLI, the tests and a USB
  phone through `adb reverse` work as before. The token is compared in constant time and never
  logged.
- The POS stores the scanned till URL and token (Android: encrypted prefs or the Keystore;
  iOS: Keychain). "Pair with till" in POS settings scans the QR; the URL can also be typed.

## The card link

`GET /v1/card/link` upgrades to a WebSocket (bearer auth as above). Text frames, JSON:

| From | Message | Meaning |
|---|---|---|
| till | `{"type":"ready","link":"<id>"}` | first frame; `<id>` is 16 random hex chars |
| till | `{"type":"apdu","id":<n>,"apdu":"<hex>"}` | send this command APDU to the card |
| phone | `{"type":"rapdu","id":<n>,"rapdu":"<hex data+SW>"}` | the card's answer |
| phone | `{"type":"lost","id":<n>}` | the card left (or the NFC session ended) before answering |
| either | close | the link is over; the till fails the job's card I/O as "card moved away" |

- The phone opens the link once it has the card (Android: tag discovered; iOS: tag connected in
  the NFC session), then sends the job with header `X-Card-Link: <id>`:
  `POST /v1/charge` and `POST /v1/card/balance`. A job with `X-Card-Link` uses only that link as
  its card; without it, the till uses `CARD_BRIDGE_URL` as today.
- An unknown or closed link id -> `409 {"ok":false,"error":"card_link_gone"}` before anything runs.
- One APDU in flight per link; the till waits up to 10 s for each answer. The link lives at most
  120 s and ends when the job's reply is sent (the till closes it).
- APDU bytes are never logged on either side, only INS and SW.

## iOS specifics

The iPhone reads the card through `NFCTagReaderSession` (ISO 14443, ISO 7816 tag). The app's
`com.apple.developer.nfc.readersession.iso7816.select-identifiers` must list every AID the till
SELECTs: `A000000151000000`, `F0464D4341524401`, `F0464D434152444D`. iOS ends a reader session
after about 60 s, so a job must finish inside one session. `NSLocalNetworkUsageDescription` and
ATS `NSAllowsLocalNetworking` are needed for `http://` on the LAN.
