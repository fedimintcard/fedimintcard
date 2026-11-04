/**
 * The Fedimint card palette, shared by every operator screen: the navy and
 * gold of the app icon (`assets/icon/icon.svg`), one dark theme. The Cashu
 * provisioner made the same move (one brand palette, no light/system switch);
 * it also keeps the card figure legible, which the old light theme did not.
 */
import {createContext, useContext} from 'react';

export interface Theme {
  dark: boolean;
  /** Screen background: the icon's deep navy. */
  bg: string;
  /** Raised surface: rows, cards, inputs. */
  card: string;
  /** An input or row that has focus or is pressed. */
  raised: string;
  /** Primary text. */
  ink: string;
  /** Secondary text. */
  dim: string;
  /** The one strong colour: the icon's gold. Live choices, progress, success. */
  accent: string;
  /** Text and glyphs drawn on the accent. */
  onAccent: string;
  /** Stops and destructive actions. */
  danger: string;
  onDanger: string;
  /** Hairlines and borders. */
  line: string;
}

export const BRAND: Theme = {
  dark: true,
  bg: '#0B1530',
  card: '#14234B',
  raised: '#1B2E5E',
  ink: '#EEF1F8',
  dim: '#A9B4D0',
  accent: '#E1CF89',
  onAccent: '#12214A',
  danger: '#F2958C',
  onDanger: '#12214A',
  line: '#2A3D72',
};

const ThemeCtx = createContext<Theme | null>(null);
export const ThemeProvider = ThemeCtx.Provider;

/** One palette; a provider exists only so a test can swap it. */
export function useTheme(): Theme {
  return useContext(ThemeCtx) ?? BRAND;
}

/** Figtree is the brand face; tabular figures wherever a number moves. */
export const FONT = {
  regular: 'Figtree-Regular',
  medium: 'Figtree-Medium',
  semibold: 'Figtree-SemiBold',
  bold: 'Figtree-Bold',
  /** Growing counters and ids: monospaced digits. */
  tabular: 'Figtree-SemiBold',
} as const;
