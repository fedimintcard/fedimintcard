/**
 * How a federation reads on screen: its name and guardians, never the raw
 * invite. An invite carries no name; the operator's tills read it from the
 * federation's config and report it to the registry (OPS-PLAN § 7, O3
 * design). Until one has, the federation reads as its short id.
 */
import {useMemo} from 'react';

import {useFederationNames} from './store';
import {
  decodeInvite,
  guardianLabel,
  shortFederationId,
  type InviteInfo,
} from './federation';

export interface FederationView {
  readonly info: InviteInfo | null;
  /** The reported name, or "Federation 9dc8 86ab" until a till reports one. */
  readonly title: string;
  /** "1 guardian · iroh node 3b30…" */
  readonly detail: string;
}

export function describeFederation(
  info: InviteInfo | null,
  name?: string,
): {
  title: string;
  detail: string;
} {
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
  const names = useFederationNames();
  return useMemo(() => {
    const info = invite ? decodeInvite(invite) : null;
    return {info, ...describeFederation(info, info ? names[info.federationId] : undefined)};
  }, [invite, names]);
}
