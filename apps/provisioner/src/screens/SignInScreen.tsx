/**
 * Operator sign-in. Keep the action above the keyboard and the form scrollable.
 *
 * Ported from cashucard's `SignInScreen` with the `env` split dropped: this app
 * has one service (the hosted registry, plan § 4), so there is no factory/issued
 * branch to select and no scope label to show. The load line names the build the
 * operator is holding, which is the only thing worth stating before sign-in.
 */
import React, {useEffect, useRef, useState} from 'react';
import {
  Keyboard,
  KeyboardAvoidingView,
  Platform,
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
import {
  FONT,
  PrimaryButton,
  Screen,
  signInReason,
  styles,
  useTheme,
  wasCancelled,
} from '../ui';

export function SignInScreen() {
  const app = useStore();
  const t = useTheme();
  const s = styles(t);
  const [name, setName] = useState('');
  const [password, setPassword] = useState('');
  const [failed, setFailed] = useState<string | null>(null);
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
          <Text style={[local.wordmark, {color: t.ink}]}>{'Fedimint'}</Text>
          <Text style={[local.wordmark, {color: t.ink}]}>{'card'}</Text>
          <Text style={[local.loadSha, {color: t.dim}]}>{LOAD_LINE}</Text>
          <Text style={[local.tagline, {color: t.dim}]}>{SIGNIN.tagline}</Text>
          {LOCAL_SESSION_ENABLED && __DEV__ ? (
            // A fake session must never look like a real one: this line is
            // what stops a walked-through batch from being read as registered.
            <Text
              style={[local.tagline, {color: t.dim}]}
              accessibilityLiveRegion="polite"
              testID="local-session-label">
              {LOCAL_SESSION_LABEL}
            </Text>
          ) : null}
          <View style={local.form}>
            <TextInput
              value={name}
              onChangeText={setName}
              placeholder={SIGNIN.namePlaceholder}
              placeholderTextColor={t.dim}
              autoCapitalize="none"
              autoCorrect={false}
              editable={!busy}
              style={s.input}
              returnKeyType="next"
              blurOnSubmit={false}
              onSubmitEditing={() => passwordInput.current?.focus()}
              autoComplete="username"
              textContentType="username"
              accessibilityLabel="Operator name"
              testID="sign-in-name"
            />
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
              style={s.input}
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
        </ScrollView>
        <View style={local.footer}>
          <PrimaryButton
            title={busy ? SIGNIN.signingIn : SIGNIN.button}
            onPress={() => {
              void attempt();
            }}
            disabled={!ready}
            testID="sign-in"
          />
          {failed ? (
            <Text style={s.failed} testID="sign-in-failed">
              {failed}
            </Text>
          ) : (
            <Text style={s.note} accessibilityLiveRegion="polite">
              {stopped
                ? 'Sign-in stopped.'
                : busy
                ? 'Checking your account… Keep this screen open.'
                : SIGNIN.note}
            </Text>
          )}
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
    alignItems: 'center',
    gap: 14,
    paddingHorizontal: 24,
    paddingVertical: 16,
  },
  wordmark: {
    fontFamily: FONT.semibold,
    fontSize: 34,
    textAlign: 'center',
    lineHeight: 38,
  },
  loadSha: {
    fontFamily: FONT.tabular,
    fontVariant: ['tabular-nums'],
    fontSize: 12,
    letterSpacing: 0.4,
  },
  tagline: {
    fontFamily: FONT.regular,
    fontSize: 15,
    lineHeight: 21,
    textAlign: 'center',
    maxWidth: 280,
  },
  form: {alignSelf: 'stretch', gap: 10, paddingHorizontal: 8},
  footer: {gap: 12, paddingBottom: 8},
});
