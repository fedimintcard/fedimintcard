import {cloudflareTest} from '@cloudflare/vitest-plugin';
import {defineConfig} from 'vitest/config';

// The integration is a Vite plugin rather than a config wrapper, so the
// options live in `plugins` instead of `test.poolOptions.workers`.
export default defineConfig({
  plugins: [
    cloudflareTest({
      // The binding and the secret come from here, so tests exercise the
      // same wiring a deployment uses rather than a mock in front of it.
      wrangler: {configPath: './wrangler.toml'},
      miniflare: {
        // The bootstrap command a deployment sets as a secret: the tests sign
        // in as this operator.
        bindings: {
          OPERATOR_SETUP: JSON.stringify({
            action: 'bootstrap',
            nonce: 'test-bootstrap-nonce-0001',
            name: 'operator',
            password: 'correct horse battery staple',
          }),
          // A test-only age key pair; its identity is in registry.test.ts.
          BACKUP_RECIPIENT: 'age1thskleqxjwaf5peh9adq5wz5jxmenhrhgwhl0c5j4xgyhwf9tf2q6zh9mt',
        },
        r2Buckets: ['BACKUPS'],
      },
      // Tests reset rows themselves in `beforeEach`, so every test starts
      // from an empty table.
    }),
  ],
});
