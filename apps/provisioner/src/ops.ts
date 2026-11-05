/**
 * What ops answers for a card's lifecycle (OPS-PLAN O5), as types only: the
 * card-manager key set the install tap rotates a card to, and the reset
 * check and completion. `api.ts` builds these from the registry's JSON and
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
}

/**
 * What a reset ends on, proved on the card in the reset tap: the Fedimint
 * package absent and the card manager on the factory keys, by a full login
 * under them. No keys, no PIN: this is the body ops records, and what the
 * phone's outbox keeps until ops has it.
 */
export interface ResetCompletion {
  /** The old card's short id. */
  readonly short_id: string;
  readonly chip: string;
  /** The key set version the factory login reported (01 on a J3R180). */
  readonly gp_key_version: number;
  readonly manager_keys: 'factory';
  readonly package: 'absent';
  /** When the tap proved it, epoch ms. */
  readonly verified_at: number;
}

/** `POST /registry/cards/:id/reset`: recorded, and anything ops still held under the old id. */
export interface ResetRecorded {
  readonly short_id: string;
  /** Whether this call released the chip's escrowed keys (false on a repeat). */
  readonly released: boolean;
  /** Live and pending notes left under the old short id; zero after a clean reset. */
  readonly stranded_notes: number;
}

/** The ops calls a card's lifecycle needs; each needs a signed-in operator. */
export interface CardOps {
  managerKeys(chip: string, shortId?: string): Promise<ManagerKeysReply>;
  resetCheck(shortId: string): Promise<ResetCheck>;
  completeReset(completion: ResetCompletion): Promise<ResetRecorded>;
}
