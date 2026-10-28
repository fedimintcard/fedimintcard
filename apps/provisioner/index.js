/**
 * @format
 */

// First: crypto.getRandomValues, for the secure channel's host challenge.
import 'react-native-get-random-values';
import {AppRegistry} from 'react-native';
import App from './App';
import {name as appName} from './app.json';

AppRegistry.registerComponent(appName, () => App);
