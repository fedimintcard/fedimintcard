import {exchange, INS_VERIFY_PIN, oldCardResetPinApdu, pinStatusApdu, selectApdu, stateApdu} from '../cardSetup';
import {CheckedPinKey, getNonceApdu, pinBlock, sealedPinApdu, TrustedCard} from '../sealedPin';
import {FakeGpCard} from './fakeGpCard';
import {TEST_CERT_KEY} from './sealedCard';

// The fake's applet 4.0 VERIFY_PIN against FedimintCardApplet.verifyPin and
// waitOwed, sealed (docs/OFFLINE-PROTOCOL.md § 9): frame checks first, then a
// live nonce, then the wait, served in stretches of at most 16 askings that
// count only while VERIFY_PIN follows VERIFY_PIN in one selection, then the
// opening, which spends the nonce. One envelope is sent through the whole wait.

const PIN = '2468';

function owing(owed: number, wrong = 1) {
  const card = new FakeGpCard().withFinishedApplet(PIN);
  card.applet!.wrong = wrong;
  card.applet!.owed = owed;
  return card;
}

const send = async (card: FakeGpCard, apdu: number[]) => (await exchange(card.transceive, apdu)).sw;

/** The host's path: the card checked, GET_NONCE, one sealed VERIFY_PIN to send as often as the wait asks. */
async function envelope(card: FakeGpCard, pin = PIN): Promise<number[]> {
  const tx = (apdu: readonly number[]) => exchange(card.transceive, apdu);
  const trusted = await TrustedCard.check(tx, TEST_CERT_KEY);
  if (!trusted.ok) {
    throw new Error(trusted.reason);
  }
  const asked = await CheckedPinKey.ask(tx, trusted.trust);
  if (!asked.ok) {
    throw new Error(asked.reason);
  }
  return sealedPinApdu(INS_VERIFY_PIN, asked.key, pinBlock(pin));
}

test('one VERIFY_PIN per selection never serves a wait, and anything in between restarts the stretch', async () => {
  const card = owing(1);
  for (let tap = 0; tap < 5; tap++) {
    expect(await send(card, selectApdu())).toBe(0x9000);
    expect(await send(card, await envelope(card))).toBe(0x6a98);
  }
  expect(card.applet!.owed).toBe(1);
  await send(card, selectApdu());
  const verify = await envelope(card);
  expect(await send(card, verify)).toBe(0x6a98);
  await send(card, stateApdu());
  expect(await send(card, verify)).toBe(0x6a98);
  expect(card.applet!.owed).toBe(1);
  // The asking after the stretch writes the wait down and goes on to the try.
  expect(await send(card, verify)).toBe(0x9000);
  expect(card.applet!.owed).toBe(0);
  expect(card.applet!.wrong).toBe(0);
});

test('a wait is served in stretches of 16 with one envelope, and a stretch cut by the lift starts again', async () => {
  const card = owing(40, 6);
  await send(card, selectApdu());
  const verify = await envelope(card);
  const answers: (number | null)[] = [];
  for (let i = 0; i < 20; i++) {
    answers.push(await send(card, verify));
  }
  expect(answers.every(sw => sw === 0x6a98)).toBe(true);
  // 16 askings and the one that wrote the first stretch down; the 3 after it are lost with the selection.
  expect(card.applet!.owed).toBe(24);
  await send(card, selectApdu());
  // The envelope's nonce went with the selection.
  expect(await send(card, verify)).toBe(0x6985);
  const again = await envelope(card);
  const rest: (number | null)[] = [];
  for (let sw = 0x6a98 as number | null; sw === 0x6a98; ) {
    sw = await send(card, again);
    rest.push(sw);
  }
  // 16 + 1, then 8 + 1, the last of which tries the PIN.
  expect(rest).toHaveLength(26);
  expect(rest[25]).toBe(0x9000);
  expect(card.applet!.owed).toBe(0);
});

test('the frame is checked before the wait: no PIN, P1/P2 (P1 00 included) and the length neither serve nor restart it', async () => {
  const card = owing(2);
  await send(card, selectApdu());
  const verify = await envelope(card);
  expect(await send(card, verify)).toBe(0x6a98);
  // The old three-byte probe and the clear VERIFY_PIN are both P1 = 00: 6B00, even while a wait is owed.
  expect(await send(card, pinStatusApdu())).toBe(0x6b00);
  expect(await send(card, oldCardResetPinApdu(INS_VERIFY_PIN, PIN))).toBe(0x6b00);
  expect(await send(card, verify.slice(0, -1).map((b, i) => (i === 4 ? 89 : b)))).toBe(0x6700);
  expect(card.applet!.asked).toBe(1);
  expect(card.applet!.owed).toBe(2);
  expect(await send(card, verify)).toBe(0x6a98);
  expect(await send(card, verify)).toBe(0x9000);

  const fresh = new FakeGpCard().withLiftedApplet(8);
  await send(fresh, selectApdu());
  expect(await send(fresh, [0xb0, 0x40, 0x01, 0x00, 90, ...new Array(90).fill(0)])).toBe(0x6984);
});

test('a wrong try answers the wrong tries in a row and owes 2^(those before it); its nonce is spent', async () => {
  const card = owing(0, 0);
  await send(card, selectApdu());
  const wrong = await envelope(card, '1111');
  expect(await send(card, wrong)).toBe(0x63c1);
  expect(card.applet!.owed).toBe(1);
  // Sent again, the same envelope finds no live nonce: refused before the wait, nothing written.
  expect(await send(card, wrong)).toBe(0x6985);
  expect(card.applet!.owed).toBe(1);
  const next = await envelope(card, '1111');
  expect(await send(card, next)).toBe(0x6a98);
  expect(await send(card, next)).toBe(0x63c2);
  expect(card.applet!.owed).toBe(2);
});

test('GET_NONCE mid-wait restarts the stretch, and the old envelope is then a wrong try with the next wait doubled', async () => {
  const card = owing(2, 1);
  await send(card, selectApdu());
  const verify = await envelope(card);
  expect(await send(card, verify)).toBe(0x6a98);
  expect(await send(card, getNonceApdu())).toBe(0x9000);
  expect(await send(card, verify)).toBe(0x6a98);
  expect(await send(card, verify)).toBe(0x6a98);
  expect(await send(card, verify)).toBe(0x63c2);
  expect(card.applet!.owed).toBe(2);
});

test('an envelope that does not open is a wrong try and ends the session; GET_NONCE wants P1 01 and the certificate', async () => {
  const card = owing(0, 0);
  await send(card, selectApdu());
  const good = await envelope(card);
  expect(await send(card, good)).toBe(0x9000);
  expect(card.applet!.verified).toBe(true);
  const spoiled = await envelope(card);
  spoiled[spoiled.length - 1] ^= 1;
  expect(await send(card, spoiled)).toBe(0x63c1);
  expect(card.applet!.verified).toBe(false);

  expect(await send(card, [0xb0, 0x44, 0x00, 0x00, 0x00])).toBe(0x6b00);
  const lifted = new FakeGpCard().withLiftedApplet(8);
  await send(lifted, selectApdu());
  expect(await send(lifted, getNonceApdu())).toBe(0x6985);
});
