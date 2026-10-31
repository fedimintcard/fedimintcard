/**
 * `env` inside a test is typed as `Cloudflare.Env`, which starts empty.
 * Declaring the bindings here is what makes `env.REGISTRY` the same type the
 * Worker's own `Env` gives it.
 *
 * `OPERATOR_PASSWORD` is deliberately absent: the Worker treats a missing
 * secret as a 503, so the `.dev.vars` run and the test run are only honest if
 * the type says the secret is optional too.
 */
declare namespace Cloudflare {
  interface Env {
    readonly REGISTRY: DurableObjectNamespace;
    readonly OPERATOR_PASSWORD?: string;
  }
}
