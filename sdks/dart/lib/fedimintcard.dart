/// Fedimint Card 4.x public record reader.
library;

import 'dart:convert';
import 'dart:typed_data';
import 'package:pointycastle/export.dart' as pc;

export 'src/till.dart';

typedef Transceive = Future<Uint8List> Function(Uint8List command);

enum SlotState { empty, keyed, note, change, spent }

class CardStatusException implements Exception {
  final int statusWord;
  const CardStatusException(this.statusWord);
  @override
  String toString() =>
      'Card returned ${statusWord.toRadixString(16).padLeft(4, '0')}.';
}

class UnsupportedApplet implements Exception {
  final int major, minor;
  const UnsupportedApplet(this.major, this.minor);
  @override
  String toString() => 'Unsupported applet $major.$minor.';
}

class CardState {
  final List<SlotState> slots;
  final int now, window, certificateLength, wrongTries, owed;
  final BigInt limitMsat, spentMsat;
  final bool hasAttestationKey,
      hasRegistryKey,
      pinSet,
      anchored,
      chipMultiply,
      nonceKeyInRam;
  CardState._(Uint8List b)
    : slots = List.unmodifiable(b.take(32).map(_state)),
      now = ByteData.sublistView(b).getUint32(32),
      window = ByteData.sublistView(b).getUint32(36),
      limitMsat = _amount(b, 40),
      spentMsat = _amount(b, 48),
      certificateLength = b[56],
      hasAttestationKey = b[57] != 0,
      hasRegistryKey = b[58] != 0,
      pinSet = b[59] != 0,
      wrongTries = b[60],
      owed = ByteData.sublistView(b).getUint16(61),
      anchored = b[63] != 0,
      chipMultiply = (b[64] & 1) != 0,
      nonceKeyInRam = (b[64] & 2) != 0;
}

class ChangeOutput {
  final Uint8List transactionId;
  final int outputIndex;
  ChangeOutput._(this.transactionId, this.outputIndex);
}

class CardSlot {
  final SlotState state;
  final Uint8List? publicKey;
  final BigInt amountMsat;
  final ChangeOutput? changeOutput;
  CardSlot._(this.state, this.publicKey, this.amountMsat, this.changeOutput);
}

class CardBalance {
  final BigInt storedMsat, owedMsat;
  const CardBalance(this.storedMsat, this.owedMsat);
}

SlotState _state(int b) {
  if (b >= SlotState.values.length)
    throw const FormatException('Unknown card slot state.');
  return SlotState.values[b];
}

BigInt _amount(Uint8List b, int start) {
  return _unsigned(b.getRange(start, start + 8));
}

bool _sameBytes(List<int> a, List<int> b) {
  if (a.length != b.length) return false;
  for (var i = 0; i < a.length; i++) {
    if (a[i] != b[i]) return false;
  }
  return true;
}

Uint8List _key(Uint8List b) {
  if (b.length != 32)
    throw const FormatException('An x-only public key needs 32 bytes.');
  final compressed = [2, ...b];
  try {
    final key = pc.ECDomainParameters(
      'secp256k1',
    ).curve.decodePoint(compressed);
    if (key != null &&
        !key.isInfinity &&
        _sameBytes(key.getEncoded(true), compressed))
      return b;
  } catch (_) {
    // The curve implementation also rejects coordinates outside its field.
  }
  throw const FormatException('Invalid secp256k1 x-only public key.');
}

BigInt _unsigned(Iterable<int> bytes) {
  var result = BigInt.zero;
  for (final b in bytes) {
    result = (result << 8) | BigInt.from(b);
  }
  return result;
}

/// Verify issuance against an independently trusted, uncompressed registry public key,
/// binding this certificate to the public key read from slot zero in the session.
bool verifyCertificate(
  Uint8List certificate,
  Uint8List registryKey,
  Uint8List cardIdentity,
) {
  if (certificate.length != 130 ||
      certificate[0] != 1 ||
      registryKey.length != 65 ||
      registryKey[0] != 4 ||
      !_sameBytes(certificate.sublist(1, 33), cardIdentity))
    return false;
  try {
    _key(cardIdentity);
    final domain = pc.ECDomainParameters('secp256r1');
    final registry = domain.curve.decodePoint([
      2 + (registryKey.last & 1),
      ...registryKey.sublist(1, 33),
    ]);
    if (registry == null ||
        registry.isInfinity ||
        !_sameBytes(registry.getEncoded(false), registryKey))
      return false;
    final attest = certificate.sublist(33, 66);
    if (attest.first != 2 && attest.first != 3) return false;
    final attestPoint = domain.curve.decodePoint(attest);
    if (attestPoint == null ||
        attestPoint.isInfinity ||
        !_sameBytes(attestPoint.getEncoded(true), attest))
      return false;
    final verifier = pc.ECDSASigner(pc.SHA256Digest())
      ..init(
        false,
        pc.PublicKeyParameter<pc.ECPublicKey>(pc.ECPublicKey(registry, domain)),
      );
    final message = Uint8List.fromList([
      ...ascii.encode('fedimintcard/cert1'),
      ...certificate.sublist(1, 66),
    ]);
    return verifier.verifySignature(
      message,
      pc.ECSignature(
        _unsigned(certificate.sublist(66, 98)),
        _unsigned(certificate.sublist(98)),
      ),
    );
  } catch (_) {
    return false;
  }
}

void _checkSlot(int slot) {
  RangeError.checkValueInInterval(slot, 0, 31, 'slot');
}

/// Bind to one connected card. Call [close] before releasing or replacing the transport.
class FedimintCardReader {
  final Transceive _transceive;
  bool _closed = false;
  bool _busy = false;
  final int minor;
  int get major => 4;
  FedimintCardReader._(this._transceive, this.minor);

  static Future<FedimintCardReader> select(Transceive transceive) async {
    final temporary = FedimintCardReader._(transceive, 0);
    final response = await temporary._exchange([
      0,
      0xa4,
      4,
      0,
      8,
      0xf0,
      0x46,
      0x4d,
      0x43,
      0x41,
      0x52,
      0x44,
      1,
      0,
    ]);
    if (response.length != 2)
      throw const FormatException('SELECT requires two version bytes.');
    if (response[0] != 4) throw UnsupportedApplet(response[0], response[1]);
    return FedimintCardReader._(transceive, response[1]);
  }

  void close() {
    _closed = true;
  }

  Future<T> _read<T>(Future<T> Function() action) async {
    if (_closed) throw StateError('Card session has ended.');
    if (_busy) throw StateError('Another card read is in progress.');
    _busy = true;
    try {
      return await action();
    } finally {
      _busy = false;
    }
  }

  Future<Uint8List> _exchange(List<int> request) async {
    var command = Uint8List.fromList(request);
    final data = BytesBuilder();
    for (var i = 0; i < 16; i++) {
      if (_closed) throw StateError('Card session has ended.');
      final Uint8List reply;
      try {
        reply = await _transceive(command);
      } catch (_) {
        close();
        rethrow;
      }
      if (_closed) throw StateError('Card session has ended.');
      if (reply.length < 2)
        throw const FormatException('Missing card status word.');
      final length = reply.length - 2,
          sw1 = reply[length],
          sw2 = reply[length + 1];
      if (sw1 == 0x6c) {
        command = Uint8List.fromList(command)..last = sw2;
        continue;
      }
      if (sw1 != 0x61 && !(sw1 == 0x90 && sw2 == 0))
        throw CardStatusException(sw1 << 8 | sw2);
      if (data.length + length > 256)
        throw const FormatException('Oversize response.');
      data.add(Uint8List.sublistView(reply, 0, length));
      if (sw1 == 0x90) return data.takeBytes();
      command = Uint8List.fromList([0, 0xc0, 0, 0, sw2]);
    }
    throw const FormatException('Response chain limit reached.');
  }

  Future<CardState> state() => _read(_readState);
  Future<CardState> _readState() async {
    final b = await _exchange([0xb0, 0x16, 0, 0, 0]);
    if (b.length != 65)
      throw const FormatException('GET_STATE requires 65 bytes.');
    return CardState._(b);
  }

  /// Slot zero identifies the card.
  Future<Uint8List> publicKey([int slot = 0]) => _read(() async {
    _checkSlot(slot);
    return _key(await _exchange([0xb0, 0x11, slot, 0, 0]));
  });

  Future<CardSlot> slot(int index) => _read(() => _readSlot(index));
  Future<CardSlot> _readSlot(int index) async {
    _checkSlot(index);
    final b = await _exchange([0xb0, 0x15, index, 0, 0]);
    if (b.length != 187)
      throw const FormatException('GET_SLOT requires 187 bytes.');
    final kind = _state(b[0]);
    return CardSlot._(
      kind,
      kind == SlotState.empty ? null : _key(b.sublist(1, 33)),
      _amount(b, 33),
      kind == SlotState.change
          ? ChangeOutput._(
              b.sublist(121, 153),
              ByteData.sublistView(b).getUint16(153),
            )
          : null,
    );
  }

  /// Unverified certificate bytes, or null when the card has no certificate.
  Future<Uint8List?> certificate() => _read(() async {
    try {
      final b = await _exchange([0xb0, 0x32, 0, 0, 0]);
      if (b.length != 130 || b[0] != 1)
        throw const FormatException('Invalid version-one card certificate.');
      return b;
    } on CardStatusException catch (e) {
      if (e.statusWord == 0x6a83) return null;
      rethrow;
    }
  });

  /// Card-reported totals. Settlement is confirmed separately by the paired till.
  Future<CardBalance> balance() => _read(() async {
    final states = (await _readState()).slots;
    var stored = BigInt.zero, owed = BigInt.zero;
    for (var i = 0; i < states.length; i++) {
      final kind = states[i];
      if (kind != SlotState.note && kind != SlotState.change) continue;
      final entry = await _readSlot(i);
      if (entry.state != kind)
        throw const FormatException('Slot changed during read.');
      if (kind == SlotState.note) {
        stored += entry.amountMsat;
      } else {
        owed += entry.amountMsat;
      }
    }
    return CardBalance(stored, owed);
  });
}
