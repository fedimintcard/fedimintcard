/**
 * Settings: what the operator sets between batches. The federation (the same
 * picker as NEW BATCH), the registry endpoint, the demo,
 * the session, and About. Reasons sit behind (i) on each header; choices save
 * as they are made, and a short line at the foot confirms each save.
 *
 * There is no theme choice: the app has one brand palette (`theme.ts`).
 */
import React, {useEffect, useState} from 'react';
import {ScrollView, StyleSheet, Text, View} from 'react-native';

import {BrandTile} from '../BrandMark';
import {FederationPicker} from '../FederationPicker';
import {
  APP_IDENTITY,
  EXPERIMENTAL_WARNING,
  isInviteCode,
  LOAD_LINE,
  REGISTRY_ENDPOINTS,
  useSettings,
  type RegistryEndpoint,
} from '../settings';
import {SETTINGS} from '../strings';
import {useStore} from '../store';
import {
  Card,
  EntryRow,
  FONT,
  PrimaryButton,
  Screen,
  SectionHeader,
  Segmented,
  SecondaryButton,
  useTheme,
} from '../ui';
import {useFederation} from '../useFederation';

const REGISTRY_LABEL: Record<RegistryEndpoint, string> = {
  test: 'Test',
  production: 'Live',
};

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
  const fed = useFederation(settings.federation);

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
