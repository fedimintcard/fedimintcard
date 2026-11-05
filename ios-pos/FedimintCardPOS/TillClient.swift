import Foundation

struct HTTPRequest {
    let method: String
    let url: URL
    var headers: [String: String]
    var body: Data?
    let timeout: TimeInterval
}

struct RawResponse: Equatable {
    let status: Int
    let body: Data
}

/// How a transport fails. `interrupted`: the request may have reached the till.
enum TransportError: Error, Equatable {
    case unreachable
    case timeout
    case interrupted
}

protocol HTTPTransport {
    func send(_ req: HTTPRequest) async throws -> RawResponse
}

enum TillError: Error, Equatable {
    case notPaired
    case unreachable
    case timeout
    /// Failure after the request was (or may have been) written: outcome unknown.
    case interrupted
    case unauthorized
    case badResponse(String)
}

enum TillResult<T: Equatable>: Equatable {
    case ok(T)
    case err(TillError)
}

enum ChargeResult: Equatable {
    case success(ChargeReceipt)
    case declined(code: ChargeErrorCode, detail: String, httpStatus: Int)
    case failed(TillError)
}

enum BalanceResult: Equatable {
    case read(CardBalance)
    case refused(code: ChargeErrorCode, detail: String)
    case failed(TillError)
}

final class TillClient {
    static let quickTimeout: TimeInterval = 10
    static let chargeTimeout: TimeInterval = 125 // contract: at least 120 s
    static let balanceTimeout: TimeInterval = 60

    private let pairing: () -> TillPairing?
    private let transport: HTTPTransport

    init(pairing: @escaping () -> TillPairing?, transport: HTTPTransport) {
        self.pairing = pairing
        self.transport = transport
    }

    fileprivate func request(_ method: String, _ path: String, body: Data?, timeout: TimeInterval, cardLink: String? = nil) -> HTTPRequest? {
        guard let p = pairing(), let url = URL(string: p.baseURL + path) else { return nil }
        var h = ["Accept": "application/json"]
        if let t = p.token { h["Authorization"] = "Bearer \(t)" }
        if body != nil { h["Content-Type"] = "application/json" }
        if let cardLink { h["X-Card-Link"] = cardLink }
        return HTTPRequest(method: method, url: url, headers: h, body: body, timeout: timeout)
    }

    fileprivate func call(_ req: HTTPRequest?) async -> Result<RawResponse, TillError> {
        guard let req else { return .failure(.notPaired) }
        do {
            return .success(try await transport.send(req))
        } catch let e as TransportError {
            switch e {
            case .unreachable: return .failure(.unreachable)
            case .timeout: return .failure(.timeout)
            case .interrupted: return .failure(.interrupted)
            }
        } catch {
            return .failure(.unreachable)
        }
    }

    func status() async -> TillResult<TillStatus> {
        switch await call(request("GET", "/v1/status", body: nil, timeout: Self.quickTimeout)) {
        case .failure(let e): return .err(e)
        case .success(let r):
            if r.status == 401 { return .err(.unauthorized) }
            return TillParse.status(r.body)
        }
    }

    func sales() async -> TillResult<SalesList> {
        switch await call(request("GET", "/v1/sales", body: nil, timeout: Self.quickTimeout)) {
        case .failure(let e): return .err(e)
        case .success(let r):
            if r.status == 401 { return .err(.unauthorized) }
            return TillParse.sales(r.body)
        }
    }

    /// The charge body. The PIN goes in as bytes, never as a `String`.
    static func chargeBody(priceMsat: Int64, clientRef: String, pin: Pin?) -> Data {
        var d = Data("{\"price_msat\":\(priceMsat),\"client_ref\":".utf8)
        d.append(jsonString(clientRef))
        if let pin {
            d.append(Data(",\"pin\":\"".utf8))
            pin.withBytes { d.append(contentsOf: $0) }
            d.append(Data("\"".utf8))
        }
        d.append(Data("}".utf8))
        return d
    }

    private static func jsonString(_ s: String) -> Data {
        let arr = (try? JSONSerialization.data(withJSONObject: [s])) ?? Data("[\"\"]".utf8)
        return arr.subdata(in: 1..<(arr.count - 1))
    }

    /// Posts a charge with the card on `cardLink`. `pin` is consumed: it is wiped as soon as the
    /// request body is built, whatever happens next.
    func charge(priceMsat: Int64, clientRef: String, pin: Pin?, cardLink: String?) async -> ChargeResult {
        var body = Self.chargeBody(priceMsat: priceMsat, clientRef: clientRef, pin: pin)
        pin?.wipe()
        let req = request("POST", "/v1/charge", body: body, timeout: Self.chargeTimeout, cardLink: cardLink)
        body.resetBytes(in: 0..<body.count)
        switch await call(req) {
        case .failure(let e): return .failed(e)
        case .success(let r): return TillParse.charge(status: r.status, body: r.body)
        }
    }

    func cardBalance(cardLink: String?) async -> BalanceResult {
        switch await call(request("POST", "/v1/card/balance", body: Data(), timeout: Self.balanceTimeout, cardLink: cardLink)) {
        case .failure(let e): return .failed(e)
        case .success(let r): return TillParse.cardBalance(status: r.status, body: r.body)
        }
    }
}

extension TillClient {
    static let topUpCreateTimeout: TimeInterval = 30

    /// Makes a top-up invoice (docs/TOPUP.md). The same `clientRef` answers the same top-up. With
    /// `shortId` the till fits the amount to that card's free slots, or refuses a full card
    /// (`card_full`) before any invoice exists.
    func createTopUp(amountMsat: Int64, clientRef: String, shortId: String? = nil) async -> TopUpReply<TopUp> {
        var o: [String: Any] = ["amount_msat": amountMsat, "client_ref": clientRef]
        if let shortId { o["short_id"] = shortId }
        let body = try? JSONSerialization.data(withJSONObject: o)
        switch await call(request("POST", "/v1/topup", body: body, timeout: Self.topUpCreateTimeout)) {
        case .failure(let e): return .failed(e)
        case .success(let r): return TillParse.topUp(status: r.status, body: r.body)
        }
    }

    func topUpStatus(_ topupId: String) async -> TopUpReply<TopUpStatus> {
        switch await call(request("GET", "/v1/topup/" + Self.pathSegment(topupId), body: nil, timeout: Self.quickTimeout)) {
        case .failure(let e): return .failed(e)
        case .success(let r): return TillParse.topUpStatus(status: r.status, body: r.body)
        }
    }

    /// Loads a paid top-up onto the card on `cardLink`. Safe to repeat: a loaded top-up answers again.
    func loadTopUp(_ topupId: String, cardLink: String?) async -> TopUpReply<TopUpStatus> {
        let path = "/v1/topup/" + Self.pathSegment(topupId) + "/load"
        switch await call(request("POST", path, body: Data("{}".utf8), timeout: Self.chargeTimeout, cardLink: cardLink)) {
        case .failure(let e): return .failed(e)
        case .success(let r): return TillParse.topUpStatus(status: r.status, body: r.body)
        }
    }

    static func pathSegment(_ s: String) -> String {
        s.addingPercentEncoding(withAllowedCharacters: .alphanumerics.union(CharacterSet(charactersIn: "-._~"))) ?? s
    }
}

final class URLSessionTransport: HTTPTransport {
    func send(_ req: HTTPRequest) async throws -> RawResponse {
        var r = URLRequest(url: req.url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: req.timeout)
        r.httpMethod = req.method
        for (k, v) in req.headers { r.setValue(v, forHTTPHeaderField: k) }
        r.httpBody = req.body
        let cfg = URLSessionConfiguration.ephemeral
        cfg.timeoutIntervalForRequest = req.timeout
        cfg.timeoutIntervalForResource = req.timeout + 5
        cfg.waitsForConnectivity = false
        let session = URLSession(configuration: cfg)
        defer { session.finishTasksAndInvalidate() }
        do {
            let (data, resp) = try await session.data(for: r)
            return RawResponse(status: (resp as? HTTPURLResponse)?.statusCode ?? 0, body: data)
        } catch let e as URLError {
            switch e.code {
            case .timedOut: throw TransportError.timeout
            case .networkConnectionLost, .badServerResponse, .cannotParseResponse, .zeroByteResource:
                throw TransportError.interrupted
            case .cancelled: throw TransportError.interrupted
            default: throw TransportError.unreachable
            }
        }
    }
}

// MARK: - Parsing

enum TillParse {
    private struct Bad: Error { let msg: String }

    private static func obj(_ d: Data) throws -> [String: Any] {
        guard let o = try? JSONSerialization.jsonObject(with: d) as? [String: Any] else { throw Bad(msg: "bad json") }
        return o
    }
    private static func reqLong(_ o: [String: Any], _ k: String) throws -> Int64 {
        guard let n = o[k] as? NSNumber else { throw Bad(msg: "missing \(k)") }
        return n.int64Value
    }
    private static func optLong(_ o: [String: Any], _ k: String, _ d: Int64 = 0) -> Int64 {
        (o[k] as? NSNumber)?.int64Value ?? d
    }
    private static func reqStr(_ o: [String: Any], _ k: String) throws -> String {
        guard let s = o[k] as? String else { throw Bad(msg: "missing \(k)") }
        return s
    }
    private static func reqBool(_ o: [String: Any], _ k: String) throws -> Bool {
        guard let n = o[k] as? NSNumber, CFGetTypeID(n) == CFBooleanGetTypeID() else { throw Bad(msg: "missing \(k)") }
        return n.boolValue
    }
    private static func optStr(_ o: [String: Any], _ k: String) -> String? { o[k] as? String }

    private static func notes(_ a: Any?) throws -> [Note] {
        guard let a = a as? [[String: Any]] else { return [] }
        return try a.map { Note(slot: Int(try reqLong($0, "slot")), msat: try reqLong($0, "msat")) }
    }

    static func status(_ body: Data) -> TillResult<TillStatus> {
        do {
            let o = try obj(body)
            return .ok(TillStatus(
                federationId: try reqStr(o, "federation_id"),
                federationName: optStr(o, "federation_name"),
                walletBalanceMsat: try reqLong(o, "wallet_balance_msat"),
                cardRelayOk: try reqBool(o, "card_relay_ok"),
                busy: try reqBool(o, "busy")))
        } catch let e as Bad { return .err(.badResponse(e.msg)) } catch { return .err(.badResponse("bad json")) }
    }

    static func charge(status: Int, body: Data) -> ChargeResult {
        do {
            let o = try obj(body)
            guard let okv = o["ok"] as? NSNumber else { throw Bad(msg: "missing ok") }
            if !okv.boolValue {
                return .declined(code: .fromWire(optStr(o, "error")), detail: optStr(o, "detail") ?? "", httpStatus: status)
            }
            return .success(ChargeReceipt(
                saleId: optLong(o, "sale_id"),
                txid: optStr(o, "txid") ?? "",
                priceMsat: optLong(o, "price_msat"),
                receivedMsat: optLong(o, "received_msat"),
                overpaidMsat: optLong(o, "overpaid_msat"),
                feesMsat: optLong(o, "fees_msat"),
                inputs: try notes(o["inputs"]),
                change: try notes(o["change"]),
                collectionPending: (o["collection_pending"] as? NSNumber)?.boolValue ?? false,
                warnings: (o["warnings"] as? [Any])?.compactMap { $0 as? String } ?? [],
                durationMs: optLong(o, "duration_ms")))
        } catch {
            let msg = (error as? Bad)?.msg ?? "bad json"
            switch status {
            case 202: return .declined(code: .undecided, detail: "", httpStatus: status)
            case 401: return .declined(code: .unauthorized, detail: "", httpStatus: status)
            case 200...299: return .failed(.interrupted) // accepted but unreadable: outcome unknown
            default: return .failed(.badResponse(msg))
            }
        }
    }

    static func sales(_ body: Data) -> TillResult<SalesList> {
        do {
            let o = try obj(body)
            guard let a = o["sales"] as? [[String: Any]] else { throw Bad(msg: "missing sales") }
            let rows = try a.map { s in
                Sale(id: try reqLong(s, "id"), tsUnixMs: optLong(s, "ts_unix_ms"), priceMsat: try reqLong(s, "price_msat"),
                     receivedMsat: optLong(s, "received_msat"), overpaidMsat: optLong(s, "overpaid_msat"),
                     feesMsat: optLong(s, "fees_msat"), txid: optStr(s, "txid") ?? "", clientRef: optStr(s, "client_ref"))
            }
            return .ok(SalesList(count: optLong(o, "count", Int64(rows.count)), totalPriceMsat: try reqLong(o, "total_price_msat"),
                                 totalReceivedMsat: try reqLong(o, "total_received_msat"), sales: rows))
        } catch { return .err(.badResponse((error as? Bad)?.msg ?? "bad json")) }
    }

    static func cardBalance(status: Int, body: Data) -> BalanceResult {
        do {
            let o = try obj(body)
            if !((o["ok"] as? NSNumber)?.boolValue ?? false) {
                // A card error or a missing card is a normal answer from this route, not a parse failure.
                let code = optStr(o, "error")
                return .refused(code: .fromWire(code), detail: optStr(o, "detail") ?? code ?? "card unreadable")
            }
            let rows = try ((o["notes"] as? [[String: Any]]) ?? []).map {
                CardNote(slot: Int(try reqLong($0, "slot")), msat: try reqLong($0, "msat"), status: optStr($0, "status") ?? "")
            }
            let free = ((o["free_slots"] as? [Any]) ?? []).compactMap { ($0 as? NSNumber)?.intValue }
            let shortId = optStr(o, "short_id").flatMap { $0.isEmpty ? nil : $0 }
            return .read(CardBalance(liveMsat: optLong(o, "live_msat"), notes: rows, freeSlots: free, shortId: shortId))
        } catch {
            if status == 401 { return .refused(code: .unauthorized, detail: "unauthorized") }
            return .failed(.badResponse((error as? Bad)?.msg ?? "bad json"))
        }
    }

    private static func topUpRefused(_ status: Int, _ o: [String: Any]) -> Bool {
        if let okv = o["ok"] as? NSNumber, !okv.boolValue { return true }
        return status >= 400
    }

    /// `POST /v1/topup`: `{topup_id, amount_msat, invoice, expires_at_ms, state}`, or `{ok:false,…}`.
    static func topUp(status: Int, body: Data) -> TopUpReply<TopUp> {
        do {
            let o = try obj(body)
            if topUpRefused(status, o) {
                return .refused(error: optStr(o, "error") ?? (status == 401 ? "unauthorized" : ""), detail: optStr(o, "detail") ?? "", httpStatus: status)
            }
            return .ok(TopUp(topupId: try reqStr(o, "topup_id"), amountMsat: try reqLong(o, "amount_msat"),
                             invoice: try reqStr(o, "invoice"), expiresAtMs: try reqLong(o, "expires_at_ms"),
                             state: optStr(o, "state") ?? "waiting"))
        } catch {
            if status == 401 { return .failed(.unauthorized) }
            return .failed(.badResponse((error as? Bad)?.msg ?? "bad json"))
        }
    }

    /// `GET /v1/topup/{id}` and `POST …/load`: `{topup_id, amount_msat, state, load_txid?, detail?}`.
    static func topUpStatus(status: Int, body: Data) -> TopUpReply<TopUpStatus> {
        do {
            let o = try obj(body)
            if topUpRefused(status, o) {
                return .refused(error: optStr(o, "error") ?? (status == 401 ? "unauthorized" : ""), detail: optStr(o, "detail") ?? "", httpStatus: status)
            }
            let txid = optStr(o, "load_txid").flatMap { $0.isEmpty ? nil : $0 }
            let detail = optStr(o, "detail").flatMap { $0.isEmpty ? nil : $0 }
            return .ok(TopUpStatus(topupId: try reqStr(o, "topup_id"), amountMsat: try reqLong(o, "amount_msat"),
                                   state: try reqStr(o, "state"), loadTxid: txid, detail: detail))
        } catch {
            if status == 401 { return .failed(.unauthorized) }
            return .failed(.badResponse((error as? Bad)?.msg ?? "bad json"))
        }
    }
}
