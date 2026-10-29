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
import React, {useCallback, useRef, useState} from 'react';
import {ScrollView, StyleSheet, Text, View} from 'react-native';

import {LocalClient} from '../api';
import {FEDIMINT_PLAN, provision} from '../cardkit';
import {generatePin, useBatchPin, usePinReveal} from '../pin';
import {LOCAL_SIDECAR_ORIGIN} from '../settings';
import {phaseMessage, withCard} from '../session';
import {LOOP} from '../strings';
import {
  type CardPhase,
  type CardStop,
  type RecordRow,
  useBatch,
} from '../store';
import {
  Card,
  Dim,
  FONT,
  PrimaryButton,
  Screen,
  SectionHeader,
  SecondaryButton,
  styles,
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
  onSummary,
  onSettings,
}: {
  onSummary: () => void;
  onSettings: () => void;
}) {
  const batch = useBatch();
  const batchPin = useBatchPin();
  const reveal = usePinReveal();
  const t = useTheme();
  const s = styles(t);

  const [busy, setBusy] = useState(false);
  const [line, setLine] = useState<string | null>(null);
  const [stopShown, setStopShown] = useState<CardStop | null>(null);
  const cancelled = useRef(false);
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
      actions.setLoop({cardActive: false, loadProgress: null});
      reveal.wipe();
    }
  }, [actions, batch, batchPin, blockedBy, busy, config, reveal]);

  if (!batch || !config || !actions) {
    return null;
  }
  const {summary, stops, records} = batch;
  const left = Math.max(0, config.count - records.length);
  const cardNumber = Math.min(records.length + 1, config.count);

  return (
    <Screen
      title={LOOP.title}
      onBack={onSettings}
      onStop={
        busy
          ? () => {
              cancelled.current = true;
              setLine('Stopping…');
            }
          : undefined
      }>
      <ScrollView contentContainerStyle={local.scroll} testID="loop-scroll">
        <View style={local.progress}>
          <Text style={[local.progressText, {color: t.ink}]}>
            {LOOP.progress(records.length, config.count)}
          </Text>
          <View style={[local.track, {backgroundColor: t.line}]}>
            <View
              style={[
                local.fill,
                {
                  backgroundColor: t.accent,
                  width: `${Math.round((records.length / config.count) * 100)}%`,
                },
              ]}
            />
          </View>
          <Dim>
            {`${summary.spendable} spendable · ${summary.failed +
              summary.empty +
              summary.unspendable} stopped`}
          </Dim>
        </View>

        {blockedBy ? (
          <Card testID="loop-blocked">
            <View style={local.panel}>
              <Text style={[local.panelTitle, {color: t.danger}]}>
                Batch paused
              </Text>
              <Dim>{STOP_NAMES[blockedBy]} holds this batch until you say.</Dim>
              <PrimaryButton
                title="ACKNOWLEDGE AND CONTINUE"
                testID="acknowledge-stop"
                onPress={() => actions.acknowledgeStop(blockedBy)}
              />
            </View>
          </Card>
        ) : (
          <>
            <Card testID="loop-card">
              <View style={local.panel}>
                <Text style={[local.cardNumber, {color: t.ink}]}>
                  {LOOP.next(cardNumber, config.count)}
                </Text>
                <Dim>{line ?? LOOP.tap}</Dim>
              </View>
            </Card>

            {stopShown ? (
              <Card testID="loop-stop">
                <View style={local.panel}>
                  <Text style={[local.panelTitle, {color: t.danger}]}>
                    Card stopped
                  </Text>
                  <Dim>{STOP_NAMES[stopShown]}</Dim>
                </View>
              </Card>
            ) : null}

            <PrimaryButton
              title={busy ? 'KEEP THE CARD STILL…' : LOOP.install}
              onPress={() => {
                void runCard();
              }}
              disabled={busy}
              testID="install-card"
            />
            {busy ? (
              <Text style={s.note} accessibilityLiveRegion="polite">
                {LOOP.busy}
              </Text>
            ) : null}
          </>
        )}

        <SectionHeader>Stops</SectionHeader>
        <Card testID="loop-stops">
          {stops.length === 0 ? (
            <View style={local.panel}>
              <Dim>No stops yet.</Dim>
            </View>
          ) : (
            stops.map(stop => (
              <View key={stop.kind} style={local.row}>
                <View
                  style={[
                    local.dot,
                    {backgroundColor: stop.blocksBatch ? t.danger : t.dim},
                  ]}
                />
                <Text style={[local.rowLabel, {color: t.ink}]}>
                  {STOP_NAMES[stop.kind]}
                </Text>
                <Text style={[local.count, {color: t.dim}]}>
                  {String(stop.cards)}
                </Text>
              </View>
            ))
          )}
        </Card>

        <SectionHeader>Cards this batch</SectionHeader>
        <Card testID="loop-record">
          {records.length === 0 ? (
            <View style={local.panel}>
              <Dim>No cards this batch yet.</Dim>
            </View>
          ) : (
            records.map(row => <RecordLine key={row.id} row={row} />)
          )}
        </Card>
      </ScrollView>

      <View style={local.footer}>
        {/* Opening the summary does not end the batch: the summary's own
            CLOSE BATCH does, once the operator has read the numbers. */}
        <SecondaryButton
          title={left === 0 ? LOOP.summary : LOOP.end}
          onPress={onSummary}
          testID="end-batch"
        />
        <Text style={s.note}>{LOOP.ended(left)}</Text>
      </View>
    </Screen>
  );
}

/** One card's row: its short id, and how it ended. */
function RecordLine({row}: {row: RecordRow}) {
  const t = useTheme();
  return (
    <View style={local.row}>
      <View
        style={[local.dot, {backgroundColor: row.ok ? t.accent : t.danger}]}
      />
      <Text style={[local.rowLabel, {color: t.ink}]}>{`…${row.id}`}</Text>
      <Text style={[local.count, {color: t.dim}]}>
        {row.ok ? 'programmed' : STOP_NAMES[row.stop ?? 'refused']}
      </Text>
    </View>
  );
}

const local = StyleSheet.create({
  scroll: {gap: 12, paddingBottom: 24},
  progress: {gap: 8},
  progressText: {fontFamily: FONT.semibold, fontSize: 22},
  track: {height: 4, borderRadius: 2, overflow: 'hidden'},
  fill: {height: '100%'},
  panel: {padding: 16, gap: 10},
  panelTitle: {fontFamily: FONT.semibold, fontSize: 18},
  cardNumber: {fontFamily: FONT.semibold, fontSize: 20},
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: 10,
    paddingHorizontal: 16,
    paddingVertical: 12,
  },
  dot: {width: 8, height: 8, borderRadius: 4},
  rowLabel: {flex: 1, fontFamily: FONT.regular, fontSize: 14},
  count: {
    fontFamily: FONT.tabular,
    fontVariant: ['tabular-nums'],
    fontSize: 13,
  },
  footer: {paddingTop: 10, paddingBottom: 8, gap: 8},
});
