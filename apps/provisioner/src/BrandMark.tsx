/**
 * The Fedimint card lockup: the app icon's mark (`assets/icon/icon.svg`, same
 * geometry) beside the wordmark. The wordmark is live text in the brand face,
 * not outlines, so it scales with the screen and stays readable to a screen
 * reader as one image labelled "Fedimint card".
 */
import React from 'react';
import {StyleSheet, Text, View} from 'react-native';
import Svg, {
  ClipPath,
  Defs,
  G,
  Line,
  LinearGradient,
  Polygon,
  Rect,
  Stop,
} from 'react-native-svg';
import {FONT, useTheme} from './theme';

const SPOKES: readonly [number, number, number, number][] = [
  [230, 186, 230, 128],
  [264.4, 202.6, 365, 120],
  [195.6, 202.6, 95, 120],
  [272.9, 239.8, 370, 262],
  [187.1, 239.8, 90, 262],
  [249.1, 269.6, 283, 340],
  [210.9, 269.6, 177, 340],
];
const HEPTAGON =
  '230,186 264.4,202.6 272.9,239.8 249.1,269.6 210.9,269.6 187.1,239.8 195.6,202.6';

/** The icon mark alone, as a rounded tile. */
export function BrandTile({size}: {size: number}) {
  return (
    <Svg width={size} height={size} viewBox="0 0 460 460">
      <Defs>
        <LinearGradient id="bt-navy" x1="0" y1="0" x2="0" y2="1">
          <Stop offset="0" stopColor="#24428E" />
          <Stop offset="1" stopColor="#12214A" />
        </LinearGradient>
        <LinearGradient id="bt-gold" x1="0" y1="0" x2="1" y2="1">
          <Stop offset="0" stopColor="#E1CF89" />
          <Stop offset="1" stopColor="#AF9244" />
        </LinearGradient>
        <ClipPath id="bt-panel">
          <Rect x="95" y="128" width="269" height="203" rx="30" />
        </ClipPath>
      </Defs>
      <Rect width="460" height="460" rx="104" fill="url(#bt-navy)" />
      <Rect x="95" y="128" width="269" height="203" rx="30" fill="url(#bt-gold)" />
      <G clipPath="url(#bt-panel)" fill="none" stroke="#24418C" strokeWidth={7}>
        {SPOKES.map(([x1, y1, x2, y2], i) => (
          <Line key={i} x1={x1} y1={y1} x2={x2} y2={y2} />
        ))}
        <Polygon points={HEPTAGON} />
      </G>
    </Svg>
  );
}

/** Mark + wordmark, for sign-in. */
export function BrandLockup({size = 52}: {size?: number}) {
  const t = useTheme();
  return (
    <View
      style={s.row}
      accessible
      accessibilityRole="image"
      accessibilityLabel="Fedimint card"
      testID="brand-lockup">
      <View style={[s.tile, {borderColor: t.line, borderRadius: size * 0.24}]}>
        <BrandTile size={size} />
      </View>
      <Text style={[s.word, {color: t.ink, fontSize: size * 0.52}]}>
        Fedimint
        <Text style={{color: t.accent, fontFamily: FONT.medium}}> card</Text>
      </Text>
    </View>
  );
}

const s = StyleSheet.create({
  row: {flexDirection: 'row', alignItems: 'center', gap: 14},
  tile: {borderWidth: 1, overflow: 'hidden'},
  word: {fontFamily: FONT.bold, letterSpacing: -0.6},
});
