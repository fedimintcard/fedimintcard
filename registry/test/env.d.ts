/**
 * `env` inside a pooled test is typed as `ProvidedEnv`, which starts empty.
 * Declaring the binding here is what makes `env.REGISTRY` the same type the
 * Worker's own `Env` gives it.
 */
declare module 'cloudflare:test' {
  interface ProvidedEnv {
    REGISTRY: DurableObjectNamespace;
    OPERATOR_PASSWORD: string;
  }
}
