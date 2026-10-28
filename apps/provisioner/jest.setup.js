/* global jest */
jest.mock('@react-native-async-storage/async-storage', () =>
  require('@react-native-async-storage/async-storage/jest/async-storage-mock'),
);
// The NFC module is native; the tests drive cardkit with their own transceivers.
// Its error classes are plain JavaScript and are the library's own, so a test
// throws what the phone would.
jest.mock('react-native-nfc-manager', () => ({
  __esModule: true,
  NfcError: jest.requireActual('react-native-nfc-manager/src/NfcError'),
  default: {
    start: jest.fn(() => Promise.resolve()),
    requestTechnology: jest.fn(() =>
      Promise.reject(new Error('no NFC in tests')),
    ),
    cancelTechnologyRequest: jest.fn(() => Promise.resolve()),
    setAlertMessageIOS: jest.fn(() => Promise.resolve()),
    setTimeout: jest.fn(() => Promise.resolve()),
    getTimeout: jest.fn(() => Promise.resolve(15000)),
    getMaxTransceiveLength: jest.fn(() => Promise.resolve(261)),
    isoDepHandler: {
      transceive: jest.fn(() => Promise.reject(new Error('no card'))),
    },
  },
  NfcTech: {IsoDep: 'IsoDep'},
}));

// react-native-svg is native; in jest every element would be undefined, so
// the mock renders a lone View (the tests never inspect the gear's insides).
jest.mock('react-native-svg', () => {
  const React = require('react');
  const {View} = require('react-native');
  const make = (name: string) =>
    React.forwardRef(function Mocked(
      props: Record<string, unknown>,
      ref: unknown,
    ) {
      return React.createElement(View, {...props, ref}, props.children);
    });
  return {
    __esModule: true,
    default: make('Svg'),
    Svg: make('Svg'),
    Path: make('Path'),
    Circle: make('Circle'),
    Rect: make('Rect'),
    G: make('G'),
  };
});

// Keep the real contexts/components; the package's mock supplies test insets.
jest.mock(
  'react-native-safe-area-context',
  () => require('react-native-safe-area-context/jest/mock').default,
);
