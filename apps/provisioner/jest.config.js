module.exports = {
  preset: 'react-native',
  setupFiles: ['<rootDir>/jest.setup.js'],
  // Exercise the same public source entry points Metro selects on the phone.
  testEnvironmentOptions: {customExportConditions: ['react-native']},
  // The preset's list, plus react-native-nfc-manager (whose error classes the
  // tests take from the library itself, see jest.setup.js) and the engine,
  // which arrives as TypeScript source through its `file:` dependency.
  moduleNameMapper: {'\\.svg$': '<rootDir>/__mocks__/svgMock.js'},
  transformIgnorePatterns: [
    'node_modules/(?!((jest-)?react-native|@react-native(-community)?|react-native-nfc-manager|@fedimintcard/cardkit)/)',
  ],
};
