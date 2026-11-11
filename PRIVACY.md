# Privacy policy

Fedimint Card Setup and Fedimint Card POS ("the apps") are published by Island Bitcoin LLC.
This policy says what the apps and the Fedimint Card registry they work with collect, and why.

Last updated: 11 November 2026.

## What we do not do

- No advertising, no ad identifiers, no analytics or tracking SDKs.
- We do not sell or share data with third parties for their own use.
- The apps never ask for your name, email address, phone number, location, contacts or photos.
  The camera is used only to scan QR codes on screen, and images are not stored or sent anywhere.

## Fedimint Card Setup

Used by card operators.

- **Operator account.** You sign in to a Fedimint Card registry with an operator name and password
  that the registry's administrator created for you. The registry stores the name and a salted hash
  of the password, never the password itself, and keeps a record of your sign-in sessions.
- **Cards you set up.** For each card, the registry stores a short card identifier, the card's
  public keys, the federation it belongs to, its batch, its PIN policy (not the PIN) and when it was
  set up. Card private keys are made on the card and never leave it. PINs are never stored or sent
  anywhere but the card.
- **Tills you approve.** The registry stores each till's name and a hash of its access token.

## Fedimint Card POS

Used by merchants. The app has no accounts.

- The app talks to the merchant's own till, a service the merchant runs and pairs the phone with.
  It sends the till the amounts entered, the card's identifier and the signatures the card makes, so
  the till can charge, check or top up the card.
- To process a payment, the till sends the card identifier, amount and the card's signed eCash notes
  to the Fedimint Card registry, which records the card's notes and transactions so a card cannot be
  spent twice. These records are tied to the card, not to a person.
- If the phone cannot reach the till, the app can still take a card payment. It keeps that payment
  (the card identifier, the amount and the signatures the card made) in the app's private storage on
  the phone, which is left out of backups, until the phone reaches the till again and hands it over.
  The till then sends it to the registry as above.

## Where data is kept and for how long

The registry runs on Cloudflare's infrastructure. Card, note and transaction records are kept for
as long as the card is in use, because they are what make the card's balance correct. Operator
accounts and till records are kept until they are removed or revoked. A till's own data stays on the
server the merchant runs it on and is under the merchant's control.

## Your choices

An operator can ask the registry's administrator to delete their account. A cardholder can ask the
operator who issued the card to retire it. For anything else, contact us.

## Contact

Island Bitcoin LLC, apple@islandbitcoin.com
