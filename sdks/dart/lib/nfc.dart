/// NFC sessions on Android and iOS for the Fedimint Card reader.
library;

import 'dart:typed_data';
import 'package:flutter_nfc_kit/flutter_nfc_kit.dart';
import 'fedimintcard.dart';
export 'fedimintcard.dart';

bool _scanActive = false;

/// Poll one card and keep the NFC session open for the awaited [read] callback.
/// Await each reader call before returning from [read].
Future<T> withFedimintCard<T>(
  Future<T> Function(FedimintCardReader reader) read,
) async {
  if (_scanActive)
    throw StateError('A Fedimint Card scan is already in progress.');
  _scanActive = true;
  FedimintCardReader? reader;
  var succeeded = false;
  try {
    final tag = await FlutterNfcKit.poll(
      androidCheckNDEF: false,
      readIso15693: false,
    );
    if (tag.type != NFCTagType.iso7816)
      throw const FormatException('Card does not speak ISO 7816.');
    reader = await FedimintCardReader.select(
      (command) => FlutterNfcKit.transceive<Uint8List>(command),
    );
    final result = await read(reader);
    succeeded = true;
    return result;
  } finally {
    reader?.close();
    try {
      await FlutterNfcKit.finish();
    } catch (_) {
      if (succeeded) rethrow;
    } finally {
      _scanActive = false;
    }
  }
}
