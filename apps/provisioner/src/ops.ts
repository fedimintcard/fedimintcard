/**
 * What ops answers for a card's lifecycle (OPS-PLAN O5), as types only: the
 * card-manager key set the install tap rotates a card to, and the reset
 * check and record. `api.ts` builds these from the registry's JSON and
 * `store.tsx` passes them through, so neither the store nor the card modules
 * import the network module.
 */

/**
 * A card's own card-manager key set, as ops escrowed it: 16 bytes each, hex.
 * Held for one tap and never written anywhere on the phone.
 */
export interface ManagerKeysReply {
  readonly chip: string;
  readonly key_version: number;
  readonly enc: string;
  readonly mac: string;
  readonly dek: string;
}

/** `GET /registry/cards/:id/reset-check`: what ops holds for a card, and whether it may be reset. */
export interface ResetCheck {
  readonly short_id: string;
  readonly chip: string | null;
  readonly empty: boolean;
  readonly resettable: boolean;
  /** Why not, in ops' words, one line each. */
  readonly reasons: readonly string[];
  readonly live_notes: number;
  readonly live_msat: number;
  readonly pending_notes: number;
  readonly pending_msat: number;
  readonly open_charges: number;
  readonly open_loads: number;
  readonly replaced_by: string | null;
}

/** The finished card a reset leaves, as the phone records it: public keys only. */
export interface ResetCardRow {
  readonly short_id: string;
  readonly installed_at: number;
  /** The eight x-only slot keys, hex, slot 0 first. */
  readonly slot_pubkeys: readonly string[];
  readonly chip: string;
}

/** `POST /registry/cards/:id/reset`: the record, and anything ops still held under the old id. */
export interface ResetRecorded {
  readonly old_short_id: string;
  readonly short_id: string;
  /** Live and pending notes left under the old short id; zero after a clean reset. */
  readonly stranded_notes: number;
}

/** The ops calls a card's lifecycle needs; each needs a signed-in operator. */
export interface CardOps {
  managerKeys(chip: string, shortId?: string): Promise<ManagerKeysReply>;
  resetCheck(shortId: string): Promise<ResetCheck>;
  recordReset(oldShortId: string, card: ResetCardRow): Promise<ResetRecorded>;
}
