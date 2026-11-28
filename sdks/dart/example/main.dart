import 'package:fedimintcard/fedimintcard.dart';

/// Pass a transport connected to a single card. Each reply includes SW1 and SW2.
Future<CardBalance> readBalance(Transceive transceive) async {
  final reader = await FedimintCardReader.select(transceive);
  try {
    return await reader.balance();
  } finally {
    reader.close();
  }
}

void main() {
  print('Connect a card transport, then call readBalance(transceive).');
}
