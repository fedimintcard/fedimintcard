/**
 * The shared pieces of the operator screens, on the brand tokens in
 * `theme.ts`: section headers in small caps (with tap-to-read help where a
 * reason is worth one tap), one raised card per group, hairline dividers, the
 * gold accent only where a choice is live.
 */
import React from 'react';
import {BackHandler, Pressable, StatusBar, StyleSheet, Text, View} from 'react-native';
import {SafeAreaView} from 'react-native-safe-area-context';

import GearSvg from './assets/gear.svg';
import {InfoButton} from './InfoHelp';
import {FONT, useTheme, type Theme} from './theme';

export {BRAND, FONT, useTheme, type Theme} from './theme';
export {HelpHeading, InfoButton} from './InfoHelp';

export function Screen({
  title,
  onBack,
  onStop,
  headerRight,
  children,
}: {
  title?: string;
  onBack?: () => void;
  onStop?: () => void;
  headerRight?: React.ReactNode;
  children: React.ReactNode;
}) {
  const t = useTheme();
  const s = styles(t);
  const exit = onStop ?? onBack;
  // Android's back gesture does what the header's exit does, never more: a
  // screen without an exit swallows it rather than leaving the app mid-batch.
  React.useEffect(() => {
    const sub = BackHandler.addEventListener('hardwareBackPress', () => {
      exit?.();
      return true;
    });
    return () => sub.remove();
  }, [exit]);
  return (
    <SafeAreaView style={s.screen}>
      <StatusBar barStyle="light-content" backgroundColor={t.bg} />
      {title || exit || headerRight ? (
        <View style={s.header}>
          {exit ? (
            <Pressable
              onPress={exit}
              hitSlop={12}
              testID={onStop ? 'screen-stop' : 'back'}
              accessibilityRole="button"
              accessibilityLabel={onStop ? 'Stop current operation' : 'Back'}>
              <Text style={s.back}>{onStop ? 'Stop' : '‹ Back'}</Text>
            </Pressable>
          ) : null}
          {title ? (
            <View style={s.titleWrap} pointerEvents="box-none">
              <Text style={s.title}>{title}</Text>
            </View>
          ) : null}
          {headerRight ? <View testID="header-action">{headerRight}</View> : null}
        </View>
      ) : null}
      <View style={s.body}>{children}</View>
    </SafeAreaView>
  );
}

/**
 * Section header in small caps: the one heading style the screens share. With
 * `help`, an (i) beside it opens the reason in a panel instead of a paragraph.
 */
export function SectionHeader({
  children,
  help,
  testID,
}: {
  children: string;
  help?: string;
  testID?: string;
}) {
  const s = styles(useTheme());
  if (!help) {
    return (
      <Text style={s.sectionHeader} accessibilityRole="header">
        {children}
      </Text>
    );
  }
  return (
    <View style={s.sectionRow}>
      <Text style={[s.sectionHeader, s.sectionGrow]} accessibilityRole="header">
        {children}
      </Text>
      <InfoButton title={children} testID={testID ?? `help-${children}`}>
        {help}
      </InfoButton>
    </View>
  );
}

/** The settings gear for a header's right slot. */
export function GearButton({onPress, testID}: {onPress: () => void; testID?: string}) {
  const t = useTheme();
  return (
    <Pressable
      onPress={onPress}
      hitSlop={12}
      testID={testID ?? 'settings'}
      style={{padding: 6}}
      accessibilityRole="button"
      accessibilityLabel="Settings">
      <GearSvg width={24} height={24} color={t.dim} />
    </Pressable>
  );
}

/** A label and its value on one line, inside a Card. */
export function ValueRow({
  label,
  value,
  testID,
  valueTone,
}: {
  label: string;
  value: string;
  testID?: string;
  valueTone?: 'ink' | 'dim' | 'accent' | 'danger';
}) {
  const t = useTheme();
  const s = styles(t);
  const color =
    valueTone === 'accent'
      ? t.accent
      : valueTone === 'danger'
        ? t.danger
        : valueTone === 'ink'
          ? t.ink
          : t.dim;
  return (
    <View style={s.row} testID={testID}>
      <Text style={[s.rowLabel, s.rowFixed]}>{label}</Text>
      <Text style={[s.rowValue, s.rowGrow, s.rowRight, {color}]} numberOfLines={1}>
        {value}
      </Text>
    </View>
  );
}

/** The raised card a group of rows sits in. */
export function Card({
  children,
  testID,
}: {
  children: React.ReactNode;
  testID?: string;
}) {
  const s = styles(useTheme());
  return (
    <View style={s.card} testID={testID}>
      {children}
    </View>
  );
}

export function Divider() {
  const s = styles(useTheme());
  return <View style={s.divider} />;
}

export function Dim({
  children,
  testID,
}: {
  children: React.ReactNode;
  testID?: string;
}) {
  const t = useTheme();
  return (
    <Text
      testID={testID}
      style={{
        color: t.dim,
        fontFamily: FONT.regular,
        fontSize: 14,
        lineHeight: 20,
      }}>
      {children}
    </Text>
  );
}

export function PrimaryButton({
  title,
  onPress,
  disabled,
  danger,
  testID,
}: {
  title: string;
  onPress: () => void;
  disabled?: boolean;
  danger?: boolean;
  testID?: string;
}) {
  const s = styles(useTheme());
  return (
    <Pressable
      testID={testID}
      accessibilityRole="button"
      accessibilityLabel={title}
      accessibilityState={{disabled: !!disabled}}
      disabled={disabled}
      onPress={onPress}
      style={({pressed}) => [
        s.primary,
        danger && s.primaryDanger,
        pressed && !disabled && s.pressed,
        disabled && s.primaryDisabled,
      ]}>
      <Text
        style={[
          s.primaryText,
          danger && s.primaryDangerText,
          disabled && s.primaryTextDisabled,
        ]}>
        {title}
      </Text>
    </Pressable>
  );
}

/** The quiet action beside the loud one: bordered, never accent. */
export function SecondaryButton({
  title,
  onPress,
  disabled,
  testID,
}: {
  title: string;
  onPress: () => void;
  disabled?: boolean;
  testID?: string;
}) {
  const s = styles(useTheme());
  return (
    <Pressable
      testID={testID}
      accessibilityRole="button"
      accessibilityLabel={title}
      accessibilityState={{disabled: !!disabled}}
      disabled={disabled}
      onPress={onPress}
      style={({pressed}) => [
        s.secondary,
        pressed && !disabled && s.secondaryPressed,
        disabled && s.secondaryDisabled,
      ]}>
      <Text style={s.secondaryText}>{title}</Text>
    </Pressable>
  );
}

/** A bordered row that opens something: label, optional sub-line, a chevron. */
export function EntryRow({
  title,
  subtitle,
  onPress,
  disabled,
  testID,
}: {
  title: string;
  subtitle?: string;
  onPress: () => void;
  disabled?: boolean;
  testID?: string;
}) {
  const s = styles(useTheme());
  return (
    <Pressable
      testID={testID}
      onPress={onPress}
      disabled={disabled}
      accessibilityRole="button"
      accessibilityState={{disabled: !!disabled}}
      style={({pressed}) => [s.row, pressed && s.secondaryPressed]}>
      <View style={s.rowLabelCol}>
        <Text style={s.rowLabel}>{title}</Text>
        {subtitle ? <Text style={s.rowSub}>{subtitle}</Text> : null}
      </View>
      <Text style={s.chevron}>›</Text>
    </Pressable>
  );
}

/** A segmented control: the active segment inverts, the rest stay quiet. */
export function Segmented<T extends string>({
  options,
  value,
  labelOf,
  onChange,
  testIDOf,
}: {
  options: readonly T[];
  value: T;
  labelOf: (option: T) => string;
  onChange: (option: T) => void;
  testIDOf?: (option: T) => string;
}) {
  const s = styles(useTheme());
  return (
    <View style={s.pill}>
      {options.map(option => {
        const on = value === option;
        return (
          <Pressable
            key={option}
            testID={testIDOf?.(option)}
            accessibilityRole="button"
            accessibilityState={{selected: on}}
            onPress={() => onChange(option)}
            style={[s.pillItem, on && s.pillItemOn]}>
            <Text style={on ? s.pillTextOn : s.pillText}>{labelOf(option)}</Text>
          </Pressable>
        );
      })}
    </View>
  );
}

/**
 * Whether a rejection means "the operator stopped it". `api.ts` owns the
 * `ApiCancelledError` class, but `store.tsx` imports no network module, so the
 * check is on the error's own name — the class's identity across module
 * boundaries, without a second import path for the api.
 */
export function wasCancelled(e: unknown): boolean {
  return e instanceof Error && e.name === 'ApiCancelledError';
}

/**
 * The reason `SIGNIN.failed` switches on. Kept apart from the api's classes for
 * the same reason: a service refusal carries its code in the message, and a
 * transport failure is the one thing the operator can fix.
 */
export function signInReason(e: unknown): string {
  if (e instanceof Error) {
    if (e.name === 'ApiUnreachableError') {
      return 'unreachable';
    }
    // The registry words its refusals in prose; the status is what is stable.
    const status = (e as {status?: unknown}).status;
    if (e.name === 'ApiServiceError' && status === 401) {
      return 'bad-credentials';
    }
    if (e.name === 'ApiServiceError' && status === 429) {
      return 'login-rate-limited';
    }
    return e.message;
  }
  return 'unknown';
}

export const styles = (t: Theme) =>
  StyleSheet.create({
    screen: {flex: 1, backgroundColor: t.bg},
    header: {
      paddingHorizontal: 16,
      paddingTop: 8,
      paddingBottom: 12,
      flexDirection: 'row',
      alignItems: 'center',
      justifyContent: 'space-between',
      gap: 12,
      minHeight: 48,
    },
    title: {
      color: t.ink,
      fontFamily: FONT.semibold,
      fontSize: 26,
      letterSpacing: 0.2,
      textAlign: 'center',
    },
    titleWrap: {flex: 1, minWidth: 0, alignItems: 'center'},
    back: {color: t.accent, fontFamily: FONT.medium, fontSize: 16},
    body: {flex: 1, paddingHorizontal: 16, gap: 12},
    sectionRow: {flexDirection: 'row', alignItems: 'center', marginBottom: -8},
    sectionGrow: {flex: 1},
    rowGrow: {flex: 1},
    rowFixed: {flexShrink: 0},
    rowRight: {textAlign: 'right'},
    pressed: {opacity: 0.85},
    secondaryPressed: {backgroundColor: t.raised},
    primaryDangerText: {color: t.onDanger},
    sectionHeader: {
      color: t.dim,
      fontFamily: FONT.semibold,
      fontSize: 12,
      letterSpacing: 1.2,
      textTransform: 'uppercase',
    },
    card: {
      backgroundColor: t.card,
      borderRadius: 16,
      borderWidth: StyleSheet.hairlineWidth,
      borderColor: t.line,
      overflow: 'hidden',
    },
    divider: {height: StyleSheet.hairlineWidth, backgroundColor: t.line},
    primary: {
      backgroundColor: t.accent,
      borderRadius: 14,
      paddingVertical: 16,
      alignItems: 'center',
    },
    primaryDanger: {backgroundColor: t.danger},
    primaryDisabled: {backgroundColor: t.raised, opacity: 0.9},
    primaryTextDisabled: {color: t.dim},
    secondaryDisabled: {opacity: 0.4},
    primaryText: {
      color: t.onAccent,
      fontFamily: FONT.semibold,
      fontSize: 17,
      letterSpacing: 0.4,
    },
    secondary: {
      borderColor: t.line,
      borderWidth: 1.5,
      borderRadius: 14,
      paddingVertical: 14,
      paddingHorizontal: 16,
      alignItems: 'center',
    },
    secondaryText: {
      color: t.ink,
      fontFamily: FONT.medium,
      fontSize: 16,
      textAlign: 'center',
    },
    row: {
      paddingHorizontal: 18,
      paddingVertical: 15,
      flexDirection: 'row',
      alignItems: 'center',
      gap: 12,
      minHeight: 52,
    },
    rowLabelCol: {flex: 1},
    rowLabel: {color: t.ink, fontFamily: FONT.medium, fontSize: 16},
    rowSub: {color: t.dim, fontFamily: FONT.regular, fontSize: 12, marginTop: 2},
    rowValue: {
      color: t.dim,
      fontFamily: FONT.tabular,
      fontSize: 15,
      fontVariant: ['tabular-nums'],
    },
    chevron: {color: t.dim, fontFamily: FONT.regular, fontSize: 22},
    pill: {
      flexDirection: 'row',
      backgroundColor: t.bg,
      borderWidth: StyleSheet.hairlineWidth,
      borderColor: t.line,
      borderRadius: 12,
      padding: 3,
      gap: 2,
      alignSelf: 'flex-start',
    },
    pillItem: {paddingHorizontal: 16, paddingVertical: 7, borderRadius: 10},
    pillItemOn: {backgroundColor: t.accent},
    pillText: {color: t.dim, fontFamily: FONT.medium, fontSize: 14},
    pillTextOn: {color: t.onAccent, fontFamily: FONT.semibold, fontSize: 14},
    input: {
      color: t.ink,
      fontFamily: FONT.regular,
      fontSize: 16,
      borderWidth: 1.5,
      borderColor: t.line,
      borderRadius: 12,
      backgroundColor: t.card,
      paddingHorizontal: 14,
      paddingVertical: 12,
    },
    note: {
      color: t.dim,
      fontFamily: FONT.regular,
      fontSize: 13,
      lineHeight: 19,
    },
    failed: {
      color: t.danger,
      fontFamily: FONT.regular,
      fontSize: 13,
      textAlign: 'center',
    },
  });
