import 'dart:convert';
import 'dart:io';

import 'package:fedimintcard_flutter/fedimintcard_flutter.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

Uint8List hex(String value) => Uint8List.fromList([
  for (var i = 0; i < value.length; i += 2)
    int.parse(value.substring(i, i + 2), radix: 16),
]);

Map<String, dynamic> vector(String file) =>
    jsonDecode(File('test/$file.json').readAsStringSync());
final vectors = vector('reader-vectors');
void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  test('native adapter polls a single tag, sends binary APDUs and closes the reader', () async {
    const channel = MethodChannel('flutter_nfc_kit/method');
    final binary =
        TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger;
    final calls = <String>[];
    binary.setMockMethodCallHandler(channel, (call) async {
      calls.add(call.method);
      switch (call.method) {
        case 'poll':
          expect(call.arguments['technologies'], 0x83);
          return jsonEncode({
            'id': '00',
            'type': 'iso7816',
            'standard': 'ISO 14443-4',
          });
        case 'transceive':
          final data = call.arguments['data'] as Uint8List;
          if (calls.length == 2) {
            expect(data, hex(vectors['select']['command']));
            return hex(vectors['select']['response']);
          }
          expect(data, hex(vectors['identity']['command']));
          return hex(vectors['identity']['response']);
        case 'finish':
          return null;
      }
      fail('unexpected channel method');
    });
    addTearDown(() => binary.setMockMethodCallHandler(channel, null));
    late FedimintCardReader escaped;
    final key = await withFedimintCard((card) async {
      escaped = card;
      await expectLater(withFedimintCard((r) => r.state()), throwsStateError);
      return card.publicKey();
    });
    expect(key, hex(vectors['identity']['xOnly']));
    expect(calls, ['poll', 'transceive', 'transceive', 'finish']);
    await expectLater(escaped.state(), throwsStateError);
  });
  test('failed selection closes NFC even when the platform reports a cleanup error', () async {
    const channel = MethodChannel('flutter_nfc_kit/method');
    final binary =
        TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger;
    final calls = <String>[];
    binary.setMockMethodCallHandler(channel, (call) async {
      calls.add(call.method);
      if (call.method == 'poll')
        return jsonEncode({
          'id': '00',
          'type': 'iso7816',
          'standard': 'ISO 14443-4',
        });
      if (call.method == 'transceive') return hex('03009000');
      throw PlatformException(code: 'gone');
    });
    addTearDown(() => binary.setMockMethodCallHandler(channel, null));
    for (var i = 0; i < 2; i++) {
      await expectLater(
        withFedimintCard((r) => r.state()),
        throwsA(isA<UnsupportedApplet>()),
      );
    }
    expect(calls, [
      'poll',
      'transceive',
      'finish',
      'poll',
      'transceive',
      'finish',
    ]);
  });
}
