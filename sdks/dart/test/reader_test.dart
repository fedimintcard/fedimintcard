import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:fedimintcard/nfc.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

Uint8List hex(String value) => Uint8List.fromList([
  for (var i = 0; i < value.length; i += 2)
    int.parse(value.substring(i, i + 2), radix: 16),
]);

Map<String, dynamic> vector(String file) =>
    jsonDecode(File('test/$file.json').readAsStringSync());
final vectors = vector('reader-vectors');
final cert = vector('certificate-vector');
typedef Step = (String, String);
Step step(Map<String, dynamic> item) =>
    (item['command'] as String, item['response'] as String);

Transceive script(List<Step> steps) {
  addTearDown(() => expect(steps, isEmpty));
  return (command) async {
    final (expected, response) = steps.removeAt(0);
    expect(command, orderedEquals(hex(expected)));
    return hex(response);
  };
}

String responseForSlot(int state, String amountHex) {
  return state.toRadixString(16).padLeft(2, '0') +
      (vectors['identity']['xOnly'] as String) +
      amountHex +
      '00' * 146 +
      '9000';
}

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  test('reads only the public output reference for owed change', () async {
    final reply = hex(responseForSlot(3, '0000000000000007'));
    reply.fillRange(121, 153, 0x11);
    reply[153] = 1;
    reply[154] = 244;
    reply.fillRange(155, 187, 0xab);
    final encoded = reply
        .map((b) => b.toRadixString(16).padLeft(2, '0'))
        .join();
    final reader = await FedimintCardReader.select(
      script([step(vectors['select']), ('b015030000', encoded)]),
    );
    final slot = await reader.slot(3);
    expect(slot.changeOutput!.transactionId, List.filled(32, 0x11));
    expect(slot.changeOutput!.outputIndex, 500);
  });
  test(
    'accepts supported minor versions and rejects old and malformed replies',
    () async {
      final select = step(vectors['select']);
      for (final row in ['select', 'futureMinor']) {
        final reader = await FedimintCardReader.select(
          script([(select.$1, vectors[row]['response'])]),
        );
        expect(reader.minor, vectors[row]['minor']);
      }
      for (final (reply, error) in [
        (vectors['legacy']['response'] as String, isA<UnsupportedApplet>()),
        ('05009000', isA<UnsupportedApplet>()),
        (vectors['absent']['response'] as String, isA<CardStatusException>()),
        (
          vectors['malformedSelect']['response'] as String,
          isA<FormatException>(),
        ),
        (vectors['malformedResponse'] as String, isA<FormatException>()),
      ]) {
        await expectLater(
          FedimintCardReader.select(script([(select.$1, reply)])),
          throwsA(error),
        );
      }
    },
  );

  test(
    'reads the public state, identity and certificate against a pinned registry key',
    () async {
      final reader = await FedimintCardReader.select(
        script([
          step(vectors['select']),
          step(vectors['state']),
          step(vectors['identity']),
          step(cert),
          ('b032000000', '6a83'),
        ]),
      );
      final state = await reader.state();
      expect(state.slots.map((e) => e.index), vectors['state']['slots']);
      expect(state.spentMsat, BigInt.parse(vectors['state']['spentMsat']));
      expect(state.limitMsat, BigInt.parse(vectors['state']['limitMsat']));
      expect(state.now, vectors['state']['now']);
      expect(state.owed, 33);
      final identity = await reader.publicKey();
      expect(identity, hex(cert['observedSlot0']));
      final certificate = (await reader.certificate())!;
      final issuer = hex(cert['registryPublicKey']);
      expect(verifyCertificate(certificate, issuer, identity), isTrue);
      expect(
        verifyCertificate(
          certificate,
          hex(cert['otherRegistryPublicKey']),
          identity,
        ),
        isFalse,
      );
      expect(verifyCertificate(certificate, issuer, Uint8List(32)), isFalse);
      certificate.last ^= 1;
      expect(verifyCertificate(certificate, issuer, identity), isFalse);
      expect(await reader.certificate(), isNull);
      await expectLater(reader.slot(32), throwsRangeError);
    },
  );

  test(
    'keeps owed change separate and detects a slot changing during the read',
    () async {
      for (final kind in [3, 4]) {
        final reader = await FedimintCardReader.select(
          script([
            step(vectors['select']),
            step(vectors['state']),
            ('b015010000', responseForSlot(2, 'ffffffffffffffff')),
            ('b015030000', responseForSlot(kind, '0000000000000007')),
          ]),
        );
        if (kind == 3) {
          final balance = await reader.balance();
          expect(
            balance.storedMsat,
            BigInt.parse('ffffffffffffffff', radix: 16),
          );
          expect(balance.owedMsat, BigInt.from(7));
        } else {
          await expectLater(reader.balance(), throwsFormatException);
        }
      }
    },
  );

  test(
    'continues a fragmented response and corrects the expected length',
    () async {
      final reader = await FedimintCardReader.select(
        script([
          (vectors['select']['command'], '046102'),
          ('00c0000002', '6c01'),
          ('00c0000001', '009000'),
        ]),
      );
      expect(reader.major, 4);
      expect(reader.minor, 0);
    },
  );

  test(
    'a failed transport invalidates the reader and simultaneous reads are rejected',
    () async {
      final reply = Completer<Uint8List>();
      var calls = 0;
      final reader = await FedimintCardReader.select((_) async {
        if (++calls == 1) return hex(vectors['select']['response']);
        return reply.future;
      });
      final first = reader.state();
      await expectLater(reader.state(), throwsStateError);
      reply.completeError(StateError('disconnected'));
      await expectLater(first, throwsStateError);
      await expectLater(reader.state(), throwsStateError);
      expect(calls, 2);
    },
  );
  test(
    'native adapter polls a single tag, sends binary APDUs and closes the reader',
    () async {
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
      expect(key, hex(cert['observedSlot0']));
      expect(calls, ['poll', 'transceive', 'transceive', 'finish']);
      await expectLater(escaped.state(), throwsStateError);
    },
  );
  test(
    'failed selection closes NFC even when the platform reports a cleanup error',
    () async {
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
    },
  );
}
