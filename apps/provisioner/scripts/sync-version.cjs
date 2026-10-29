#!/usr/bin/env node
/**
 * One version, three files. `package.json` is the source; this writes it into
 * the two native projects, and `--check` fails when they have drifted.
 *
 * Why a script and not a Gradle/Xcode build setting: the two platforms read
 * their versions through completely different mechanisms (Gradle's
 * `versionName`/`versionCode`, Xcode's `MARKETING_VERSION`/`CURRENT_PROJECT_VERSION`
 * plus `Info.plist`), and a store submission is rejected — or worse, silently
 * rejected as a duplicate build number — when one of them is stale. There is no
 * shared file either toolchain reads, so the sync has to be explicit.
 *
 *   node scripts/sync-version.cjs            # write the native projects
 *   node scripts/sync-version.cjs --check    # exit 1 if they disagree
 *
 * `version` is the marketing version ("1.0.0"); `provisionerBuild` is the
 * integer build number both stores require to increase on every upload.
 *
 * Both stores also fix the version of an upload: a build number can never be
 * reused for a version once uploaded, so bump `provisionerBuild` before every
 * submission, including a re-upload of the same marketing version.
 */
'use strict';

const fs = require('node:fs');
const path = require('node:path');

const ROOT = path.resolve(__dirname, '..');
const checking = process.argv.includes('--check');

const pkg = JSON.parse(
  fs.readFileSync(path.join(ROOT, 'package.json'), 'utf8'),
);
const version = pkg.version;
const build = pkg.provisionerBuild;

const problems = [];

if (typeof version !== 'string' || !/^\d+\.\d+\.\d+$/.test(version)) {
  problems.push(`package.json version is \`${version}\`, expected MAJOR.MINOR.PATCH`);
}
if (!Number.isInteger(build) || build < 1) {
  problems.push(`package.json provisionerBuild is \`${build}\`, expected a positive integer`);
}
if (problems.length > 0) {
  for (const problem of problems) {
    console.error(`sync-version: ${problem}`);
  }
  process.exit(1);
}

/** Replace every match, recording whether the file already agreed. */
function sync(relPath, pattern, replacement, {label, expected} = {}) {
  const file = path.join(ROOT, relPath);
  const before = fs.readFileSync(file, 'utf8');
  const after = before.replace(pattern, replacement);
  const matches = before.match(pattern);
  if (!matches) {
    problems.push(`${relPath}: no ${label ?? pattern} to update`);
    return;
  }
  if (after === before) {
    return;
  }
  if (checking) {
    problems.push(`${relPath}: ${label ?? 'version'} is not ${expected}`);
  } else {
    fs.writeFileSync(file, after);
  }
}

// Android: one place each, in the defaultConfig block.
sync('android/app/build.gradle', /(\n\s*versionCode\s+)\d+/, `$1${build}`, {
  label: 'versionCode',
  expected: build,
});
sync('android/app/build.gradle', /(\n\s*versionName\s+)"[^"]*"/, `$1"${version}"`, {
  label: 'versionName',
  expected: version,
});

// iOS: both build configurations (Debug and Release) carry the pair.
sync(
  'ios/FedimintProvisioner.xcodeproj/project.pbxproj',
  /(CURRENT_PROJECT_VERSION = )\d+;/g,
  `$1${build};`,
  {label: 'CURRENT_PROJECT_VERSION', expected: build},
);
sync(
  'ios/FedimintProvisioner.xcodeproj/project.pbxproj',
  /(MARKETING_VERSION = )[^;]+;/g,
  `$1${version};`,
  {label: 'MARKETING_VERSION', expected: version},
);

// Info.plist states the marketing version literally, and a literal that
// disagrees with MARKETING_VERSION is what a store reads.
sync(
  'ios/FedimintProvisioner/Info.plist',
  /(<key>CFBundleShortVersionString<\/key>\s*<string>)[^<]*(<\/string>)/,
  `$1${version}$2`,
  {label: 'CFBundleShortVersionString', expected: version},
);

if (problems.length > 0) {
  for (const problem of problems) {
    console.error(`sync-version: ${problem}`);
  }
  process.exit(1);
}

console.log(
  `sync-version: ${version} (${build}) ${checking ? 'is consistent' : 'written to Android and iOS'}`,
);
