/**
 * The federation field, shared by NEW BATCH and Settings: the chosen
 * federation as a card (short id and guardians) with
 * "Scan new invite" and "Paste invite", or an empty state whose one loud
 * action is the scan. A scanned or pasted invite is handed to `onChoose`,
 * which saves it: choosing is the operator's decision, so there is no second
 * SAVE press.
 */
import React, {useState} from 'react';
import {Pressable, StyleSheet, Text, TextInput, View} from 'react-native';

import {normalizeInvite} from './federation';
import {ScanInviteSheet} from './ScanInvite';
import {SETUP} from './strings';
import {Card, Divider, FONT, PrimaryButton, styles, useTheme} from './ui';
import type {FederationView} from './useFederation';

export function FederationPicker({
  fed,
  hasInvite,
  onChoose,
  testID = 'federation',
}: {
  fed: FederationView;
  /** Whether a valid invite is chosen; the empty state otherwise. */
  hasInvite: boolean;
  onChoose: (invite: string) => void;
  testID?: string;
}) {
  const t = useTheme();
  const s = styles(t);
  const [pasteOpen, setPasteOpen] = useState(false);
  const [pasted, setPasted] = useState('');
  const [scanning, setScanning] = useState(false);
  const pastedInvite = normalizeInvite(pasted);

  const choose = (invite: string) => {
    setPasteOpen(false);
    setPasted('');
    onChoose(invite);
  };

  return (
    <>
      {hasInvite ? (
        <Card testID={`${testID}-card`}>
          <View style={local.body}>
            <Text
              style={[local.title, {color: t.ink}]}
              numberOfLines={1}
              testID={`${testID}-title`}>
              {fed.title}
            </Text>
            <Text style={[local.small, {color: t.dim}]} numberOfLines={1}>
              {fed.detail}
            </Text>
          </View>
          <Divider />
          <View style={local.actions}>
            <Pressable
              onPress={() => setScanning(true)}
              style={({pressed}) => [local.action, pressed && {backgroundColor: t.raised}]}
              accessibilityRole="button"
              testID={`${testID}-scan`}>
              <Text style={[local.actionText, {color: t.accent}]}>{SETUP.scanNew}</Text>
            </Pressable>
            <View style={[local.vline, {backgroundColor: t.line}]} />
            <Pressable
              onPress={() => setPasteOpen(v => !v)}
              style={({pressed}) => [local.action, pressed && {backgroundColor: t.raised}]}
              accessibilityRole="button"
              accessibilityState={{expanded: pasteOpen}}
              testID={`${testID}-paste`}>
              <Text style={[local.actionText, {color: t.accent}]}>{SETUP.paste}</Text>
            </Pressable>
          </View>
        </Card>
      ) : (
        <Card testID={`${testID}-empty`}>
          <View style={local.empty}>
            <Text style={[local.emptyTitle, {color: t.ink}]}>{SETUP.emptyTitle}</Text>
            <Text style={[local.small, {color: t.dim}]}>{SETUP.emptyBody}</Text>
            <PrimaryButton
              title={SETUP.scanFederation}
              onPress={() => setScanning(true)}
              testID={`${testID}-scan`}
            />
            <Pressable
              onPress={() => setPasteOpen(v => !v)}
              hitSlop={8}
              accessibilityRole="button"
              accessibilityState={{expanded: pasteOpen}}
              testID={`${testID}-paste`}>
              <Text style={[local.link, {color: t.accent}]}>{SETUP.pasteInstead}</Text>
            </Pressable>
          </View>
        </Card>
      )}
      {pasteOpen ? (
        <Card>
          <View style={local.pasteBox}>
            <TextInput
              value={pasted}
              onChangeText={setPasted}
              placeholder={SETUP.customPlaceholder}
              placeholderTextColor={t.dim}
              autoCapitalize="none"
              autoCorrect={false}
              autoFocus
              keyboardType="url"
              style={[s.input, local.pasteInput]}
              testID={`${testID}-custom-input`}
            />
            {pasted.trim() !== '' && !pastedInvite ? (
              <Text style={[local.small, {color: t.danger}]}>{SETUP.notInvite}</Text>
            ) : null}
            <PrimaryButton
              title={SETUP.useInvite}
              disabled={!pastedInvite}
              onPress={() => pastedInvite && choose(pastedInvite)}
              testID={`${testID}-use`}
            />
          </View>
        </Card>
      ) : null}
      <ScanInviteSheet
        visible={scanning}
        onClose={() => setScanning(false)}
        onInvite={found => {
          setScanning(false);
          choose(found);
        }}
      />
    </>
  );
}

const local = StyleSheet.create({
  body: {paddingHorizontal: 18, paddingVertical: 12, gap: 2},
  title: {fontFamily: FONT.semibold, fontSize: 20, letterSpacing: -0.3},
  small: {fontFamily: FONT.regular, fontSize: 13, lineHeight: 19},
  actions: {flexDirection: 'row'},
  action: {flex: 1, alignItems: 'center', paddingVertical: 12},
  actionText: {fontFamily: FONT.semibold, fontSize: 15},
  vline: {width: StyleSheet.hairlineWidth},
  empty: {padding: 18, gap: 12},
  emptyTitle: {fontFamily: FONT.semibold, fontSize: 18},
  link: {fontFamily: FONT.semibold, fontSize: 15, textAlign: 'center', paddingVertical: 4},
  pasteBox: {padding: 14, gap: 10},
  pasteInput: {minHeight: 48},
});
