/**
 * Settings: what the operator sets between batches. The federation (the same
 * picker as NEW BATCH), the top-up per card, the registry endpoint, the demo,
 * the session, and About. Reasons sit behind (i) on each header; choices save
 * as they are made, and a short line at the foot confirms each save.
 *
 * There is no theme choice: the app has one brand palette (`theme.ts`).
 */
import React, {useEffect, useState} from 'react';
import {Pressable, ScrollView, StyleSheet, Text, TextInput, View} from 'react-native';

import {BrandTile} from '../BrandMark';
import {FederationPicker} from '../FederationPicker';
import {
  APP_IDENTITY,
  EXPERIMENTAL_WARNING,
  isInviteCode,
  isValidTopup,
  LOAD_LINE,
  MAX_TOPUP_NOTES,
  REGISTRY_ENDPOINTS,
  topupNotesMsat,
  useSettings,
  type RegistryEndpoint,
} from '../settings';
import {SETTINGS} from '../strings';
import {useStore} from '../store';
import {
  Card,
  Divider,
  EntryRow,
  FONT,
  PrimaryButton,
  Screen,
  SectionHeader,
  Segmented,
  SecondaryButton,
  styles,
  useTheme,
} from '../ui';
import {useFederation} from '../useFederation';

const REGISTRY_LABEL: Record<RegistryEndpoint, string> = {
  test: 'Test',
  production: 'Live',
};

/** Sums whose notes are all 8 sat or more: no note below a federation's floor. */
const TOPUP_PRESETS: readonly number[] = [0, 1024, 2048, 4096];

function parseAmount(text: string): number {
  const digits = text.replace(/[,\s]/g, '');
  return /^[0-9]+$/.test(digits) ? Number(digits) : NaN;
}

export function SettingsScreen({
  onBack,
  onDemo,
}: {
  onBack: () => void;
  onDemo: () => void;
}) {
  const settings = useSettings();
  const app = useStore();
  const t = useTheme();
  const s = styles(t);
  const fed = useFederation(settings.federation);

  const [custom, setCustom] = useState('');
  const [saved, setSaved] = useState<string | null>(null);
  const [confirmSignOut, setConfirmSignOut] = useState(false);
  const running = app.batch?.phase === 'running';

  // The confirmation line fades after a moment; the saved value is on screen.
  useEffect(() => {
    if (!saved) {
      return;
    }
    const timer = setTimeout(() => setSaved(null), 2500);
    return () => clearTimeout(timer);
  }, [saved]);

  const parsed = parseAmount(custom);
  const customValid = custom.trim() !== '' && isValidTopup(parsed);
  const isPreset = TOPUP_PRESETS.includes(settings.topupSats);
  const notes = topupNotesMsat(settings.topupSats);

  const saveTopup = (sats: number) => {
    if (!isValidTopup(sats)) {
      return;
    }
    settings.setTopupSats(sats);
    setCustom('');
    setSaved(SETTINGS.amountsSaved(sats));
  };

  return (
    <Screen title={SETTINGS.title} onBack={onBack}>
      <ScrollView
        contentContainerStyle={local.scroll}
        keyboardShouldPersistTaps="handled"
        testID="settings-scroll">
        <View style={local.section}>
          <SectionHeader help={SETTINGS.federationHelp} testID="settings-federation-help">
            {SETTINGS.federation}
          </SectionHeader>
          <FederationPicker
            fed={fed}
            hasInvite={isInviteCode(settings.federation)}
            onChoose={invite => {
              settings.setFederation(invite);
              setSaved(SETTINGS.federationSaved);
            }}
            testID="settings-federation"
          />
        </View>

        <View style={local.section}>
          <SectionHeader help={SETTINGS.topupHelp} testID="settings-topup-help">
            {SETTINGS.topup}
          </SectionHeader>
          <Card>
            <View style={local.chips}>
              {TOPUP_PRESETS.map(sats => {
                const on = settings.topupSats === sats;
                return (
                  <Pressable
                    key={sats}
                    onPress={() => saveTopup(sats)}
                    accessibilityRole="radio"
                    accessibilityState={{selected: on}}
                    testID={`settings-topup-${sats}`}
                    style={[
                      local.chip,
                      {borderColor: on ? t.accent : t.line},
                      on && {backgroundColor: t.accent},
                    ]}>
                    <Text style={[local.chipText, {color: on ? t.onAccent : t.ink}]}>
                      {sats === 0 ? SETTINGS.installOnly : sats.toLocaleString()}
                    </Text>
                  </Pressable>
                );
              })}
            </View>
            <Divider />
            <View style={local.customRow}>
              <TextInput
                value={custom}
                onChangeText={setCustom}
                placeholder={
                  isPreset
                    ? SETTINGS.customPlaceholder
                    : `${settings.topupSats.toLocaleString()} sat`
                }
                placeholderTextColor={t.dim}
                keyboardType="number-pad"
                returnKeyType="done"
                onSubmitEditing={() => customValid && saveTopup(parsed)}
                style={[s.input, local.customInput]}
                testID="settings-amounts"
              />
              <Pressable
                onPress={() => saveTopup(parsed)}
                disabled={!customValid}
                accessibilityRole="button"
                accessibilityState={{disabled: !customValid}}
                style={[local.setBtn, {backgroundColor: customValid ? t.accent : t.raised}]}
                testID="settings-amounts-save">
                <Text style={[local.setText, {color: customValid ? t.onAccent : t.dim}]}>
                  {SETTINGS.set}
                </Text>
              </Pressable>
            </View>
            <Text
              style={[
                local.small,
                local.inset,
                {color: custom.trim() !== '' && !customValid ? t.danger : t.dim},
              ]}
              testID="settings-amounts-total">
              {custom.trim() !== '' && !customValid
                ? SETTINGS.amountsProblem(MAX_TOPUP_NOTES)
                : SETTINGS.amountsTotal(notes.length, settings.topupSats)}
            </Text>
          </Card>
        </View>

        <View style={local.section}>
          <SectionHeader help={SETTINGS.registryHelp} testID="settings-registry-help">
            {SETTINGS.registry}
          </SectionHeader>
          <Card>
            <View style={local.registryRow}>
              <Text style={[local.small, local.grow, {color: t.dim}]}>
                {SETTINGS.registryNow(REGISTRY_LABEL[settings.registryEndpoint])}
              </Text>
              <Segmented<RegistryEndpoint>
                options={REGISTRY_ENDPOINTS}
                value={settings.registryEndpoint}
                labelOf={e => REGISTRY_LABEL[e]}
                onChange={e => {
                  settings.setRegistryEndpoint(e);
                  setSaved(SETTINGS.registryNow(REGISTRY_LABEL[e]));
                }}
                testIDOf={e => `settings-registry-${e}`}
              />
            </View>
          </Card>
        </View>

        {/* Above Session on purpose: a reviewer reaches this before signing in
            to anything, which is the point of the demo (plan § 7, 2.1). */}
        <View style={local.section}>
          <SectionHeader help={SETTINGS.demoHelp} testID="settings-demo-help">
            {SETTINGS.demo}
          </SectionHeader>
          <Card>
            <EntryRow
              title={SETTINGS.demoTitle}
              subtitle={running ? SETTINGS.demoBlocked : SETTINGS.demoSub}
              onPress={onDemo}
              disabled={running}
              testID="settings-demo-entry"
            />
          </Card>
        </View>

        <View style={local.section}>
          <SectionHeader>{SETTINGS.session}</SectionHeader>
          <Card>
            <EntryRow
              title={SETTINGS.signOut}
              subtitle={running ? SETTINGS.signOutBlocked : SETTINGS.signOutSub}
              onPress={() => {
                if (!running) {
                  setConfirmSignOut(true);
                }
              }}
              testID="settings-sign-out-entry"
            />
            {confirmSignOut ? (
              <View style={local.confirm}>
                <PrimaryButton
                  title={SETTINGS.signOut}
                  danger
                  onPress={() => {
                    setConfirmSignOut(false);
                    app.signOut();
                  }}
                  testID="settings-sign-out"
                />
                <SecondaryButton
                  title={SETTINGS.keepSession}
                  onPress={() => setConfirmSignOut(false)}
                  testID="settings-keep-session"
                />
              </View>
            ) : null}
          </Card>
        </View>

        <View style={local.section}>
          <SectionHeader help={EXPERIMENTAL_WARNING} testID="settings-about-help">
            {SETTINGS.about}
          </SectionHeader>
          <Card>
            <View style={local.about}>
              <BrandTile size={44} />
              <View style={local.grow}>
                <Text style={[local.aboutName, {color: t.ink}]}>{SETTINGS.appName}</Text>
                <Text style={[local.small, {color: t.dim}]} testID="settings-load-line">
                  {SETTINGS.cardSoftware(LOAD_LINE)}
                </Text>
                <Text style={[local.small, {color: t.dim}]} testID="settings-identity">
                  {APP_IDENTITY.androidApplicationId}
                </Text>
              </View>
            </View>
          </Card>
          <Text style={[local.small, {color: t.dim}]}>{SETTINGS.experimentalShort}</Text>
        </View>
      </ScrollView>

      {saved ? (
        <View style={[local.toast, {backgroundColor: t.raised, borderColor: t.line}]}>
          <Text
            style={[local.toastText, {color: t.ink}]}
            testID="settings-saved"
            accessibilityLiveRegion="polite">
            {saved}
          </Text>
        </View>
      ) : null}
    </Screen>
  );
}

const local = StyleSheet.create({
  grow: {flex: 1},
  scroll: {gap: 24, paddingTop: 4, paddingBottom: 40},
  section: {gap: 10},
  small: {fontFamily: FONT.regular, fontSize: 13, lineHeight: 19},
  inset: {paddingHorizontal: 16, paddingBottom: 14},
  chips: {flexDirection: 'row', gap: 6, padding: 14},
  chip: {
    flexGrow: 1,
    alignItems: 'center',
    borderWidth: 1.5,
    borderRadius: 999,
    paddingHorizontal: 6,
    paddingVertical: 8,
  },
  chipText: {fontFamily: FONT.semibold, fontSize: 13, fontVariant: ['tabular-nums']},
  customRow: {flexDirection: 'row', gap: 10, padding: 14, paddingBottom: 8},
  customInput: {flex: 1, minHeight: 46},
  setBtn: {
    borderRadius: 12,
    paddingHorizontal: 18,
    alignItems: 'center',
    justifyContent: 'center',
  },
  setText: {fontFamily: FONT.semibold, fontSize: 15},
  registryRow: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: 12,
    paddingHorizontal: 16,
    paddingVertical: 12,
  },
  confirm: {padding: 14, gap: 10},
  about: {flexDirection: 'row', alignItems: 'center', gap: 14, padding: 16},
  aboutName: {fontFamily: FONT.semibold, fontSize: 16},
  toast: {
    position: 'absolute',
    left: 16,
    right: 16,
    bottom: 12,
    borderRadius: 12,
    borderWidth: 1,
    paddingHorizontal: 16,
    paddingVertical: 12,
  },
  toastText: {fontFamily: FONT.medium, fontSize: 14, textAlign: 'center'},
});
