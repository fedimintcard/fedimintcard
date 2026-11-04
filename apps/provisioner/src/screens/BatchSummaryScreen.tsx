/**
 * S4. Batch summary and the record.
 *
 * Ported from cashucard's `BatchSummaryScreen` with plan § 5's trim: the rows
 * and the pending warning stay, and share/export goes (there is no
 * `Share` sheet here: the record is the registry's, and a screenshot of this
 * screen is not a stronger artefact than the rows the service holds).
 *
 * Three figures over one grouped card, per-row state dots and tabular ids, and
 * the two actions pinned in a footer. The numbers are the store's
 * `summary`, not a count taken here: two live versions of "programmed" is the
 * bug this screen exists to avoid.
 *
 * A generated per-card PIN is shown here once, in the row it belongs to, and is
 * not retrievable after the batch ends (`pin.ts`'s reveal slot, wiped by
 * `actions.end`).
 */
import React from 'react';
import {ScrollView, StyleSheet, Text, View} from 'react-native';

import {maskedPin, usePinReveal} from '../pin';
import {SUMMARY} from '../strings';
import {type RecordRow, useBatch} from '../store';
import {
  Card,
  Divider,
  FONT,
  PrimaryButton,
  Screen,
  SectionHeader,
  SecondaryButton,
  useTheme,
} from '../ui';

function timeOf(at: number): string {
  return new Date(at).toISOString().slice(11, 19);
}

export function BatchSummaryScreen({
  onBack,
  onClose,
}: {
  onBack: () => void;
  onClose: () => void;
}) {
  const batch = useBatch();
  const reveal = usePinReveal();
  const t = useTheme();
  const [pendingConfirm, setPendingConfirm] = React.useState(false);

  if (!batch) {
    return null;
  }
  const {summary, records, config, phase} = batch;
  const left = Math.max(0, config.count - records.length);
  // The one generated PIN this batch may reveal: per-card policy only, and only
  // until the batch ends.
  const shown = config.pinPolicy === 'perCard' ? reveal.reveal() : null;

  const stopped = summary.failed + summary.empty + summary.unspendable;
  const closing = summary.pending > 0 && pendingConfirm;
  return (
    <Screen title={SUMMARY.title} onBack={phase !== 'ended' ? onBack : undefined}>
      <ScrollView contentContainerStyle={local.scroll} testID="summary-scroll">
        <Text style={[local.headline, {color: t.ink}]} accessibilityRole="header">
          {SUMMARY.headline(summary.spendable, config.count)}
        </Text>
        <View style={local.stats}>
          <Stat value={summary.spendable} label={SUMMARY.programmed} tone="accent" />
          <Stat value={stopped} label={SUMMARY.stopped} tone={stopped > 0 ? 'danger' : 'ink'} />
          <Stat value={summary.pending} label={SUMMARY.pending} tone="ink" />
        </View>
        {shown ? (
          <View
            style={[local.pinCard, {backgroundColor: t.card, borderColor: t.accent}]}
            testID="summary-pin">
            <View style={local.pinText}>
              <Text style={[local.pinLabel, {color: t.ink}]}>{SUMMARY.pinTitle}</Text>
              <Text style={[local.small, {color: t.dim}]}>{SUMMARY.pinBody}</Text>
            </View>
            <Text style={[local.pinValue, {color: t.accent}]} testID="summary-pin-value">
              {maskedPin(shown.pin)}
            </Text>
          </View>
        ) : null}
        <View style={local.section}>
          <SectionHeader help={SUMMARY.recordHelp} testID="summary-record-help">
            {SUMMARY.record}
          </SectionHeader>
          <Card>
            {records.length === 0 ? (
              <View style={local.emptyRow}>
                <Text style={[local.small, {color: t.dim}]}>{SUMMARY.empty}</Text>
              </View>
            ) : (
              records.map((r, i) => (
                <React.Fragment key={r.id}>
                  {i > 0 ? <Divider /> : null}
                  <Row row={r} n={i + 1} />
                </React.Fragment>
              ))
            )}
          </Card>
          {left > 0 ? (
            <Text style={[local.small, {color: t.dim}]} testID="summary-not-reached">
              {SUMMARY.notReached(left)}
            </Text>
          ) : null}
        </View>
      </ScrollView>
      <View style={local.footer}>
        {closing ? (
          <Text style={[local.small, {color: t.danger}]} testID="close-pending-note">
            {SUMMARY.pendingNote}
          </Text>
        ) : summary.pending > 0 ? (
          <Text style={[local.small, {color: t.dim}]} testID="summary-pending-note">
            {SUMMARY.pendingRetry}
          </Text>
        ) : null}
        <PrimaryButton
          title={closing ? SUMMARY.closeAnyway : SUMMARY.close}
          danger={closing}
          testID="close-batch"
          onPress={() => {
            // Pending rows re-post on their own; ending a batch with pending
            // rows says so plainly first — a confirm, not a block.
            if (summary.pending > 0 && !pendingConfirm) {
              setPendingConfirm(true);
              return;
            }
            batch.actions.end();
            onClose();
          }}
        />
        {/* Back to the batch is only honest while it still accepts cards: once
            ended, its numbers are frozen and its PIN slot is wiped. */}
        {phase !== 'ended' ? (
          <SecondaryButton
            title={left > 0 ? SUMMARY.backToLoop : SUMMARY.backToCards}
            testID="summary-back"
            onPress={onBack}
          />
        ) : null}
      </View>
    </Screen>
  );
}

/** One figure of the header: the number over its name, in a tile. */
function Stat({
  value,
  label,
  tone,
}: {
  value: number;
  label: string;
  tone: 'accent' | 'danger' | 'ink';
}) {
  const t = useTheme();
  const color = tone === 'accent' ? t.accent : tone === 'danger' ? t.danger : t.ink;
  return (
    <View style={[local.stat, {backgroundColor: t.card, borderColor: t.line}]}>
      <Text style={[local.statFigure, {color}]}>{value}</Text>
      <Text
        style={[local.statLabel, {color: t.dim}]}
        numberOfLines={1}
        adjustsFontSizeToFit
        minimumFontScale={0.75}>
        {label}
      </Text>
    </View>
  );
}

/** One record row: a state dot, the card's number and short id, the time, and a pending flag. */
function Row({row, n}: {row: RecordRow; n: number}) {
  const t = useTheme();
  return (
    <View style={local.row}>
      <View
        style={[
          local.dot,
          row.pending
            ? {borderWidth: 1.5, borderColor: t.accent}
            : {backgroundColor: row.ok ? t.accent : t.danger},
        ]}
      />
      <Text style={[local.rowId, {color: t.ink}]}>{SUMMARY.cardRow(n, row.id)}</Text>
      <Text style={[local.rowAt, {color: row.pending ? t.accent : t.dim}]}>
        {row.pending ? SUMMARY.pending : timeOf(row.at)}
      </Text>
    </View>
  );
}

const local = StyleSheet.create({
  scroll: {gap: 20, paddingTop: 4, paddingBottom: 24, flexGrow: 1},
  headline: {fontFamily: FONT.semibold, fontSize: 24, letterSpacing: -0.4},
  stats: {flexDirection: 'row', gap: 10},
  stat: {
    flex: 1,
    borderRadius: 16,
    borderWidth: StyleSheet.hairlineWidth,
    paddingVertical: 16,
    paddingHorizontal: 12,
    gap: 2,
  },
  statFigure: {fontFamily: FONT.semibold, fontSize: 32, fontVariant: ['tabular-nums']},
  statLabel: {fontFamily: FONT.medium, fontSize: 13},
  section: {gap: 10},
  small: {fontFamily: FONT.regular, fontSize: 13, lineHeight: 19},
  pinCard: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: 14,
    padding: 16,
    borderRadius: 16,
    borderWidth: 1.5,
  },
  pinText: {flex: 1, gap: 4},
  pinLabel: {fontFamily: FONT.semibold, fontSize: 16},
  pinValue: {fontFamily: FONT.tabular, fontSize: 22, letterSpacing: 4},
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: 10,
    paddingHorizontal: 16,
    paddingVertical: 13,
    minHeight: 46,
  },
  dot: {width: 9, height: 9, borderRadius: 5},
  rowId: {fontFamily: FONT.medium, fontSize: 15, flex: 1},
  rowAt: {fontFamily: FONT.tabular, fontVariant: ['tabular-nums'], fontSize: 13},
  emptyRow: {paddingHorizontal: 16, paddingVertical: 14},
  footer: {gap: 10, paddingBottom: 8, paddingTop: 8},
});
