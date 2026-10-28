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
  tagline: 'Programs blank cards into Fedimint cards. Operators only.',
  button: 'SIGN IN',
  signingIn: 'SIGNING IN…',
  note: 'Use the operator account provided by your administrator.',
  namePlaceholder: 'Operator name',
  passwordPlaceholder: 'Password',
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
  title: 'NEW BATCH',
  federation: 'Federation',
  federationFailed:
    'The federation list could not be loaded. Check the connection and retry, or enter a federation invite.',
  federationEmpty:
    'No federation invites are known. Enter the invite below, or set one in Settings.',
  customFederation: 'Custom',
  customPlaceholder: 'fedimint://…',
  pin: 'PIN',
  count: 'Cards in the batch',
  countRange: 'Enter a whole number from 1 to the batch maximum.',
  plan: 'Top-up plan',
  planNone: 'No top-up: this batch only installs and keys the cards.',
  planPerCard: (total: string, notes: number) =>
    `${total} sats in ${notes} ${notes === 1 ? 'note' : 'notes'} per card`,
  planBatch: (total: string, count: number) =>
    `${total} sats across ${count} ${count === 1 ? 'card' : 'cards'}`,
  start: 'START BATCH',
  buildLine: (load: string) => `Load file ${load}`,
};

export const LOOP = {
  title: 'BATCH',
  /** The counter under the title: cards done, of the batch. */
  progress: (done: number, of: number) => `${done} of ${of} cards`,
  done: (done: number, of: number) => `${done} of ${of} cards finished`,
  next: (card: number, of: number) => `Card ${card} of ${of}`,
  tap: 'Hold the card against the phone.',
  install: 'INSTALL THIS CARD',
  busy: 'Keep the card against the phone until this line says done.',
  stop: 'STOP THIS CARD',
  ended: (left: number) =>
    `This batch ends with ${left} ${left === 1 ? 'card' : 'cards'} unprogrammed.`,
  end: 'END BATCH',
  summary: 'BATCH SUMMARY',
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
  title: 'Batch summary',
  programmed: 'Programmed',
  stopped: 'Stopped',
  pending: 'Pending',
  record: 'The record',
  empty: 'No cards this batch yet.',
  notReached: (left: number) =>
    `This batch ended with ${left} ${left === 1 ? 'card' : 'cards'} not attempted.`,
  pendingNote: 'Some rows have not reached the registry and will retry.',
  acknowledge: 'ACKNOWLEDGE STOP',
  close: 'CLOSE BATCH',
  closeAnyway: 'CLOSE ANYWAY',
  backToLoop: 'BACK TO THE BATCH',
};

export const SETTINGS = {
  title: 'Settings',
  federation: 'Federation',
  federationHelp:
    'Which federation this batch joins. One value for the batch; change it here between batches.',
  federationSaved: 'Federation saved for the next batch.',
  inviteProblem: 'A federation invite starts with fedimint://.',
  saveFederation: 'SAVE FEDERATION',
  registry: 'Registry endpoint',
  registryNow: (label: string) => `Recording to the ${label.toLowerCase()} registry.`,
  registryHelp:
    'Where batch and card rows are recorded. Test and live are the same service under two endpoints.',
  topup: 'Top-up amounts',
  topupHelp:
    'The sats each card receives, one note per amount, largest first. The sidecar previews this plan before a batch starts.',
  amountsSaved: (sats: number) => `Saved: ${sats.toLocaleString()} sat per card.`,
  amountsTotal: (notes: number, sats: number) =>
    `${notes} ${notes === 1 ? 'note' : 'notes'} · ${sats.toLocaleString()} sat per card`,
  amountsProblem: (most: number) =>
    `Use 1–${most} whole sat amounts, each greater than zero.`,
  saveAmounts: 'SAVE AMOUNTS',
  theme: 'Theme',
  themeSystem: 'System',
  themeDark: 'Dark',
  themeLight: 'Light',
  about: 'About',
  experimental: 'Experimental',
  session: 'Session',
  signOut: 'Sign out',
  signOutSub: 'Signs out without deleting the settings saved on this phone.',
  signOutBlocked: 'A batch is running. End or leave it before signing out.',
  keepSession: 'KEEP SESSION',
} as const;
