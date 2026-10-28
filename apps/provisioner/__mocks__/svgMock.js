// The jest stand-in for a *.svg import (react-native-svg-transformer
// covers Metro; jest needs a plain component module). Renders a View
// carrying the svg's path data as a prop for debugging.
const React = require('react');
const {View} = require('react-native');

module.exports = React.forwardRef(function SvgMock(props, ref) {
  return React.createElement(View, {...props, ref});
});
