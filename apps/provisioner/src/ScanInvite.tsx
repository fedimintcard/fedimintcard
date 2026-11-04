/**
 * The federation-invite scanner: a full-screen sheet with the back camera and
 * a QR code scanner. It hands back the first code that is a federation invite
 * (`normalizeInvite`) and closes; any other code shows one line and keeps
 * scanning. Camera permission is asked on open; a refusal shows the way to
 * Settings rather than a dead viewfinder.
 */
import React, {useCallback, useEffect, useRef, useState} from 'react';
import {Linking, Modal, StyleSheet, Text, View} from 'react-native';
import {
  Camera,
  useCameraDevice,
  useCameraPermission,
  useCodeScanner,
} from 'react-native-vision-camera';
import {normalizeInvite} from './federation';
import {SCAN} from './strings';
import {FONT, PrimaryButton, SecondaryButton} from './ui';

export function ScanInviteSheet({
  visible,
  onClose,
  onInvite,
}: {
  visible: boolean;
  onClose: () => void;
  /** Called once per opening, with the normalized invite. */
  onInvite: (invite: string) => void;
}) {
  const {hasPermission, requestPermission} = useCameraPermission();
  const device = useCameraDevice('back');
  const [asked, setAsked] = useState(false);
  const [rejected, setRejected] = useState<string | null>(null);
  // One result per opening: the scanner fires for every frame that shows the
  // code, and the sheet must not hand the same invite back twice.
  const taken = useRef(false);

  useEffect(() => {
    if (!visible) {
      taken.current = false;
      setRejected(null);
      return;
    }
    if (!hasPermission && !asked) {
      setAsked(true);
      requestPermission().catch(() => undefined);
    }
  }, [visible, hasPermission, asked, requestPermission]);

  const codeScanner = useCodeScanner({
    codeTypes: ['qr'],
    onCodeScanned: codes => {
      if (taken.current) {
        return;
      }
      for (const code of codes) {
        const invite = code.value ? normalizeInvite(code.value) : null;
        if (invite) {
          taken.current = true;
          onInvite(invite);
          return;
        }
      }
      if (codes.length > 0) {
        setRejected(SCAN.notInvite);
      }
    },
  });

  const allow = useCallback(() => {
    requestPermission()
      .then(granted => {
        if (!granted) {
          return Linking.openSettings();
        }
        return undefined;
      })
      .catch(() => undefined);
  }, [requestPermission]);

  const ready = hasPermission && device !== undefined;

  return (
    <Modal
      visible={visible}
      animationType="slide"
      onRequestClose={onClose}
      testID="scan-invite">
      <View style={local.root}>
        {ready ? (
          <Camera
            style={StyleSheet.absoluteFill}
            device={device}
            isActive={visible}
            codeScanner={codeScanner}
          />
        ) : null}
        <View style={local.overlay}>
          <View style={local.top}>
            <Text style={local.title}>{SCAN.title}</Text>
            <Text style={local.hint}>
              {ready
                ? rejected ?? SCAN.hint
                : hasPermission
                  ? SCAN.noCamera
                  : SCAN.noPermission}
            </Text>
          </View>
          {ready ? <View style={local.frame} /> : <View style={local.spacer} />}
          <View style={local.bottom}>
            {!hasPermission ? (
              <PrimaryButton
                title={asked ? SCAN.openSettings : SCAN.allow}
                onPress={allow}
                testID="scan-allow"
              />
            ) : null}
            <View style={local.cancel}>
              <SecondaryButton
                title={SCAN.cancel}
                onPress={onClose}
                testID="scan-cancel"
              />
            </View>
          </View>
        </View>
      </View>
    </Modal>
  );
}

const local = StyleSheet.create({
  root: {flex: 1, backgroundColor: '#0B1530'},
  overlay: {
    ...StyleSheet.absoluteFillObject,
    paddingHorizontal: 24,
    paddingTop: 72,
    paddingBottom: 48,
    justifyContent: 'space-between',
    alignItems: 'center',
  },
  top: {alignItems: 'center', gap: 8},
  title: {
    color: '#fff',
    fontFamily: FONT.semibold,
    fontSize: 20,
    textAlign: 'center',
  },
  hint: {
    color: 'rgba(255,255,255,0.85)',
    fontFamily: FONT.regular,
    fontSize: 15,
    textAlign: 'center',
  },
  frame: {
    width: 240,
    height: 240,
    borderRadius: 24,
    borderWidth: 3,
    borderColor: '#E1CF89',
  },
  spacer: {flex: 1},
  bottom: {alignSelf: 'stretch', gap: 12},
  cancel: {backgroundColor: 'rgba(11,21,48,0.88)', borderRadius: 14},
});
