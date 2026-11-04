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
import {CardFigure} from '../CardFigure';
import {phaseMessage} from '../session';
import type {CardStop} from '../store';
import {CALM, STATE} from '../strings';
import {STOP_NAMES} from './LoopScreen';
import {
  Card,
  Divider,
  EntryRow,
  FONT,
  PrimaryButton,
  Screen,
  SecondaryButton,
  SectionHeader,
  useTheme,
} from '../ui';

const COPY = {
  title: 'Demo',
  intro: 'Pick a scenario. Each one runs the real install engine against a simulated card.',
  help:
    'The demo programs a simulated card with the same engine the loop uses on a real one: no reader, no network, and nothing recorded to the registry. Each scenario is a situation an operator meets at the table, and ends in the words the loop would show.',
  badge: 'Simulated card',
  starting: 'Starting…',
  programmed: 'Card programmed',
  programmedBody: 'Installed, keyed and checked. On a real card this is where the next one goes on.',
  stopped: 'Card stopped',
  noCard: 'Nothing was sent to a card.',
  apdus: (n: number) => `${n} card commands exchanged`,
  again: 'Run again',
  back: 'Back to the scenarios',
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
  const [selected, setSelected] = useState<DemoStateId | null>(null);
  const [outcome, setOutcome] = useState<Outcome>(null);
  const [busy, setBusy] = useState(false);
  const [line, setLine] = useState<string>(COPY.starting);
  const [fill, setFill] = useState(0);

  const run = useCallback(async (state: DemoStateId) => {
    setBusy(true);
    setOutcome(null);
    setFill(0);
    setLine(COPY.starting);
    try {
      // `speed` is left at 1: the demo runs in about ten seconds and the wait
      // is the engine's own work, not an animation's. The figure and the line
      // follow the engine's events exactly as the loop's do.
      const result = await runDemo(state, {
        onEvent: event => {
          if (event.type === 'phase') {
            setLine(phaseMessage(event.phase));
          } else if (event.type === 'block') {
            setFill(event.index / Math.max(1, event.of));
            setLine(`Writing ${Math.round((event.index / event.of) * 100)}%`);
          }
        },
      });
      setOutcome({state, run: result});
      if (stopFor(result) === 'ok') {
        setFill(1);
      }
    } finally {
      setBusy(false);
    }
  }, []);

  if (selected === null) {
    return (
      <Screen title={COPY.title} onBack={onBack}>
        <ScrollView contentContainerStyle={local.scroll} testID="demo-states">
          <SectionHeader help={COPY.help} testID="demo-help">
            {COPY.badge}
          </SectionHeader>
          <Text style={[local.intro, {color: t.dim}]}>{COPY.intro}</Text>
          <Card>
            {DEMO_STATES.map((state, k) => (
              <React.Fragment key={state.id}>
                {k > 0 ? <Divider /> : null}
                <EntryRow
                  title={state.name}
                  subtitle={state.sub}
                  testID={`demo-state-${state.id}`}
                  onPress={() => {
                    setSelected(state.id);
                    void run(state.id);
                  }}
                />
              </React.Fragment>
            ))}
          </Card>
        </ScrollView>
      </Screen>
    );
  }

  const state = DEMO_STATES.find(d => d.id === selected);
  const stop = outcome ? stopFor(outcome.run) : null;
  const shown = outcome?.run.result ?? null;
  const loopStop = stop && stop !== 'ok' ? cardStop(stop) : null;
  const figureState = busy ? 'running' : stop === 'ok' ? 'done' : stop ? 'danger' : 'idle';
  const leave = () => {
    if (busy) {
      return;
    }
    setSelected(null);
    setOutcome(null);
  };

  return (
    <Screen title={COPY.title} onBack={busy ? undefined : leave}>
      <ScrollView contentContainerStyle={local.scroll} testID="demo-result">
        <View style={local.head}>
          <Text style={[local.badge, {color: t.accent, borderColor: t.accent}]}>
            {COPY.badge}
          </Text>
          <Text style={[local.name, {color: t.ink}]}>{state?.name}</Text>
          <Text style={[local.intro, {color: t.dim}]}>{state?.sub}</Text>
        </View>
        <View style={local.figure}>
          <CardFigure fill={fill} state={figureState} />
          <Text
            style={[local.line, {color: t.ink}]}
            accessibilityLiveRegion="polite"
            testID="demo-running">
            {busy || !outcome ? line : ''}
          </Text>
        </View>
        {outcome && stop === 'ok' ? (
          <View
            style={[local.panel, {backgroundColor: t.card, borderColor: t.accent}]}
            testID="demo-stop">
            <Text style={[local.panelTitle, {color: t.accent}]}>{COPY.programmed}</Text>
            <Text style={[local.body, {color: t.ink}]}>{COPY.programmedBody}</Text>
            {shown ? (
              <Text style={[local.body, {color: t.dim}]} testID="demo-apdu-count">
                {COPY.apdus(shown.apdus.length)}
              </Text>
            ) : null}
          </View>
        ) : null}
        {outcome && loopStop ? (
          <View
            style={[local.panel, {backgroundColor: t.card, borderColor: t.danger}]}
            testID="demo-stop">
            <Text style={[local.eyebrow, {color: t.danger}]}>{COPY.stopped}</Text>
            <Text style={[local.panelTitle, {color: t.ink}]}>{STOP_NAMES[loopStop]}</Text>
            {outcome.run.sessionError ? (
              <Text style={[local.body, {color: t.ink}]} testID="demo-session-error">
                {`${outcome.run.sessionError} ${COPY.noCard}`}
              </Text>
            ) : (
              <>
                <Text style={[local.body, {color: t.ink}]}>{CALM[loopStop]}</Text>
                <Text style={[local.body, {color: t.dim}]}>{STATE[loopStop]}</Text>
              </>
            )}
            {shown ? (
              <Text style={[local.body, {color: t.dim}]} testID="demo-apdu-count">
                {COPY.apdus(shown.apdus.length)}
              </Text>
            ) : null}
          </View>
        ) : null}
      </ScrollView>
      <View style={local.footer}>
        <PrimaryButton
          title={busy ? line : COPY.again}
          disabled={busy}
          onPress={() => void run(selected)}
          testID="demo-run"
        />
        <SecondaryButton title={COPY.back} disabled={busy} onPress={leave} testID="demo-back" />
      </View>
    </Screen>
  );
}

const local = StyleSheet.create({
  scroll: {gap: 18, paddingTop: 4, paddingBottom: 20, flexGrow: 1},
  intro: {fontFamily: FONT.regular, fontSize: 14, lineHeight: 20},
  head: {gap: 6},
  badge: {
    alignSelf: 'flex-start',
    fontFamily: FONT.semibold,
    fontSize: 11,
    letterSpacing: 1.2,
    textTransform: 'uppercase',
    borderWidth: 1,
    borderRadius: 6,
    paddingHorizontal: 8,
    paddingVertical: 3,
    overflow: 'hidden',
  },
  name: {fontFamily: FONT.semibold, fontSize: 24, letterSpacing: -0.4},
  figure: {alignItems: 'center', gap: 14, flex: 1, justifyContent: 'center', marginVertical: 8},
  line: {fontFamily: FONT.medium, fontSize: 18, textAlign: 'center', minHeight: 26},
  panel: {borderRadius: 16, borderWidth: 1.5, padding: 16, gap: 6},
  eyebrow: {fontFamily: FONT.semibold, fontSize: 12, letterSpacing: 1.2, textTransform: 'uppercase'},
  panelTitle: {fontFamily: FONT.semibold, fontSize: 19},
  body: {fontFamily: FONT.regular, fontSize: 15, lineHeight: 21},
  footer: {gap: 10, paddingBottom: 8, paddingTop: 6},
});
