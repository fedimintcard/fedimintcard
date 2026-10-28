/**
 * The five routes (docs/PROVISIONER-PLAN.md § 5, "Trimmed surface"): SignIn,
 * BatchSetup, the Loop, BatchSummary, Settings. There is no card browser, no
 * load-test bench and no reset flow; those live in the laptop's `cardctl`.
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
import {RegistryClient, RegistryStoreApi, registryOrigin} from './src/api';
import {BatchSetupScreen} from './src/screens/BatchSetupScreen';
import {BatchSummaryScreen} from './src/screens/BatchSummaryScreen';
import {LoopScreen} from './src/screens/LoopScreen';
import {SettingsScreen} from './src/screens/SettingsScreen';
import {SignInScreen} from './src/screens/SignInScreen';
import {SettingsProvider, useSettings} from './src/settings';
import {StoreProvider, useStore} from './src/store';

type Route = 'home' | 'settings' | 'summary';

function Navigator(): React.JSX.Element {
  const {batch, signedIn} = useStore();
  const [route, setRoute] = React.useState<Route>('home');
  const phase = batch?.phase ?? 'idle';

  if (!signedIn) {
    return <SignInScreen />;
  }
  // A batch owns the screen from START until CLOSE: a torn install is never
  // navigated away from, and the loop's stops are the only way out.
  if (route === 'summary' && phase === 'summary') {
    return (
      <BatchSummaryScreen
        onBack={() => setRoute('home')}
        onClose={() => setRoute('home')}
      />
    );
  }
  if (phase === 'running' || phase === 'summary') {
    return (
      <LoopScreen
        onSummary={() => setRoute('summary')}
        onSettings={() => setRoute('settings')}
      />
    );
  }
  if (route === 'settings') {
    return <SettingsScreen onBack={() => setRoute('home')} />;
  }
  return (
    <BatchSetupScreen
      onStart={() => {}}
      onSettings={() => setRoute('settings')}
    />
  );
}

/**
 * The registry client, built from the endpoint the settings name. Null when
 * this build has no origin for it (`REGISTRY_ORIGINS` is empty until P1c
 * ships a registry), in which case sign-in fails with a message that says so
 * rather than posting credentials at a guessed host.
 */
function ConnectedStore({
  children,
}: {
  children: React.ReactNode;
}): React.JSX.Element {
  const {registryEndpoint} = useSettings();
  const api = React.useMemo<RegistryStoreApi | undefined>(() => {
    const origin = registryOrigin(registryEndpoint);
    return origin === null
      ? undefined
      : new RegistryStoreApi(new RegistryClient({baseUrl: origin}), origin);
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
