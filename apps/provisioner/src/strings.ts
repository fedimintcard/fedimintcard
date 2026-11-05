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
  outboxPending: 'Some rows have not reached the registry yet. They retry on their own.',
} as const;

/** What the card is now, after each stop. The second half of "what happened". */
export const STATE = {
  pulled: 'The card is unchanged: the next tap starts again from the check.',
  refused: 'This card needs another look before another attempt. Set it aside and continue with a new one.',
  wrongKeys: 'No further attempt until this stop is acknowledged, so a locked card is not driven further.',
  guarded: 'Use a blank card. This app never removes a card that already holds keys.',
  differentCard: 'Set this card aside; it was not issued to.',
  pinNotSet: 'This card is unspendable and must not be handed out. Set it aside.',
  noManagerKeys: 'The card is unchanged. Check the connection, or whose card it is, and tap it again.',
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
  resetSub: 'Wipe an empty card and set it up again, with a new PIN',
  resetBlocked: 'A batch is running. End it before resetting a card.',
  cardsHelp:
    'Reset a card that is empty: the card is wiped, set up again with new keys and a new PIN, and comes back as a new card. Ops must show nothing on it, and the holder types their PIN to agree.',
} as const;

/** Settings → Reset a card (OPS-PLAN O5). */
export const RESET = {
  title: 'Reset a card',
  intro:
    'Hold the card to the phone. The app reads which card it is and asks ops whether it may be reset. Nothing is changed by this tap.',
  help:
    'A reset deletes the card\'s applet and installs it again: the card gets new keys, a new short id and a new PIN, and ops links the old record to the new one. Value on the card would be lost, so ops refuses while it shows a live or pending note, an open charge or an open load. Ops cannot see a load made around it, so the holder\'s PIN is the consent. Other applets on the chip are never touched.',
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
  reset: 'Reset this card',
  resetting: 'Keep the card still…',
  stop: 'Stop',
  phase: {
    check: 'Asking ops',
    consent: 'Checking the holder\'s PIN',
    chip: 'Reading the chip',
    install: 'Wiping and installing — keep the card still',
    rotate: 'Locking the card to its own keys',
    keys: 'Making the card\'s keys',
    pin: 'Setting the new PIN',
  } as const,
  doneTitle: (id: string) => `Reset · now …${id.slice(-4).toUpperCase()}`,
  newPin: 'The card\'s new PIN',
  newPinBody: 'Write it down or hand it over with the card. It is not shown again.',
  recorded: 'Ops has the new card and links the old one to it.',
  notRecorded: 'The card is finished, but ops has not recorded it yet. Record it before closing.',
  record: 'Record now',
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
    'Only if the card is lost or was reset elsewhere: a wiped card cannot be finished without this record.',
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
    recordPending: 'Only the record is left',
    pulled: 'The card left the phone',
    refused: 'The card refused',
    wrongKeys: 'Neither the factory keys nor the card\'s own keys work',
    guarded: 'The card refused',
    noManagerKeys: 'Ops did not answer the card\'s manager keys',
    notFinished: 'The card\'s keys or PIN did not finish',
  },
  resumable: 'The card may be wiped now. Hold the same card to the phone to finish.',
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
