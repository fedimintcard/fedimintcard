/**
 * How a federation reads on screen: its short id and guardians, decoded from
 * the invite on the phone, never the raw invite. An invite carries no name,
 * and the provisioner talks to no service that could vouch for one: the sidecar
 * that used to report it left the operator's loop (docs/OPS-PLAN.md § 4).
 */
import {useMemo} from 'react';
import {
  decodeInvite,
  guardianLabel,
  shortFederationId,
  type InviteInfo,
} from './federation';

export interface FederationView {
  readonly info: InviteInfo | null;
  /** "Federation 9dc8 86ab". */
  readonly title: string;
  /** "1 guardian · iroh node 3b30…" */
  readonly detail: string;
}

export function describeFederation(info: InviteInfo | null): {
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
    title: `Federation ${shortFederationId(info.federationId)}`,
    detail: `${n} ${n === 1 ? 'guardian' : 'guardians'}${where}`,
  };
}

export function useFederation(invite: string): FederationView {
  return useMemo(() => {
    const info = invite ? decodeInvite(invite) : null;
    return {info, ...describeFederation(info)};
  }, [invite]);
}
