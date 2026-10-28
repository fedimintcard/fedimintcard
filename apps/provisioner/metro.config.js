const {getDefaultConfig, mergeConfig} = require('@react-native/metro-config');

/**
 * Metro configuration
 * https://reactnative.dev/docs/metro
 *
 * `@fedimintcard/cardkit` is an installed package (`file:../../tools/cardkit`),
 * so Metro resolves it through node_modules like any other dependency: its
 * package `main` is `index.ts`, which the RN transformer compiles from source.
 * No `watchFolders` alias and no repository-relative resolver hack: a second
 * way in would be a second copy of the engine in the bundle.
 *
 * The load file is cardkit's own (`FEDIMINT_LOAD_FILE`, re-exported by
 * `src/cardkit.ts`), so the bytes the app sends are the bytes the package
 * ships. The app carries no second copy to drift.
 *
 * Package exports are enabled because the engine's `exports` map selects the
 * react-native condition; `react-native-svg-transformer` renders `.svg` assets
 * (the card figure) as components.
 *
 * @type {import('@react-native/metro-config').MetroConfig}
 */
const defaults = getDefaultConfig(__dirname);
defaults.transformer.getTransformOptions = async () => ({
  transform: {experimentalImportSupport: false, inlineRequires: true},
});

const config = {
  transformer: {
    babelTransformerPath: require.resolve('react-native-svg-transformer'),
  },
  resolver: {
    assetExts: defaults.resolver.assetExts.filter(ext => ext !== 'svg'),
    sourceExts: [...defaults.resolver.sourceExts, 'svg'],
    unstable_enablePackageExports: true,
  },
};

module.exports = mergeConfig(defaults, config);
