/**
 * How a federation reads on screen: its name when the phone's sidecar can
 * vouch for it, otherwise its short id, never the raw invite. The sidecar
 * (`till.rs` `status()`) reports the federation its wallet joined; when that
 * id equals the invite's, its `federation_name` is this federation's name.
 * When the ids differ, cards would be loaded from a different federation than
 * the one the batch records, and the screen says so.
 */
import {useEffect, useState} from 'react';
import {LocalClient} from './api';
import {
  decodeInvite,
  guardianLabel,
  shortFederationId,
  type InviteInfo,
} from './federation';
import {LOCAL_SIDECAR_ORIGIN} from './settings';

export type SidecarCheck = 'checking' | 'match' | 'mismatch' | 'offline';

export interface FederationView {
  readonly info: InviteInfo | null;
  /** The name, or "Federation 9dc8 86ab". */
  readonly title: string;
  /** "1 guardian · iroh node 3b30…" */
  readonly detail: string;
  readonly sidecar: SidecarCheck;
}

const STATUS_TIMEOUT_MS = 3000;

export function describeFederation(
  info: InviteInfo | null,
  name: string | null,
): {title: string; detail: string} {
  if (!info) {
    return {title: 'Federation', detail: 'Invite saved'};
  }
  const n = info.guardians.length;
  const first = info.guardians[0];
  const where = first ? ` · ${guardianLabel(first.url)}` : '';
  return {
    title: name ?? `Federation ${shortFederationId(info.federationId)}`,
    detail: `${n} ${n === 1 ? 'guardian' : 'guardians'}${where}`,
  };
}

export function useFederation(invite: string): FederationView {
  const info = invite ? decodeInvite(invite) : null;
  const id = info?.federationId ?? null;
  const [state, setState] = useState<{
    id: string | null;
    check: SidecarCheck;
    name: string | null;
  }>({id: null, check: 'checking', name: null});

  useEffect(() => {
    if (!id) {
      return;
    }
    let alive = true;
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), STATUS_TIMEOUT_MS);
    setState({id, check: 'checking', name: null});
    new LocalClient({baseUrl: LOCAL_SIDECAR_ORIGIN})
      .status(controller.signal)
      .then(status => {
        if (!alive) {
          return;
        }
        const same = status.federation_id.toLowerCase() === id;
        setState({
          id,
          check: same ? 'match' : 'mismatch',
          name: same ? status.federation_name : null,
        });
      })
      .catch(() => {
        if (alive) {
          setState({id, check: 'offline', name: null});
        }
      })
      .finally(() => clearTimeout(timer));
    return () => {
      alive = false;
      clearTimeout(timer);
      controller.abort();
    };
  }, [id]);

  const current = state.id === id ? state : {check: 'checking' as const, name: null};
  return {
    info,
    ...describeFederation(info, current.name),
    sidecar: id ? current.check : 'offline',
  };
}
