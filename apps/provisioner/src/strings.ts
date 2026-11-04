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
    'The federation these cards join and draw their money from. Scan the invite QR from the federation\'s guardian or client page, or paste the invite text.\n\nThe phone checks the invite against the sidecar: when they name the same federation the screen shows its name; when they differ, cards would be loaded from the sidecar\'s federation, not this one, so fix that before starting.',
  emptyTitle: 'Add the federation',
  emptyBody: 'Scan its invite QR code, or paste the invite.',
  scanFederation: 'Scan invite QR',
  scanNew: 'Scan new invite',
  paste: 'Paste invite',
  pasteInstead: 'Paste an invite instead',
  customPlaceholder: 'fed11… or fedimint…',
  notInvite: 'That is not a federation invite. It starts with fed11 or fedimint.',
  useInvite: 'Use this invite',
  sidecarMatch: 'Sidecar is on this federation',
  sidecarMismatchShort: 'Sidecar is on a different federation',
  sidecarMismatch:
    'The sidecar is on a different federation. Cards would be loaded from the sidecar\'s federation, not this one.',
  sidecarChecking: 'Checking the sidecar…',
  sidecarOffline: 'Sidecar not reachable from this phone',
  count: 'Cards',
  countLabel: 'Cards in this batch',
  countHelp: (max: number) =>
    `How many cards you will program in this sitting, 1 to ${max}. The loop counts them off one tap at a time; you can end the batch early from the loop.`,
  pin: 'PIN',
  pinHelp:
    'The card refuses to sign without its PIN.\n\nOne PIN for the batch: you type it once here and every card gets it. It is never stored or sent anywhere but the card.\n\nA PIN per card: each card gets a fresh PIN, shown once on the batch summary. Write each one down before the card is handed out.',
  pinPlaceholder: (max: number) => `4 to ${max} digits`,
  perCardSub: 'Shown once on the summary. Write each one down.',
  pinBatchShort: 'One for the batch',
  pinPerCardShort: 'One per card',
  plan: 'Top-up',
  topupHelp:
    'What each card leaves with. Install only keys the card and puts no money on it; the holder funds it later.\n\nA top-up sum is split by the sidecar into notes, one per card slot, from the sidecar\'s wallet. Change it in Settings.',
  topupLabel: 'Top-up',
  topupChange: 'Change in Settings',
  installOnly: 'Install only',
  planPerCard: (total: string, notes: number) =>
    `${total} sat per card · ${notes} ${notes === 1 ? 'note' : 'notes'}`,
  batchTotal: 'Batch total',
  review: 'Review batch',
  reviewTitle: 'Review',
  reviewHeader: 'This batch',
  reviewHelp:
    'Check the batch before it starts. Starting records the batch with the registry and opens the card loop; nothing is written to a card until you hold one to the phone.',
  reviewLine: (count: number, installOnly: boolean) =>
    `Start opens the loop for ${count} ${count === 1 ? 'card' : 'cards'} and gets ready for card 1: have it in hand. ${installOnly ? 'Each card is installed and keyed with no money on it.' : 'Each card is installed, keyed and topped up.'}`,
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
  cardRow: (n: number, id: string) => `Card ${n} · …${id}`,
  tap: 'Press Install, then hold the card to the phone.',
  tryNext: 'Set that card aside. Press Install when the next one is ready.',
  allDone: 'Every card in this batch has been tried. Open the summary.',
  stopping: 'Stopping after this step…',
  waitStopped: 'Stopped before a card was read. Press Install when ready.',
  install: (n: number) => `Install card ${n}`,
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
  wrongKeys: 'The card did not prove the manager keys this batch uses.',
  guarded: 'This card already holds something this app never removes.',
  differentCard: 'The key the card read back is not the one this batch expected.',
  pinNotSet: 'The card took the PIN but did not confirm it.',
  issueFailed: 'The card is programmed but the top-up did not go through.',
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
  issueFailed: 'The card is empty; handing it out would promise value that is not there.',
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
  cardRow: (n: number, id: string) => `Card ${n} · …${id}`,
  empty: 'No cards this batch yet.',
  notReached: (left: number) =>
    `${left} ${left === 1 ? 'card was' : 'cards were'} not attempted.`,
  pinTitle: 'Last card\'s PIN',
  pinBody: 'Shown once. It is not kept after the batch ends.',
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
  topup: 'Top-up per card',
  topupHelp:
    'What each card leaves with. Install only keys the card and puts no money on it.\n\nThe sidecar splits a sum into notes, at most one per card slot (8). The quick picks split into notes of 8 sat or more; a sum with smaller notes can be refused by the federation when the batch runs.',
  installOnly: 'Install only',
  customPlaceholder: 'Other amount, sat',
  set: 'Set',
  amountsSaved: (sats: number) =>
    sats === 0 ? 'Saved: install only.' : `Saved: ${sats.toLocaleString()} sat per card.`,
  amountsTotal: (notes: number, sats: number) =>
    sats === 0
      ? 'Cards are installed and keyed with no money on them.'
      : `${sats.toLocaleString()} sat per card, in ${notes} ${notes === 1 ? 'note' : 'notes'}.`,
  amountsProblem: (most: number) =>
    `Use a whole number of sats that splits into at most ${most} notes.`,
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
