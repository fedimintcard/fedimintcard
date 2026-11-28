import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:pointycastle/digests/sha256.dart';

enum TillScope { pos, pay }

/// Supply a wallet decoder that validates the BOLT11 signature and expected network.
typedef VerifiedInvoice = ({
  int amountMsat,
  String paymentHash,
  int expiresAtMs,
});
typedef InvoiceVerifier = FutureOr<VerifiedInvoice> Function(String bolt11);
typedef TillBalance = ({int liveMsat, int strandedMsat, String? shortId});
typedef SaleReceipt = ({
  int saleId,
  String txid,
  int priceMsat,
  int receivedMsat,
  bool collectionPending,
  bool offline,
  bool rejected,
  bool doubleSpent,
});
typedef PaymentReceipt = ({
  int payId,
  String preimage,
  int priceMsat,
  int receivedMsat,
});
typedef PaymentStatus = ({
  int payId,
  String state,
  String paymentHash,
  int priceMsat,
  int refundOwedMsat,
});

/// Waiting quotes expose an invoice only after its signature, amounts and expiry agree.
typedef TopUp = ({
  String id,
  String state,
  int creditMsat,
  String? txid,
  ({String invoice, int feeMsat, int totalMsat, int expiresAtMs})? quote,
});

/// A trusted replacement transport receives the pairing token and potentially the PIN.
/// Send once, without redirects. Return response text and the original HTTP status.
typedef TillTransport =
    Future<({int status, String body})> Function(
      String method,
      Uri uri,
      Map<String, String> headers,
      String? body,
    );

class TillException implements Exception {
  final String code;
  final int? httpStatus, payId;
  final String? payState;
  const TillException(this.code, {this.httpStatus, this.payId, this.payState});
  @override
  String toString() => 'TillException($code)';
}

const _payStates = {
  'charging',
  'not_charged',
  'charged',
  'paying',
  'paid',
  'pay_failed',
  'refunding',
  'refunded',
};
typedef _Json = Map<String, dynamic>;
Never _invalid() => throw const TillException('invalid_till_response');
bool _matches(String value, RegExp expression) =>
    expression.matchAsPrefix(value)?.end == value.length;
bool _hex(Object? value, int length) =>
    value is String &&
    value.length == length &&
    _matches(value, RegExp('[0-9a-f]+'));
String _checkedId(String value, int length) {
  if (!_hex(value, length)) throw ArgumentError('Invalid identifier.');
  return value;
}

String _reference(String value, int max) {
  if (value.length > max || !_matches(value, RegExp(r'[A-Za-z0-9._:-]+'))) {
    throw ArgumentError('Use a persistent ASCII reference for the operation.');
  }
  return value;
}

String _pin(String value) {
  if (!_matches(value, RegExp(r'[0-9]{4,8}')))
    throw ArgumentError('Use a 4 to 8 digit PIN.');
  return value;
}

int _amount(int value, int min, int max) {
  if (value < min || value > max)
    throw ArgumentError('Amount is outside the supported range.');
  return value;
}

_Json _object(Object? value) => value is _Json ? value : _invalid();
int _uint(Object? value) => value is int && value >= 0 ? value : _invalid();
int _positive(Object? value) => value is int && value > 0 ? value : _invalid();
bool _bool(Object? value) => value is bool ? value : _invalid();
String _responseHex(Object? value, int length) =>
    _hex(value, length) ? value as String : _invalid();
Duration _timeout(String method) =>
    Duration(seconds: method == 'GET' ? 15 : 150);

Future<({int status, String body})> _nativeRequest(
  String method,
  Uri uri,
  Map<String, String> headers,
  String? body,
) async {
  final http = HttpClient();
  try {
    Future<({int status, String body})> run() async {
      final request = await http.openUrl(method, uri);
      request.followRedirects = false;
      headers.forEach(request.headers.set);
      if (body != null) request.add(utf8.encode(body));
      final response = await request.close();
      if (response.statusCode >= 300 && response.statusCode < 400)
        throw const TillException('unexpected_redirect');
      return (
        status: response.statusCode,
        body: await response.transform(utf8.decoder).join(),
      );
    }

    return await run().timeout(_timeout(method));
  } finally {
    http.close(force: true);
  }
}

SaleReceipt _receipt(_Json row, Object? id) => (
  saleId: _positive(id),
  txid: _responseHex(row['txid'], 64),
  priceMsat: _uint(row['price_msat']),
  receivedMsat: _uint(row['received_msat']),
  collectionPending: _bool(row['collection_pending']),
  offline: _bool(row['offline'] ?? false),
  rejected: _bool(row['rejected'] ?? false),
  doubleSpent: _bool(row['double_spent'] ?? false),
);

/// HTTPS client for a paired POS or pay-only wallet. Card operations use the
/// link from the active foreground card session. No operation is retried here.
class TillClient {
  final Uri origin;
  final TillScope scope;
  final String _token;
  final InvoiceVerifier? _verify;
  final TillTransport _send;
  const TillClient._(
    this.origin,
    this.scope,
    this._token,
    this._verify,
    this._send,
  );

  factory TillClient.fromPairing(
    String text, {
    InvoiceVerifier? verifyInvoice,
    TillTransport? transport,
  }) {
    final trimmed = text.trim();
    final match = RegExp(
      r'fedimintcard-till(-pay)?:(https://[^/\\?#@%\s]+?)/?\?t=([A-Za-z0-9]{1,256})',
    ).matchAsPrefix(trimmed);
    if (match == null || match.end != trimmed.length)
      throw const FormatException('Invalid HTTPS till pairing.');
    // Uri may accept a non-numeric or out-of-range port until the port is read.
    try {
      final uri = Uri.parse(match[2]!);
      if (uri.host.isEmpty || uri.port <= 0 || uri.port > 65535)
        throw const FormatException();
      return TillClient._(
        uri,
        match[1] == null ? TillScope.pos : TillScope.pay,
        match[3]!,
        verifyInvoice,
        transport ?? _nativeRequest,
      );
    } on FormatException {
      throw const FormatException('Invalid HTTPS till pairing.');
    }
  }

  void _require(TillScope expected) {
    if (scope != expected) throw const TillException('wrong_pairing_scope');
  }

  Future<_Json> _request(
    String method,
    String path, {
    _Json? body,
    String? cardLink,
  }) async {
    final headers = {
      'Authorization': 'Bearer $_token',
      'Accept': 'application/json',
      if (cardLink != null) 'X-Card-Link': _checkedId(cardLink, 16),
      if (body != null) 'Content-Type': 'application/json',
    };
    final ({int status, String body}) response;
    try {
      response = await _send(
        method,
        origin.resolve(path),
        headers,
        body == null ? null : jsonEncode(body),
      ).timeout(_timeout(method));
    } on TillException {
      rethrow;
    } catch (_) {
      throw const TillException('till_request_interrupted');
    }
    if (response.status >= 300 && response.status < 400)
      throw const TillException('unexpected_redirect');
    final _Json row;
    try {
      row = _object(jsonDecode(response.body));
    } on FormatException {
      return _invalid();
    }
    if (response.status != 200 || row['ok'] == false) {
      final error = row['error'], id = row['pay_id'], state = row['state'];
      final code =
          error is String && _matches(error, RegExp('[a-z][a-z0-9_]{0,63}'))
          ? error
          : 'unexpected_till_status';
      throw TillException(
        code,
        httpStatus: response.status,
        payId: id is int && id > 0 ? id : null,
        payState: state is String && _payStates.contains(state) ? state : null,
      );
    }
    if (row['ok'] != true && path != '/v1/sales') return _invalid();
    return row;
  }

  Future<VerifiedInvoice> _signed(String bolt11) async {
    final verify = _verify;
    if (verify == null) throw const TillException('invoice_verifier_required');
    try {
      if (bolt11.isEmpty) throw const FormatException();
      final invoice = await verify(bolt11);
      if (invoice.amountMsat <= 0 ||
          invoice.expiresAtMs <= 0 ||
          !_hex(invoice.paymentHash, 64)) {
        throw const FormatException();
      }
      return invoice;
    } catch (_) {
      throw const TillException('invalid_signed_invoice');
    }
  }

  Future<TillBalance> balance(String cardLink) async {
    _require(TillScope.pos);
    final row = await _request('POST', '/v1/card/balance', cardLink: cardLink);
    return (
      liveMsat: _uint(row['live_msat']),
      strandedMsat: _uint(row['stranded_msat'] ?? 0),
      shortId: row['short_id'] == null
          ? null
          : _responseHex(row['short_id'], 8),
    );
  }

  /// Persist [clientRef] before asking the till to charge. An interrupted call
  /// has an unknown outcome. Check [findSale] with the same reference.
  Future<SaleReceipt> charge({
    required int priceMsat,
    required String clientRef,
    required String pin,
    required String cardLink,
  }) async {
    _require(TillScope.pos);
    final row = await _request(
      'POST',
      '/v1/charge',
      cardLink: cardLink,
      body: {
        'price_msat': _amount(priceMsat, 1, 1 << 40),
        'client_ref': _reference(clientRef, 128),
        'pin': _pin(pin),
      },
    );
    final sale = _receipt(row, row['sale_id']);
    if (sale.priceMsat != priceMsat) return _invalid();
    return sale;
  }

  /// A missing entry can mean settlement is still in progress.
  Future<SaleReceipt?> findSale(String clientRef) async {
    _require(TillScope.pos);
    _reference(clientRef, 128);
    final sales = (await _request('GET', '/v1/sales'))['sales'];
    if (sales is! List) return _invalid();
    SaleReceipt? result;
    for (final raw in sales) {
      final row = _object(raw);
      if (row['client_ref'] != clientRef) continue;
      if (result != null) return _invalid();
      result = _receipt(row, row['id']);
    }
    return result;
  }

  Future<TopUp> _topUp(_Json row, [String? expectedId]) async {
    final id = _responseHex(row['topup_id'], 16);
    if (expectedId != null && id != expectedId) return _invalid();
    final credit = _positive(row['amount_msat']);
    final state = row['state'];
    switch (state) {
      case 'loaded':
        return (
          id: id,
          creditMsat: credit,
          state: 'loaded',
          txid: _responseHex(row['load_txid'], 64),
          quote: null,
        );
      case 'paid':
      case 'failed':
      case 'expired':
        return (
          id: id,
          creditMsat: credit,
          state: state as String,
          txid: null,
          quote: null,
        );
      case 'waiting':
        final expires = _positive(row['expires_at_ms']);
        if (expires <= DateTime.now().millisecondsSinceEpoch) {
          return (
            id: id,
            creditMsat: credit,
            state: 'settlement_pending',
            txid: null,
            quote: null,
          );
        }
        final invoice = row['invoice'];
        if (row['fee_quote_version'] is! int ||
            row['fee_quote_version'] != 1 ||
            invoice is! String ||
            invoice.isEmpty)
          return _invalid();
        final total = _positive(row['invoice_amount_msat']),
            fee = _uint(row['topup_fee_msat']);
        final signed = await _signed(invoice);
        if (total - credit != fee ||
            signed.amountMsat != total ||
            signed.expiresAtMs < expires)
          return _invalid();
        if (expires <= DateTime.now().millisecondsSinceEpoch)
          return (
            id: id,
            creditMsat: credit,
            state: 'settlement_pending',
            txid: null,
            quote: null,
          );
        return (
          id: id,
          creditMsat: credit,
          state: 'waiting',
          txid: null,
          quote: (
            invoice: invoice,
            feeMsat: fee,
            totalMsat: total,
            expiresAtMs: expires,
          ),
        );
      default:
        return _invalid();
    }
  }

  Future<TopUp> quoteTopUp({
    required int amountMsat,
    required String clientRef,
    required String shortId,
  }) async {
    _require(TillScope.pos);
    if (_verify == null) throw const TillException('invoice_verifier_required');
    final row = await _request(
      'POST',
      '/v1/topup/fee-inclusive',
      body: {
        'fee_quote_version': 1,
        'amount_msat': _amount(amountMsat, 1, 100000000),
        'short_id': _checkedId(shortId, 8),
        'client_ref': _reference(clientRef, 64),
      },
    );
    return _topUp(row);
  }

  Future<TopUp> topUp(String id) async {
    _require(TillScope.pos);
    return _topUp(await _request('GET', '/v1/topup/${_checkedId(id, 16)}'), id);
  }

  Future<TopUp> loadTopUp(String id, String cardLink) async {
    _require(TillScope.pos);
    return _topUp(
      await _request(
        'POST',
        '/v1/topup/${_checkedId(id, 16)}/load',
        cardLink: cardLink,
      ),
      id,
    );
  }

  /// A pay-only token pays the signed invoice. The till may add a merchant
  /// margin to the invoice and fee reserve. Preserve the reference before calling.
  Future<PaymentReceipt> payInvoice({
    required String invoice,
    required int maxFeeMsat,
    required String clientRef,
    required String pin,
    required String cardLink,
  }) async {
    _require(TillScope.pay);
    final body = {
      'bolt11': invoice,
      'max_fee_msat': _amount(maxFeeMsat, 0, 1 << 40),
      'client_ref': _reference(clientRef, 124),
      'pin': _pin(pin),
    };
    final signed = await _signed(invoice);
    if (signed.expiresAtMs <= DateTime.now().millisecondsSinceEpoch)
      throw const TillException('expired_invoice');
    final row = await _request(
      'POST',
      '/v1/pay',
      body: body,
      cardLink: cardLink,
    );
    final preimage = _responseHex(row['preimage'], 64);
    final proof = Uint8List.fromList([
      for (var i = 0; i < 64; i += 2)
        int.parse(preimage.substring(i, i + 2), radix: 16),
    ]);
    final hash = SHA256Digest()
        .process(proof)
        .map((byte) => byte.toRadixString(16).padLeft(2, '0'))
        .join();
    if (row['state'] != 'paid' ||
        row['client_ref'] != clientRef ||
        row['payment_hash'] != signed.paymentHash ||
        _uint(row['invoice_msat']) != signed.amountMsat ||
        hash != signed.paymentHash)
      return _invalid();
    return (
      payId: _positive(row['pay_id']),
      priceMsat: _uint(row['price_msat']),
      receivedMsat: _uint(row['received_msat']),
      preimage: preimage,
    );
  }

  /// Returns the till's reported state. A status read carries no payment proof.
  Future<PaymentStatus> payStatus(int id, String clientRef) async {
    _require(TillScope.pay);
    if (id <= 0) throw ArgumentError('Invalid pay ID.');
    final ref = _reference(clientRef, 124);
    final row = await _request(
      'GET',
      '/v1/pay/$id?client_ref=${Uri.encodeQueryComponent(ref)}',
    );
    final state = row['state'];
    if (_positive(row['pay_id']) != id ||
        row['client_ref'] != ref ||
        state is! String ||
        !_payStates.contains(state))
      return _invalid();
    return (
      payId: id,
      state: state,
      paymentHash: _responseHex(row['payment_hash'], 64),
      priceMsat: _uint(row['price_msat']),
      refundOwedMsat: _uint(row['refund_owed_msat'] ?? 0),
    );
  }
}
