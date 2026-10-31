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
        bindings: {OPERATOR_PASSWORD: 'correct horse battery staple'},
      },
      // Tests reset rows themselves in `beforeEach`, so every test starts
      // from an empty table.
    }),
  ],
});
