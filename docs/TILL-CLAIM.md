# Till claim: connecting a till to the registry without a password on the server

Status: agreed 2026-11-05 (owner: "build B"), being built.

A till needs a till token (`till_…`) to use the registry. Today an operator makes one with
`registry/scripts/new-till.sh` on a computer and pastes it into the till. The claim flow lets the
till ask for its own token and the operator approve it from the provisioner app they are already
signed in to. No operator password is ever typed on the till's server, and no till token is
stored in plain text anywhere but the till.

## Flow

1. The till (StartOS action **Connect to registry**, or `card-sidecar till claim`) starts a claim
   and shows a QR code and a short code.
2. The operator, in the provisioner app, opens **Settings → Tills → Add a till**, scans the QR
   (or types the code), sees the till's name, and approves.
3. The till, polling, receives its till token once and stores it. The claim is gone.

## Registry routes

| Route | Auth | Body → reply |
|---|---|---|
| `POST /till-claims` | none | `{name}` → `{claim_id, code, poll_secret, expires_at}` |
| `GET /till-claims/:code` | operator | → `{name, created_at, expires_at, status}` (`pending`, `approved`) |
| `POST /till-claims/:code/approve` | operator | `{}` → `{ok:true}`; the claim is now tied to this operator |
| `POST /till-claims/:claim_id/poll` | `poll_secret` in the body | `{poll_secret}` → `{status:"pending"}` or `{status:"approved", till_id, token}` (once; the claim is deleted in the same transaction) |

- `name`: 1–64 printable characters, what the operator sees before approving and what
  `GET /tills` lists.
- `claim_id`: 16 random bytes, hex. `poll_secret`: 32 random bytes, hex; the registry stores only
  its SHA-256. `code`: 8 characters from an unambiguous alphabet (no 0/O/1/I/L), shown as
  `XXXX-XXXX`, unique among live claims; case-insensitive on input.
- A claim lives 10 minutes. Expired claims answer 404 (`no claim`) and are swept.
- The token is created at the approved poll, not at approval, so the registry never holds a till
  token in plain text. It is created exactly as `POST /tills` does, under the approving operator.
- An approve for a code that is unknown, expired or already approved answers 404 or 409.
  A wrong `poll_secret` answers 404 (as if there were no claim).
- `POST /till-claims` is rate limited per client IP (e.g. 10 an hour) and globally capped in live
  claims (e.g. 200), refusing with 429.

## QR text

`fedimintcard-till-claim:<registry origin>?c=<code>` — for example
`fedimintcard-till-claim:https://registry.fedimintcard.org?c=K7QM-3XRP`. The app refuses a claim
whose registry origin is not the one it is signed in to, and says which registry the till asked.

## The till side

- `card-sidecar till claim --ops-url URL --name NAME` starts a claim, prints the QR (or, with
  `--text`, the QR text and the code), writes the claim (`claim_id`, `poll_secret`, expiry, ops URL)
  to `<data_dir>/till-claim.json` (0600), and exits.
- `till serve` with no till token but a live `till-claim.json` polls it every few seconds; on
  `approved` it writes the token where it keeps its till token (0600), deletes the claim file, and
  carries on as a till on ops. Without a token or a claim it serves `/v1/status` saying it is not
  connected, and refuses charges with `ops_not_connected`.
- StartOS: **Connect to registry** (input: till name, registry URL default
  `https://registry.fedimintcard.org`) runs `till claim` and shows the QR and the code as its
  result. Health reports "Waiting for approval in the provisioner app" while a claim is live.

## The provisioner app

- **Settings → Tills**: lists the operator's tills (`GET /tills`), with revoke (`DELETE /tills/:id`,
  confirmed: that till stops charging at once).
- **Add a till**: scan the QR (the app's existing camera scanner) or type the code → shows the
  till's name and when it asked → **Approve** → done. Refuses a QR for another registry.
