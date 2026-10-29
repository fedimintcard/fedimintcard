/**
 * S5. Settings: the operator's runtime surface, and nothing else.
 *
 * Ported from cashucard's `SettingsScreen` and cut to plan § 5's line target.
 * What survives is what the operator genuinely varies at runtime — the
 * federation, the registry endpoint, the top-up amounts and the theme — plus
 * sign-out and one About block that says which build this is.
 *
 * Cut, and why:
 * - the provisioning-scope block and its help popover: this app has one service
 *   and one operator account, so the scope label would always read the same;
 * - the card-tools list (check a card, read identity, reset, bench): those
 *   screens are not ported (§ 5), and a row that opens nothing is a dead end;
 * - the CAP pin line: this build loads one applet and has one load file.
 *
 * Build-time constants are shown, never edited: they render as plain rows with
 * no control, which is the honest rendering of a frozen value.
 *
 * The federation is stored as the invite *string* (`Settings.federation`), not
 * a `{invite,label}` object, so `federationOptions(saved)` is what turns the
 * saved invite back into a pickable row — a typed custom invite therefore stays
 * visible and selected instead of vanishing the moment a preset is tapped.
 */
import React, {useState} from 'react';
import {ScrollView, StyleSheet, Text, TextInput, View} from 'react-native';

import {
  APP_IDENTITY,
  EXPERIMENTAL_WARNING,
  federationLabel,
  federationOptions,
  isValidTopup,
  LOAD_LINE,
  MAX_TOPUP_NOTES,
  REGISTRY_ENDPOINTS,
  topupTotal,
  useSettings,
  type RegistryEndpoint,
  type ThemeChoice,
} from '../settings';
import {SETTINGS} from '../strings';
import {useStore} from '../store';
import {
  Card,
  Dim,
  Divider,
  EntryRow,
  PrimaryButton,
  Screen,
  SectionHeader,
  Segmented,
  SecondaryButton,
  styles,
  useTheme,
} from '../ui';

/** The registry endpoints, as the operator's words: `production` is "Live". */
const REGISTRY_LABEL: Record<RegistryEndpoint, string> = {
  test: 'Test',
  production: 'Live',
};

const THEME_LABEL: Record<ThemeChoice, string> = {
  system: 'System',
  dark: 'Dark',
  light: 'Light',
};

/** "1048, 524, 262" → [1048, 524, 262]; anything else is dropped, not coerced. */
function parseAmounts(text: string): readonly number[] {
  return text
    .split(/[^0-9]+/)
    .filter(part => part.length > 0)
    .map(Number)
    .filter(n => Number.isSafeInteger(n) && n > 0);
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

  const [invite, setInvite] = useState(settings.federation);
  const [amounts, setAmounts] = useState(settings.topupSats.join(', '));
  const [saved, setSaved] = useState<string | null>(null);
  const [confirmSignOut, setConfirmSignOut] = useState(false);

  const parsed = parseAmounts(amounts);
  const amountsValid = isValidTopup(parsed);
  const inviteValid = invite.trim().startsWith('fedimint://');
  const running = app.batch?.phase === 'running';

  const saveFederation = () => {
    const trimmed = invite.trim();
    if (!trimmed.startsWith('fedimint://')) {
      return;
    }
    settings.setFederation(trimmed);
    setInvite(trimmed);
    setSaved(SETTINGS.federationSaved);
  };

  const saveAmounts = () => {
    if (!isValidTopup(parsed)) {
      return;
    }
    settings.setTopupSats(parsed);
    setSaved(SETTINGS.amountsSaved(topupTotal(parsed)));
  };

  return (
    <Screen title={SETTINGS.title} onBack={onBack}>
      <ScrollView contentContainerStyle={local.scroll} testID="settings-scroll">
        <View style={local.section}>
          <SectionHeader>{SETTINGS.federation}</SectionHeader>
          <Card>
            {federationOptions(settings.federation).map(f => (
              <React.Fragment key={f.invite}>
                <EntryRow
                  title={federationLabel(f)}
                  subtitle={f.invite}
                  onPress={() => {
                    settings.setFederation(f.invite);
                    setInvite(f.invite);
                    setSaved(SETTINGS.federationSaved);
                  }}
                  testID={`settings-federation-${f.invite}`}
                />
                <Divider />
              </React.Fragment>
            ))}
            <View style={local.field}>
              <TextInput
                value={invite}
                onChangeText={setInvite}
                placeholder="fedimint://…"
                placeholderTextColor={t.dim}
                autoCapitalize="none"
                autoCorrect={false}
                keyboardType="url"
                style={s.input}
                testID="settings-federation-input"
              />
              {inviteValid ? null : (
                <Text style={s.failed} testID="settings-federation-problem">
                  {SETTINGS.inviteProblem}
                </Text>
              )}
              <SecondaryButton
                title={SETTINGS.saveFederation}
                onPress={saveFederation}
                disabled={!inviteValid}
                testID="settings-federation-save"
              />
            </View>
          </Card>
          <Text style={s.note}>{SETTINGS.federationHelp}</Text>
        </View>

        <View style={local.section}>
          <SectionHeader>{SETTINGS.registry}</SectionHeader>
          <Card>
            <View style={local.field}>
              <Segmented<RegistryEndpoint>
                options={REGISTRY_ENDPOINTS}
                value={settings.registryEndpoint}
                labelOf={endpoint => REGISTRY_LABEL[endpoint]}
                onChange={settings.setRegistryEndpoint}
                testIDOf={endpoint => `settings-registry-${endpoint}`}
              />
              <Dim testID="settings-registry-current">
                {SETTINGS.registryNow(REGISTRY_LABEL[settings.registryEndpoint])}
              </Dim>
            </View>
          </Card>
          <Text style={s.note}>{SETTINGS.registryHelp}</Text>
        </View>

        <View style={local.section}>
          <SectionHeader>{SETTINGS.topup}</SectionHeader>
          <Card>
            <View style={local.field}>
              <TextInput
                value={amounts}
                onChangeText={setAmounts}
                keyboardType="numbers-and-punctuation"
                style={s.input}
                testID="settings-amounts"
              />
              {amountsValid ? (
                <Dim testID="settings-amounts-total">
                  {SETTINGS.amountsTotal(parsed.length, topupTotal(parsed))}
                </Dim>
              ) : (
                <Text style={s.failed} testID="settings-amounts-problem">
                  {SETTINGS.amountsProblem(MAX_TOPUP_NOTES)}
                </Text>
              )}
              <SecondaryButton
                title={SETTINGS.saveAmounts}
                onPress={saveAmounts}
                disabled={!amountsValid}
                testID="settings-amounts-save"
              />
            </View>
          </Card>
          <Text style={s.note}>{SETTINGS.topupHelp}</Text>
        </View>

        <View style={local.section}>
          <SectionHeader>{SETTINGS.theme}</SectionHeader>
          <Card>
            <View style={local.field}>
              <Segmented<ThemeChoice>
                options={['system', 'dark', 'light']}
                value={settings.theme}
                labelOf={choice => THEME_LABEL[choice]}
                onChange={settings.setTheme}
                testIDOf={choice => `theme-${choice}`}
              />
            </View>
          </Card>
        </View>

        {/* Above Session on purpose: a reviewer reaches this before signing in
            to anything, which is the point of the demo (plan § 7, 2.1). */}
        <View style={local.section}>
          <SectionHeader>{SETTINGS.demo}</SectionHeader>
          <Card>
            <EntryRow
              title={SETTINGS.demoTitle}
              subtitle={running ? SETTINGS.demoBlocked : SETTINGS.demoSub}
              onPress={onDemo}
              disabled={running}
              testID="settings-demo-entry"
            />
          </Card>
          <Text style={s.note}>{SETTINGS.demoHelp}</Text>
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
              <View style={local.field}>
                <PrimaryButton
                  title={SETTINGS.signOut}
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
          <SectionHeader>{SETTINGS.about}</SectionHeader>
          <Card>
            <View style={local.about}>
              <Dim>{APP_IDENTITY.reactNativeName}</Dim>
              <Dim testID="settings-load-line">{LOAD_LINE}</Dim>
              <Dim testID="settings-identity">
                {APP_IDENTITY.androidApplicationId}
              </Dim>
            </View>
          </Card>
          <Text style={s.note}>{EXPERIMENTAL_WARNING}</Text>
        </View>
      </ScrollView>

      {saved ? (
        <Text
          style={s.note}
          testID="settings-saved"
          accessibilityLiveRegion="polite">
          {saved}
        </Text>
      ) : null}
    </Screen>
  );
}

const local = StyleSheet.create({
  scroll: {gap: 22, paddingBottom: 32},
  section: {gap: 8},
  field: {padding: 14, gap: 10},
  about: {paddingVertical: 20, alignItems: 'center', gap: 4},
});
