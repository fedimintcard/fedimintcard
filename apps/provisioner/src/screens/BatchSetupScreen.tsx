/**
 * S2. Batch setup: the federation, the count, the PIN policy and the top-up
 * plan, in that order, then START BATCH.
 *
 * Ported from cashucard's `BatchSetupScreen` with plan § 5's trim applied
 * literally. Kept: the federation picker (grid, custom invite, failure
 * banner), the count, the PIN policy radio with its required batch PIN, and the
 * plan preview. Dropped:
 *
 * - `dailyUsd` / `noPinUsd`: limits are shape C, which this applet does not
 *   have; a field the card cannot enforce is a promise the app cannot keep.
 * - the card design chip: there is no equivalent in this repo.
 * - the installation-only review branch: this app has one shape (B), and the
 *   operator's review of what will be written is the plan preview.
 *
 * The screen holds no card state. The PIN is a `PinDraft` from `pin.ts`, which
 * lives in this component's state and in nothing else; the store receives the
 * *policy*, never the value.
 */
import React, {useState} from 'react';
import {
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
import {
  DEFAULT_FEDERATION,
  federationLabel,
  federationOptions,
  LOAD_LINE,
  MAX_COUNT,
  settingsAreReady,
  useSettings,
} from '../settings';
import {SETUP} from '../strings';
import {useStore} from '../store';
import {
  Card,
  Dim,
  Divider,
  EntryRow,
  FONT,
  PrimaryButton,
  Screen,
  SectionHeader,
  styles,
  useTheme,
} from '../ui';

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
  const [customOpen, setCustomOpen] = useState(false);
  const [count, setCount] = useState('1');
  const [policy, setPolicy] = useState<PinPolicy>('batch');
  const draft = usePinDraft();
  // The batch PIN crosses to the loop through `pin.ts`'s provider slot, never
  // through the store: `BatchConfig` has no field for a value (§ 5).
  const batchPin = useBatchPin();
  const [startFailed, setStartFailed] = useState<string | null>(null);

  const countNumber = Number(count);
  const validCount =
    /^\d+$/.test(count) &&
    Number.isSafeInteger(countNumber) &&
    countNumber >= 1 &&
    countNumber <= MAX_COUNT;

  const pinProblemText = policy === 'batch' ? draft.problem : null;
  const pinReady = policy === 'perCard' || draft.valid;
  // The saved invite first, then every preset: a typed custom invite stays
  // visible and selected instead of vanishing when a preset is tapped.
  const options = federationOptions(federation);
  const preview = app.planPreview(validCount ? countNumber : 1);
  // One rule for "a batch can start", shared with Settings: an invite that is
  // a `fedimint://` URL and amounts the sidecar can realise exactly.
  const inviteReady = federation.trim().startsWith('fedimint://');
  const ready =
    validCount &&
    pinReady &&
    preview.ok &&
    settingsAreReady({...settings, federation: federation.trim()});

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
        plan: {amounts: [...preview.amounts]},
      });
      onStart();
    } catch (e) {
      setStartFailed(e instanceof Error ? e.message : 'The batch could not start.');
    }
  };

  return (
    <Screen
      title={SETUP.title}
      onBack={onSettings}
      onStop={onStop}
      headerRight={
        <Pressable
          onPress={onSettings}
          hitSlop={12}
          testID="settings"
          accessibilityRole="button"
          accessibilityLabel="Settings">
          <Text style={[s.back, {color: t.dim}]}>Settings</Text>
        </Pressable>
      }>
      <ScrollView contentContainerStyle={local.scroll} testID="setup-scroll">
        {!inviteReady ? (
          <Text style={s.failed} testID="federation-failed">
            {SETUP.federationFailed}
          </Text>
        ) : null}

        <View style={local.section}>
          <SectionHeader>{SETUP.federation}</SectionHeader>
          <Card>
            <View style={local.grid}>
              {options.map((f, i) => {
                const on = federation === f.invite;
                return (
                  <Pressable
                    key={f.invite}
                    onPress={() => {
                      setFederation(f.invite);
                      setCustomOpen(false);
                      settings.setFederation(f.invite);
                    }}
                    testID={`federation-${f.invite}`}
                    accessibilityRole="radio"
                    accessibilityState={{selected: on}}
                    style={[
                      local.gridCell,
                      i % 2 === 1 && local.gridCellEdgeR,
                      i >= options.length - 2 && local.gridCellEdgeB,
                      on && {backgroundColor: t.accent},
                    ]}>
                    <Text
                      numberOfLines={1}
                      adjustsFontSizeToFit
                      minimumFontScale={0.8}
                      style={[
                        local.gridLabel,
                        {color: on ? '#fff' : t.ink},
                      ]}>
                      {federationLabel(f)}
                    </Text>
                  </Pressable>
                );
              })}
              <Pressable
                onPress={() => setCustomOpen(v => !v)}
                testID="federation-custom"
                accessibilityRole="radio"
                accessibilityState={{selected: customOpen}}
                style={[
                  local.gridCell,
                  {backgroundColor: customOpen ? t.accent : 'transparent'},
                ]}>
                <Text
                  style={[
                    local.gridLabel,
                    {color: customOpen ? '#fff' : t.ink},
                  ]}>
                  {SETUP.customFederation}
                </Text>
              </Pressable>
            </View>
            {customOpen ? (
              <>
                <Divider />
                <TextInput
                  value={federation}
                  onChangeText={setFederation}
                  placeholder={SETUP.customPlaceholder}
                  placeholderTextColor={t.dim}
                  keyboardType="url"
                  autoCapitalize="none"
                  autoCorrect={false}
                  style={[s.input, local.customInput]}
                  testID="federation-custom-input"
                />
              </>
            ) : null}
          </Card>
        </View>

        <View style={local.section}>
          <SectionHeader>{SETUP.count}</SectionHeader>
          <Card>
            <View style={local.fieldRow}>
              <Text style={local.fieldLabel}>{SETUP.count}</Text>
              <Pressable
                accessibilityRole="button"
                accessibilityLabel="Decrease cards in batch"
                accessibilityState={{
                  disabled: !validCount || countNumber <= 1,
                }}
                disabled={!validCount || countNumber <= 1}
                onPress={() => setCount(String(countNumber - 1))}
                style={[
                  local.countStep,
                  (!validCount || countNumber <= 1) && local.countStepOff,
                ]}
                testID="batch-count-decrement">
                <Text style={local.countStepText}>−</Text>
              </Pressable>
              <TextInput
                value={count}
                onChangeText={v => setCount(digitsOnly(v))}
                keyboardType="number-pad"
                selectTextOnFocus
                accessibilityLabel="Cards in batch"
                style={[local.fieldInput, local.countInput]}
                testID="batch-count"
              />
              <Pressable
                accessibilityRole="button"
                accessibilityLabel="Increase cards in batch"
                accessibilityState={{disabled: countNumber >= MAX_COUNT}}
                disabled={countNumber >= MAX_COUNT}
                onPress={() =>
                  setCount(String(validCount ? countNumber + 1 : 1))
                }
                style={[
                  local.countStep,
                  countNumber >= MAX_COUNT && local.countStepOff,
                ]}
                testID="batch-count-increment">
                <Text style={local.countStepText}>+</Text>
              </Pressable>
            </View>
            {!validCount ? (
              <Text style={local.fieldError}>
                {`${SETUP.countRange} (1–${MAX_COUNT} here.)`}
              </Text>
            ) : null}
          </Card>
        </View>

        <View style={local.section}>
          <SectionHeader>{SETUP.pin}</SectionHeader>
          <Card>
            <PolicyRow
              label={PIN_POLICY_LABEL.batch}
              on={policy === 'batch'}
              onPress={() => setPolicy('batch')}
              testID="pin-batch"
            />
            <Divider />
            <PolicyRow
              label={PIN_POLICY_LABEL.perCard}
              on={policy === 'perCard'}
              onPress={() => setPolicy('perCard')}
              testID="pin-perCard"
            />
            {policy === 'batch' ? (
              <>
                <Divider />
                <TextInput
                  value={draft.value ?? ''}
                  onChangeText={v => draft.set(v)}
                  placeholder={`4–${PIN_MAX} digits`}
                  placeholderTextColor={t.dim}
                  keyboardType="number-pad"
                  secureTextEntry
                  textContentType="none"
                  autoComplete="off"
                  maxLength={PIN_MAX}
                  style={[s.input, local.pinInput]}
                  testID="pin-batch-input"
                />
                {pinProblemText ? (
                  <Text style={local.fieldError} testID="pin-batch-problem">
                    {pinProblemText}
                  </Text>
                ) : null}
              </>
            ) : (
              <View style={local.pinNote}>
                <Dim>
                  Each card gets a fresh PIN, shown once on the summary. The
                  operator writes it down before the card is handed out.
                </Dim>
              </View>
            )}
          </Card>
        </View>

        <View style={local.section}>
          <SectionHeader>{SETUP.plan}</SectionHeader>
          <Card testID="plan-preview">
            {preview.installOnly ? (
              <View style={local.fieldRow}>
                <Dim>{SETUP.planNone}</Dim>
              </View>
            ) : (
              <>
                <View style={local.fieldRow}>
                  <Text style={local.fieldLabel}>
                    {SETUP.planPerCard(
                      preview.perCard.toLocaleString(),
                      preview.amounts.length,
                    )}
                  </Text>
                </View>
                <Divider />
                {preview.lines.map((line, i) => (
                  <React.Fragment key={`${line}-${i}`}>
                    {i > 0 ? <Divider /> : null}
                    <View style={local.noteRow}>
                      <Text style={local.noteIndex}>{i + 1}</Text>
                      <Text style={local.noteAmount}>{line}</Text>
                    </View>
                  </React.Fragment>
                ))}
                <Divider />
                <View style={local.fieldRow}>
                  <Text style={local.fieldLabel}>Batch total</Text>
                  <Text style={local.fieldValue}>
                    {`${preview.batchTotal.toLocaleString()} sat`}
                  </Text>
                </View>
              </>
            )}
            {preview.problem ? (
              <Text style={local.fieldError} testID="plan-problem">
                {preview.problem}
              </Text>
            ) : null}
          </Card>
          <EntryRow
            title="Change the amounts"
            subtitle="Top-up amounts are a setting, not a batch field."
            onPress={onSettings}
            testID="plan-settings"
          />
        </View>
      </ScrollView>

      <View style={local.footer}>
        <PrimaryButton
          title={SETUP.start}
          disabled={!ready}
          onPress={start}
          testID="start-batch"
        />
        <Text style={local.buildLine}>{SETUP.buildLine(LOAD_LINE)}</Text>
        {startFailed ? (
          <Text style={s.failed} testID="start-failed">
            {startFailed}
          </Text>
        ) : null}
      </View>
    </Screen>
  );
}

/** One radio row of the PIN policy: a dot in a ring, the label. */
function PolicyRow({
  label,
  on,
  onPress,
  testID,
}: {
  label: string;
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
      <View
        style={[
          local.radio,
          {borderColor: on ? t.accent : t.line},
          on && {backgroundColor: t.accent},
        ]}>
        {on ? <View style={[local.radioDot, {backgroundColor: t.bg}]} /> : null}
      </View>
      <Text style={[local.fieldLabel, {color: t.ink}]}>{label}</Text>
    </Pressable>
  );
}

const local = StyleSheet.create({
  scroll: {gap: 20, paddingBottom: 24},
  section: {gap: 8},
  grid: {flexDirection: 'row', flexWrap: 'wrap'},
  gridCell: {
    width: '50%',
    paddingHorizontal: 14,
    paddingVertical: 14,
    alignItems: 'flex-start',
    justifyContent: 'center',
  },
  gridCellEdgeR: {borderRightWidth: StyleSheet.hairlineWidth},
  gridCellEdgeB: {borderBottomWidth: StyleSheet.hairlineWidth},
  gridLabel: {fontFamily: FONT.medium, fontSize: 15},
  customInput: {borderWidth: 0, borderRadius: 0, backgroundColor: 'transparent'},
  fieldRow: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: 10,
    paddingHorizontal: 14,
    paddingVertical: 13,
  },
  fieldLabel: {flex: 1, fontFamily: FONT.medium, fontSize: 16},
  fieldValue: {
    fontFamily: FONT.tabular,
    fontVariant: ['tabular-nums'],
    fontSize: 15,
  },
  fieldInput: {
    fontFamily: FONT.tabular,
    fontVariant: ['tabular-nums'],
    fontSize: 16,
    textAlign: 'center',
    minWidth: 44,
  },
  countInput: {minWidth: 56},
  countStep: {
    width: 34,
    height: 34,
    borderRadius: 17,
    borderWidth: 1.5,
    alignItems: 'center',
    justifyContent: 'center',
  },
  countStepOff: {opacity: 0.35},
  countStepText: {fontFamily: FONT.medium, fontSize: 20, lineHeight: 24},
  fieldError: {
    paddingHorizontal: 14,
    paddingBottom: 12,
    fontFamily: FONT.regular,
    fontSize: 13,
  },
  policyRow: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: 12,
    paddingHorizontal: 16,
    paddingVertical: 14,
  },
  radio: {
    width: 22,
    height: 22,
    borderRadius: 11,
    borderWidth: 1.5,
    alignItems: 'center',
    justifyContent: 'center',
  },
  radioDot: {width: 8, height: 8, borderRadius: 4},
  pinInput: {
    borderWidth: 0,
    borderRadius: 0,
    backgroundColor: 'transparent',
    letterSpacing: 6,
  },
  pinNote: {paddingHorizontal: 16, paddingBottom: 14},
  noteRow: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: 12,
    paddingHorizontal: 16,
    paddingVertical: 11,
  },
  noteIndex: {
    fontFamily: FONT.tabular,
    fontVariant: ['tabular-nums'],
    fontSize: 13,
    width: 18,
    color: 'rgba(128,128,128,0.9)',
  },
  noteAmount: {
    fontFamily: FONT.tabular,
    fontVariant: ['tabular-nums'],
    fontSize: 15,
  },
  footer: {paddingTop: 10, paddingBottom: 8, gap: 8},
  buildLine: {
    fontFamily: FONT.regular,
    fontSize: 12,
    textAlign: 'center',
  },
});
