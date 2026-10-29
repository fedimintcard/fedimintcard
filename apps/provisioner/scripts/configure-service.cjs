#!/usr/bin/env node
/**
 * Validate (or report) the service configuration a build would ship.
 *
 * `android/app/build.gradle` runs this before every release build and before
 * the JS bundle is created, because the two things it checks are exactly the
 * two things a release must not get wrong: an app that can sign in against no
 * registry, or one that reaches a server that is not the operator's.
 *
 *   node scripts/configure-service.cjs --check                  # any build
 *   node scripts/configure-service.cjs --check --production     # a release
 *
 * A release adds two rules over a debug check:
 *
 *  - `LOCAL_SESSION_ENABLED` must be `false`. The flag supplies a fake operator
 *    session (`src/localSession.ts`); shipping it would sign anyone in with no
 *    registry and record no card anywhere.
 *  - Each registry origin must be empty (no host is deployed yet, so sign-in
 *    refuses with a message saying so) or an `https://` URL. `http://` is
 *    refused: the app posts an operator's provisioning records and the bearer
 *    token to it, and a release must not send either in the clear.
 *
 * It reads the TypeScript sources as text rather than importing them. These
 * modules import React Native, which is not loadable from Node, and a release
 * gate that needs a bundler is a gate that gets skipped.
 */
'use strict';

const fs = require('node:fs');
const path = require('node:path');

const ROOT = path.resolve(__dirname, '..');

const args = process.argv.slice(2);
const production = args.includes('--production');
const checking = args.includes('--check') || production;

/** The `= <value>` on the line that declares `name`. */
function readDeclaration(file, name) {
  const source = fs.readFileSync(path.join(ROOT, file), 'utf8');
  const line = source
    .split('\n')
    .find(l => new RegExp(`^\\s*(export\\s+)?const\\s+${name}\\s*=`).test(l));
  if (!line) {
    throw new Error(`${file}: no \`const ${name} =\` declaration`);
  }
  return line.slice(line.indexOf('=') + 1).trim().replace(/;$/, '');
}

/** The `RegistryEndpoint` → origin map, as the module states it. */
function readRegistryOrigins() {
  const source = fs.readFileSync(path.join(ROOT, 'src/api.ts'), 'utf8');
  const match = source.match(/REGISTRY_ORIGINS[^=]*=\s*Object\.freeze\(\{([\s\S]*?)\}\s*as const\)/);
  if (!match) {
    throw new Error('src/api.ts: no REGISTRY_ORIGINS object literal');
  }
  const origins = {};
  for (const [, key, , value] of match[1].matchAll(/(\w+)\s*:\s*(['"])(.*?)\2/g)) {
    origins[key] = value;
  }
  return origins;
}

const problems = [];
const facts = [];

try {
  const localSession = readDeclaration('src/localSession.ts', 'LOCAL_SESSION_ENABLED');
  const sidecar = readDeclaration('src/settings.tsx', 'LOCAL_SIDECAR_ORIGIN');
  const origins = readRegistryOrigins();

  if (!/^(true|false)$/.test(localSession)) {
    problems.push(`LOCAL_SESSION_ENABLED is \`${localSession}\`, not a boolean literal`);
  } else if (production && localSession === 'true') {
    problems.push(
      'LOCAL_SESSION_ENABLED is true: a release would sign in with no registry and record nothing',
    );
  } else {
    facts.push(`local session: ${localSession === 'true' ? 'enabled' : 'disabled'}`);
  }

  // Loopback only, on a fixed port: the till listens on 127.0.0.1 and the
  // phone reaches it through adb reverse (plan § 7).
  const sidecarUrl = sidecar.replace(/^['"]|['"]$/g, '');
  const parsed = new URL(sidecarUrl);
  if (parsed.protocol !== 'http:') {
    problems.push(`LOCAL_SIDECAR_ORIGIN is ${parsed.protocol}, expected http: (loopback)`);
  }
  if (!['127.0.0.1', 'localhost'].includes(parsed.hostname)) {
    problems.push(
      `LOCAL_SIDECAR_ORIGIN is ${sidecarUrl}: the sidecar must be loopback only (` +
        'a LAN cleartext exception is not defensible at review, plan § 7)',
    );
  }
  facts.push(`sidecar: ${sidecarUrl}`);

  const keys = Object.keys(origins);
  if (keys.length === 0) {
    problems.push('REGISTRY_ORIGINS declares no endpoint');
  }
  for (const [name, origin] of Object.entries(origins)) {
    if (origin === '') {
      // Intended today: the app names no registry host, so sign-in refuses.
      facts.push(`registry ${name}: unset (sign-in refuses by design)`);
      continue;
    }
    let url;
    try {
      url = new URL(origin);
    } catch {
      problems.push(`registry ${name}: \`${origin}\` is not a URL`);
      continue;
    }
    if (production && url.protocol !== 'https:') {
      problems.push(
        `registry ${name}: \`${origin}\` is not https — a release must not post a bearer ` +
          'token and provisioning records in the clear',
      );
      continue;
    }
    if (url.pathname !== '/' || url.search || url.hash) {
      problems.push(`registry ${name}: \`${origin}\` must be an origin, with no path`);
      continue;
    }
    facts.push(`registry ${name}: ${origin}`);
  }
} catch (error) {
  console.error(`configure-service: ${error.message}`);
  process.exit(1);
}

for (const fact of facts) {
  console.log(`  ${fact}`);
}

if (problems.length > 0) {
  for (const problem of problems) {
    console.error(`configure-service: ${problem}`);
  }
  process.exit(1);
}

if (checking) {
  console.log(
    `configure-service: ${production ? 'production' : 'debug'} configuration OK`,
  );
}
