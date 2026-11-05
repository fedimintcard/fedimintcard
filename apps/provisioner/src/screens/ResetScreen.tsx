/**
 * Settings → Reset a card (OPS-PLAN O5). Two taps: the first reads which card
 * it is and asks ops whether it may go; the second, once the holder has typed
 * their PIN (or the operator has confirmed the blocked-PIN warning), takes the
 * card back to factory: applet deleted, factory keys, both proved. The card
 * logic is `reset.ts`; this screen holds the holder's PIN for that one tap and
 * keeps nothing.
 *
 * An unfinished reset (the journal, `reset.ts`) is offered first: a card part
 * way through has no applet to say which card it was, so only the journal can
 * finish it. Finished resets ops has not acknowledged (the outbox) are sent
 * when the screen opens and after each reset, and on the operator's button;
 * they never block the operator, since the card itself is done.
 */
import AsyncStorage from '@react-native-async-storage/async-storage';
import React, {useCallback, useEffect, useRef, useState} from 'react';
import {ScrollView, StyleSheet, Text, TextInput, View} from 'react-native';

import type {ResetCheck} from '../ops';
import {PIN_MAX, usePinDraft} from '../pin';
import {
  clearJournal,
  flushOutbox,
  identifyCard,
  readJournal,
  readOutbox,
  resetTap,
  type JournalStorage,
  type PinStatus,
  type ResetJournal,
  type ResetPhase,
  type ResetStop,
} from '../reset';
import {CANCELLED, createCardSession} from '../session';
import {RESET} from '../strings';
import {formatShortId, useStore} from '../store';
import {
  Card,
  Divider,
  FONT,
  PrimaryButton,
  Screen,
  SectionHeader,
  SecondaryButton,
  ValueRow,
  styles,
  useTheme,
} from '../ui';

const journal: JournalStorage = AsyncStorage;

type Step =
  | {kind: 'start'}
  | {kind: 'review'; shortId: string; pin: PinStatus; check: ResetCheck}
  | {kind: 'done'; oldId: string; recorded: boolean; stranded: number};

/** One line for a stop, and whether the card may now be wiped. */
function stopLine(stop: ResetStop): string {
  switch (stop.kind) {
    case 'wrongPin':
      return RESET.stops.wrongPin(stop.triesLeft);
    case 'notResettable':
      return `${RESET.stops.notResettable}: ${stop.reasons.join('; ')}`;
    case 'differentCard':
      return `${RESET.stops.differentCard}: ${stop.reason}`;
    case 'pinBlocked':
      return RESET.blockedTitle(stop.pin);
    default: {
      const reason = 'reason' in stop && stop.reason ? ` (${stop.reason})` : '';
      return `${RESET.stops[stop.kind]}${reason}`;
    }
  }
}

export function ResetScreen({onBack}: {onBack: () => void}) {
  const {session, ops} = useStore();
  const t = useTheme();
  const s = styles(t);
  const holderPin = usePinDraft();
  const [step, setStep] = useState<Step>({kind: 'start'});
  const [saved, setSaved] = useState<ResetJournal | null>(null);
  /** This operator's finished resets ops has not acknowledged. */
  const [waiting, setWaiting] = useState(0);
  const [busy, setBusy] = useState(false);
  const [line, setLine] = useState<string | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [blockedConfirmed, setBlockedConfirmed] = useState(false);
  const [confirmForget, setConfirmForget] = useState(false);
  const card = useRef(createCardSession()).current;
  const operatorId = session?.operatorId ?? -1;

  const reload = useCallback(async () => {
    setSaved(await readJournal(journal).catch(() => null));
    const outbox = await readOutbox(journal).catch(() => []);
    setWaiting(outbox.filter(p => p.operator_id === operatorId).length);
  }, [operatorId]);

  /**
   * Sends the outbox to ops. Answers whether `oldId` (when given) is now
   * recorded, and what ops still held under it.
   */
  const send = useCallback(
    async (oldId?: string): Promise<{recorded: boolean; stranded: number}> => {
      const flushed = await flushOutbox(ops, journal, operatorId).catch(e => ({
        recorded: [],
        failed: {
          short_id: '',
          reason: e instanceof Error ? e.message : String(e),
        },
        left: [],
      }));
      await reload();
      if (flushed.failed) {
        setProblem(flushed.failed.reason);
      }
      const mine = flushed.recorded.find(r => r.short_id === oldId);
      return {
        recorded: oldId !== undefined && mine !== undefined,
        stranded: mine?.stranded_notes ?? 0,
      };
    },
    [ops, operatorId, reload],
  );

  useEffect(() => {
    void reload().then(async () => {
      // A completion left from an earlier visit goes to ops now; the card needs nothing.
      const outbox = await readOutbox(journal).catch(() => []);
      if (outbox.some(p => p.operator_id === operatorId)) {
        await send();
      }
    });
  }, [reload, send, operatorId]);
  // Leaving the screen ends any tap and drops the holder's PIN.
  useEffect(
    () => () => {
      void card.cancel();
    },
    [card],
  );

  const leave = () => {
    holderPin.wipe();
    onBack();
  };

  /** Tap 1: which card, and ops' answer for it. Writes nothing. */
  const read = async () => {
    setBusy(true);
    setProblem(null);
    setLine(RESET.reading);
    try {
      const {value: id} = await card.run(RESET.reading, identifyCard);
      if (!id.ok) {
        setProblem(RESET.identify[id.why]);
        return;
      }
      const check = await ops.resetCheck(id.shortId);
      setBlockedConfirmed(false);
      holderPin.clear();
      setStep({kind: 'review', shortId: id.shortId, pin: id.pin, check});
    } catch (e) {
      // api.ts's refusal, told apart by name: this screen imports no network module.
      const status = (e as {status?: unknown} | null)?.status;
      setProblem(
        e instanceof Error && e.name === 'ApiServiceError' && status === 404
          ? RESET.notYours
          : e instanceof Error && e.message !== CANCELLED
          ? e.message
          : RESET.noCard,
      );
    } finally {
      setBusy(false);
      setLine(null);
    }
  };

  /** The operator's retry from the done step: the outbox again. */
  const record = async (oldId: string) => {
    setBusy(true);
    setProblem(null);
    try {
      const sent = await send(oldId);
      setStep({kind: 'done', oldId, ...sent});
    } finally {
      setBusy(false);
    }
  };

  /** Tap 2, or the tap that finishes a journalled reset (`expected` null). */
  const reset = async (expected: string | null) => {
    setBusy(true);
    setProblem(null);
    setLine(RESET.resetting);
    try {
      const {value: r} = await card.run(RESET.resetting, tx =>
        resetTap(tx, {
          operatorId,
          expected,
          holderPin:
            expected !== null && step.kind === 'review' && step.pin === 'set'
              ? holderPin.value
              : null,
          blockedConfirmed,
          ops,
          journal,
          random: n => Array.from({length: n}, () => (Math.random() * 256) | 0),
          now: Date.now,
          onPhase: (p: ResetPhase) => setLine(RESET.phase[p]),
        }),
      );
      holderPin.wipe();
      if (r.ok) {
        setLine(RESET.phase.check);
        const oldId = r.completion.short_id;
        const sent = await send(oldId);
        setStep({kind: 'done', oldId, ...sent});
        return;
      }
      if (r.stop.kind === 'pinRequired' && saved) {
        // The journalled card still has its applet: start over with the holder's PIN.
        await clearJournal(journal);
        await reload();
      }
      setProblem(
        `${stopLine(r.stop)}. ${
          r.resumable ? RESET.resumable : RESET.untouched
        }`,
      );
      await reload();
    } catch (e) {
      holderPin.wipe();
      setProblem(
        e instanceof Error && e.message !== CANCELLED
          ? `${RESET.stops.pulled}. ${RESET.untouched}`
          : RESET.noCard,
      );
      await reload();
    } finally {
      setBusy(false);
      setLine(null);
    }
  };

  const ownJournal = saved !== null && saved.operator_id === operatorId;
  const review = step.kind === 'review' ? step : null;
  const canReset =
    review !== null &&
    review.check.resettable &&
    (review.pin === 'set' ? holderPin.valid : blockedConfirmed);

  return (
    <Screen
      title={RESET.title}
      onBack={busy ? undefined : leave}
      onStop={busy ? () => void card.cancel() : undefined}>
      <ScrollView
        contentContainerStyle={local.scroll}
        keyboardShouldPersistTaps="handled"
        testID="reset-scroll">
        {saved && step.kind !== 'done' ? (
          <View
            style={[
              local.panel,
              {backgroundColor: t.card, borderColor: t.danger},
            ]}
            testID="reset-unfinished">
            <Text style={[local.body, {color: t.ink}]}>
              {ownJournal
                ? RESET.unfinished(saved.old_short_id)
                : RESET.unfinishedOther}
            </Text>
            {ownJournal ? (
              <PrimaryButton
                title={RESET.finish}
                disabled={busy}
                onPress={() => void reset(null)}
                testID="reset-finish"
              />
            ) : null}
            {confirmForget ? (
              <>
                <Text style={[local.small, {color: t.dim}]}>
                  {RESET.forgetConfirm}
                </Text>
                <PrimaryButton
                  title={RESET.forget}
                  danger
                  onPress={() => {
                    setConfirmForget(false);
                    void clearJournal(journal).then(reload);
                  }}
                  testID="reset-forget-confirm"
                />
              </>
            ) : (
              <SecondaryButton
                title={RESET.forget}
                disabled={busy}
                onPress={() => setConfirmForget(true)}
                testID="reset-forget"
              />
            )}
          </View>
        ) : null}

        {waiting > 0 && step.kind !== 'done' ? (
          <View style={local.section} testID="reset-waiting">
            <Text style={[local.body, {color: t.dim}]}>
              {RESET.waiting(waiting)}
            </Text>
            <SecondaryButton
              title={RESET.record}
              disabled={busy}
              onPress={() => {
                setProblem(null);
                void send();
              }}
              testID="reset-record"
            />
          </View>
        ) : null}

        {step.kind === 'start' && !saved ? (
          <View style={local.section}>
            <SectionHeader help={RESET.help} testID="reset-help">
              {RESET.card}
            </SectionHeader>
            <Text style={[local.body, {color: t.dim}]}>{RESET.intro}</Text>
          </View>
        ) : null}

        {review ? (
          <>
            <View style={local.section}>
              <SectionHeader help={RESET.help} testID="reset-help">
                {RESET.card}
              </SectionHeader>
              <Card testID="reset-review">
                <ValueRow
                  label={RESET.card}
                  value={`…${formatShortId(review.shortId).slice(-4)}`}
                  valueTone="ink"
                />
                <Divider />
                <ValueRow
                  label={RESET.pinState}
                  value={RESET.pinStates[review.pin]}
                  valueTone={review.pin === 'set' ? 'dim' : 'danger'}
                />
                <Divider />
                <ValueRow
                  label={RESET.ops}
                  value={
                    review.check.empty
                      ? RESET.opsEmpty
                      : review.check.reasons.join('; ')
                  }
                  valueTone={review.check.resettable ? 'accent' : 'danger'}
                  testID="reset-ops"
                />
              </Card>
            </View>

            {!review.check.resettable ? (
              <View
                style={[
                  local.panel,
                  {backgroundColor: t.card, borderColor: t.danger},
                ]}
                testID="reset-refused">
                <Text style={[local.eyebrow, {color: t.danger}]}>
                  {RESET.refusedTitle}
                </Text>
                {review.check.reasons.map(reason => (
                  <Text key={reason} style={[local.body, {color: t.ink}]}>
                    {reason}
                  </Text>
                ))}
                <Text style={[local.body, {color: t.dim}]}>
                  {RESET.refusedBody}
                </Text>
              </View>
            ) : review.pin === 'set' ? (
              <View style={local.section}>
                <SectionHeader
                  help={RESET.holderPinHelp}
                  testID="reset-pin-help">
                  {RESET.holderPin}
                </SectionHeader>
                <TextInput
                  value={holderPin.value ?? ''}
                  onChangeText={v => holderPin.set(v)}
                  placeholder={RESET.holderPinPlaceholder}
                  placeholderTextColor={t.dim}
                  keyboardType="number-pad"
                  secureTextEntry
                  textContentType="none"
                  autoComplete="off"
                  importantForAutofill="no"
                  maxLength={PIN_MAX}
                  style={[s.input, local.pinInput]}
                  testID="reset-holder-pin"
                />
              </View>
            ) : (
              <View
                style={[
                  local.panel,
                  {backgroundColor: t.card, borderColor: t.danger},
                ]}
                testID="reset-blocked">
                <Text style={[local.eyebrow, {color: t.danger}]}>
                  {RESET.blockedTitle(review.pin)}
                </Text>
                <Text style={[local.body, {color: t.ink}]}>
                  {RESET.blockedBody}
                </Text>
                {blockedConfirmed ? (
                  <Text style={[local.body, {color: t.dim}]}>
                    {RESET.blockedConfirmed}
                  </Text>
                ) : (
                  <PrimaryButton
                    title={RESET.blockedConfirm}
                    danger
                    onPress={() => setBlockedConfirmed(true)}
                    testID="reset-blocked-confirm"
                  />
                )}
              </View>
            )}
          </>
        ) : null}

        {step.kind === 'done' ? (
          <View style={local.section} testID="reset-done">
            <Text style={[local.title, {color: t.ink}]}>
              {RESET.doneTitle(step.oldId)}
            </Text>
            <Text style={[local.body, {color: t.ink}]}>{RESET.doneBody}</Text>
            <Text
              style={[local.body, {color: step.recorded ? t.dim : t.danger}]}>
              {step.recorded ? RESET.recorded : RESET.notRecorded}
            </Text>
            {step.stranded > 0 ? (
              <Text style={[local.body, {color: t.danger}]}>
                {RESET.stranded(step.stranded)}
              </Text>
            ) : null}
            {!step.recorded ? (
              <PrimaryButton
                title={RESET.record}
                disabled={busy}
                onPress={() => void record(step.oldId)}
                testID="reset-record-again"
              />
            ) : null}
          </View>
        ) : null}

        {line ? (
          <Text
            style={[local.phase, {color: t.ink}]}
            accessibilityLiveRegion="polite"
            testID="reset-line">
            {line}
          </Text>
        ) : null}
        {problem ? (
          <Text
            style={[local.body, {color: t.danger}]}
            accessibilityLiveRegion="assertive"
            testID="reset-problem">
            {problem}
          </Text>
        ) : null}
      </ScrollView>

      <View style={local.footer}>
        {step.kind === 'review' && step.check.resettable ? (
          <PrimaryButton
            title={busy ? RESET.resetting : RESET.reset}
            danger
            disabled={busy || !canReset}
            onPress={() => void reset(step.shortId)}
            testID="reset-go"
          />
        ) : null}
        {step.kind === 'done' ? (
          <PrimaryButton
            title={RESET.another}
            disabled={busy}
            onPress={() => setStep({kind: 'start'})}
            testID="reset-another"
          />
        ) : (
          <SecondaryButton
            title={busy ? RESET.reading : RESET.read}
            disabled={busy || (saved !== null && step.kind === 'start')}
            onPress={() => void read()}
            testID="reset-read"
          />
        )}
        {step.kind === 'done' ? (
          <SecondaryButton
            title={RESET.done}
            disabled={busy}
            onPress={leave}
            testID="reset-close"
          />
        ) : null}
      </View>
    </Screen>
  );
}

const local = StyleSheet.create({
  scroll: {gap: 18, paddingTop: 4, paddingBottom: 24},
  section: {gap: 10},
  panel: {borderRadius: 16, borderWidth: 1.5, padding: 16, gap: 10},
  eyebrow: {
    fontFamily: FONT.semibold,
    fontSize: 12,
    letterSpacing: 1.2,
    textTransform: 'uppercase',
  },
  title: {fontFamily: FONT.semibold, fontSize: 19},
  body: {fontFamily: FONT.regular, fontSize: 15, lineHeight: 21},
  small: {fontFamily: FONT.regular, fontSize: 13, lineHeight: 19},
  pinInput: {fontSize: 20, letterSpacing: 6, minHeight: 50},
  phase: {
    fontFamily: FONT.medium,
    fontSize: 17,
    lineHeight: 24,
    textAlign: 'center',
  },
  footer: {paddingTop: 10, paddingBottom: 8, gap: 10},
});
