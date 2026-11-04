/**
 * Operator sign-in. Keep the action above the keyboard and the form scrollable.
 *
 * Ported from cashucard's `SignInScreen` with the `env` split dropped: this app
 * has one service (the hosted registry, plan § 4), so there is no factory/issued
 * branch to select and no scope label to show. The load line names the build the
 * operator is holding; it lives in the (i) panel beside the heading, as the
 * Cashu provisioner's sign-in keeps its build line.
 */
import React, {useEffect, useRef, useState} from 'react';
import {
  ActivityIndicator,
  Keyboard,
  KeyboardAvoidingView,
  Platform,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  View,
} from 'react-native';

import {LOCAL_SESSION_ENABLED, LOCAL_SESSION_LABEL} from '../localSession';
import {LOAD_LINE} from '../settings';
import {SIGNIN} from '../strings';
import {useStore} from '../store';
import {BrandLockup} from '../BrandMark';
import {
  Card,
  EntryRow,
  FONT,
  InfoButton,
  Screen,
  signInReason,
  styles,
  useTheme,
  wasCancelled,
} from '../ui';

export function SignInScreen({onDemo}: {onDemo?: () => void} = {}) {
  const app = useStore();
  const t = useTheme();
  const s = styles(t);
  const [name, setName] = useState('');
  const [password, setPassword] = useState('');
  const [failed, setFailed] = useState<string | null>(null);
  const [focused, setFocused] = useState<'name' | 'password' | null>(null);
  const focusRing = {borderColor: t.accent, backgroundColor: t.raised};
  const [busy, setBusy] = useState(false);
  const [stopped, setStopped] = useState(false);
  const passwordInput = useRef<TextInput>(null);
  const request = useRef<AbortController | null>(null);

  // A sign-in in flight when the screen goes away is aborted: the operator
  // stopped asking.
  useEffect(
    () => () => {
      request.current?.abort();
      request.current = null;
    },
    [],
  );

  const stop = () => {
    request.current?.abort();
    request.current = null;
    setBusy(false);
    setPassword('');
    setFailed(null);
    setStopped(true);
  };

  const attempt = async () => {
    if (busy || name.trim() === '' || password === '') {
      return;
    }
    Keyboard.dismiss();
    const controller = new AbortController();
    request.current = controller;
    setBusy(true);
    setStopped(false);
    setFailed(null);
    try {
      // The operator's name and password are typed here and held in memory
      // only: the password is the operator's one secret, never stored by the
      // app (the service keeps its scrypt hash).
      await app.signIn(name.trim(), password, {signal: controller.signal});
    } catch (e) {
      if (request.current !== controller || wasCancelled(e)) {
        return;
      }
      setFailed(SIGNIN.failed(signInReason(e)));
    } finally {
      if (request.current === controller) {
        request.current = null;
        setBusy(false);
      }
    }
  };

  const ready = !busy && name.trim() !== '' && password !== '';

  return (
    <KeyboardAvoidingView
      style={local.grow}
      enabled={Platform.OS === 'ios'}
      behavior={Platform.OS === 'ios' ? 'padding' : undefined}>
      <Screen onStop={busy ? stop : undefined}>
        <ScrollView
          style={local.grow}
          contentContainerStyle={local.center}
          keyboardShouldPersistTaps="handled"
          keyboardDismissMode="on-drag">
          <BrandLockup size={52} />
          <View style={local.intro}>
            <Text style={[local.eyebrow, {color: t.dim}]}>{SIGNIN.eyebrow}</Text>
            <View style={local.headingRow}>
              <Text
                style={[local.heading, {color: t.ink}]}
                accessibilityRole="header">
                {SIGNIN.heading}
              </Text>
              <InfoButton title={SIGNIN.helpTitle} testID="sign-in-help">
                {SIGNIN.help(LOAD_LINE)}
              </InfoButton>
            </View>
            {LOCAL_SESSION_ENABLED && __DEV__ ? (
              // A fake session must never look like a real one: this line is
              // what stops a walked-through batch from being read as registered.
              <Text
                style={[local.devLabel, {color: t.danger, borderColor: t.danger}]}
                accessibilityLiveRegion="polite"
                testID="local-session-label">
                {LOCAL_SESSION_LABEL}
              </Text>
            ) : null}
          </View>
          <View style={local.form}>
            <View style={local.field}>
              <Text style={[local.label, {color: t.ink}]}>{SIGNIN.nameLabel}</Text>
              <TextInput
                value={name}
                onChangeText={setName}
                placeholder={SIGNIN.namePlaceholder}
                placeholderTextColor={t.dim}
                autoCapitalize="none"
                autoCorrect={false}
                editable={!busy}
                style={[s.input, local.input, focused === 'name' && focusRing]}
                onFocus={() => setFocused('name')}
                onBlur={() => setFocused(null)}
                returnKeyType="next"
                blurOnSubmit={false}
                onSubmitEditing={() => passwordInput.current?.focus()}
                autoComplete="username"
                textContentType="username"
                accessibilityLabel="Operator name"
                testID="sign-in-name"
              />
            </View>
            <View style={local.field}>
              <Text style={[local.label, {color: t.ink}]}>
                {SIGNIN.passwordLabel}
              </Text>
              <TextInput
                ref={passwordInput}
                value={password}
                onChangeText={setPassword}
                placeholder={SIGNIN.passwordPlaceholder}
                placeholderTextColor={t.dim}
                autoCapitalize="none"
                autoCorrect={false}
                secureTextEntry
                editable={!busy}
                style={[s.input, local.input, focused === 'password' && focusRing]}
                onFocus={() => setFocused('password')}
                onBlur={() => setFocused(null)}
                returnKeyType="go"
                onSubmitEditing={() => {
                  void attempt();
                }}
                autoComplete="current-password"
                textContentType="password"
                accessibilityLabel="Password"
                testID="sign-in-password"
              />
            </View>
            {failed ? (
              <Text
                style={[local.status, {color: t.danger}]}
                accessibilityLiveRegion="assertive"
                testID="sign-in-failed">
                {failed}
              </Text>
            ) : busy || stopped ? (
              <Text
                style={[local.status, {color: t.dim}]}
                accessibilityLiveRegion="polite">
                {stopped ? SIGNIN.stopped : SIGNIN.checking}
              </Text>
            ) : null}
          </View>
        </ScrollView>
        <View style={local.footer}>
          <Pressable
            testID="sign-in"
            accessibilityRole="button"
            accessibilityLabel={busy ? SIGNIN.signingIn : SIGNIN.button}
            accessibilityState={{disabled: !ready, busy}}
            disabled={!ready}
            onPress={() => {
              void attempt();
            }}
            style={({pressed}) => [
              local.submit,
              {backgroundColor: ready || busy ? t.accent : t.raised},
              pressed && local.pressed,
            ]}>
            <Text
              style={[local.submitText, {color: ready || busy ? t.onAccent : t.dim}]}>
              {busy ? SIGNIN.signingIn : SIGNIN.button}
            </Text>
            {busy ? (
              <ActivityIndicator color={t.onAccent} />
            ) : (
              <Text
                style={[local.submitArrow, {color: ready ? t.onAccent : t.dim}]}>
                →
              </Text>
            )}
          </Pressable>
          {/* No account, no card, no registry — the reviewer's way in
              (plan § 7, Guideline 2.1). A row, not a button: it opens a
              screen, and the demo chooser draws its own entries this way. */}
          {onDemo ? (
            <Card>
              <EntryRow
                title={SIGNIN.demo}
                subtitle={SIGNIN.demoSub}
                onPress={onDemo}
                disabled={busy}
                testID="sign-in-demo"
              />
            </Card>
          ) : null}
        </View>
      </Screen>
    </KeyboardAvoidingView>
  );
}

const local = StyleSheet.create({
  grow: {flex: 1},
  center: {
    flexGrow: 1,
    justifyContent: 'center',
    width: '100%',
    maxWidth: 440,
    alignSelf: 'center',
    gap: 30,
    paddingHorizontal: 4,
    paddingTop: 28,
    paddingBottom: 16,
  },
  intro: {gap: 8},
  eyebrow: {
    fontFamily: FONT.semibold,
    fontSize: 12,
    letterSpacing: 2,
    textTransform: 'uppercase',
  },
  headingRow: {flexDirection: 'row', alignItems: 'center', gap: 8},
  heading: {
    flex: 1,
    fontFamily: FONT.semibold,
    fontSize: 36,
    lineHeight: 44,
    letterSpacing: -1.2,
  },
  devLabel: {
    alignSelf: 'flex-start',
    fontFamily: FONT.semibold,
    fontSize: 11,
    letterSpacing: 1,
    borderWidth: 1,
    borderRadius: 6,
    paddingHorizontal: 8,
    paddingVertical: 3,
    overflow: 'hidden',
  },
  form: {gap: 18},
  field: {gap: 8},
  label: {fontFamily: FONT.medium, fontSize: 13},
  input: {minHeight: 54, borderRadius: 12, paddingHorizontal: 15},
  status: {fontFamily: FONT.regular, fontSize: 14, lineHeight: 20},
  footer: {
    width: '100%',
    maxWidth: 440,
    alignSelf: 'center',
    gap: 12,
    paddingBottom: 10,
  },
  submit: {
    minHeight: 58,
    paddingHorizontal: 22,
    borderRadius: 14,
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
  },
  submitText: {fontFamily: FONT.semibold, fontSize: 17},
  submitArrow: {fontFamily: FONT.medium, fontSize: 24, marginTop: -2},
  pressed: {opacity: 0.85},
});
