/**
 * The card figure: the card's own face, drawn here rather than shipped as
 * artwork, so it carries the federation's mark at any size and owes nothing to
 * another product's brand. The face is dimmed while the card is blank and
 * revealed left to right as the write's blocks land — the sweep is the run's
 * real block count, never an animation.
 *
 * The face is the app icon's navy with its gold chip and the name, and the
 * eight note slots along the bottom, so the figure says what the card can hold
 * as well as how far the write has got.
 */
import React from 'react';
import {StyleSheet, View} from 'react-native';
import Svg, {
  ClipPath,
  Defs,
  G,
  Line,
  LinearGradient,
  Polygon,
  Rect,
  Stop,
  Text as SvgText,
  TSpan,
} from 'react-native-svg';

import {FONT, useTheme, type Theme} from './ui';

const W = 240;
/** ISO/IEC 7810 ID-1: 85.60 × 53.98 mm. The figure keeps the card's own ratio. */
const H = W / (85.6 / 53.98);

export type FigureState = 'idle' | 'running' | 'done' | 'stop' | 'danger';

/** The chip, from the app icon (`assets/icon/icon.svg`): gold, with the mark cut in. */
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
const CHIP_W = 42;
const CHIP_SCALE = CHIP_W / 269;

/** The card's face, at full ink: the icon's navy, its gold chip, the name. */
function Face({id}: {id: string}) {
  return (
    <G>
      <Defs>
        <LinearGradient id={`${id}-face`} x1="0" y1="0" x2="1" y2="1">
          <Stop offset="0" stopColor="#24428E" />
          <Stop offset="1" stopColor="#12214A" />
        </LinearGradient>
        <LinearGradient id={`${id}-gold`} x1="0" y1="0" x2="1" y2="1">
          <Stop offset="0" stopColor="#E1CF89" />
          <Stop offset="1" stopColor="#AF9244" />
        </LinearGradient>
        <ClipPath id={`${id}-chip`}>
          <Rect x="95" y="128" width="269" height="203" rx="30" />
        </ClipPath>
      </Defs>
      <Rect width={W} height={H} rx={16} fill={`url(#${id}-face)`} />

      {/* the name, top left, in the icon's gold */}
      <SvgText
        x={22}
        y={34}
        fontSize={17}
        fill="#E1CF89"
        fontFamily={FONT.bold}
        letterSpacing={0.2}>
        Fedimint
        <TSpan fill="#EEF1F8" fontFamily={FONT.medium}>
          {' card'}
        </TSpan>
      </SvgText>

      {/* the chip: the icon's own, so the card and the app are one object */}
      <G
        transform={`translate(${22 - 95 * CHIP_SCALE} ${56 - 128 * CHIP_SCALE}) scale(${CHIP_SCALE})`}>
        <Rect x="95" y="128" width="269" height="203" rx="30" fill={`url(#${id}-gold)`} />
        <G clipPath={`url(#${id}-chip)`} fill="none" stroke="#24418C" strokeWidth={9}>
          {SPOKES.map(([x1, y1, x2, y2], k) => (
            <Line key={k} x1={x1} y1={y1} x2={x2} y2={y2} />
          ))}
          <Polygon points={HEPTAGON} />
        </G>
      </G>

      {/* the eight note slots, along the base */}
      <G transform={`translate(22 ${H - 26})`}>
        {Array.from({length: 8}, (_, k) => (
          <Rect
            key={k}
            x={k * 12}
            y={0}
            width={8}
            height={4}
            rx={2}
            fill="#EEF1F8"
            opacity={0.35}
          />
        ))}
      </G>
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
  // The blank card is the same face, faded: visible on navy, plainly unwritten.
  const ghost = fill < 1 ? 0.38 : 1;
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
          <Face id="ghost" />
        </G>
      </Svg>
      {/* the written sweep: the same face at full ink, clipped to the write */}
      {swept > 0 ? (
        <View style={[s.sweep, {width: W * swept, height: H}]}>
          <Svg width={W} height={H} viewBox={`0 0 ${W} ${H}`}>
            <Face id="ink" />
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
      backgroundColor: t.bg,
      alignSelf: 'center',
    },
    sweep: {position: 'absolute', left: 0, top: 0, overflow: 'hidden'},
  });
