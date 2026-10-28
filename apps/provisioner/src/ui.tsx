/**
 * The shared pieces of the five screens, in the design language the plan's
 * trimmed surface keeps: section headers in small caps, one raised card per
 * group, hairline dividers, the accent only where a choice is live.
 *
 * Every colour comes from a token (`theme.ts`); nothing here draws its own.
 * The theme is read from `settings.tsx`, not from the phone's scheme alone:
 * the operator's theme choice is a runtime setting (plan § 5) and the two
 * classes of setting stay apart.
 */
import React from 'react';
import {
  Pressable,
  StatusBar,
  StyleSheet,
  Text,
  useColorScheme,
  View,
} from 'react-native';
import {SafeAreaView} from 'react-native-safe-area-context';

import {useSettings, type ThemeChoice} from './settings';

export interface Theme {
  dark: boolean;
  /** Screen background. */
  bg: string;
  /** Raised surface: rows, the batch card, inputs. */
  card: string;
  /** Primary text. */
  ink: string;
  /** Secondary text. */
  dim: string;
  /** The one strong color; the success state and the progress fill. */
  accent: string;
  /** Danger stripe and its ink. */
  danger: string;
  /** Hairlines between rows. */
  line: string;
}

export const DARK: Theme = {
  dark: true,
  bg: '#050505',
  card: '#1C1C1E',
  ink: '#FFFFFF',
  dim: 'rgba(255,255,255,0.6)',
  accent: '#3AB54A',
  danger: '#E5484D',
  line: 'rgba(255,255,255,0.12)',
};

export const LIGHT: Theme = {
  dark: false,
  bg: '#FAFAF8',
  card: '#F7F7F5',
  ink: '#0B0B0B',
  dim: 'rgba(11,11,11,0.6)',
  accent: '#3AB54A',
  danger: '#D13438',
  line: 'rgba(11,11,11,0.12)',
};

/** The operator's choice, resolved against the phone's own scheme. */
export function resolveTheme(
  choice: ThemeChoice,
  system: 'light' | 'dark' | null | undefined,
): Theme {
  if (choice === 'dark') {
    return DARK;
  }
  if (choice === 'light') {
    return LIGHT;
  }
  return system === 'light' ? LIGHT : DARK;
}

/**
 * Figtree is the brand face; tabular figures wherever a number moves. The
 * bundle ships the fonts (see `App.tsx`'s font registration); a missing face
 * falls back to the platform's, which is ugly but never wrong.
 */
export const FONT = {
  regular: 'Figtree-Regular',
  medium: 'Figtree-Medium',
  semibold: 'Figtree-SemiBold',
  /** Growing counters and ids: monospaced digits. */
  tabular: 'Figtree-SemiBold',
} as const;

/**
 * The theme the screens draw with: the operator's choice, and for `system` the
 * phone's own scheme. `useColorScheme` is the only platform read here.
 */
export function useTheme(): Theme {
  const {theme} = useSettings();
  const system = useColorScheme();
  return resolveTheme(theme, system);
}

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
  return (
    <SafeAreaView style={s.screen}>
      <StatusBar barStyle={t.dark ? 'light-content' : 'dark-content'} />
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

/** Section header in small caps: the one heading style the screens share. */
export function SectionHeader({children}: {children: React.ReactNode}) {
  const s = styles(useTheme());
  return <Text style={s.sectionHeader}>{children}</Text>;
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
      style={[s.primary, danger && s.primaryDanger, disabled && s.primaryDisabled]}>
      <Text style={s.primaryText}>{title}</Text>
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
      style={[s.secondary, disabled && s.primaryDisabled]}>
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
      style={s.row}>
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
    sectionHeader: {
      color: t.dim,
      fontFamily: FONT.semibold,
      fontSize: 12,
      letterSpacing: 1.2,
      textTransform: 'uppercase',
    },
    card: {backgroundColor: t.card, borderRadius: 16, overflow: 'hidden'},
    divider: {height: StyleSheet.hairlineWidth, backgroundColor: t.line},
    primary: {
      backgroundColor: t.accent,
      borderRadius: 14,
      paddingVertical: 16,
      alignItems: 'center',
    },
    primaryDanger: {backgroundColor: t.danger},
    primaryDisabled: {opacity: 0.35},
    primaryText: {
      color: '#fff',
      fontFamily: FONT.semibold,
      fontSize: 17,
      letterSpacing: 0.4,
    },
    secondary: {
      borderColor: t.line,
      borderWidth: 1.5,
      borderRadius: 14,
      paddingVertical: 14,
      alignItems: 'center',
    },
    secondaryText: {color: t.ink, fontFamily: FONT.medium, fontSize: 16},
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
      borderRadius: 12,
      padding: 3,
      gap: 2,
      alignSelf: 'flex-start',
    },
    pillItem: {paddingHorizontal: 16, paddingVertical: 7, borderRadius: 10},
    pillItemOn: {backgroundColor: t.ink},
    pillText: {color: t.dim, fontFamily: FONT.medium, fontSize: 14},
    pillTextOn: {color: t.bg, fontFamily: FONT.semibold, fontSize: 14},
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
