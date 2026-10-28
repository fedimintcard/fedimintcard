/**
 * One NFC session: an armed IsoDep request, its transceiver and its teardown.
 * One at a time; cancelled in `finally`, by the Cancel button, and when the
 * screen unmounts. Its errors are told apart by the NFC library's classes
 * (nfcErrorName): on iOS the class is all an error says.
 *
 * Ported from cashucard `apps/provisioner/src/session.ts` (plan § 5: "portable
 * almost unchanged"), checked against react-native-nfc-manager 3.17.2's own
 * `index.d.ts`: `start`, `requestTechnology(NfcTech.IsoDep, {alertMessage})`,
 * `isoDepHandler.transceive(number[]) -> Promise<number[]>`,
 * `setTimeout/getTimeout/getMaxTransceiveLength` (Android) and
 * `setAlertMessageIOS` (iOS) are all still there with those shapes, and
 * `NfcError` still exports the 23 classes the table below names plus its base.
 * Nothing in the cable path has drifted.
 *
 * Two departures from upstream, both required by this card:
 *
 * 1. **The applet is selected before any other APDU.** cardkit's
 *    `fedimintPlan` sets no `guard` (this applet has no GET_INFO), so
 *    `provision()` starts at `selectIsd` and would send GET DATA to whatever
 *    applet the card last selected — usually nothing, since SELECT of the card
 *    manager leaves the ISD selected, but not on every card. The transceiver
 *    therefore SELECTs the card's applet itself once the card connects, so a
 *    run always starts from the same state. A card with no applet answers
 *    `6A 82`; cardkit sees that as a refusal and, with no probe phase, simply
 *    goes on to install. See `selectApplet`.
 * 2. **The plan is not an input.** Upstream's `withCard(alertMessage, run)`
 *    signature is kept; the engine and the AID come from `./cardkit`, the one
 *    seam.
 *
 * Nothing here watches AppState, as neither reference session does. Android
 * delivers the tag through onNewIntent (nfc-manager's default foreground
 * dispatch), which it brackets with onPause and onResume, so AppState goes
 * 'background' and back to 'active' on every tap; iOS reports 'inactive'
 * while its NFC sheet covers the app. A cancel on either would close the
 * session the tap has just opened. When the app does leave the foreground,
 * iOS invalidates its reader session itself and nfc-manager stops Android's
 * dispatch in onHostPause.
 */
import {Platform} from 'react-native';
import NfcManager, {NfcError, NfcTech} from 'react-native-nfc-manager';

import {CARD_AID, type Transceiver} from './cardkit';

/**
 * Android's IsoDep timeout for one APDU. The default, 618 ms, is shorter than
 * the last LOAD block (about 730 ms on a reader, more at a phone's field
 * strength) and than a mute INSTALL takes to give up (about 7 s), and Android
 * reports it as TagLost, like a card that moved.
 */
export const ANDROID_TIMEOUT_MS = 15000;

/** ISO 7816-4 SELECT by AID. */
const SELECT_APDU = [0x00, 0xa4, 0x04, 0x00] as const;

/** `6A 82`: nothing installed answers to that AID. The install starts from here. */
export const SW_FILE_NOT_FOUND = 0x6a82;

/** When the session was armed, when the card connected, when the run ended (ms). */
export interface Stamps {
  arm: number;
  connect?: number;
  done?: number;
}

export interface SessionMeta {
  platform: string;
  os: string;
  /** What Android reports after setTimeout(15000): it may clamp it. Null when it would not say. */
  androidTimeoutMs?: number | null;
  /** Android's largest transceive, in bytes. */
  maxTransceive?: number | null;
  /**
   * What the SELECT of the applet answered. `6a82` means the card has no
   * fedimint applet yet, which is the normal start of an install;
   * `0x0000` would mean the SELECT never left (it cannot, it runs first).
   */
  selectSw?: number;
}

/**
 * The NFC library's error classes by name: every class react-native-nfc-manager
 * 3.17.2 exports but its base. Its iOS side turns each Core NFC code into one
 * of these with no message (src/NfcError.js, buildNfcExceptionIOS), so on iOS
 * the class is all an error says, and String() of it is just "Error". The
 * names are string literals matched with instanceof, because a Release
 * bundle's minifier renames the classes. The base, NfcErrorBase, is what the
 * library throws for everything else, and it carries the native message: every
 * Android error but "cancelled" (UserCancel), and any iOS error string that is
 * not a Core NFC code it knows.
 */
const NFC_ERRORS = [
  [NfcError.UnsupportedFeature, 'UnsupportedFeature'],
  [NfcError.SecurityViolation, 'SecurityViolation'],
  [NfcError.InvalidParameter, 'InvalidParameter'],
  [NfcError.InvalidParameterLength, 'InvalidParameterLength'],
  [NfcError.ParameterOutOfBound, 'ParameterOutOfBound'],
  [NfcError.RadioDisabled, 'RadioDisabled'],
  // a transceive's
  [NfcError.TagConnectionLost, 'TagConnectionLost'],
  [NfcError.RetryExceeded, 'RetryExceeded'],
  [NfcError.TagResponseError, 'TagResponseError'],
  [NfcError.SessionInvalidated, 'SessionInvalidated'],
  [NfcError.TagNotConnected, 'TagNotConnected'],
  [NfcError.PacketTooLong, 'PacketTooLong'],
  // the reader session's
  [NfcError.UserCancel, 'UserCancel'],
  [NfcError.Timeout, 'Timeout'],
  [NfcError.Unexpected, 'Unexpected'],
  [NfcError.SystemBusy, 'SystemBusy'],
  [NfcError.FirstNdefInvalid, 'FirstNdefInvalid'],
  // a tag command's configuration, and the NDEF reader's
  [NfcError.InvalidConfiguration, 'InvalidConfiguration'],
  [NfcError.TagNotWritable, 'TagNotWritable'],
  [NfcError.TagUpdateFailure, 'TagUpdateFailure'],
  [NfcError.TagSizeTooSmall, 'TagSizeTooSmall'],
  [NfcError.ZeroLengthMessage, 'ZeroLengthMessage'],
] as const;

export type NfcErrorName = (typeof NFC_ERRORS)[number][1];

/** The NFC library's name for `error`, or undefined when it is none of the library's classes. */
export function nfcErrorName(error: unknown): NfcErrorName | undefined {
  for (const [type, name] of NFC_ERRORS) {
    if (error instanceof type) {
      return name;
    }
  }
  return undefined;
}

/**
 * The status word of a reply that ends in `90 00`, or of the last two bytes
 * otherwise. `null` for a reply too short to carry one, which a transceiver
 * never returns (cardkit's `transmit` would report it as a transport error).
 */
export function statusWord(reply: readonly number[]): number | null {
  if (reply.length < 2) {
    return null;
  }
  return (reply[reply.length - 2] << 8) | reply[reply.length - 1];
}

interface ActiveSession {
  owner: object;
  cancelled: boolean;
}

/** The one session; a second run is refused so two screens cannot drive one card. */
let inFlight: ActiveSession | null = null;

/** Whether a session is armed right now; the loop shows Cancel on it, never itself. */
export function sessionActive(): boolean {
  return inFlight !== null;
}

/** Cancellation belongs to this caller, never a different screen's session. */
export function createCardSession() {
  const owner = {};
  return {
    run<T>(
      message: string,
      run: (tx: Transceiver) => Promise<T>,
      signal?: AbortSignal,
    ) {
      return runCardSession(owner, message, run, Date.now, signal);
    },
    async cancel(): Promise<void> {
      if (inFlight?.owner === owner) {
        await cancelCard();
      }
    },
  };
}

/** Ends any session; never throws (cancelling nothing rejects, and that is fine). */
export async function cancelCard(): Promise<void> {
  if (inFlight) {
    inFlight.cancelled = true;
  }
  try {
    await NfcManager.cancelTechnologyRequest();
  } catch {
    // no session to cancel
  }
}

/** The text in iOS's NFC sheet; fire and forget, a no-op on Android. */
export function setCardMessage(message: string): void {
  if (Platform.OS !== 'ios') {
    return;
  }
  try {
    NfcManager.setAlertMessageIOS(message).catch(() => undefined);
  } catch {
    // the session is gone
  }
}

/**
 * What the loop asks iOS's sheet to say, by the engine's phase. Android shows
 * nothing here, and `setCardMessage` is a no-op there.
 */
export function phaseMessage(phase: string): string {
  switch (phase) {
    case 'selectIsd':
    case 'probe':
      return 'Hold the card against the top of the phone';
    case 'auth':
      return 'Authenticating';
    case 'sweep':
      return 'Removing what is on the card';
    case 'installForLoad':
      return 'Preparing the install';
    case 'load':
      return 'Loading the applet — keep the card still';
    case 'installOpener':
    case 'installCard':
      return 'Installing the applet';
    case 'verify':
      return 'Checking the applet';
    default:
      return 'Keep the card still';
  }
}

/** The SELECT of the applet, as one APDU. Never MACed: it is sent outside any secure channel. */
export function selectAppletApdu(): number[] {
  return [...SELECT_APDU, CARD_AID.length, ...CARD_AID, 0x00];
}

/**
 * Arms a session, waits for the card, raises Android's timeout, selects the
 * applet, and runs `run` with a transceiver to the card. The session is always
 * torn down.
 */
export async function withCard<T>(
  alertMessage: string,
  run: (tx: Transceiver) => Promise<T>,
  now: () => number = Date.now,
  signal?: AbortSignal,
): Promise<{value: T; stamps: Stamps; meta: SessionMeta}> {
  return runCardSession({}, alertMessage, run, now, signal);
}

/** What a run rejects with when it is cancelled, by button or by unmount. */
export const CANCELLED = 'Card session cancelled';

async function runCardSession<T>(
  owner: object,
  alertMessage: string,
  run: (tx: Transceiver) => Promise<T>,
  now: () => number,
  signal?: AbortSignal,
): Promise<{value: T; stamps: Stamps; meta: SessionMeta}> {
  if (inFlight) {
    throw new Error('a run is already going');
  }
  const active = {owner, cancelled: signal?.aborted === true};
  inFlight = active;
  // The signal only has to end the wait: requestTechnology gives up when
  // cancelTechnologyRequest is called, which cancelCard does, and the
  // transceiver refuses further APDUs from the next check onward.
  signal?.addEventListener('abort', cancelCard, {once: true});
  const checkCancelled = () => {
    if (active.cancelled) {
      throw new Error(CANCELLED);
    }
  };
  const stamps: Stamps = {arm: now()};
  const meta: SessionMeta = {
    platform: Platform.OS,
    os: String(Platform.Version),
  };
  try {
    await NfcManager.start();
    checkCancelled();
    await NfcManager.requestTechnology(NfcTech.IsoDep, {alertMessage});
    // iOS's sheet carries one line while the card is held; Android has no such
    // call and setCardMessage is a no-op there. Setting it here means the
    // session's own message survives a screen that re-renders mid-run.
    setCardMessage(alertMessage);
    checkCancelled();
    stamps.connect = now();
    if (Platform.OS === 'android') {
      try {
        await NfcManager.setTimeout(ANDROID_TIMEOUT_MS);
        meta.androidTimeoutMs = (await NfcManager.getTimeout()) ?? null;
      } catch {
        meta.androidTimeoutMs = null;
      }
      try {
        meta.maxTransceive = await NfcManager.getMaxTransceiveLength();
      } catch {
        meta.maxTransceive = null;
      }
    }
    // cardkit gives a failed transceive's message as the reason the run
    // stopped (TransportError, the readout's `why`). An iOS error has no
    // message, only its class, so it is rethrown under the library's name for
    // it; any other error (Android's carry their messages) is rethrown as is.
    const tx: Transceiver = async apdu => {
      checkCancelled();
      try {
        return Array.from(await NfcManager.isoDepHandler.transceive(apdu));
      } catch (e) {
        const name = nfcErrorName(e);
        throw name ? new Error(name) : e;
      }
    };
    checkCancelled();
    // Departure 1 in the header: the run starts from a known selection. A
    // reply without a status word is a broken card, and cardkit would report
    // it as a transport error one phase later with no clue where it broke.
    const selected = await tx(selectAppletApdu());
    const sw = statusWord(selected);
    if (sw === null) {
      throw new Error('SELECT of the applet answered no status word');
    }
    meta.selectSw = sw;
    checkCancelled();
    const value = await run(tx);
    stamps.done = now();
    return {value, stamps, meta};
  } finally {
    signal?.removeEventListener('abort', cancelCard);
    await cancelCard();
    inFlight = null;
  }
}
