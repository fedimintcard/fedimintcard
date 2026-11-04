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
 * (`session.ts` → `provision`). `keySetup` and `setPin` then run on the local
 * sidecar, which drives the card over the relay after the install session
 * closes: a freshly installed applet has eight empty slots, and only a host can
 * ask the card to generate its keys. `issue` hands the card its notes.
 *
 * The screen computes no card state. Every outcome goes to
 * `actions.recordCard`, the only way card state enters the store; the counter,
 * the stop list and the acknowledgement gate come back from `useBatch()`.
 *
 * A per-card PIN is generated here and written to `pin.ts`'s reveal slot for
 * the summary's one showing; a batch PIN is read from the batch-PIN slot and
 * never copied. Neither value goes to the store, the registry or a log.
 */
import React, {useCallback, useEffect, useRef, useState} from 'react';
import {ScrollView, StyleSheet, Text, View} from 'react-native';

import {CardFigure} from '../CardFigure';

import {LocalClient} from '../api';
import {FEDIMINT_PLAN, provision} from '../cardkit';
import {generatePin, useBatchPin, usePinReveal} from '../pin';
import {LOCAL_SIDECAR_ORIGIN} from '../settings';
import {phaseMessage, withCard} from '../session';
import {CALM, LOOP, STATE} from '../strings';
import {
  useLoop,
  type CardPhase,
  type CardStop,
  type RecordRow,
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
  issueFailed: 'Top-up failed',
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
  const api = useRef<LocalClient | null>(null);

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
    const sidecar =
      api.current ??
      (api.current = new LocalClient({baseUrl: LOCAL_SIDECAR_ORIGIN}));
    cancelled.current = false;
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
    try {
      const {value: installed} = await withCard(
        phaseMessage('probe'),
        tx =>
          provision(tx, FEDIMINT_PLAN, {
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
          }),
        Date.now,
        controller.signal,
      );
      if (cancelled.current) {
        throw new Error('CardSessionCancelled');
      }
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

      phase = 'keySetup';
      actions.setLoop({phase, cardActive: false, loadProgress: null});
      setLine('Setting up keys — keep the card against the phone');
      // The PIN goes in this request body and nowhere else. The sidecar forwards
      // it to the card as SET_PIN and keeps nothing.
      const keyed = await sidecar.keySetup({pin, replace: false});
      if (!keyed.pin_set) {
        actions.recordCard({
          ok: false,
          phase: 'setPin',
          stop: 'pinNotSet',
          pinPolicy: policy,
          pinState: 'notSet',
          notesIssued: [],
          reason: 'SET_PIN answered without 9000',
        });
        setStopShown('pinNotSet');
        return;
      }
      phase = 'setPin';
      actions.setLoop({phase});
      if (policy === 'perCard') {
        // Shown once, on the summary, keyed by the row it belongs to.
        reveal.grant(keyed.short_id, pin);
      }

      // The row's id comes from the card's key, so a key setup that reports no
      // key cannot produce a record at all: stop rather than invent an id.
      const first = keyed.keys[0];
      if (!first) {
        actions.recordCard({
          ok: false,
          phase: 'keySetup',
          stop: 'refused',
          pinPolicy: policy,
          pinState: 'notSet',
          notesIssued: [],
          reason: 'key setup reported no card key',
        });
        setStopShown('refused');
        return;
      }
      pubkey = Array.from(
        {length: first.pubkey.length / 2},
        (_, i) => parseInt(first.pubkey.slice(i * 2, i * 2 + 2), 16),
      );

      phase = 'issue';
      actions.setLoop({phase});
      setLine('Loading notes onto the card');
      let notes: number[] = [];
      if (config.plan && config.plan.amounts.length > 0) {
        // One key per tap, minted here and never reused for a second attempt:
        // a repeat with the same key returns the same result.
        const issued = await sidecar.issue({
          amounts_msat: config.plan.amounts.map((n: number) => n * 1000),
          client_ref: `${keyed.short_id}:${Date.now()}`,
        });
        notes = issued.notes.map(n => Math.floor(n.msat / 1000));
      }
      phase = 'done';
      actions.recordCard({
        ok: true,
        phase,
        pinPolicy: policy,
        pinState: 'set',
        notesIssued: notes,
        pubkey,
      });
      setLine(`Done · ${keyed.short_id.slice(-4)}`);
    } catch (e) {
      // Stopped while waiting, before any card answered: no card was tried, so
      // nothing is recorded and the batch's count is untouched.
      if (cancelled.current && !touched) {
        setLine(LOOP.waitStopped);
        return;
      }
      // A cancelled run is still a card's outcome (`pulled`), and the store
      // refuses a failed record that names no stop: every branch here names one.
      // `keySetup` and `setPin` are one step to the operator: a thrown
      // key setup is a card whose PIN is not on it.
      const stop: CardStop =
        phase === 'keySetup' || phase === 'setPin'
          ? 'pinNotSet'
          : phase === 'issue'
          ? 'issueFailed'
          : 'pulled';
      try {
        actions.recordCard({
          ok: false,
          phase,
          stop,
          pinPolicy: policy,
          pinState:
            phase === 'keySetup' || phase === 'setPin' ? 'notSet' : 'unknown',
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
      reveal.wipe();
    }
  }, [actions, batch, batchPin, blockedBy, busy, config, reveal]);

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
  const stoppedCount = summary.failed + summary.empty + summary.unspendable;
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
