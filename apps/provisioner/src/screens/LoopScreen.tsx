/**
 * S3. The loop: the card path, ported as is (plan § 5: "kept as is — it is the
 * card path").
 *
 * One card per tap, eleven phases, seven stops, and the counter that is the
 * batch's denominator. The phases are `store.tsx`'s `CARD_PHASES`:
 *
 *   probe → selectIsd → auth → sweep → load → install → verify → keySetup →
 *   setPin → issue → done
 *
 * The first seven are cardkit's install over the phone's NFC session
 * (`session.ts` → `provision`). `keySetup` and `setPin` follow in the same
 * session (`cardSetup.ts`): a freshly installed applet has eight empty slots,
 * so the phone asks the card to generate its keys, then sets and verifies the
 * PIN. No sidecar, no relay (docs/OPS-PLAN.md § 4). The provisioner loads no
 * value; the holder's wallet does (§ 6).
 *
 * The screen computes no card state. Every outcome goes to
 * `actions.recordCard`, the only way card state enters the store; the counter,
 * the stop list and the acknowledgement gate come back from `useBatch()`.
 *
 * A per-card PIN is generated here and written to `pin.ts`'s reveal slot. It
 * is shown with the finished card until the next card's tap starts, as the
 * Cashu provisioner shows it, and nowhere else. A batch PIN is read from the
 * batch-PIN slot and never copied. Neither value goes to the store, the
 * registry or a log.
 */
import React, {useCallback, useEffect, useRef, useState} from 'react';
import {ScrollView, StyleSheet, Text, View} from 'react-native';

import {CardFigure} from '../CardFigure';

import {FEDIMINT_PLAN, provision} from '../cardkit';
import {finishCard, probeApplet, type SetupResult} from '../cardSetup';
import {generatePin, useBatchPin, usePinReveal} from '../pin';
import {phaseMessage, setCardMessage, withCard} from '../session';
import {CALM, LOOP, STATE} from '../strings';
import {
  useLoop,
  type CardPhase,
  type CardStop,
  type RecordRow,
  shortIdOf,
  useBatch,
} from '../store';
import {
  Card,
  Divider,
  FONT,
  GearButton,
  PrimaryButton,
  Screen,
  SectionHeader,
  SecondaryButton,
  useTheme,
} from '../ui';

/** What the stop list calls each kind: short, because it sits in a row. */
export const STOP_NAMES: Record<CardStop, string> = {
  pulled: 'Card left early',
  refused: 'Card refused',
  wrongKeys: 'Wrong manager keys',
  guarded: 'Card holds something',
  differentCard: 'Different card',
  pinNotSet: 'PIN not set',
};

/**
 * Where a stopped run stopped, in the store's phase vocabulary. cardkit reports
 * its own phase names (`installForLoad`, `installOpener`, …); the operator sees
 * seven.
 */
function foldedPhase(phase: string): CardPhase {
  switch (phase) {
    case 'probe':
    case 'selectIsd':
    case 'auth':
    case 'sweep':
      return phase;
    case 'installForLoad':
    case 'load':
      return 'load';
    case 'installOpener':
    case 'installCard':
    case 'installTag':
      return 'install';
    default:
      return 'verify';
  }
}

export function LoopScreen({
  armFirst = false,
  onArmed,
  onSummary,
  onSettings,
}: {
  /** Arm card 1 on open: the review page's START already said "go". */
  armFirst?: boolean;
  onArmed?: () => void;
  onSummary: () => void;
  onSettings: () => void;
}) {
  const batch = useBatch();
  const liveLoop = useLoop();
  const batchPin = useBatchPin();
  const reveal = usePinReveal();
  const t = useTheme();

  const [busy, setBusy] = useState(false);
  // True from Install until the card first answers.
  const [waiting, setWaiting] = useState(false);
  const [line, setLine] = useState<string | null>(null);
  const [stopShown, setStopShown] = useState<CardStop | null>(null);
  const cancelled = useRef(false);
  // Ends the wait for a card: `withCard` cancels the NFC request on abort.
  const abort = useRef<AbortController | null>(null);
  // The last finished card's generated PIN, read from the reveal slot. Bumped
  // to re-render when the slot changes, since the slot is a ref.
  const [shownPin, setShownPin] = useState<{n: number; id: string; pin: string} | null>(null);

  // Read once, before any early return: a component that returns early must
  // not call fewer hooks on the next pass.
  const config = batch?.config ?? null;
  const blockedBy = batch?.blockedBy ?? null;
  const actions = batch?.actions ?? null;

  const runCard = useCallback(async () => {
    if (!batch || !config || !actions || busy || blockedBy !== null) {
      return;
    }
    if (batch.records.length >= config.count) {
      return;
    }
    const policy = config.pinPolicy;
    const pin = policy === 'perCard' ? generatePin() : batchPin.value;
    if (pin === null) {
      setLine('Set the batch PIN before the first card.');
      return;
    }
    cancelled.current = false;
    // The previous card's PIN leaves the screen as this card's tap starts.
    reveal.wipe();
    setShownPin(null);
    const controller = new AbortController();
    abort.current = controller;
    // Set by the engine's first event: from then on a card is on the phone,
    // and a stop is that card's outcome. Before it, nothing was touched.
    let touched = false;
    setWaiting(true);
    setBusy(true);
    setStopShown(null);
    actions.setLoop({phase: 'probe', cardActive: true, loadProgress: null});
    setLine(phaseMessage('probe'));

    // Where the run is, kept local: `actions.setLoop` is for the screen, and a
    // card's row must name the phase the engine actually reached.
    let phase: CardPhase = 'probe';
    let pubkey: readonly number[] | undefined;
    const enter = (next: CardPhase, message: string) => {
      phase = next;
      actions.setLoop({phase: next, loadProgress: null});
      setLine(message);
      setCardMessage(message);
    };
    try {
      const {value: run} = await withCard(
        phaseMessage('probe'),
        async tx => {
          // Before anything is written: a card that already has keys is never
          // installed over, since the sweep would delete them with any value
          // they hold; one with the applet but no keys skips the install.
          const before = await probeApplet(tx);
          if (!touched) {
            touched = true;
            setWaiting(false);
          }
          if (before === 'keyed') {
            return {
              installed: {
                ok: false as const,
                kind: 'guarded' as const,
                sw: undefined,
                reason: 'the card already has keys; this app never installs over them',
              },
              setup: null as SetupResult | null,
            };
          }
          const installed =
            before === 'unkeyed'
              ? {ok: true as const}
              : await provision(tx, FEDIMINT_PLAN, {
                random: length =>
                  Array.from({length}, () => (Math.random() * 256) | 0),
                now: Date.now,
                onEvent: event => {
                  if (!touched) {
                    touched = true;
                    setWaiting(false);
                  }
                  if (cancelled.current) {
                    return;
                  }
                  if (event.type === 'phase') {
                    phase = foldedPhase(event.phase);
                    actions.setLoop({phase, loadProgress: null});
                    setLine(phaseMessage(event.phase));
                  } else if (event.type === 'block') {
                    phase = 'load';
                    actions.setLoop({
                      phase,
                      loadProgress: {index: event.index, of: event.of},
                    });
                    setLine(
                      `Writing ${Math.round((event.index / event.of) * 100)}%`,
                    );
                  }
                },
              });
          if (!installed.ok || cancelled.current) {
            return {installed, setup: null as SetupResult | null};
          }
          // The same session: the card is still on the phone.
          const setup = await finishCard(tx, pin, step => {
            if (step === 'keys') {
              enter('keySetup', LOOP.phaseKeys);
            } else if (step === 'setPin') {
              enter('setPin', LOOP.phasePin);
            }
          });
          return {installed, setup};
        },
        Date.now,
        controller.signal,
      );
      if (cancelled.current) {
        throw new Error('CardSessionCancelled');
      }
      const {installed, setup} = run;
      if (!installed.ok) {
        actions.recordCard({
          ok: false,
          phase,
          stop: installed.kind ?? 'refused',
          pinPolicy: policy,
          pinState: 'unknown',
          notesIssued: [],
          sw: installed.sw,
          reason: installed.reason,
        });
        setStopShown(installed.kind ?? 'refused');
        return;
      }
      if (!setup || !setup.ok) {
        // Installed, but no keys or no PIN: the card cannot be handed out.
        const keysDone = setup !== null && setup.step !== 'select' && setup.step !== 'keys';
        actions.recordCard({
          ok: false,
          phase: keysDone ? 'setPin' : 'keySetup',
          stop: 'pinNotSet',
          pinPolicy: policy,
          pinState: 'notSet',
          notesIssued: [],
          sw: setup?.sw ?? undefined,
          reason: setup?.reason ?? 'key setup did not run',
        });
        setStopShown('pinNotSet');
        return;
      }

      pubkey = setup.keys[0];
      const id = shortIdOf(pubkey);
      phase = 'done';
      actions.recordCard({
        ok: true,
        phase,
        pinPolicy: policy,
        pinState: 'set',
        notesIssued: [],
        pubkey,
      });
      if (policy === 'perCard') {
        reveal.grant(id, pin);
        setShownPin({n: batch.records.length + 1, id, pin});
      }
      setLine(LOOP.finished(id));
    } catch (e) {
      // Stopped while waiting, before any card answered: no card was tried, so
      // nothing is recorded and the batch's count is untouched.
      if (cancelled.current && !touched) {
        setLine(LOOP.waitStopped);
        return;
      }
      // A cancelled run is still a card's outcome (`pulled`), and the store
      // refuses a failed record that names no stop: every branch here names one.
      // `keySetup` and `setPin` are one step to the operator: a card that left
      // during either has no PIN the batch can rely on.
      // Widened back: `phase` is assigned inside callbacks TS cannot follow.
      const reached = phase as CardPhase;
      const keying = reached === 'keySetup' || reached === 'setPin';
      const stop: CardStop = keying ? 'pinNotSet' : 'pulled';
      try {
        actions.recordCard({
          ok: false,
          phase: reached,
          stop,
          pinPolicy: policy,
          pinState: keying ? 'notSet' : 'unknown',
          notesIssued: [],
          pubkey,
          reason:
            e instanceof Error ? e.message : String(e ?? 'the run stopped'),
        });
      } catch {
        // `recordCard` refuses a malformed row (an `ok` result with no key, a
        // stop it cannot name). That is a bug in this screen, not a card's
        // outcome, and it must not keep the loop from reporting the stop.
      }
      setStopShown(stop);
    } finally {
      setBusy(false);
      setWaiting(false);
      actions.setLoop({cardActive: false, loadProgress: null});
    }
  }, [actions, batch, batchPin, blockedBy, busy, config, reveal]);

  // The shown PIN never outlives the screen.
  useEffect(() => () => reveal.wipe(), [reveal]);

  // Once, on the first render after START: arm card 1 without a third press.
  const armed = useRef(false);
  useEffect(() => {
    if (!armFirst || armed.current || !batch || batch.records.length > 0) {
      return;
    }
    armed.current = true;
    onArmed?.();
    void runCard();
  }, [armFirst, batch, onArmed, runCard]);

  if (!batch || !config || !actions) {
    return null;
  }

  const {summary, stops, records} = batch;
  const loop = liveLoop;
  // The write's own progress, and 1 once the card is written. A batch with no
  // card on the phone yet shows a blank face.
  const figureFill =
    !busy && records.length > 0 && records[records.length - 1]?.ok
      ? 1
      : loop?.loadProgress
        ? loop.loadProgress.index / Math.max(1, loop.loadProgress.of)
        : 0;
  const figureState = stopShown
    ? 'danger'
    : !busy && records.length > 0 && records[records.length - 1]?.ok
      ? 'done'
      : busy
        ? 'running'
        : 'idle';
  const left = Math.max(0, config.count - records.length);
  const cardNumber = Math.min(records.length + 1, config.count);
  const finished = left === 0;
  const stoppedCount = summary.failed + summary.unspendable;
  // One status line under the card: the live phase while a card is on the
  // phone, the instruction otherwise. Never the same sentence twice.
  const status = busy
    ? line ?? (loop?.phase ? phaseMessage(loop.phase) : LOOP.tap)
    : finished
      ? LOOP.allDone
      : stopShown
        ? LOOP.tryNext
        : line ?? LOOP.tap;

  return (
    <Screen
      title={LOOP.title}
      headerRight={busy ? undefined : <GearButton onPress={onSettings} />}
      onStop={
        busy
          ? () => {
              cancelled.current = true;
              abort.current?.abort();
              setLine(LOOP.stopping);
            }
          : undefined
      }>
      <ScrollView contentContainerStyle={local.scroll} testID="loop-scroll">
        {/* The counter is the screen's headline: the batch's own denominator,
            with the run's real progress under it. */}
        <View style={local.head}>
          <Text testID="counter" style={[local.counter, {color: t.ink}]}>
            {LOOP.of(records.length, config.count)}
          </Text>
          <View style={[local.track, {backgroundColor: t.line}]}>
            <View
              style={[
                local.fill,
                {
                  backgroundColor: t.accent,
                  width: `${Math.round(
                    (records.length / Math.max(1, config.count)) * 100,
                  )}%`,
                },
              ]}
            />
          </View>
          <View style={local.tally}>
            <Tally color={t.accent} label={LOOP.programmed(summary.spendable)} />
            <Tally color={stoppedCount > 0 ? t.danger : t.dim} label={LOOP.stopped(stoppedCount)} />
          </View>
        </View>

        {/* The card itself, filling as the write's blocks land. The sweep is
            `loadProgress`, the block the card last acknowledged — never a
            timed estimate, so a slow card looks slow. */}
        <View style={local.figureWrap}>
          <CardFigure fill={figureFill} state={figureState} />
          {!finished ? (
            <Text style={[local.cardNumber, {color: t.dim}]}>
              {LOOP.next(cardNumber, config.count)}
            </Text>
          ) : null}
          <Text
            style={[local.phase, {color: t.ink}]}
            accessibilityLiveRegion="polite"
            testID="loop-status">
            {status}
          </Text>
        </View>

        {shownPin && !busy ? (
          <View
            style={[local.pinCard, {backgroundColor: t.card, borderColor: t.accent}]}
            testID="loop-pin">
            <View style={local.pinText}>
              <Text style={[local.pinLabel, {color: t.ink}]}>{LOOP.pinTitle(shownPin.n)}</Text>
              <Text style={[local.small, local.left, {color: t.dim}]}>{LOOP.pinBody}</Text>
            </View>
            <Text
              style={[local.pinValue, {color: t.accent}]}
              selectable={false}
              testID="loop-pin-value">
              {shownPin.pin}
            </Text>
          </View>
        ) : null}

        {blockedBy ? (
          <StopPanel
            title={LOOP.paused}
            name={STOP_NAMES[blockedBy]}
            what={CALM[blockedBy]}
            now={STATE[blockedBy]}
            testID="loop-blocked">
            <PrimaryButton
              title={LOOP.acknowledge}
              testID="acknowledge-stop"
              onPress={() => actions.acknowledgeStop(blockedBy)}
            />
          </StopPanel>
        ) : stopShown ? (
          <StopPanel
            title={LOOP.cardStopped}
            name={STOP_NAMES[stopShown]}
            what={CALM[stopShown]}
            now={STATE[stopShown]}
            testID="loop-stop"
          />
        ) : null}

        {records.length > 0 ? (
          <View style={local.section}>
            <SectionHeader>{LOOP.thisBatch}</SectionHeader>
            <Card testID="loop-record">
              {records.map((row, k) => (
                <React.Fragment key={row.id}>
                  {k > 0 ? <Divider /> : null}
                  <RecordLine row={row} n={k + 1} />
                </React.Fragment>
              ))}
            </Card>
          </View>
        ) : null}

        {stops.length > 0 ? (
          <View style={local.section}>
            <SectionHeader>{LOOP.stops}</SectionHeader>
            <Card testID="loop-stops">
              {stops.map((stop, k) => (
                <React.Fragment key={stop.kind}>
                  {k > 0 ? <Divider /> : null}
                  <View style={local.row}>
                    <View
                      style={[
                        local.dot,
                        {backgroundColor: stop.blocksBatch ? t.danger : t.dim},
                      ]}
                    />
                    <Text style={[local.rowLabel, {color: t.ink}]}>
                      {STOP_NAMES[stop.kind]}
                    </Text>
                    <Text style={[local.count, {color: t.dim}]}>{String(stop.cards)}</Text>
                  </View>
                </React.Fragment>
              ))}
            </Card>
          </View>
        ) : null}
      </ScrollView>

      <View style={local.footer}>
        {!finished && !blockedBy ? (
          <PrimaryButton
            title={busy ? (waiting ? LOOP.waiting : LOOP.keepStill) : LOOP.install(cardNumber)}
            onPress={() => {
              void runCard();
            }}
            disabled={busy}
            testID="install-card"
          />
        ) : null}
        {/* Opening the summary does not end the batch: the summary's own
            Close batch does, once the operator has read the numbers. */}
        {finished ? (
          <PrimaryButton title={LOOP.summary} onPress={onSummary} testID="end-batch" />
        ) : (
          <SecondaryButton
            title={LOOP.end}
            onPress={onSummary}
            disabled={busy}
            testID="end-batch"
          />
        )}
        {!finished && records.length > 0 && !busy ? (
          <Text style={[local.small, {color: t.dim}]}>{LOOP.ended(left)}</Text>
        ) : null}
      </View>
    </Screen>
  );
}

function Tally({color, label}: {color: string; label: string}) {
  const t = useTheme();
  return (
    <View style={local.tallyItem}>
      <View style={[local.dot, {backgroundColor: color}]} />
      <Text style={[local.small, {color: t.dim}]}>{label}</Text>
    </View>
  );
}

/** A stop, said the way the app's rule says it: what happened, what the card is now. */
function StopPanel({
  title,
  name,
  what,
  now,
  testID,
  children,
}: {
  title: string;
  name: string;
  what: string;
  now: string;
  testID: string;
  children?: React.ReactNode;
}) {
  const t = useTheme();
  return (
    <View
      style={[local.stop, {backgroundColor: t.card, borderColor: t.danger}]}
      testID={testID}
      accessibilityLiveRegion="assertive">
      <Text style={[local.stopEyebrow, {color: t.danger}]}>{title}</Text>
      <Text style={[local.stopName, {color: t.ink}]}>{name}</Text>
      <Text style={[local.stopBody, {color: t.ink}]}>{what}</Text>
      <Text style={[local.stopBody, {color: t.dim}]}>{now}</Text>
      {children}
    </View>
  );
}

/** One card's row: its number, its short id, and how it ended. */
function RecordLine({row, n}: {row: RecordRow; n: number}) {
  const t = useTheme();
  return (
    <View style={local.row}>
      <View style={[local.dot, {backgroundColor: row.ok ? t.accent : t.danger}]} />
      <Text style={[local.rowLabel, {color: t.ink}]}>{LOOP.cardRow(n, row.id)}</Text>
      <Text style={[local.count, {color: row.ok ? t.accent : t.danger}]}>
        {row.ok ? LOOP.programmedOne : STOP_NAMES[row.stop ?? 'refused']}
      </Text>
    </View>
  );
}

const local = StyleSheet.create({
  pinCard: {
    gap: 10,
    borderWidth: 1.5,
    borderRadius: 16,
    padding: 16,
  },
  pinText: {gap: 4},
  left: {textAlign: 'left'},
  pinLabel: {fontFamily: FONT.semibold, fontSize: 16},
  pinValue: {fontFamily: FONT.tabular, fontSize: 34, letterSpacing: 8},
  scroll: {gap: 18, paddingBottom: 20, flexGrow: 1},
  section: {gap: 10},
  head: {alignItems: 'stretch', gap: 10, marginTop: 2},
  counter: {
    fontFamily: FONT.semibold,
    fontVariant: ['tabular-nums'],
    fontSize: 46,
    letterSpacing: -1,
    textAlign: 'center',
  },
  track: {height: 6, borderRadius: 3, overflow: 'hidden'},
  fill: {height: '100%', borderRadius: 3},
  tally: {flexDirection: 'row', justifyContent: 'center', gap: 18},
  tallyItem: {flexDirection: 'row', alignItems: 'center', gap: 6},
  figureWrap: {
    alignItems: 'center',
    gap: 8,
    marginVertical: 8,
    flex: 1,
    justifyContent: 'center',
  },
  cardNumber: {
    fontFamily: FONT.semibold,
    fontSize: 13,
    letterSpacing: 1.6,
    textTransform: 'uppercase',
    marginTop: 10,
  },
  phase: {
    fontFamily: FONT.medium,
    fontSize: 18,
    lineHeight: 25,
    textAlign: 'center',
    minHeight: 50,
    paddingHorizontal: 12,
  },
  stop: {borderRadius: 16, borderWidth: 1.5, padding: 16, gap: 6},
  stopEyebrow: {
    fontFamily: FONT.semibold,
    fontSize: 12,
    letterSpacing: 1.2,
    textTransform: 'uppercase',
  },
  stopName: {fontFamily: FONT.semibold, fontSize: 19, marginBottom: 2},
  stopBody: {fontFamily: FONT.regular, fontSize: 15, lineHeight: 21},
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: 10,
    paddingHorizontal: 16,
    paddingVertical: 13,
  },
  dot: {width: 8, height: 8, borderRadius: 4},
  rowLabel: {flex: 1, fontFamily: FONT.medium, fontSize: 15},
  count: {fontFamily: FONT.medium, fontSize: 14},
  small: {fontFamily: FONT.regular, fontSize: 13, lineHeight: 19, textAlign: 'center'},
  footer: {paddingTop: 10, paddingBottom: 8, gap: 10},
});
