/**
 * The one seam between the app and the engine (docs/PROVISIONER-PLAN.md § 5:
 * "`src/cardkit.ts` re-export of tools/cardkit + FEDIMINT_PLAN"). Nothing else
 * in `src/` may import `@fedimintcard/cardkit`: a second import path would be a
 * second place to change when the engine moves.
 *
 * The engine is consumed as-is, exports included: `export *` means a function
 * added in `tools/cardkit` is reachable without an edit here. What this file
 * adds is the plan, bound to the one load file cardkit ships — so the app can
 * no more carry a second copy of those 5,513 bytes than it can install a second
 * applet. `tools/cardkit/make-assets.py --check` pins them, and cardkit
 * re-hashes the bytes before it sends one.
 *
 * `FEDIMINT_PLAN` is cardkit's `fedimintPlan(loadFile)` applied to that asset.
 * It is a `const`, not a factory: the plan carries no per-run state, and a card
 * is only correct if the bytes it receives are the ones the build pinned.
 */
import {FEDIMINT_LOAD_FILE, fedimintPlan} from '@fedimintcard/cardkit';

export * from '@fedimintcard/cardkit';

/** sha256 of the load file, read off the asset rather than restated. */
export const LOAD_FILE_SHA256: string = FEDIMINT_LOAD_FILE.sha256;

/** The load file's size in bytes; 5,513, which is 23 LOAD commands at 247 bytes. */
export const LOAD_FILE_SIZE: number = FEDIMINT_LOAD_FILE.size;

/**
 * The install: probe → selectIsd → auth → sweep → installForLoad → load →
 * installOpener → installCard → verify (→ done). No `guard`: this applet has no
 * `GET_INFO` to read, so the sweep is gated only by the sidecar pre-check and
 * the operator's Replace switch (plan § 5, § 10).
 */
export const FEDIMINT_PLAN = fedimintPlan(FEDIMINT_LOAD_FILE);
