#!/usr/bin/env node
/**
 * Refuse a release build that cannot be uploaded, or that would be signed with
 * a key nobody controls.
 *
 * `android/app/build.gradle` gives `signingConfigs.release` nothing but the
 * environment variables below, so a release build without them either fails
 * deep inside Gradle with an opaque message or — worse, if someone leaves a
 * committed keystore behind — succeeds with a key in the repository. This runs
 * first and says which variable is missing.
 *
 *   node scripts/check-android-signing.cjs           # exit 1 on any problem
 *   node scripts/check-android-signing.cjs --report  # print what it found, exit 0
 *
 * A debug build does not go through this task; only `preReleaseBuild` and
 * `validateSigningRelease` depend on it.
 *
 * The keystore file is checked for existence but never opened: reading its
 * bytes is the business of Gradle and `keytool`, and this script has no
 * password to give them. Presence plus a non-placeholder password is the
 * whole claim it can honestly make.
 */
'use strict';

const fs = require('node:fs');

const REQUIRED = [
  ['FEDIMINTCARD_ANDROID_UPLOAD_KEYSTORE', 'path to the upload keystore (.jks/.keystore)'],
  ['FEDIMINTCARD_ANDROID_UPLOAD_STORE_PASSWORD', 'keystore password'],
  ['FEDIMINTCARD_ANDROID_UPLOAD_KEY_ALIAS', 'key alias inside the keystore'],
  ['FEDIMINTCARD_ANDROID_UPLOAD_KEY_PASSWORD', 'key password'],
];

/**
 * Values that mean "nobody set this properly". `android` is the debug
 * keystore's password, and cashucard's release line carried it literally —
 * copying that is how a release ends up signed with a key anyone can use.
 */
const PLACEHOLDERS = new Set(['android', 'password', 'changeme', 'test']);

const report = process.argv.includes('--report');
const problems = [];

for (const [name, description] of REQUIRED) {
  const value = process.env[name];
  if (!value) {
    problems.push(`${name} is not set (${description})`);
    continue;
  }
  if (name.endsWith('_PASSWORD') && PLACEHOLDERS.has(value.toLowerCase())) {
    problems.push(`${name} is a placeholder value; the release key must be its own`);
  }
  if (report) {
    const shown = name.endsWith('_PASSWORD') ? '(set)' : value;
    console.log(`  ${name}: ${shown}`);
  }
}

const keystore = process.env.FEDIMINTCARD_ANDROID_UPLOAD_KEYSTORE;
if (keystore && !fs.existsSync(keystore)) {
  problems.push(`FEDIMINTCARD_ANDROID_UPLOAD_KEYSTORE points at ${keystore}, which does not exist`);
}

if (problems.length > 0) {
  for (const problem of problems) {
    console.error(`check-android-signing: ${problem}`);
  }
  if (!report) {
    console.error(
      'check-android-signing: a release needs the upload key in its environment. ' +
        'See plan § 7 — the key is supplied by CI secrets, never committed.',
    );
    process.exit(1);
  }
}

if (report) {
  console.log('check-android-signing: report only');
} else {
  console.log('check-android-signing: upload signing configured');
}
