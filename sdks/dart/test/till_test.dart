import 'dart:convert';
import 'dart:io';

import 'package:fedimintcard/src/till.dart';
import 'package:test/test.dart';

const pos = 'fedimintcard-till:https://shop.example:9443/?t=abc123';
const pay = 'fedimintcard-till-pay:https://shop.example:9443?t=abc456';
const link = '0011223344556677', id = '89abcdef12345678', shortId = 'abcd1234';
const reference = 'w:3.6-7',
    txid = 'abababababababababababababababababababababababababababababababab';
const proof =
    '7777777777777777777777777777777777777777777777777777777777777777';
const hash = 'e29442e61ad354e5cb0831e2e8359e8fb50cf024ad5a8f407c8f9de63bdf7371';
const bolt = 'in-memory-signed-invoice-fixture';
final future = DateTime.now().millisecondsSinceEpoch + 600000;

VerifiedInvoice verify(String text) {
  if (text != bolt) throw const FormatException();
  return (amountMsat: 2105000, paymentHash: hash, expiresAtMs: future);
}

typedef Request = ({
  String method,
  Uri uri,
  Map<String, String> headers,
  Object? body,
});
typedef Reply = ({int status, String body});
Reply reply(Object? json, [int status = 200]) =>
    (status: status, body: jsonEncode(json));
Matcher fails(String code) =>
    throwsA(isA<TillException>().having((e) => e.code, 'code', code));

class Script {
  final List<Request> requests = [];
  final List<Object> replies;
  Script(this.replies);
  TillClient client([
    String pairing = pos,
    InvoiceVerifier? verifier = verify,
  ]) =>
      TillClient.fromPairing(pairing, verifyInvoice: verifier, transport: send);
  Future<Reply> send(
    String method,
    Uri uri,
    Map<String, String> headers,
    String? body,
  ) async {
    requests.add((
      method: method,
      uri: uri,
      headers: headers,
      body: body == null ? null : jsonDecode(body),
    ));
    final next = replies.removeAt(0);
    if (next is Reply) return next;
    throw next;
  }
}

final sale = {
  'ok': true,
  'sale_id': 1,
  'txid': txid,
  'price_msat': 5000,
  'received_msat': 5000,
  'collection_pending': false,
};
final quote = {
  'ok': true,
  'state': 'waiting',
  'topup_id': id,
  'amount_msat': 2100224,
  'topup_fee_msat': 4776,
  'invoice_amount_msat': 2105000,
  'fee_quote_version': 1,
  'invoice': bolt,
  'expires_at_ms': future - 10000,
};
final paid = {
  'ok': true,
  'state': 'paid',
  'pay_id': 9,
  'client_ref': reference,
  'payment_hash': hash,
  'invoice_msat': 2105000,
  'received_msat': 2109100,
  'price_msat': 2109000,
  'preimage': proof,
};

Future<SaleReceipt> charge(TillClient client) => client.charge(
  priceMsat: 5000,
  clientRef: reference,
  pin: '0004',
  cardLink: link,
);
Future<TopUp> create(TillClient client) => client.quoteTopUp(
  amountMsat: 2100000,
  clientRef: reference,
  shortId: shortId,
);
Future<PaymentReceipt> payBolt(TillClient client) => client.payInvoice(
  invoice: bolt,
  maxFeeMsat: 3000,
  clientRef: reference,
  pin: '0004',
  cardLink: link,
);

void main() {
  test(
    'accepts two exact HTTPS pairing scopes; ambiguous and untrusted origins fail before any request',
    () {
      expect(TillClient.fromPairing(pos).scope, TillScope.pos);
      expect(TillClient.fromPairing(pay).scope, TillScope.pay);
      expect(
        TillClient.fromPairing(pos).origin.toString(),
        'https://shop.example:9443',
      );
      for (final text in [
        pos.replaceFirst('https', 'http'),
        pos.replaceFirst('/?', '/admin?'),
        pos + '&t=abc',
        pos + '#token',
        pos.replaceFirst('shop.example', 'shop.example@other'),
        pos.replaceFirst('shop.example', r'shop.example\other'),
        pos.replaceFirst('shop.example', 'shop%2eexample'),
        pos.replaceFirst('9443', '99999'),
      ]) {
        expect(
          () => TillClient.fromPairing(text),
          throwsA(
            isA<FormatException>().having(
              (e) => e.toString(),
              'redacts token',
              isNot(contains('abc123')),
            ),
          ),
        );
      }
    },
  );
  test(
    'foreground balance and referenced charge use the paired origin and typed receipt',
    () async {
      final script = Script([
        reply({
          'ok': true,
          'live_msat': 8000,
          'stranded_msat': 17,
          'short_id': shortId,
        }),
        reply(sale),
      ]);
      final client = script.client();
      expect(await client.balance(link), (
        liveMsat: 8000,
        strandedMsat: 17,
        shortId: shortId,
      ));
      final got = await charge(client);
      expect(got.priceMsat, 5000);
      expect(got.receivedMsat, 5000);
      expect(got.collectionPending, false);
      for (final call in script.requests) {
        expect(call.headers['X-Card-Link'], link);
        expect(call.headers['Authorization'], 'Bearer abc123');
        expect(call.uri.origin, 'https://shop.example:9443');
      }
      expect(script.requests.last.body, {
        'price_msat': 5000,
        'client_ref': reference,
        'pin': '0004',
      });
    },
  );
  test(
    'charge uncertainty is never replayed; stable reference can recover an offline or rejected sale',
    () async {
      final row = {
        ...sale,
        'id': 1,
        'client_ref': reference,
        'received_msat': 0,
        'offline': true,
        'double_spent': true,
      };
      final script = Script([
        const SocketException('test interrupt'),
        reply({
          'sales': [row],
        }),
        reply({'sales': []}),
        reply({
          'sales': [row, row],
        }),
      ]);
      final client = script.client();
      await expectLater(charge(client), fails('till_request_interrupted'));
      final found = (await client.findSale(reference))!;
      expect(found.receivedMsat, 0);
      expect(found.offline && found.doubleSpent, true);
      expect(await client.findSale(reference), isNull);
      await expectLater(
        client.findSale(reference),
        fails('invalid_till_response'),
      );
      expect(script.requests.map((r) => r.method), [
        'POST',
        'GET',
        'GET',
        'GET',
      ]);
    },
  );
  test(
    'scope, status, exact amount, response types, errors and redirects fail closed',
    () async {
      final script = Script([
        reply({'ok': false, 'error': 'wrong_federation'}, 409),
        reply({...sale, 'price_msat': 4}),
        reply({...sale, 'received_msat': 5.1}),
        reply({'ok': true}, 302),
        (status: 200, body: 'not json'),
      ]);
      final client = script.client();
      await expectLater(
        charge(script.client(pay)),
        fails('wrong_pairing_scope'),
      );
      await expectLater(client.balance('invalid'), throwsArgumentError);
      await expectLater(charge(client), fails('wrong_federation'));
      for (var i = 0; i < 2; i++) {
        await expectLater(charge(client), fails('invalid_till_response'));
      }
      await expectLater(charge(client), fails('unexpected_redirect'));
      await expectLater(charge(client), fails('invalid_till_response'));
      expect(script.requests.length, 5);
    },
  );
  test(
    'shows only a verified fee-inclusive quote with its actual rounded credit, exact fee, amount and expiry',
    () async {
      final script = Script([
        reply(quote),
        reply({
          ...quote,
          'invoice_amount_msat': 2105001,
          'topup_fee_msat': 4777,
        }),
        reply({...quote, 'topup_fee_msat': 4777}),
        reply({...quote, 'expires_at_ms': future + 1}),
        reply({...quote, 'fee_quote_version': 0}),
        reply({...quote, 'invoice': 'bad'}),
        reply({'ok': false, 'error': 'not_found'}, 404),
      ]);
      final client = script.client();
      final got = await create(client);
      expect(got.creditMsat, 2100224);
      expect(got.state, 'waiting');
      expect(got.quote, (
        invoice: bolt,
        feeMsat: 4776,
        totalMsat: 2105000,
        expiresAtMs: future - 10000,
      ));
      for (var i = 0; i < 4; i++) {
        await expectLater(create(client), fails('invalid_till_response'));
      }
      await expectLater(create(client), fails('invalid_signed_invoice'));
      await expectLater(create(client), fails('not_found'));
      expect(
        script.requests.every((r) => r.uri.path == '/v1/topup/fee-inclusive'),
        true,
      );
      expect(script.requests.first.body, {
        'amount_msat': 2100000,
        'short_id': shortId,
        'client_ref': reference,
        'fee_quote_version': 1,
      });
    },
  );
  test(
    'expiry preserves unknown settlement; paid top-ups load under the original id and a fresh card link',
    () async {
      final script = Script([
        reply({...quote, 'expires_at_ms': 1, 'invoice': ''}),
        reply({...quote, 'state': 'paid'}),
        reply({...quote, 'state': 'loaded', 'load_txid': txid}),
        reply({...quote, 'topup_id': link}),
      ]);
      final client = script.client();
      final old = await client.topUp(id);
      expect(old.state, 'settlement_pending');
      expect(old.quote, isNull);
      expect((await client.topUp(id)).state, 'paid');
      final retap = await client.loadTopUp(id, link);
      expect(retap.txid, txid);
      expect(retap.state, 'loaded');
      expect(script.requests[2].uri.path, '/v1/topup/$id/load');
      expect(script.requests[2].headers['X-Card-Link'], link);
      await expectLater(client.topUp(id), fails('invalid_till_response'));
    },
  );
  test(
    'wallet requires a valid invoice, its exact hash and preimage; status uses its own id plus original reference',
    () async {
      final script = Script([
        reply(paid),
        reply({...paid, 'preimage': txid}),
        reply({...paid, 'client_ref': 'different'}),
        reply({...paid, 'invoice_msat': 42}),
        reply({...paid, 'payment_hash': txid}),
        reply({
          'ok': false,
          'error': 'undecided',
          'state': 'paying',
          'pay_id': 9,
        }, 202),
        reply({...paid, 'preimage': null}),
        reply({...paid, 'client_ref': 'wrong'}),
      ]);
      final client = script.client(pay);
      final expired = script.client(
        pay,
        (invoice) => (amountMsat: 1, paymentHash: hash, expiresAtMs: 1),
      );
      await expectLater(payBolt(expired), fails('expired_invoice'));
      await expectLater(
        payBolt(script.client(pay, null)),
        fails('invoice_verifier_required'),
      );
      await expectLater(
        payBolt(script.client(pos)),
        fails('wrong_pairing_scope'),
      );
      await expectLater(
        create(script.client(pos, null)),
        fails('invoice_verifier_required'),
      );
      expect(script.requests, isEmpty);
      expect((await payBolt(client)).preimage, proof);
      for (var i = 0; i < 4; i++) {
        await expectLater(payBolt(client), fails('invalid_till_response'));
      }
      await expectLater(
        payBolt(client),
        throwsA(
          isA<TillException>()
              .having((e) => e.payId, 'id', 9)
              .having((e) => e.payState, 'state', 'paying'),
        ),
      );
      final status = await client.payStatus(9, reference);
      expect(status.paymentHash, hash);
      expect(status.state, 'paid');
      expect(status.refundOwedMsat, 0);
      expect(script.requests.last.uri.queryParameters, {
        'client_ref': reference,
      });
      await expectLater(
        client.payStatus(9, reference),
        fails('invalid_till_response'),
      );
    },
  );
}
