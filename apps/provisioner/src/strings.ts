/**
 * The words the five screens show. Kept in one file so a copy change is not a
 * code change, and because the rule the whole app keeps: every stop says what
 * happened, what the card is now, and what to do, in that order; the word
 * "error" appears on no screen.
 *
 * Only the five screens named in the plan's trim may appear here. A string for
 * a dropped screen (card status, identity, reset, bench) has no home.
 */

export const APP_NAME = 'Fedimint card provisioner';

export const SIGNIN = {
  eyebrow: 'Card provisioner',
  heading: 'Sign in',
  helpTitle: 'Operator sign-in',
  help: (loadLine: string) =>
    `Programs blank cards into Fedimint cards. Operators only: use the account your administrator gave you.\n\nYour password is held in memory for this session and never stored on the phone.\n\nCard software in this build: ${loadLine}.`,
  button: 'Sign in',
  signingIn: 'Signing in…',
  checking: 'Checking your account. Keep this screen open.',
  stopped: 'Sign-in stopped.',
  nameLabel: 'Operator name',
  passwordLabel: 'Password',
  demo: 'Try the demo',
  demoSub: 'Real engine, simulated card, no account.',
  namePlaceholder: 'Your operator name',
  passwordPlaceholder: 'Enter your password',
  failed: (reason: string) =>
    reason === 'bad-credentials'
      ? 'Check your operator name and password, then try again.'
      : reason === 'login-rate-limited'
      ? 'Too many sign-in attempts. Wait a minute, then try again.'
      : reason === 'unreachable'
      ? 'The service could not be reached. Check your connection and try again.'
      : 'Sign-in could not be completed. Try again, or ask your administrator for help.',
};

export const SETUP = {
  title: 'New batch',
  federation: 'Federation',
  federationHelp:
    'The federation these cards are made for, recorded with the batch. Scan the invite QR from the federation\'s guardian or client page, or paste the invite text.\n\nThe card holds no money when it leaves here: the holder loads it from their own wallet.',
  emptyTitle: 'Add the federation',
  emptyBody: 'Scan its invite QR code, or paste the invite.',
  scanFederation: 'Scan invite QR',
  scanNew: 'Scan new invite',
  paste: 'Paste invite',
  pasteInstead: 'Paste an invite instead',
  customPlaceholder: 'fed11… or fedimint…',
  notInvite: 'That is not a federation invite. It starts with fed11 or fedimint.',
  useInvite: 'Use this invite',
  count: 'Cards',
  countLabel: 'Cards in this batch',
  countHelp: (max: number) =>
    `How many cards you will program in this sitting, 1 to ${max}. The loop counts them off one tap at a time; you can end the batch early from the loop.`,
  pin: 'PIN',
  pinHelp:
    'The card refuses to sign without its PIN.\n\nOne PIN for the batch: you type it once here and every card gets it. It is never stored or sent anywhere but the card.\n\nA PIN per card: each card gets a fresh PIN, shown with the finished card until you start the next one. Write it down, or hand it over with the card, before you go on. It is not shown again.',
  pinPlaceholder: (max: number) => `4 to ${max} digits`,
  perCardSub: 'Shown with each finished card. Write it down before the next.',
  pinBatchShort: 'One for the batch',
  pinPerCardShort: 'One per card',
  review: 'Review batch',
  reviewTitle: 'Review',
  reviewHeader: 'This batch',
  reviewHelp:
    'Check the batch before it starts. Starting records the batch with the registry and opens the card loop; nothing is written to a card until you hold one to the phone.',
  reviewLine: (count: number) =>
    `Start opens the loop for ${count} ${count === 1 ? 'card' : 'cards'} and gets ready for card 1: have it in hand. Each card is installed, keyed and given its PIN in one tap, with no money on it.`,
  edit: 'Edit batch',
  start: 'Start batch',
  startFailed: 'The batch could not start.',
  needFederation: 'Add the federation to continue.',
  needCount: (max: number) => `Enter 1 to ${max} cards.`,
  needPin: 'Enter a batch PIN, or choose a PIN per card.',
  buildLine: (load: string) => `Card software ${load}`,
};

export const LOOP = {
  title: 'Batch',
  /** The counter under the title: cards done, of the batch. */
  progress: (done: number, of: number) => `${done} of ${of} cards`,
  /** The stage's headline: the batch's own denominator, no noun. */
  of: (done: number, of: number) => `${done} of ${of}`,
  done: (done: number, of: number) => `${done} of ${of} cards finished`,
  next: (card: number, of: number) => `Card ${card} of ${of}`,
  programmed: (n: number) => `${n} programmed`,
  stopped: (n: number) => `${n} stopped`,
  programmedOne: 'Programmed',
  cardRow: (n: number, id: string) => `Card ${n} · …${id.slice(-4).toUpperCase()}`,
  tap: 'Press Install, then hold the card to the phone.',
  tryNext: 'Set that card aside. Press Install when the next one is ready.',
  /** A tap lifted while the card's keys or PIN were written: the same card finishes it. */
  sameCard: 'Hold the same card to the phone and press Finish to complete it.',
  finishCard: (n: number) => `Finish card ${n}`,
  heldTitle: 'Card not finished',
  heldName: 'Tap the same card again',
  heldWhat: 'The card left the phone while its keys or its PIN were being written.',
  heldNow: 'Nothing is lost and it did not use up a place in this batch. Hold the same card to the phone again to finish it.',
  allDone: 'Every card in this batch has been tried. Open the summary.',
  stopping: 'Stopping after this step…',
  waitStopped: 'Stopped before a card was read. Press Install when ready.',
  noCard: 'No card was read before the scan closed. Press Install when ready.',
  install: (n: number) => `Install card ${n}`,
  phaseKeys: 'Making the card\'s keys',
  phasePin: 'Setting the PIN',
  finished: (id: string) => `Done · …${id.slice(-4).toUpperCase()}`,
  pinTitle: (n: number) => `Card ${n}\'s PIN`,
  pinBody: 'Write it down or hand it over with the card. It goes when the next card starts, and is not shown again.',
  keepStill: 'Keep the card still…',
  waiting: 'Waiting for the card…',
  busy: 'Keep the card against the phone until this line says done.',
  paused: 'Batch paused',
  cardStopped: 'Card stopped',
  acknowledge: 'Acknowledge and continue',
  thisBatch: 'Cards this batch',
  stops: 'Stops',
  stop: 'Stop this card',
  ended: (left: number) =>
    `Ending now leaves ${left} ${left === 1 ? 'card' : 'cards'} unprogrammed.`,
  end: 'End batch',
  summary: 'Batch summary',
};

export const CALM = {
  pulled: 'The card left the phone before it finished.',
  refused: 'The card answered something this batch did not expect.',
  wrongKeys: 'The card proved neither its factory keys nor the keys ops holds for it.',
  guarded: 'This card already holds something this app never removes.',
  differentCard: 'The key the card read back is not the one this batch expected.',
  pinNotSet: 'The card was installed, but its keys or its PIN did not finish.',
  noManagerKeys: 'Ops did not answer this card\'s manager keys, so nothing was written.',
  needsReset: 'The card has a PIN this phone cannot prove, so it was not finished.',
  outboxPending: 'Some rows have not reached the registry yet. They retry on their own.',
} as const;

/** What the card is now, after each stop. The second half of "what happened". */
export const STATE = {
  pulled: 'The card is unchanged: the next tap starts again from the check.',
  refused: 'This card needs another look before another attempt. Set it aside and continue with a new one.',
  wrongKeys: 'No further attempt until this stop is acknowledged, so a locked card is not driven further.',
  guarded: 'Use a blank card. This app never removes a card that already holds keys.',
  differentCard: 'Set this card aside; it was not issued to.',
  pinNotSet: 'Do not hand this card out. Settings → Reset a card takes it back to blank; then install it again.',
  noManagerKeys: 'The card is unchanged. Check the connection, or whose card it is, and tap it again.',
  needsReset: 'Do not hand this card out. Settings → Reset a card takes it back to blank, and asks for no PIN: this card was never finished. Then install it again.',
  outboxPending: 'The card work is done; only the registry row waits.',
} as const;

export const SUMMARY = {
  title: 'Summary',
  headline: (programmed: number, of: number) =>
    `${programmed} of ${of} ${of === 1 ? 'card' : 'cards'} programmed`,
  programmed: 'Programmed',
  stopped: 'Stopped',
  pending: 'Pending',
  record: 'Cards',
  recordHelp:
    'Each card tried in this batch, in order, with the time its row was recorded. A hollow dot is a row still on its way to the registry; it retries on its own.',
  cardRow: (n: number, id: string) => `Card ${n} · …${id.slice(-4).toUpperCase()}`,
  empty: 'No cards this batch yet.',
  notReached: (left: number) =>
    `${left} ${left === 1 ? 'card was' : 'cards were'} not attempted.`,
  pendingRetry: 'Some rows have not reached the registry yet. They retry on their own.',
  pendingNote:
    'Some rows have not reached the registry. Closing now ends the batch; they keep retrying in the background.',
  acknowledge: 'Acknowledge stop',
  close: 'Close batch',
  closeAnyway: 'Close anyway',
  backToLoop: 'Back to the batch',
  backToCards: 'Back to the cards',
};

export const SETTINGS = {
  title: 'Settings',
  appName: 'Fedimint card provisioner',
  federation: 'Federation',
  federationHelp:
    'The federation new batches join. Scan its invite QR or paste the invite; the choice is saved at once and New batch starts from it. A batch already running keeps the federation it started with.',
  federationSaved: 'Federation saved for the next batch.',
  registry: 'Registry',
  registryNow: (label: string) => `Recording to the ${label.toLowerCase()} registry.`,
  registryHelp:
    'Where batch and card rows are recorded after each card. Test and Live are the same service under two endpoints; use Test while rehearsing.',
  about: 'About',
  cardSoftware: (load: string) => `Card software ${load}`,
  experimentalShort: 'Experimental software. Tap (i) on About before handing out cards.',
  session: 'Session',
  demo: 'Demo',
  demoTitle: 'Run the demo',
  demoSub: 'The whole install loop on a simulated card',
  demoHelp:
    'Programs a simulated card with the real engine: no reader, no network, and nothing recorded to the registry. Start here if you do not have a card to hand.',
  demoBlocked: 'A batch is running. End it before running the demo.',
  signOut: 'Sign out',
  signOutSub: 'Your settings stay on this phone.',
  signOutBlocked: 'A batch is running. End or leave it before signing out.',
  keepSession: 'Stay signed in',
  cards: 'Cards',
  resetTitle: 'Reset a card',
  resetSub: 'Take an empty card back to its factory state',
  resetBlocked: 'A batch is running. End it before resetting a card.',
  cardsHelp:
    'Reset a card that is empty: the Fedimint applet is deleted and the card goes back to the chip maker\'s keys, blank. Ops must show nothing on it, and the holder types their PIN to agree. To use it again, tap it in a new batch.',
} as const;

/** Settings → Reset a card (OPS-PLAN O5). */
export const RESET = {
  title: 'Reset a card',
  intro:
    'Hold the card to the phone. The app reads which card it is and asks ops whether it may be reset. Nothing is changed by this tap.',
  help:
    'A reset deletes the card\'s Fedimint applet and puts the card back on the chip maker\'s keys, then logs in with those keys to prove it. The card is blank afterwards: ops marks the old record reset and frees the chip, and a new batch can install it again. Value on the card would be lost, so ops refuses while it shows a live or pending note, an open charge or an open load. Ops cannot see a load made around it, so the holder\'s PIN is the consent. Other applets on the chip are never touched.',
  read: 'Read the card',
  reading: 'Hold the card to the phone…',
  card: 'Card',
  pinState: 'PIN',
  pinStates: {set: 'Set', blocked: 'Blocked', notSet: 'Never set'},
  ops: 'At ops',
  opsEmpty: 'Empty',
  refusedTitle: 'Ops will not reset this card',
  refusedBody: 'Handle what ops still shows first, then read the card again.',
  holderPin: 'Holder\'s PIN',
  holderPinHelp:
    'The holder types their PIN here. It is sent to the card once, to show they agree, and is not kept. A wrong PIN uses one of the card\'s three tries.',
  holderPinPlaceholder: 'Holder\'s PIN',
  blockedTitle: (pin: 'blocked' | 'notSet') =>
    pin === 'blocked' ? 'This card\'s PIN is blocked' : 'This card never had a PIN set',
  blockedBody:
    'The holder cannot agree with their PIN. Ops shows the card empty, but a load made around ops would be lost with the card\'s keys. Reset only if you are sure the card holds nothing.',
  blockedConfirm: 'I am sure: reset without the PIN',
  blockedConfirmed: 'Confirmed. Hold the card to the phone to reset it.',
  noHolderPinTitle: 'No PIN needed',
  noHolderPinBody:
    'This card was never finished, so it has no holder PIN to ask for. Ops shows nothing on it and its chip is yours. Hold the card to the phone to reset it.',
  reset: 'Reset this card',
  resetting: 'Keep the card still…',
  stop: 'Stop',
  phase: {
    check: 'Asking ops',
    consent: 'Checking the holder\'s PIN',
    chip: 'Reading the chip',
    erase: 'Deleting the applet — keep the card still',
    restore: 'Putting back the factory keys',
    prove: 'Checking the card is blank',
  } as const,
  doneTitle: (id: string) => `Card …${id.slice(-4).toUpperCase()} is reset`,
  doneBody:
    'The card is blank and back on its factory keys. To use it again, tap it in a new batch: it gets its keys back from ops and a new short id.',
  recorded: 'Ops has the reset: the old record is closed and the chip is free.',
  notRecorded:
    'Ops has not been told yet. The card itself is finished; this phone keeps the record and sends it when ops answers, at the latest the next time you open Reset a card.',
  waiting: (n: number) =>
    `${n} finished ${n === 1 ? 'reset is' : 'resets are'} waiting to be sent to ops.`,
  record: 'Send to ops now',
  stranded: (n: number) =>
    `Ops still held ${n} ${n === 1 ? 'note' : 'notes'} under the old card. They cannot be spent now.`,
  done: 'Done',
  another: 'Reset another card',
  unfinished: (id: string) =>
    `A reset of card …${id.slice(-4).toUpperCase()} did not finish. Hold the same card to the phone to finish it.`,
  unfinishedOther: 'Another operator\'s reset did not finish on this phone.',
  finish: 'Finish the reset',
  forget: 'Forget the unfinished reset',
  forgetConfirm:
    'Only if the card is lost or was reset elsewhere: a card part way through cannot be finished without this record.',
  noCard: 'No card was read before the scan closed.',
  notYours: 'Ops has no such card for this operator: it is unknown, or another operator\'s.',
  stops: {
    differentCard: 'Not the same card',
    notResettable: 'Ops will not reset this card',
    wrongPin: (left: number) => `Wrong PIN · ${left} ${left === 1 ? 'try' : 'tries'} left`,
    pinBlocked: 'The PIN is blocked',
    pinRequired: 'The holder\'s PIN is needed',
    journal: 'The reset could not be saved on this phone; nothing was changed',
    otherOperator: 'Another operator\'s reset is unfinished on this phone',
    pulled: 'The card left the phone',
    refused: 'The card refused',
    wrongKeys: 'Neither the factory keys nor the card\'s own keys work',
    guarded: 'The reset stopped to keep the card safe',
    noManagerKeys: 'Ops did not answer the card\'s manager keys',
  },
  resumable: 'The card may be part way through. Hold the same card to the phone to finish.',
  untouched: 'The card is unchanged.',
  identify: {
    noApplet: 'This card has no Fedimint applet: there is nothing to reset. A blank card goes through a batch.',
    unkeyed: 'This card was installed but never keyed. Finish it in a batch instead.',
    refused: 'The card did not answer as a Fedimint card.',
  },
} as const;

/**
 * The invite scanner, a sheet over Settings and New batch rather than a sixth
 * screen: it exists only to fill their federation field.
 */
export const SCAN = {
  title: 'Scan a federation invite',
  hint: "Point the camera at the federation's QR code.",
  notInvite: 'That QR code is not a federation invite.',
  noPermission: 'The camera is off for this app. Allow it to scan an invite.',
  allow: 'Allow camera',
  openSettings: 'Open Settings',
  noCamera: 'This phone has no camera the app can use. Paste the invite instead.',
  cancel: 'Cancel',
};
