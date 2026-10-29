/**
 * The demo: the whole install loop, on a simulated card, with no reader and no
 * registry. This is the App Review path (plan § 7, Guideline 2.1) — a reviewer
 * cannot program a J3R180, so without this screen the app cannot be reviewed at
 * all and is rejected for incompleteness.
 *
 * It is a first-class screen, not a debug affordance: it ships in release
 * builds and is reachable from Settings for everyone. Nothing here touches a
 * card, the network, or the store's batch machine — `runDemo` drives the real
 * `provision` engine over `fakeCard`, which is why what the reviewer sees is
 * the engine's own behaviour rather than a scripted animation.
 *
 * Why it is not wired into `LoopScreen`: the loop's job is one real card per
 * tap, and threading a simulation through it would put a branch in the path
 * that programs hardware. The demo shows the same stops in the same words
 * (`STOP_NAMES`, imported from the loop) and reports the same numbers, from the
 * same engine.
 */
import React, {useCallback, useState} from 'react';
import {ScrollView, StyleSheet, Text, View} from 'react-native';

import {
  DEMO_STATES,
  runDemo,
  stopFor,
  type DemoRun,
  type DemoStateId,
  type DemoStop,
} from '../demo/runDemo';
import type {CardStop} from '../store';
import {STOP_NAMES} from './LoopScreen';
import {
  Card,
  Dim,
  EntryRow,
  PrimaryButton,
  Screen,
  styles,
  useTheme,
} from '../ui';

const COPY = {
  title: 'Demo',
  sub: 'Install, key and load a simulated card. No reader, no network, nothing recorded.',
  run: 'RUN THIS DEMO',
  running: 'Running…',
  again: 'RUN AGAIN',
  result: 'Result',
  programmed: 'Card programmed',
  stopped: 'Stopped',
  noCard: 'Nothing was sent to a card.',
  apdus: 'APDUs exchanged',
  back: 'BACK TO THE LIST',
  card: 'SIMULATED CARD',
} as const;

/** A finished run, or null before the first tap. */
type Outcome = {state: DemoStateId; run: DemoRun} | null;

/**
 * The demo's stop in the loop's vocabulary. `foreign`, `hasOld` and
 * `oldVersion` are demo scenarios, not engine stops: each ends in a
 * `refused`, which is what the loop would show for them on a real card.
 */
function cardStop(stop: DemoStop): CardStop {
  switch (stop) {
    case 'ok':
      return 'guarded'; // unreachable: callers branch on 'ok' first
    case 'foreign':
    case 'hasOld':
    case 'oldVersion':
      // The card is not one this app programs, or is one it would reinstall:
      // both end in the engine's own refusal on a real tap.
      return 'refused';
    case 'noSession':
      // No session was opened, so nothing reached a card; `refused` is the
      // loop's word for a run that ended before it could program.
      return 'refused';
    default:
      return stop;
  }
}

export function DemoScreen({onBack}: {onBack: () => void}) {
  const t = useTheme();
  const s = styles(t);
  const [selected, setSelected] = useState<DemoStateId | null>(null);
  const [outcome, setOutcome] = useState<Outcome>(null);
  const [busy, setBusy] = useState(false);

  const run = useCallback(async (state: DemoStateId) => {
    setBusy(true);
    setOutcome(null);
    try {
      // `speed` is left at 1: the demo runs in about ten seconds and the wait
      // is the engine's own work, not an animation's.
      const result = await runDemo(state);
      setOutcome({state, run: result});
    } finally {
      setBusy(false);
    }
  }, []);

  if (selected === null) {
    return (
      <Screen title={COPY.title} onBack={onBack}>
        <ScrollView contentContainerStyle={local.scroll} testID="demo-states">
          <Text style={s.note}>{COPY.sub}</Text>
          <Card>
            {DEMO_STATES.map(state => (
              <EntryRow
                key={state.id}
                title={state.name}
                subtitle={state.sub}
                testID={`demo-state-${state.id}`}
                onPress={() => {
                  setSelected(state.id);
                  setOutcome(null);
                  void run(state.id);
                }}
              />
            ))}
          </Card>
        </ScrollView>
      </Screen>
    );
  }

  const state = DEMO_STATES.find(d => d.id === selected);
  const stop = outcome ? stopFor(outcome.run) : null;
  const shown = outcome?.run.result ?? null;

  return (
    <Screen
      title={state?.name ?? COPY.title}
      onBack={() => {
        setSelected(null);
        setOutcome(null);
      }}>
      <ScrollView contentContainerStyle={local.scroll} testID="demo-result">
        <Text style={s.note}>{state?.sub}</Text>

        {busy ? (
          <Text style={s.note} testID="demo-running">
            {COPY.running}
          </Text>
        ) : null}

        {outcome && stop ? (
          <Card>
            <View style={local.field}>
              <Dim>{COPY.card}</Dim>
              <Text style={s.rowLabel}>{COPY.result}</Text>
            </View>
            <View style={local.field}>
              <Dim>{stop === 'ok' ? COPY.programmed : COPY.stopped}</Dim>
              <Text
                style={[
                  s.rowLabel,
                  stop === 'ok' ? {color: t.accent} : s.failed,
                ]}
                testID="demo-stop">
                {stop === 'ok'
                  ? COPY.programmed
                  : STOP_NAMES[cardStop(stop)]}
              </Text>
            </View>
            {outcome.run.sessionError ? (
              <View style={local.field}>
                <Dim testID="demo-session-error">
                  {outcome.run.sessionError}
                </Dim>
                <Text style={s.note}>{COPY.noCard}</Text>
              </View>
            ) : null}
            {shown ? (
              <View style={local.field}>
                <Dim>{COPY.apdus}</Dim>
                <Text testID="demo-apdu-count">{String(shown.apdus.length)}</Text>
              </View>
            ) : null}
          </Card>
        ) : null}

        <View style={local.field}>
          <PrimaryButton
            title={outcome ? COPY.again : COPY.run}
            disabled={busy}
            onPress={() => void run(selected)}
            testID="demo-run"
          />
        </View>
      </ScrollView>
    </Screen>
  );
}

const local = StyleSheet.create({
  scroll: {padding: 16, gap: 16},
  field: {gap: 4},
});
