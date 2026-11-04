/**
 * Tap-to-read help, from the Cashu provisioner: routine explanations live in a
 * panel behind an (i) instead of as paragraphs under every field, so the
 * screens show what the operator acts on and the reasons are one tap away.
 */
import React, {useEffect, useRef, useState} from 'react';
import {
  AccessibilityInfo,
  findNodeHandle,
  Keyboard,
  Modal,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  View,
  type StyleProp,
  type TextStyle,
} from 'react-native';
import {SafeAreaView} from 'react-native-safe-area-context';
import {FONT, useTheme, type Theme} from './theme';

const panelStyles = (t: Theme) =>
  StyleSheet.create({
    trigger: {
      minWidth: 44,
      minHeight: 44,
      alignItems: 'center',
      justifyContent: 'center',
    },
    icon: {
      width: 22,
      height: 22,
      borderRadius: 11,
      borderWidth: 1.25,
      borderColor: t.dim,
      color: t.dim,
      fontFamily: FONT.semibold,
      fontSize: 13,
      lineHeight: 19,
      textAlign: 'center',
    },
    overlay: {
      flex: 1,
      backgroundColor: 'rgba(3,8,22,0.72)',
      justifyContent: 'flex-end',
      padding: 16,
    },
    panel: {
      maxHeight: '85%',
      backgroundColor: t.card,
      borderRadius: 22,
      borderWidth: 1,
      borderColor: t.line,
      padding: 20,
      gap: 14,
    },
    header: {flexDirection: 'row', alignItems: 'center', gap: 12},
    title: {flex: 1, color: t.ink, fontFamily: FONT.semibold, fontSize: 21},
    close: {
      minWidth: 60,
      minHeight: 44,
      alignItems: 'center',
      justifyContent: 'center',
    },
    closeText: {color: t.accent, fontFamily: FONT.semibold, fontSize: 16},
    body: {color: t.ink, fontFamily: FONT.regular, fontSize: 16, lineHeight: 24},
  });

export function InfoButton({
  title,
  children,
  testID,
  accessibilityLabel,
}: {
  title: string;
  children: string;
  testID: string;
  accessibilityLabel?: string;
}) {
  const t = useTheme();
  const s = panelStyles(t);
  const [open, setOpen] = useState(false);
  const trigger = useRef<View>(null);
  const heading = useRef<Text>(null);
  const frame = useRef<number | null>(null);
  useEffect(
    () => () => {
      if (frame.current !== null) {
        cancelAnimationFrame(frame.current);
      }
    },
    [],
  );
  const focus = (target: View | Text | null) => {
    const handle = target && findNodeHandle(target);
    if (handle) {
      AccessibilityInfo.setAccessibilityFocus(handle);
    }
  };
  const close = () => {
    setOpen(false);
    if (frame.current !== null) {
      cancelAnimationFrame(frame.current);
    }
    // Focus returns to the (i) once the panel is gone, not to the top.
    frame.current = requestAnimationFrame(() => {
      frame.current = null;
      focus(trigger.current);
    });
  };
  return (
    <>
      <Pressable
        ref={trigger}
        testID={testID}
        style={s.trigger}
        accessibilityRole="button"
        accessibilityLabel={accessibilityLabel ?? `About ${title}`}
        accessibilityState={{expanded: open}}
        onPress={() => {
          Keyboard.dismiss();
          setOpen(!open);
        }}>
        <Text style={s.icon} accessible={false} allowFontScaling={false}>
          i
        </Text>
      </Pressable>
      {open ? (
        <Modal
          transparent
          visible
          animationType="fade"
          onRequestClose={close}
          onShow={() => focus(heading.current)}>
          <View style={s.overlay}>
            <Pressable
              accessible={false}
              style={StyleSheet.absoluteFill}
              onPress={close}
            />
            <SafeAreaView
              edges={['bottom']}
              style={s.panel}
              accessibilityViewIsModal
              onAccessibilityEscape={close}>
              <View style={s.header}>
                <Text ref={heading} style={s.title} accessibilityRole="header">
                  {title}
                </Text>
                <Pressable
                  testID={`${testID}-close`}
                  style={s.close}
                  accessibilityRole="button"
                  accessibilityLabel="Close explanation"
                  onPress={close}>
                  <Text style={s.closeText}>Close</Text>
                </Pressable>
              </View>
              <ScrollView>
                <Text style={s.body} testID={`${testID}-body`}>
                  {children}
                </Text>
              </ScrollView>
            </SafeAreaView>
          </View>
        </Modal>
      ) : null}
    </>
  );
}

/** A heading with its (i) beside it. */
export function HelpHeading({
  title,
  children,
  testID,
  titleStyle,
}: {
  title: string;
  children: string;
  testID: string;
  titleStyle?: StyleProp<TextStyle>;
}) {
  const t = useTheme();
  return (
    <View style={headingStyles.row}>
      <Text
        style={[
          {color: t.ink, fontFamily: FONT.medium, fontSize: 17},
          headingStyles.label,
          titleStyle,
        ]}
        accessibilityRole="header">
        {title}
      </Text>
      <InfoButton title={title} testID={testID}>
        {children}
      </InfoButton>
    </View>
  );
}

const headingStyles = StyleSheet.create({
  row: {flexDirection: 'row', alignItems: 'center', gap: 4, minHeight: 32},
  label: {flex: 1},
});
