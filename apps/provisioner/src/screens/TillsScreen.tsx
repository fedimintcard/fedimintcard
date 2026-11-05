/**
 * Settings → Tills (docs/TILL-CLAIM.md): the operator's tills, a Stop on each
 * behind a confirm, and Add a till — scan the till's QR or type its code, see
 * the name it asked under, approve. The steps are `tillClaim.ts`'s reducer;
 * this file draws them.
 */
import React, {useCallback, useEffect, useReducer, useState} from 'react';
import {ScrollView, StyleSheet, Text, TextInput, View} from 'react-native';

import type {TillRow} from '../ops';
import {ScanSheet, type ScanWords} from '../ScanInvite';
import {SCAN, SETTINGS, TILLS} from '../strings';
import {useStore} from '../store';
import {
  ADD_START,
  addReducer,
  parseClaimText,
  readTypedClaim,
  registryLabel,
  tillFailure,
  whenText,
  type AddProblem,
  type ClaimText,
} from '../tillClaim';
import {
  Card,
  Dim,
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

const TILL_WORDS: ScanWords = {
  title: SCAN.tillTitle,
  hint: SCAN.tillHint,
  rejected: SCAN.notTill,
  noPermission: SCAN.tillNoPermission,
  noCamera: SCAN.tillNoCamera,
};

type ListState =
  | {readonly kind: 'loading'}
  | {readonly kind: 'loaded'; readonly tills: readonly TillRow[]}
  | {readonly kind: 'failed'; readonly problem: AddProblem};

export function problemText(problem: AddProblem, current: string): string {
  switch (problem.kind) {
    case 'badCode':
      return TILLS.problems.badCode;
    case 'otherRegistry':
      return TILLS.problems.otherRegistry(registryLabel(problem.asked), registryLabel(current));
    case 'other':
      return TILLS.problems.other(problem.detail);
    default:
      return TILLS.problems[problem.kind];
  }
}

export function TillsScreen({onBack}: {onBack: () => void}) {
  const {tills, serviceUrl} = useStore();
  const t = useTheme();
  const s = styles(t);

  const [mode, setMode] = useState<'list' | 'add'>('list');
  const [list, setList] = useState<ListState>({kind: 'loading'});
  const [confirming, setConfirming] = useState<number | null>(null);
  const [revoking, setRevoking] = useState<number | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [add, dispatch] = useReducer(addReducer, ADD_START);
  const [typed, setTyped] = useState('');
  const [scanning, setScanning] = useState(false);
  const [now, setNow] = useState(() => Date.now());

  const load = useCallback(() => {
    let live = true;
    setList({kind: 'loading'});
    tills
      .listTills()
      .then(rows => {
        if (live) {
          setNow(Date.now());
          setList({kind: 'loaded', tills: rows});
        }
      })
      .catch((e: unknown) => {
        if (live) {
          const kind = tillFailure(e, 'list');
          setList({
            kind: 'failed',
            problem: kind === 'other' ? {kind, detail: errorDetail(e)} : {kind},
          });
        }
      });
    return () => {
      live = false;
    };
  }, [tills]);

  useEffect(() => (mode === 'list' ? load() : undefined), [mode, load]);

  // Look the code up as soon as the reducer has one to check.
  const checkingCode = add.step === 'checking' ? add.code : null;
  useEffect(() => {
    if (checkingCode === null) {
      return;
    }
    let live = true;
    tills
      .claimInfo(checkingCode)
      .then(claim => {
        if (live) {
          setNow(Date.now());
          dispatch({type: 'found', claim});
        }
      })
      .catch((e: unknown) => {
        if (live) {
          dispatch({type: 'failed', failure: tillFailure(e, 'claim'), detail: errorDetail(e)});
        }
      });
    return () => {
      live = false;
    };
  }, [checkingCode, tills]);

  const approvingCode = add.step === 'approving' ? add.code : null;
  useEffect(() => {
    if (approvingCode === null) {
      return;
    }
    let live = true;
    tills
      .approveClaim(approvingCode)
      .then(() => live && dispatch({type: 'approved'}))
      .catch((e: unknown) => {
        if (live) {
          dispatch({type: 'failed', failure: tillFailure(e, 'claim'), detail: errorDetail(e)});
        }
      });
    return () => {
      live = false;
    };
  }, [approvingCode, tills]);

  const submit = (claim: ClaimText) => dispatch({type: 'submit', claim});

  const openAdd = () => {
    setNote(null);
    setConfirming(null);
    setTyped('');
    dispatch({type: 'restart'});
    setMode('add');
  };
  const backToList = () => {
    setScanning(false);
    setMode('list');
  };

  const revoke = (till: TillRow) => {
    setRevoking(till.till_id);
    tills
      .revokeTill(till.till_id)
      .then(() => {
        setConfirming(null);
        setNote(TILLS.revokedNote(till.name));
        load();
      })
      .catch((e: unknown) => {
        const kind = tillFailure(e, 'revoke');
        setNote(problemText(kind === 'other' ? {kind, detail: errorDetail(e)} : {kind}, serviceUrl));
      })
      .finally(() => setRevoking(null));
  };

  if (mode === 'add') {
    return (
      <Screen title={TILLS.add} onBack={add.step === 'approving' ? undefined : backToList}>
        <ScrollView
          contentContainerStyle={local.scroll}
          keyboardShouldPersistTaps="handled"
          testID="tills-add">
          {add.step === 'enter' ? (
            <View style={local.section}>
              <Dim>{TILLS.addIntro}</Dim>
              {add.problem ? (
                <Text style={[local.problem, {color: t.danger}]} testID="tills-problem">
                  {problemText(add.problem, serviceUrl)}
                </Text>
              ) : null}
              <PrimaryButton
                title={TILLS.scan}
                onPress={() => setScanning(true)}
                testID="tills-scan"
              />
              <Text style={[local.label, {color: t.dim}]}>{TILLS.codeLabel}</Text>
              <TextInput
                value={typed}
                onChangeText={setTyped}
                placeholder={TILLS.codePlaceholder}
                placeholderTextColor={t.dim}
                autoCapitalize="characters"
                autoCorrect={false}
                style={[s.input, local.code]}
                returnKeyType="go"
                onSubmitEditing={() => typed.trim() && submit(readTypedClaim(typed, serviceUrl))}
                testID="tills-code"
              />
              <SecondaryButton
                title={TILLS.check}
                disabled={typed.trim() === ''}
                onPress={() => submit(readTypedClaim(typed, serviceUrl))}
                testID="tills-check"
              />
            </View>
          ) : null}

          {add.step === 'checking' ? <Dim testID="tills-checking">{TILLS.checking}</Dim> : null}

          {add.step === 'review' || add.step === 'approving' ? (
            <View style={local.section}>
              <SectionHeader>{TILLS.reviewTitle}</SectionHeader>
              <Card>
                <ValueRow label={TILLS.name} value={add.claim.name} valueTone="ink" testID="tills-claim-name" />
                <Divider />
                <ValueRow label={TILLS.asked} value={whenText(add.claim.created_at, now)} />
                <Divider />
                <ValueRow label={TILLS.codeLabel} value={add.code} />
              </Card>
              <Dim>{TILLS.reviewBody}</Dim>
              <PrimaryButton
                title={add.step === 'approving' ? TILLS.approving : TILLS.approve}
                disabled={add.step === 'approving'}
                onPress={() => dispatch({type: 'approve'})}
                testID="tills-approve"
              />
              <SecondaryButton
                title={TILLS.cancel}
                disabled={add.step === 'approving'}
                onPress={backToList}
                testID="tills-cancel"
              />
            </View>
          ) : null}

          {add.step === 'done' ? (
            <View style={local.section}>
              <Text style={[local.doneTitle, {color: t.ink}]} testID="tills-done">
                {TILLS.doneTitle(add.name)}
              </Text>
              <Dim>{TILLS.doneBody}</Dim>
              <PrimaryButton title={TILLS.done} onPress={backToList} testID="tills-done-button" />
            </View>
          ) : null}
        </ScrollView>
        <ScanSheet<ClaimText>
          visible={scanning}
          onClose={() => setScanning(false)}
          read={text => parseClaimText(text, serviceUrl)}
          onResult={claim => {
            setScanning(false);
            submit(claim);
          }}
          words={TILL_WORDS}
          testID="scan-till"
        />
      </Screen>
    );
  }

  return (
    <Screen title={TILLS.title} onBack={onBack}>
      <ScrollView contentContainerStyle={local.scroll} testID="tills-list">
        <View style={local.section}>
          <SectionHeader help={SETTINGS.tillsHelp} testID="tills-help">
            {TILLS.yours}
          </SectionHeader>
          {list.kind === 'loading' ? <Dim testID="tills-loading">{TILLS.loading}</Dim> : null}
          {list.kind === 'failed' ? (
            <>
              <Text style={[local.problem, {color: t.danger}]} testID="tills-list-failed">
                {`${TILLS.listFailed} ${problemText(list.problem, serviceUrl)}`}
              </Text>
              <SecondaryButton title={TILLS.retry} onPress={load} testID="tills-retry" />
            </>
          ) : null}
          {list.kind === 'loaded' && list.tills.length === 0 ? (
            <Dim testID="tills-none">{TILLS.none}</Dim>
          ) : null}
          {list.kind === 'loaded' && list.tills.length > 0 ? (
            <Card>
              {list.tills.map((till, i) => (
                <View key={till.till_id}>
                  {i > 0 ? <Divider /> : null}
                  <View style={local.till} testID={`till-${till.till_id}`}>
                    <View style={local.grow}>
                      <Text style={[local.tillName, {color: till.revoked_at === null ? t.ink : t.dim}]}>
                        {till.name}
                      </Text>
                      <Text style={[local.small, {color: t.dim}]}>
                        {till.revoked_at === null
                          ? TILLS.added(whenText(till.created_at, now))
                          : TILLS.revoked(whenText(till.revoked_at, now))}
                      </Text>
                    </View>
                    {till.revoked_at === null && confirming !== till.till_id ? (
                      <SecondaryButton
                        title={TILLS.revoke}
                        onPress={() => {
                          setNote(null);
                          setConfirming(till.till_id);
                        }}
                        testID={`till-${till.till_id}-stop`}
                      />
                    ) : null}
                  </View>
                  {confirming === till.till_id ? (
                    <View style={local.confirm}>
                      <Text style={[local.confirmTitle, {color: t.ink}]}>
                        {TILLS.revokeTitle(till.name)}
                      </Text>
                      <Dim>{TILLS.revokeBody}</Dim>
                      <PrimaryButton
                        title={TILLS.revokeConfirm}
                        danger
                        disabled={revoking !== null}
                        onPress={() => revoke(till)}
                        testID={`till-${till.till_id}-confirm`}
                      />
                      <SecondaryButton
                        title={TILLS.keep}
                        disabled={revoking !== null}
                        onPress={() => setConfirming(null)}
                        testID={`till-${till.till_id}-keep`}
                      />
                    </View>
                  ) : null}
                </View>
              ))}
            </Card>
          ) : null}
          {note ? (
            <Text style={[local.small, {color: t.dim}]} testID="tills-note" accessibilityLiveRegion="polite">
              {note}
            </Text>
          ) : null}
        </View>
        <PrimaryButton title={TILLS.add} onPress={openAdd} testID="tills-add-entry" />
      </ScrollView>
    </Screen>
  );
}

/** The service's own words, for a failure the screen has no plain sentence for. */
function errorDetail(e: unknown): string | undefined {
  return e instanceof Error && e.name === 'ApiServiceError' ? e.message : undefined;
}

const local = StyleSheet.create({
  grow: {flex: 1},
  scroll: {gap: 24, paddingTop: 4, paddingBottom: 40},
  section: {gap: 12},
  small: {fontFamily: FONT.regular, fontSize: 13, lineHeight: 19},
  label: {fontFamily: FONT.medium, fontSize: 13, marginTop: 8},
  code: {fontFamily: FONT.semibold, fontSize: 20, letterSpacing: 2, textAlign: 'center'},
  problem: {fontFamily: FONT.regular, fontSize: 14, lineHeight: 20},
  till: {flexDirection: 'row', alignItems: 'center', gap: 12, paddingHorizontal: 16, paddingVertical: 12},
  tillName: {fontFamily: FONT.semibold, fontSize: 16},
  confirm: {padding: 14, gap: 10},
  confirmTitle: {fontFamily: FONT.semibold, fontSize: 15},
  doneTitle: {fontFamily: FONT.semibold, fontSize: 20},
});
