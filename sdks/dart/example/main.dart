import 'package:fedimintcard/nfc.dart';
import 'package:flutter/material.dart';

void main() => runApp(const MaterialApp(home: ReaderExample()));

class ReaderExample extends StatefulWidget {
  const ReaderExample({super.key});
  @override
  State<ReaderExample> createState() => _ReaderExampleState();
}

class _ReaderExampleState extends State<ReaderExample> {
  String result = '';
  bool scanning = false;
  Future<void> scan() async {
    setState(() => scanning = true);
    try {
      final amount = await withFedimintCard((reader) => reader.balance());
      if (mounted)
        setState(
          () => result =
              'Stored ${amount.storedMsat} msat, owed ${amount.owedMsat} msat',
        );
    } catch (error) {
      if (mounted) setState(() => result = error.toString());
    } finally {
      if (mounted) setState(() => scanning = false);
    }
  }

  @override
  Widget build(BuildContext context) => Scaffold(
    body: Center(
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          Text(result),
          ElevatedButton(
            onPressed: scanning ? null : scan,
            child: const Text('Read card'),
          ),
        ],
      ),
    ),
  );
}
