import {defineWorkersConfig} from '@cloudflare/vitest-pool-workers/config';

export default defineWorkersConfig({
  test: {
    poolOptions: {
      workers: {
        // The binding and the secret come from here, so tests exercise the
        // same wiring a deployment uses rather than a mock in front of it.
        wrangler: {configPath: './wrangler.toml'},
        miniflare: {
          bindings: {OPERATOR_PASSWORD: 'correct horse battery staple'},
        },
        // The pool isolates storage per test by stacking SQLite files, and the
        // SQLite-backed Durable Object leaves a `.sqlite-shm` sidecar it does
        // not recognise (developers.cloudflare.com/workers/testing/vitest-
        // integration/known-issues/#isolated-storage). Tests reset rows
        // themselves in `beforeEach` instead, which is the same guarantee for
        // this suite: every test starts from an empty table.
        isolatedStorage: false,
      },
    },
  },
});
