import {exchange, INS_VERIFY_PIN, pinApdu, pinStatusApdu, selectApdu, stateApdu} from '../cardSetup';
import {FakeGpCard} from './fakeGpCard';

// The fake's applet 4.0 VERIFY_PIN against FedimintCardApplet.verifyPin and
// waitOwed: frame checks first, then the wait, served in stretches of at most
// 16 askings that count only while VERIFY_PIN follows VERIFY_PIN in one selection.

const PIN = '2468';

function owing(owed: number, wrong = 1) {
  const card = new FakeGpCard().withFinishedApplet(PIN);
  card.applet!.wrong = wrong;
  card.applet!.owed = owed;
  return card;
}

const send = async (card: FakeGpCard, apdu: number[]) => (await exchange(card.transceive, apdu)).sw;
const verify = (card: FakeGpCard, pin = PIN) => send(card, pinApdu(INS_VERIFY_PIN, pin));

test('one VERIFY_PIN per selection never serves a wait, and anything in between restarts the stretch', async () => {
  const card = owing(1);
  for (let tap = 0; tap < 5; tap++) {
    expect(await send(card, selectApdu())).toBe(0x9000);
    expect(await verify(card)).toBe(0x6a98);
  }
  expect(card.applet!.owed).toBe(1);
  await send(card, selectApdu());
  expect(await verify(card)).toBe(0x6a98);
  await send(card, stateApdu());
  expect(await verify(card)).toBe(0x6a98);
  expect(card.applet!.owed).toBe(1);
  // The asking after the stretch writes the wait down and goes on to the try.
  expect(await verify(card)).toBe(0x9000);
  expect(card.applet!.owed).toBe(0);
  expect(card.applet!.wrong).toBe(0);
});

test('a wait is served in stretches of 16, and a stretch cut by the lift starts again', async () => {
  const card = owing(40, 6);
  await send(card, selectApdu());
  const answers: (number | null)[] = [];
  for (let i = 0; i < 20; i++) {
    answers.push(await verify(card));
  }
  expect(answers.every(sw => sw === 0x6a98)).toBe(true);
  // 16 askings and the one that wrote the first stretch down; the 3 after it are lost with the selection.
  expect(card.applet!.owed).toBe(24);
  await send(card, selectApdu());
  const rest: (number | null)[] = [];
  for (let sw = 0x6a98 as number | null; sw === 0x6a98; ) {
    sw = await verify(card);
    rest.push(sw);
  }
  // 16 + 1, then 8 + 1, the last of which tries the PIN.
  expect(rest).toHaveLength(26);
  expect(rest[25]).toBe(0x9000);
  expect(card.applet!.owed).toBe(0);
});

test('the frame is checked before the wait: no PIN, length and P1/P2 neither serve nor restart it', async () => {
  const card = owing(2);
  await send(card, selectApdu());
  expect(await verify(card)).toBe(0x6a98);
  // The old three-byte probe answers 6700 even while a wait is owed.
  expect(await send(card, pinStatusApdu())).toBe(0x6700);
  const p1 = pinApdu(INS_VERIFY_PIN, PIN);
  p1[2] = 0x01;
  expect(await send(card, p1)).toBe(0x6b00);
  expect(card.applet!.asked).toBe(1);
  expect(card.applet!.owed).toBe(2);
  expect(await verify(card)).toBe(0x6a98);
  expect(await verify(card)).toBe(0x9000);

  const fresh = new FakeGpCard().withLiftedApplet(8);
  await send(fresh, selectApdu());
  expect(await verify(fresh)).toBe(0x6984);
});

test('a wrong try answers the wrong tries in a row and owes 2^(those before it)', async () => {
  const card = owing(0, 0);
  await send(card, selectApdu());
  expect(await verify(card, '1111')).toBe(0x63c1);
  expect(card.applet!.owed).toBe(1);
  expect(await verify(card, '1111')).toBe(0x6a98);
  expect(await verify(card, '1111')).toBe(0x63c2);
  expect(card.applet!.owed).toBe(2);
});
