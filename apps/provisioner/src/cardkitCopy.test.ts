/**
 * The app bundles `@fedimintcard/cardkit` from node_modules, and yarn copies a
 * `file:` dependency at install time instead of linking it. A change to
 * tools/cardkit therefore does not reach the app until the copy is refreshed,
 * and a stale copy installs the old applet with every test still green: on
 * 2026-11-06 a build carried the 8-slot load file a day after cardkit moved to
 * applet 2.1. This test fails instead. To refresh:
 *
 *   rm -rf node_modules/@fedimintcard/cardkit && yarn install --check-files
 */
import * as fs from 'fs';
import * as path from 'path';

const SOURCE = path.resolve(__dirname, '../../../tools/cardkit');
const INSTALLED = path.dirname(
  require.resolve('@fedimintcard/cardkit/package.json'),
);

/** What Metro bundles: the package's `react-native` entry and the engine beside it. */
function bundled(root: string): string[] {
  const gp = fs
    .readdirSync(path.join(root, 'gp'))
    .filter(f => f.endsWith('.ts'))
    .map(f => `gp/${f}`);
  return ['index.ts', ...gp].sort();
}

test('the installed cardkit is the same source as tools/cardkit', () => {
  expect(bundled(INSTALLED)).toEqual(bundled(SOURCE));
  // the J3R452's SCP03 and the APDU primitives both channels share are files
  // of their own beside the SCP02 hub, and Metro bundles them with it
  expect(bundled(INSTALLED)).toEqual(
    expect.arrayContaining(['gp/apdu.ts', 'gp/scp03.ts']),
  );
  for (const f of bundled(SOURCE)) {
    const same =
      fs.readFileSync(path.join(INSTALLED, f), 'utf8') ===
      fs.readFileSync(path.join(SOURCE, f), 'utf8');
    expect([f, same]).toEqual([f, true]);
  }
});
