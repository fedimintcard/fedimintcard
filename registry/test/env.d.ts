/**
 * `env` inside a test is typed as `Cloudflare.Env`, which starts empty.
 * Declaring the bindings here is what makes `env.REGISTRY` the same type the
 * Worker's own `Env` gives it.
 *
 * `OPERATOR_SETUP` is optional, as in a deployment: without it no operator
 * exists and sign-in answers 503.
 */
declare namespace Cloudflare {
  interface Env {
    readonly REGISTRY: DurableObjectNamespace;
    readonly OPERATOR_SETUP?: string;
    readonly BACKUPS?: R2Bucket;
    readonly BACKUP_RECIPIENT?: string;
    readonly CARD_MANAGER_MASTER?: string;
    readonly CERT_SIGNING_KEY?: string;
  }
}
