/**
 * The six routes (docs/PROVISIONER-PLAN.md § 5, "Trimmed surface", and
 * OPS-PLAN O5): SignIn, BatchSetup, the Loop, BatchSummary, Settings, and
 * Reset a card under Settings. There is no card browser and no load-test
 * bench; those live in the laptop's `cardctl`.
 *
 * Which screen a batch shows is the batch's `phase`, not a route table: the
 * loop runs until END BATCH (`summary`), and the summary is the one screen that
 * closes a batch (`ended`). `route` carries only the thing the phase cannot say
 * — Settings, which the operator may open between batches.
 *
 * Provider order is a contract: `SettingsProvider` → `StoreProvider` (the store
 * reads the settings) → `PinRevealProvider` → `BatchPinProvider` (the screens
 * call `usePinReveal`/`useBatchPin`, so a missing one throws on first render).
 */
import React from 'react';
import {
  SafeAreaProvider,
  initialWindowMetrics,
} from 'react-native-safe-area-context';

import {BatchPinProvider, PinRevealProvider} from './src/pin';
import {LOCAL_SESSION_ENABLED, localStoreApi} from './src/localSession';
import {RegistryClient, RegistryStoreApi, registryOrigin} from './src/api';
import {BatchSetupScreen} from './src/screens/BatchSetupScreen';
import {BatchSummaryScreen} from './src/screens/BatchSummaryScreen';
import {DemoScreen} from './src/screens/DemoScreen';
import {LoopScreen} from './src/screens/LoopScreen';
import {ResetScreen} from './src/screens/ResetScreen';
import {SettingsScreen} from './src/screens/SettingsScreen';
import {SignInScreen} from './src/screens/SignInScreen';
import {SettingsProvider, useSettings} from './src/settings';
import {StoreProvider, useStore} from './src/store';
import type {StoreApi} from './src/store';

type Route = 'home' | 'settings' | 'summary' | 'demo' | 'reset';

function Navigator(): React.JSX.Element {
  const {batch, signedIn} = useStore();
  const [route, setRoute] = React.useState<Route>('home');
  // Set by START on the review page: the loop arms card 1 as it opens, so a
  // reviewed batch reaches its first card in two presses (as Cashu's does).
  const [armFirst, setArmFirst] = React.useState(false);
  const phase = batch?.phase ?? 'idle';

  // The demo is deliberately ahead of the sign-in gate: App Review opens a
  // cold build with no operator account, and a demo they cannot reach is a
  // rejection (plan § 7, Guideline 2.1). It touches no card and no registry,
  // so there is nothing here to protect.
  if (route === 'demo') {
    return <DemoScreen onBack={() => setRoute('home')} />;
  }
  if (!signedIn) {
    return <SignInScreen onDemo={() => setRoute('demo')} />;
  }
  // A batch owns the screen from START until CLOSE: a torn install is never
  // navigated away from, and the loop's stops are the only way out.
  // The summary opens from the loop at any point: when every card is tried,
  // and when the operator ends the batch early. Closing it ends the batch.
  if (route === 'summary' && (phase === 'summary' || phase === 'running')) {
    return (
      <BatchSummaryScreen
        onBack={() => setRoute('home')}
        onClose={() => setRoute('home')}
      />
    );
  }
  // Settings opens over a running batch from the loop's gear, which the loop
  // hides while a card is on the phone; Back returns to the loop. Settings
  // itself refuses the demo and sign-out while a batch runs.
  // Never over a running batch: Settings disables the entry while one runs.
  if (route === 'reset' && phase !== 'running') {
    return <ResetScreen onBack={() => setRoute('settings')} />;
  }
  if (route === 'settings') {
    return (
      <SettingsScreen
        onBack={() => setRoute('home')}
        onDemo={() => setRoute('demo')}
        onReset={() => setRoute('reset')}
      />
    );
  }
  if (phase === 'running' || phase === 'summary') {
    return (
      <LoopScreen
        armFirst={armFirst}
        onArmed={() => setArmFirst(false)}
        onSummary={() => setRoute('summary')}
        onSettings={() => setRoute('settings')}
      />
    );
  }
  return (
    <BatchSetupScreen
      onStart={() => setArmFirst(true)}
      onSettings={() => setRoute('settings')}
    />
  );
}

/**
 * The registry client, built from the endpoint the settings name. Null when
 * this build has no origin for it (both `REGISTRY_ORIGINS` entries name a
 * deployed registry, so a build from this tree always has one), in which case
 * sign-in fails with a message that says so rather than posting credentials at
 * a guessed host. A local dev build with `LOCAL_SESSION_ENABLED` gets the fake
 * client instead, and SignIn says so.
 */
function ConnectedStore({
  children,
}: {
  children: React.ReactNode;
}): React.JSX.Element {
  const {registryEndpoint} = useSettings();
  const api = React.useMemo<StoreApi | undefined>(() => {
    const origin = registryOrigin(registryEndpoint);
    if (origin !== null) {
      return new RegistryStoreApi(
        new RegistryClient({baseUrl: origin}),
        origin,
      );
    }
    // Both endpoints name a registry now, so this branch is unreachable in a
    // build from this tree; it stays because a forked build may empty
    // `REGISTRY_ORIGINS`, and then a local dev build must still be able to
    // walk past SignIn on a fake session (`src/localSession.ts`). A Release
    // bundle cannot, because that module gates on `__DEV__` too.
    return LOCAL_SESSION_ENABLED && __DEV__ ? localStoreApi() : undefined;
  }, [registryEndpoint]);
  return <StoreProvider api={api}>{children}</StoreProvider>;
}

export default function App(): React.JSX.Element {
  return (
    <SafeAreaProvider initialMetrics={initialWindowMetrics}>
      <SettingsProvider>
        <ConnectedStore>
          <PinRevealProvider>
            <BatchPinProvider>
              <Navigator />
            </BatchPinProvider>
          </PinRevealProvider>
        </ConnectedStore>
      </SettingsProvider>
    </SafeAreaProvider>
  );
}
