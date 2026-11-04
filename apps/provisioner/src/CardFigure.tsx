/**
 * The card figure: the card's own face, drawn here rather than shipped as
 * artwork, so it carries the federation's mark at any size and owes nothing to
 * another product's brand. The face is dimmed while the card is blank and
 * revealed left to right as the write's blocks land — the sweep is the run's
 * real block count, never an animation.
 *
 * The card is an NXP J3R180: a black face, the chip at the left, and the eight
 * note slots drawn along the bottom so the figure says what the card is holding
 * as well as how far the write has got.
 */
import React from 'react';
import {StyleSheet, View} from 'react-native';
import Svg, {
  Defs,
  G,
  LinearGradient,
  Rect,
  Stop,
  Text as SvgText,
} from 'react-native-svg';

import {FONT, useTheme, type Theme} from './ui';

const W = 240;
/** ISO/IEC 7810 ID-1: 85.60 × 53.98 mm. The figure keeps the card's own ratio. */
const H = W / (85.6 / 53.98);

export type FigureState = 'idle' | 'running' | 'done' | 'stop' | 'danger';

/** The card's face, at full ink. */
function Face({t}: {t: Theme}) {
  return (
    <G>
      <Defs>
        <LinearGradient id="face" x1="0" y1="0" x2="1" y2="1">
          <Stop offset="0" stopColor={t.dark ? '#131316' : '#101014'} />
          <Stop offset="1" stopColor={t.dark ? '#050506' : '#000000'} />
        </LinearGradient>
      </Defs>
      <Rect width={W} height={H} rx={16} fill="url(#face)" />

      {/* the chip: contact pads, so the face reads as a card and not a ticket */}
      <Rect x={22} y={54} width={38} height={30} rx={5} fill="#C9A227" opacity={0.92} />
      <Rect x={22} y={54} width={38} height={30} rx={5} fill="none" stroke="#8A6E12" strokeWidth={1} />
      <Rect x={22} y={66.5} width={38} height={1} fill="#8A6E12" opacity={0.7} />
      <Rect x={34} y={54} width={1} height={30} fill="#8A6E12" opacity={0.7} />
      <Rect x={47} y={54} width={1} height={30} fill="#8A6E12" opacity={0.7} />

      {/* the federation's mark: the lightning bolt the brand uses */}
      <G transform="translate(150 34)">
        <Rect width={56} height={56} rx={28} fill={t.accent} opacity={0.16} />
        <Rect
          width={56}
          height={56}
          rx={28}
          fill="none"
          stroke={t.accent}
          strokeWidth={1.5}
          opacity={0.55}
        />
        <SvgText
          x={28}
          y={41}
          fontSize={30}
          textAnchor="middle"
          fill={t.accent}
          fontFamily={FONT.semibold}>
          ⚡
        </SvgText>
      </G>

      {/* the eight slots, along the base */}
      <G transform={`translate(22 ${H - 34})`}>
        {Array.from({length: 8}, (_, i) => (
          <Rect
            key={i}
            x={i * 11}
            y={0}
            width={7}
            height={4}
            rx={2}
            fill={t.ink}
            opacity={0.22}
          />
        ))}
      </G>

      {/* the card's own name, under the chip */}
      <SvgText
        x={22}
        y={H - 46}
        fontSize={13}
        fill={t.ink}
        opacity={0.85}
        fontFamily={FONT.semibold}
        letterSpacing={2.4}>
        FEDIMINT
      </SvgText>
    </G>
  );
}

export function CardFigure({
  fill,
  state,
}: {
  /** 0..1 of the write done; 1 = written. */
  fill: number;
  state: FigureState;
}) {
  const t = useTheme();
  const s = styles(t);
  const ghost = fill < 1 ? 0.22 : 1;
  const border =
    state === 'danger'
      ? t.danger
      : state === 'done'
        ? t.accent
        : t.line;
  const swept = Math.max(0, Math.min(1, fill));

  return (
    <View
      style={[s.box, {borderColor: border, width: W, height: H}]}
      accessible
      accessibilityRole="image"
      accessibilityLabel={`Card ${Math.round(swept * 100)} percent written`}>
      {/* the unwritten card: the face at ghost opacity */}
      <Svg width={W} height={H} viewBox={`0 0 ${W} ${H}`}>
        <G opacity={ghost}>
          <Face t={t} />
        </G>
      </Svg>
      {/* the written sweep: the same face at full ink, clipped to the write */}
      {swept > 0 ? (
        <View style={[s.sweep, {width: W * swept, height: H}]}>
          <Svg width={W} height={H} viewBox={`0 0 ${W} ${H}`}>
            <Face t={t} />
          </Svg>
        </View>
      ) : null}
    </View>
  );
}

const styles = (t: Theme) =>
  StyleSheet.create({
    box: {
      borderRadius: 16,
      overflow: 'hidden',
      borderWidth: 2,
      backgroundColor: t.card,
      alignSelf: 'center',
    },
    sweep: {position: 'absolute', left: 0, top: 0, overflow: 'hidden'},
  });
