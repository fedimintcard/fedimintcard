/**
 * The local-session dev flag.
 *
 * Purpose: `REGISTRY_ORIGINS` is empty by design until a registry is deployed,
 * and `src/api.ts:registryOrigin` therefore answers null, so `StoreProvider`
 * gets no `api` and `signIn` throws "no registry client configured for this
 * build". That is the right behaviour at review, but it means nobody can walk
 * past SignIn on a device — the batch loop, the registry outbox, the summary
 * and the settings are all unreachable without a server.
 *
 * This flag supplies a **fake** session for exactly that purpose on a local
 * build. It is off unless a build turns it on, and when it is on the app says
 * so on screen: a fake session that looked like a real one is the failure mode
 * that puts an unprovisioned card in someone's hands.
 *
 * Three things make it safe to leave in the tree:
 *
 *  - `LOCAL_SESSION_ENABLED` is `false` here. Turning it on is a one-line local
 *    edit, never a release build — `__DEV__` also has to be true, so a
 *    production bundle cannot take this path even if the constant is flipped.
 *  - It does not touch `REGISTRY_ORIGINS`: the app names no registry host, and
 *    this flag never sends a byte anywhere. Nothing typed into SignIn leaves
 *    the device; there is no client to send it with.
 *  - The store records rows into its own outbox only. The summary's "pending"
 *    warning is then the truth — the rows are pending, because nothing was
 *    ever posted.
 */
import type {StoreApi} from './store';

/**
 * Flip to `true` for a local device walk. Never in a release: `__DEV__` gates
 * it as well, so a Release build ignores this constant.
 */
export const LOCAL_SESSION_ENABLED = false;

/** The dev session's identity, as the UI shows it. */
export const LOCAL_SESSION_LABEL =
  'LOCAL SESSION — nothing is sent to a registry';

/**
 * The `StoreApi` a local session runs on. Every call that would leave the
 * device throws instead of resolving, so a path that wrongly assumed a live
 * service fails loudly at the call site rather than reporting a card as
 * registered. The batch machine already holds rows pending on a throw, which
 * is the behaviour wanted.
 */
export function localStoreApi(): StoreApi {
  const unserviceable = (call: string) => (): Promise<never> =>
    Promise.reject(
      new Error(
        `local session: ${call} needs a live registry, and this build has none`,
      ),
    );
  return {
    serviceUrl: 'local-session',
    async signIn() {
      // A fixed operator id: the screens show it, and there is no service to
      // assign a real one.
      return {operatorId: 0, token: 'local-session'};
    },
    postBatch: unserviceable('postBatch'),
    postCard: unserviceable('postCard'),
    retryPending: unserviceable('retryPending'),
  };
}
