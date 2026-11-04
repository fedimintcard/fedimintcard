/**
 * S2. Batch setup, in two presses as the Cashu provisioner does it: the form
 * (federation, cards, PIN, top-up), then a review page whose one button
 * starts the batch. Nothing reaches a card or the registry before that press.
 *
 * The federation is shown by name when the phone's sidecar vouches for it, by
 * short id otherwise, and never as the raw invite. The reasons behind each
 * field live in (i) panels on the section headers, not under the fields.
 *
 * The screen holds no card state. The PIN is a `PinDraft` from `pin.ts`, which
 * lives in this component's state and in nothing else; the store receives the
 * *policy*, never the value.
 */
import React, {useState} from 'react';
import {
  KeyboardAvoidingView,
  Platform,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  View,
} from 'react-native';

import {
  digitsOnly,
  PIN_MAX,
  PIN_POLICY_LABEL,
  type PinPolicy,
  useBatchPin,
  usePinDraft,
} from '../pin';
import {FederationPicker} from '../FederationPicker';
import {
  DEFAULT_FEDERATION,
  isInviteCode,
  LOAD_LINE,
  MAX_COUNT,
  settingsAreReady,
  useSettings,
} from '../settings';
import {SETUP} from '../strings';
import {useStore} from '../store';
import {
  Card,
  Divider,
  EntryRow,
  FONT,
  GearButton,
  PrimaryButton,
  Screen,
  SecondaryButton,
  SectionHeader,
  styles,
  useTheme,
  ValueRow,
  type Theme,
} from '../ui';
import {useFederation} from '../useFederation';

export function BatchSetupScreen({
  onStart,
  onSettings,
  onStop,
}: {
  onStart: () => void;
  onSettings: () => void;
  onStop?: () => void;
}) {
  const app = useStore();
  const settings = useSettings();
  const t = useTheme();
  const s = styles(t);

  // The invite string, exactly as `Settings.federation` stores it.
  const [federation, setFederation] = useState<string>(
    settings.federation || DEFAULT_FEDERATION,
  );
  const [count, setCount] = useState('1');
  const [policy, setPolicy] = useState<PinPolicy>('batch');
  const [reviewing, setReviewing] = useState(false);
  const draft = usePinDraft();
  // The batch PIN crosses to the loop through `pin.ts`'s provider slot, never
  // through the store: `BatchConfig` has no field for a value (§ 5).
  const batchPin = useBatchPin();
  const [startFailed, setStartFailed] = useState<string | null>(null);
  const fed = useFederation(federation);

  const countNumber = Number(count);
  const validCount =
    /^\d+$/.test(count) &&
    Number.isSafeInteger(countNumber) &&
    countNumber >= 1 &&
    countNumber <= MAX_COUNT;

  const pinReady = policy === 'perCard' || draft.valid;
  const preview = app.planPreview(validCount ? countNumber : 1);
  const inviteReady = isInviteCode(federation);
  const ready =
    validCount &&
    pinReady &&
    preview.ok &&
    settingsAreReady({...settings, federation: federation.trim()});

  // What the operator still has to do, one line, in screen order.
  const missing = !inviteReady
    ? SETUP.needFederation
    : !validCount
      ? SETUP.needCount(MAX_COUNT)
      : !pinReady
        ? SETUP.needPin
        : !preview.ok
          ? preview.problem
          : null;

  // A scanned or pasted invite is the batch's federation and the saved one:
  // choosing it is the operator's decision, as tapping a preset was.
  const choose = (invite: string) => {
    setFederation(invite);
    settings.setFederation(invite);
  };

  const start = () => {
    if (!ready) {
      return;
    }
    // The value reaches the loop via the provider slot, which the loop wipes at
    // the end of the batch; the store's config carries the policy name only.
    if (policy === 'batch' && draft.value !== null) {
      batchPin.set(draft.value);
      draft.clear();
    }
    try {
      app.startBatch({
        federation: federation.trim(),
        count: countNumber,
        pinPolicy: policy,
        plan: preview.installOnly ? null : {amounts: [...preview.amounts]},
      });
      onStart();
    } catch (e) {
      setStartFailed(e instanceof Error ? e.message : SETUP.startFailed);
    }
  };

  const topupLine = preview.installOnly
    ? SETUP.installOnly
    : SETUP.planPerCard(preview.perCard.toLocaleString(), preview.lines.length);

  if (reviewing) {
    return (
      <Screen title={SETUP.reviewTitle} onBack={() => setReviewing(false)} onStop={onStop}>
        <ScrollView contentContainerStyle={local.scroll} testID="review-scroll">
          <View style={local.section}>
            <SectionHeader help={SETUP.reviewHelp} testID="review-help">
              {SETUP.reviewHeader}
            </SectionHeader>
            <Card testID="review-card">
              <ValueRow label={SETUP.federation} value={fed.title} valueTone="ink" />
              <Divider />
              <ValueRow
                label={SETUP.countLabel}
                value={String(countNumber)}
                valueTone="ink"
                testID="review-count"
              />
              <Divider />
              <ValueRow
                label={SETUP.pin}
                value={policy === 'batch' ? SETUP.pinBatchShort : SETUP.pinPerCardShort}
                valueTone="ink"
              />
              <Divider />
              <ValueRow label={SETUP.topupLabel} value={topupLine} valueTone="ink" />
              {!preview.installOnly ? (
                <>
                  <Divider />
                  <ValueRow
                    label={SETUP.batchTotal}
                    value={`${preview.batchTotal.toLocaleString()} sat`}
                    valueTone="accent"
                  />
                </>
              ) : null}
            </Card>
            {fed.sidecar === 'mismatch' ? (
              <Text style={[local.warn, {color: t.danger}]} testID="review-mismatch">
                {SETUP.sidecarMismatch}
              </Text>
            ) : null}
            <Text style={[local.small, {color: t.dim}]}>
              {SETUP.reviewLine(countNumber, preview.installOnly)}
            </Text>
          </View>
        </ScrollView>
        <View style={local.footer}>
          <PrimaryButton title={SETUP.start} onPress={start} testID="start-batch" />
          <SecondaryButton
            title={SETUP.edit}
            onPress={() => setReviewing(false)}
            testID="edit-batch"
          />
          {startFailed ? (
            <Text style={s.failed} testID="start-failed">
              {startFailed}
            </Text>
          ) : null}
        </View>
      </Screen>
    );
  }

  return (
    <KeyboardAvoidingView
      style={local.grow}
      enabled={Platform.OS === 'ios'}
      behavior={Platform.OS === 'ios' ? 'padding' : undefined}>
      <Screen
        title={SETUP.title}
        onStop={onStop}
        headerRight={<GearButton onPress={onSettings} />}>
        <ScrollView
          contentContainerStyle={local.scroll}
          keyboardShouldPersistTaps="handled"
          keyboardDismissMode="on-drag"
          testID="setup-scroll">
          <View style={local.section}>
            <SectionHeader help={SETUP.federationHelp} testID="federation-help">
              {SETUP.federation}
            </SectionHeader>
            <FederationPicker
              fed={fed}
              hasInvite={inviteReady}
              onChoose={choose}
            />
          </View>

          <View style={local.section}>
            <SectionHeader help={SETUP.countHelp(MAX_COUNT)} testID="count-help">
              {SETUP.count}
            </SectionHeader>
            <Card>
              <View style={local.fieldRow}>
                <Text style={[local.fieldLabel, {color: t.ink}]}>{SETUP.countLabel}</Text>
                <Stepper
                  t={t}
                  value={count}
                  valid={validCount}
                  onChange={v => setCount(digitsOnly(v))}
                  onStep={d =>
                    setCount(String(validCount ? Math.min(MAX_COUNT, Math.max(1, countNumber + d)) : 1))
                  }
                  min={countNumber <= 1}
                  max={countNumber >= MAX_COUNT}
                />
              </View>
            </Card>
          </View>

          <View style={local.section}>
            <SectionHeader help={SETUP.pinHelp} testID="pin-help">
              {SETUP.pin}
            </SectionHeader>
            <Card>
              <PolicyRow
                label={PIN_POLICY_LABEL.batch}
                on={policy === 'batch'}
                onPress={() => setPolicy('batch')}
                testID="pin-batch"
              />
              {policy === 'batch' ? (
                <View style={local.pinBox}>
                  <TextInput
                    value={draft.value ?? ''}
                    onChangeText={v => draft.set(v)}
                    placeholder={SETUP.pinPlaceholder(PIN_MAX)}
                    placeholderTextColor={t.dim}
                    keyboardType="number-pad"
                    secureTextEntry
                    textContentType="none"
                    autoComplete="off"
                    maxLength={PIN_MAX}
                    style={[s.input, local.pinInput, draft.value ? local.pinSpaced : null]}
                    testID="pin-batch-input"
                  />
                  {draft.value && draft.problem ? (
                    <Text style={[local.small, {color: t.danger}]} testID="pin-batch-problem">
                      {draft.problem}
                    </Text>
                  ) : null}
                </View>
              ) : null}
              <Divider />
              <PolicyRow
                label={PIN_POLICY_LABEL.perCard}
                sub={policy === 'perCard' ? SETUP.perCardSub : undefined}
                on={policy === 'perCard'}
                onPress={() => setPolicy('perCard')}
                testID="pin-perCard"
              />
            </Card>
          </View>

          <View style={local.section}>
            <SectionHeader help={SETUP.topupHelp} testID="topup-help">
              {SETUP.plan}
            </SectionHeader>
            <Card testID="plan-preview">
              <EntryRow
                title={topupLine}
                subtitle={SETUP.topupChange}
                onPress={onSettings}
                testID="plan-settings"
              />
              {!preview.installOnly ? (
                <>
                  <Divider />
                  <View style={local.notes}>
                    {preview.lines.map((line, i) => (
                      <Text
                        key={`${line}-${i}`}
                        style={[local.noteChip, {color: t.ink, borderColor: t.line}]}>
                        {line}
                      </Text>
                    ))}
                  </View>
                </>
              ) : null}
              {preview.problem ? (
                <Text style={[local.small, local.inset, {color: t.danger}]} testID="plan-problem">
                  {preview.problem}
                </Text>
              ) : null}
            </Card>
          </View>
        </ScrollView>
        <View style={local.footer}>
          <PrimaryButton
            title={SETUP.review}
            disabled={!ready}
            onPress={() => {
              setStartFailed(null);
              setReviewing(true);
            }}
            testID="review-batch"
          />
          <Text style={[local.small, local.center, {color: t.dim}]} testID="setup-missing">
            {missing ?? SETUP.buildLine(LOAD_LINE)}
          </Text>
        </View>
      </Screen>
    </KeyboardAvoidingView>
  );
}

function Stepper({
  t,
  value,
  valid,
  onChange,
  onStep,
  min,
  max,
}: {
  t: Theme;
  value: string;
  valid: boolean;
  onChange: (v: string) => void;
  onStep: (delta: number) => void;
  min: boolean;
  max: boolean;
}) {
  const step = (delta: number, off: boolean, label: string, id: string) => (
    <Pressable
      accessibilityRole="button"
      accessibilityLabel={label}
      accessibilityState={{disabled: off}}
      disabled={off}
      onPress={() => onStep(delta)}
      hitSlop={6}
      style={[local.step, {borderColor: t.line, backgroundColor: t.bg}, off && local.off]}
      testID={id}>
      <Text style={[local.stepText, {color: t.ink}]}>{delta < 0 ? '−' : '+'}</Text>
    </Pressable>
  );
  return (
    <View style={local.stepper}>
      {step(-1, !valid || min, 'Fewer cards', 'batch-count-decrement')}
      <TextInput
        value={value}
        onChangeText={onChange}
        keyboardType="number-pad"
        selectTextOnFocus
        accessibilityLabel="Cards in batch"
        style={[local.countInput, {color: valid ? t.ink : t.danger}]}
        testID="batch-count"
      />
      {step(1, max, 'More cards', 'batch-count-increment')}
    </View>
  );
}

function PolicyRow({
  label,
  sub,
  on,
  onPress,
  testID,
}: {
  label: string;
  sub?: string;
  on: boolean;
  onPress: () => void;
  testID: string;
}) {
  const t = useTheme();
  return (
    <Pressable
      style={local.policyRow}
      onPress={onPress}
      testID={testID}
      accessibilityRole="radio"
      accessibilityState={{selected: on}}>
      <View style={[local.radio, {borderColor: on ? t.accent : t.dim}]}>
        {on ? <View style={[local.radioDot, {backgroundColor: t.accent}]} /> : null}
      </View>
      <View style={local.grow}>
        <Text style={[local.fieldLabel, {color: t.ink}]}>{label}</Text>
        {sub ? <Text style={[local.small, {color: t.dim}]}>{sub}</Text> : null}
      </View>
    </Pressable>
  );
}

const local = StyleSheet.create({
  grow: {flex: 1},
  scroll: {gap: 24, paddingTop: 4, paddingBottom: 24},
  section: {gap: 10},
  center: {textAlign: 'center'},
  inset: {paddingHorizontal: 18, paddingBottom: 14},
  small: {fontFamily: FONT.regular, fontSize: 13, lineHeight: 19},
  warn: {fontFamily: FONT.medium, fontSize: 14, lineHeight: 20},
  fieldRow: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
    paddingHorizontal: 18,
    paddingVertical: 12,
    minHeight: 60,
  },
  fieldLabel: {fontFamily: FONT.medium, fontSize: 16},
  stepper: {flexDirection: 'row', alignItems: 'center', gap: 6},
  step: {
    width: 40,
    height: 40,
    borderRadius: 20,
    borderWidth: 1.5,
    alignItems: 'center',
    justifyContent: 'center',
  },
  stepText: {fontFamily: FONT.medium, fontSize: 22, lineHeight: 26},
  off: {opacity: 0.35},
  countInput: {
    minWidth: 52,
    textAlign: 'center',
    fontFamily: FONT.semibold,
    fontSize: 22,
    fontVariant: ['tabular-nums'],
    paddingVertical: 4,
  },
  policyRow: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: 14,
    paddingHorizontal: 18,
    paddingVertical: 15,
  },
  radio: {
    width: 22,
    height: 22,
    borderRadius: 11,
    borderWidth: 2,
    alignItems: 'center',
    justifyContent: 'center',
  },
  radioDot: {width: 10, height: 10, borderRadius: 5},
  pinBox: {paddingHorizontal: 18, paddingBottom: 14, gap: 6},
  pinInput: {fontSize: 16, minHeight: 50},
  pinSpaced: {fontSize: 20, letterSpacing: 6},
  notes: {flexDirection: 'row', flexWrap: 'wrap', gap: 8, padding: 14},
  noteChip: {
    fontFamily: FONT.medium,
    fontSize: 13,
    fontVariant: ['tabular-nums'],
    borderWidth: 1,
    borderRadius: 8,
    paddingHorizontal: 8,
    paddingVertical: 4,
    overflow: 'hidden',
  },
  footer: {gap: 10, paddingBottom: 8, paddingTop: 4},
});
