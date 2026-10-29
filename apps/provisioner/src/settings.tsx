/**
 * The two classes of setting, kept apart on purpose (docs/PROVISIONER-PLAN.md
 * § 5): what the operator varies between batches and relaunches, and what has
 * exactly one correct value for this build.
 *
 * - Build-time (`BUILD`): applet and package AIDs, the load file's sha256, the
 *   cardkit version, the app identity. Frozen. A change here is a new build and
 *   a store review, because it means "this app installs different card
 *   software" or "this is a different app".
 * - Runtime (`Settings`): federation, registry endpoint, top-up amounts, theme.
 *   Persisted under one AsyncStorage key, read by a provider and written through
 *   a hook. These are operational decisions; freezing them would make a copy of
 *   the app's own config cost a review cycle.
 *
 * Nothing here comes from the network, so the module resolves at import and the
 * first render has its values; only the stored runtime block arrives a tick
 * later, behind `loading`.
 */
import React, {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react';
import AsyncStorage from '@react-native-async-storage/async-storage';

import {LOAD_FILE_SHA256, LOAD_FILE_SIZE} from './cardkit';

/* ------------------------------------------------------------------------- */
/* Build-time constants. Frozen: never editable, never persisted.             */
/* ------------------------------------------------------------------------- */

/**
 * The app identity (plan § 8.6, decided 2026-10-28): the reverse-domain root
 * `org.fedimintcard` already ships as `android-relay`'s, and `.provisioner` is a
 * sibling so both apps install side by side and neither can capture the other's
 * intents. Android `applicationId` + `namespace`, iOS
 * `PRODUCT_BUNDLE_IDENTIFIER`, and the RN `name`/`displayName` are all set from
 * this one value at native-project level; this constant is what Settings shows.
 */
export const APP_IDENTITY = Object.freeze({
  androidApplicationId: 'org.fedimintcard.provisioner',
  androidNamespace: 'org.fedimintcard.provisioner',
  iosBundleIdentifier: 'org.fedimintcard.provisioner',
  reactNativeName: 'FedimintProvisioner',
  displayName: 'FedimintProvisioner',
} as const);

/**
 * The AIDs, from `tools/cardkit/gp/aids.ts` (`CARD_AID`, `OPENER_AID`,
 * `FEDIMINT_PACKAGE_AID`, `ISD_AID`); the card's own three come from
 * `applet/build.xml`, which is the only file that states them. They are written
 * here as hex strings rather than converted from the engine's bytes because the
 * iOS entitlement and the Android manifest take strings, and a hex string is
 * what a reviewer can diff against the build file.
 *
 *   package F0 46 4D 43 41 52 44      (F0 then "FMCARD")
 *   opener  F0 46 4D 43 41 52 44 4D   (instantiated first; the J3R180 will not
 *                                      take the applet as its package's first
 *                                      instance)
 *   applet  F0 46 4D 43 41 52 44 01   (what hosts SELECT)
 */
export const BUILD = Object.freeze({
  appIdentity: APP_IDENTITY,
  /**
   * The applet: what hosts SELECT, and what the iOS entitlement must list.
   * Hex as written above, not re-derived from bytes.
   */
  appletAid: 'F0464D4341524401',
  /** The card's package, F0 then "FMCARD". */
  packageAid: 'F0464D43415244',
  /** The opener, instantiated before the applet. */
  openerAid: 'F0464D434152444D',
  /** The card manager, never deleted. Also listed in the iOS entitlement. */
  isdAid: 'A000000151000000',
  /** sha256 of the load file this build sends. cardkit re-hashes before sending. */
  loadSha256: LOAD_FILE_SHA256,
  /** The load file's size in bytes; 23 LOAD commands at 247 bytes. */
  loadSize: LOAD_FILE_SIZE,
  /** The engine version (`tools/cardkit/package.json`). */
  cardkitVersion: '0.1.0',
});

/**
 * The two AIDs the iOS NFC entitlement's `iso7816.select-identifiers` must list
 * (plan § 7): the applet *and* the card manager are both selected.
 */
export const SELECT_IDENTIFIERS: readonly string[] = Object.freeze([
  BUILD.appletAid,
  BUILD.isdAid,
] as const);

/** The one line Settings shows about what the build installs. */
export const LOAD_LINE = `${LOAD_FILE_SHA256.slice(0, 8)} · ${LOAD_FILE_SIZE} bytes`;

/**
 * These bytes are not a custody claim. The README's warning, repeated in About
 * (Guideline 2.3.3): the app programs a card and loads value; it does not store
 * or secure funds beyond that.
 */
export const EXPERIMENTAL_WARNING =
  'Experimental software. This app installs card software and loads value ' +
  'onto a card. The card refuses to sign without the PIN set here; it has no ' +
  'spending limits, no owner key and no note list, and this app makes no claim ' +
  'to custody.';

/* ------------------------------------------------------------------------- */
/* Runtime settings. Persisted, editable in Settings.                         */
/* ------------------------------------------------------------------------- */

export type ThemeChoice = 'system' | 'dark' | 'light';
export type RegistryEndpoint = 'test' | 'production';

/**
 * The two registry deployments. There is no registry origin in this repo yet
 * (P1c builds it), so the endpoint is stored as a *name*, not a URL typed here:
 * a made-up host would be a constant that routes provisioning records nowhere
 * and a store disclosure naming a server that does not exist. `src/api.ts`
 * supplies the origin for each name, and the single place the URL lives.
 */
export const REGISTRY_ENDPOINTS: readonly RegistryEndpoint[] = Object.freeze([
  'test',
  'production',
] as const);

/**
 * The local sidecar (plan § 4, § 7), which keeps the wallet, the key setup and
 * the issue step on the operator's machine: available, `precheck`, `keySetup`,
 * `issue`. Fixed, loopback only, as the till keeps it; a LAN cleartext
 * exception is not defensible at review.
 */
export const LOCAL_SIDECAR_ORIGIN = 'http://127.0.0.1:9877';

/**
 * The federation, as an invite string the picker also accepts typed by hand
 * (plan § 5: a second federation is expected; a rebuild to switch is a trap).
 *
 * The default is *empty*, and the picker requires one: this repo records no
 * invite string for any federation it ships (docs/DEMO.md rehearses against the
 * Island Bitcoin Community federation but names no URL), and a checked-in
 * invite for a federation the operator does not run would send a card's notes
 * to the wrong issuer. `settingsAreReady` refuses to start a batch without one.
 * The picker's presets live in `federation.ts`, next to the type, because they
 * too are empty for that reason.
 */
export const DEFAULT_FEDERATION = '';

/**
 * Top-up amounts: the notes `issue` asks for, in satoshi, largest first. The
 * demo starts from 2,000 sats of ecash and funds four notes (docs/DEMO.md:
 * `card topup 1048576 524288 262144 131072` msat = 1,048,576 + 524,288 +
 * 262,144 + 131,072 msat). The sidecar's `plan.rs` previews this plan and
 * refuses one it cannot make exact (plan § 5), so the app shows the plan the
 * server will honour, not a sum it guessed.
 */
export const DEFAULT_TOPUP_SATS: readonly number[] = Object.freeze(
  [1048, 524, 262, 131] as const,
);

/**
 * The applet has 8 slots (`tools/cardkit/gp/provision.ts`'s `checks` probe
 * exactly 0–7), and every issued note occupies one, so a plan of more than 8
 * notes cannot be held by any card this app programs.
 */
export const MAX_TOPUP_NOTES = 8;

/**
 * Whether the operator's amounts are one the sidecar can realise: whole sats,
 * positive, no more notes than the card has slots, and (from `plan.rs`) powers
 * of two msat — the mint's denominations — listed largest first. The batch
 * preview shows what the sidecar will honour, so the client refuses the same
 * shapes `plan.rs` would.
 */
export function isValidTopup(amounts: readonly number[]): boolean {
  return (
    amounts.length > 0 &&
    amounts.length <= MAX_TOPUP_NOTES &&
    amounts.every(
      (n, i) =>
        Number.isInteger(n) &&
        n > 0 &&
        (n & (n - 1)) === 0 && // a power of two, as plan.rs's denominations are
        (i === 0 || n < amounts[i - 1]),
    )
  );
}

/** The total the BatchSetup preview shows above the per-note list. */
export function topupTotal(amounts: readonly number[]): number {
  return amounts.reduce((sum, n) => sum + n, 0);
}

/** Everything the operator can change, and nothing else. */
export interface Settings {
  /** The federation invite. Required; empty until the operator picks one. */
  federation: string;
  registryEndpoint: RegistryEndpoint;
  topupSats: readonly number[];
  theme: ThemeChoice;
}

export const DEFAULT_SETTINGS: Readonly<Settings> = Object.freeze({
  federation: DEFAULT_FEDERATION,
  registryEndpoint: 'test',
  topupSats: DEFAULT_TOPUP_SATS,
  theme: 'system',
} as const);

/**
 * Whether the batch can start: a federation is chosen and the amounts are one
 * the sidecar's `plan.rs` can make exact. BatchSetup shows this as the enabled
 * state of its start button, so a batch never begins against a blank invite.
 */
export function settingsAreReady(settings: Settings): boolean {
  return (
    settings.federation.startsWith('fedimint://') &&
    isValidTopup(settings.topupSats)
  );
}

/**
 * The batch's denominator's upper bound: the P6 campaign is 50 cards (plan
 * § 6), and the loop's own counter is the only thing this bounds.
 */
export const MAX_COUNT = 50;

/** The picker's presets, their framing and their names live in `federation.ts`. */
export {federationLabel, federationOptions, KNOWN_FEDERATIONS} from './federation';
export type {Federation} from './federation';

/** The one AsyncStorage key. Bumping the suffix discards older shapes. */
export const SETTINGS_KEY = 'prov.settings.v1';

/**
 * Startup read. Every field is validated on the way in: storage is a place an
 * older build, a half-finished write or a person with a debugger writes, and a
 * malformed amount list must not reach the sidecar. A field that fails falls
 * back to its default silently; the app never refuses to start over a setting.
 */
export function parseSettings(raw: string | null): Settings {
  if (!raw) {
    return DEFAULT_SETTINGS;
  }
  let json: unknown;
  try {
    json = JSON.parse(raw);
  } catch {
    return DEFAULT_SETTINGS;
  }
  if (!json || typeof json !== 'object') {
    return DEFAULT_SETTINGS;
  }
  const o = json as Record<string, unknown>;
  const federation =
    typeof o.federation === 'string' && o.federation.startsWith('fedimint://')
      ? o.federation
      : DEFAULT_SETTINGS.federation;
  const registryEndpoint = REGISTRY_ENDPOINTS.includes(
    o.registryEndpoint as RegistryEndpoint,
  )
    ? (o.registryEndpoint as RegistryEndpoint)
    : DEFAULT_SETTINGS.registryEndpoint;
  const topupSats =
    Array.isArray(o.topupSats) &&
    o.topupSats.every(n => typeof n === 'number') &&
    isValidTopup(o.topupSats as number[])
      ? [...(o.topupSats as number[])]
      : DEFAULT_SETTINGS.topupSats;
  const theme =
    o.theme === 'system' || o.theme === 'dark' || o.theme === 'light'
      ? o.theme
      : DEFAULT_SETTINGS.theme;
  return {federation, registryEndpoint, topupSats, theme};
}

/** What the rest of the app sees. */
export interface SettingsState extends Settings {
  /** True until the stored settings have been read; the app shows no form before. */
  loading: boolean;
  setFederation(invite: string): void;
  setRegistryEndpoint(endpoint: RegistryEndpoint): void;
  setTopupSats(amounts: readonly number[]): void;
  setTheme(theme: ThemeChoice): void;
  /** One atomic write of everything; `set*` go through it. */
  update(patch: Partial<Settings>): void;
}

const Ctx = createContext<SettingsState | null>(null);

/**
 * Reads once, writes on change. A write is debounced by nothing: each setter is
 * one small JSON object, and a lost write on a kill is the one thing an operator
 * would have to redo. A failed write is not surfaced as an error: the running
 * app already holds the new value, and storage is best-effort by contract.
 */
export function SettingsProvider({
  children,
  storage = AsyncStorage,
}: {
  children: React.ReactNode;
  /** Injectable for tests. */
  storage?: {
    getItem(key: string): Promise<string | null>;
    setItem(key: string, value: string): Promise<void>;
  };
}) {
  const [settings, setSettings] = useState<Settings>(DEFAULT_SETTINGS);
  const [loading, setLoading] = useState(true);
  /** The last value written, so an in-flight read cannot overwrite an edit. */
  const touched = useRef(false);

  useEffect(() => {
    let live = true;
    storage
      .getItem(SETTINGS_KEY)
      .then(raw => {
        if (live && !touched.current) {
          setSettings(parseSettings(raw));
        }
      })
      .catch(() => {
        /* an unreadable store keeps the defaults */
      })
      .finally(() => {
        if (live) {
          setLoading(false);
        }
      });
    return () => {
      live = false;
    };
  }, [storage]);

  const update = useCallback(
    (patch: Partial<Settings>) => {
      setSettings(current => {
        const next = {...current, ...patch};
        // Marked before the write is issued so a read still in flight cannot
        // land a stale block over this edit.
        touched.current = true;
        storage.setItem(SETTINGS_KEY, JSON.stringify(next)).catch(() => {
          /* best-effort: the value is live in memory either way */
        });
        return next;
      });
    },
    [storage],
  );

  const setFederation = useCallback(
    (federation: string) => update({federation}),
    [update],
  );
  const setRegistryEndpoint = useCallback(
    (registryEndpoint: RegistryEndpoint) => update({registryEndpoint}),
    [update],
  );
  const setTopupSats = useCallback(
    (topupSats: readonly number[]) => {
      if (isValidTopup(topupSats)) {
        update({topupSats: [...topupSats]});
      }
    },
    [update],
  );
  const setTheme = useCallback(
    (theme: ThemeChoice) => update({theme}),
    [update],
  );

  const value = useMemo<SettingsState>(
    () => ({
      ...settings,
      loading,
      setFederation,
      setRegistryEndpoint,
      setTopupSats,
      setTheme,
      update,
    }),
    [
      settings,
      loading,
      setFederation,
      setRegistryEndpoint,
      setTopupSats,
      setTheme,
      update,
    ],
  );

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useSettings(): SettingsState {
  const value = useContext(Ctx);
  if (!value) {
    throw new Error(
      'useSettings: no SettingsProvider above. The screens and the store ' +
        'must sit inside one; tests wrap the tree in SettingsProvider.',
    );
  }
  return value;
}
