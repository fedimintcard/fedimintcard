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
  Dim,
  FONT,
  PrimaryButton,
  Screen,
  SectionHeader,
  SecondaryButton,
  styles,
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
  const s = styles(t);
  const [pendingConfirm, setPendingConfirm] = React.useState(false);

  if (!batch) {
    return null;
  }
  const {summary, records, config, phase} = batch;
  const left = Math.max(0, config.count - records.length);
  // The one generated PIN this batch may reveal: per-card policy only, and only
  // until the batch ends.
  const shown = config.pinPolicy === 'perCard' ? reveal.reveal() : null;

  return (
    <Screen title={SUMMARY.title} onBack={onBack}>
      <ScrollView contentContainerStyle={local.scroll} testID="summary-scroll">
        <View style={local.stats}>
          <Stat value={summary.spendable} label={SUMMARY.programmed} accent />
          <Stat
            value={summary.failed + summary.empty + summary.unspendable}
            label={SUMMARY.stopped}
          />
          <Stat value={summary.pending} label={SUMMARY.pending} />
        </View>

        {shown ? (
          <Card testID="summary-pin">
            <View style={local.pinRow}>
              <View style={local.pinText}>
                <Text style={[local.pinLabel, {color: t.ink}]}>
                  The last card&apos;s PIN
                </Text>
                <Dim>
                  Write it down before the card is handed out. It is shown once
                  and is not kept after the batch ends.
                </Dim>
              </View>
              <Text style={[local.pinValue, {color: t.ink}]} testID="summary-pin-value">
                {maskedPin(shown.pin)}
              </Text>
            </View>
          </Card>
        ) : null}

        <View style={local.section}>
          <SectionHeader>{SUMMARY.record}</SectionHeader>
          <Card>
            {records.length === 0 ? (
              <View style={local.emptyRow}>
                <Text style={s.note}>{SUMMARY.empty}</Text>
              </View>
            ) : (
              records.map((r, i) => (
                <React.Fragment key={r.id}>
                  {i > 0 ? <View style={[local.divider, {backgroundColor: t.line}]} /> : null}
                  <Row row={r} />
                </React.Fragment>
              ))
            )}
          </Card>
          {summary.pending > 0 ? (
            <Text style={s.note} testID="summary-pending-note">
              {SUMMARY.pendingNote}
            </Text>
          ) : null}
          {left > 0 ? (
            <Text style={s.note} testID="summary-not-reached">
              {SUMMARY.notReached(left)}
            </Text>
          ) : null}
        </View>
      </ScrollView>

      <View style={local.footer}>
        <PrimaryButton
          title={
            summary.pending > 0 && !pendingConfirm
              ? 'CLOSE BATCH'
              : SUMMARY.close
          }
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
        {pendingConfirm ? (
          <Text style={s.note} testID="close-pending-note">
            {SUMMARY.pendingNote}
          </Text>
        ) : null}
        {/* Back to the batch is only honest while it still accepts cards: once
            ended, its numbers are frozen and its PIN slot is wiped. */}
        {phase === 'summary' ? (
          <SecondaryButton
            title={SUMMARY.backToLoop}
            testID="summary-back"
            onPress={onBack}
          />
        ) : null}
      </View>
    </Screen>
  );
}

/** One figure of the header: the number over its name. */
function Stat({
  value,
  label,
  accent,
}: {
  value: number;
  label: string;
  accent?: boolean;
}) {
  const t = useTheme();
  return (
    <View style={local.stat}>
      <Text
        style={[
          local.statFigure,
          {color: accent ? t.accent : t.ink},
        ]}>
        {value}
      </Text>
      <Text style={[local.statLabel, {color: t.dim}]}>{label}</Text>
    </View>
  );
}

/** One record row: a state dot, the short id, the time, and a pending flag. */
function Row({row}: {row: RecordRow}) {
  const t = useTheme();
  const pendingStyle = {
    width: 8,
    height: 8,
    borderRadius: 4,
    borderWidth: 1.5,
    borderColor: t.accent,
  };
  return (
    <View style={local.row}>
      <View
        style={[
          row.pending
            ? pendingStyle
            : {width: 8, height: 8, borderRadius: 4, backgroundColor: row.ok ? t.accent : t.danger},
        ]}
      />
      <Text style={[local.rowId, {color: t.ink}]}>{`…${row.id}`}</Text>
      <Text style={[local.rowAt, {color: t.dim}]}>{timeOf(row.at)}</Text>
      {row.pending ? (
        <Text style={[local.rowPending, {color: t.dim}]}>
          {SUMMARY.pending}
        </Text>
      ) : null}
    </View>
  );
}

const local = StyleSheet.create({
  scroll: {gap: 20, paddingBottom: 24, flexGrow: 1},
  stats: {flexDirection: 'row', gap: 10},
  stat: {
    flex: 1,
    paddingVertical: 14,
    gap: 2,
  },
  statFigure: {
    fontFamily: FONT.semibold,
    fontSize: 30,
    fontVariant: ['tabular-nums'],
  },
  statLabel: {fontFamily: FONT.regular, fontSize: 13},
  section: {gap: 8},
  pinRow: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: 14,
    padding: 16,
  },
  pinText: {flex: 1, gap: 4},
  pinLabel: {fontFamily: FONT.medium, fontSize: 16},
  pinValue: {
    fontFamily: FONT.tabular,
    fontSize: 20,
    letterSpacing: 4,
  },
  divider: {height: StyleSheet.hairlineWidth},
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: 10,
    paddingHorizontal: 16,
    paddingVertical: 12,
    minHeight: 44,
  },
  rowId: {
    fontFamily: FONT.tabular,
    fontVariant: ['tabular-nums'],
    fontSize: 15,
    flex: 1,
  },
  rowAt: {
    fontFamily: FONT.tabular,
    fontVariant: ['tabular-nums'],
    fontSize: 13,
  },
  rowPending: {fontFamily: FONT.medium, fontSize: 12},
  emptyRow: {paddingHorizontal: 16, paddingVertical: 14},
  footer: {gap: 10, paddingBottom: 4, paddingTop: 8},
});
