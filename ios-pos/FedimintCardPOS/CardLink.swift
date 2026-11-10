import Foundation

/// The till's side of the link is a WebSocket of JSON text frames (docs/POS-LINK.md, "The card link").
protocol LinkSocket: AnyObject {
    func send(_ text: String) async throws
    /// The next text frame, or nil once the link is closed.
    func receive() async throws -> String?
    func close()
}

/// The card in the field. `transmit` returns the response data followed by SW1 SW2.
protocol CardChannel: AnyObject {
    func transmit(_ apdu: Data) async throws -> Data
}

enum LinkError: Error, Equatable {
    case unauthorized
    case unreachable
    /// The first frame was not `{"type":"ready","link":"<id>"}`.
    case badHandshake
    case timeout

    /// The link's HTTP answer when the WebSocket did not open: 401 and 403 are this phone refused
    /// (the pairing is dead, no offline fallback); anything else is the till not serving.
    static func forStatus(_ status: Int?) -> LinkError {
        status == 401 || status == 403 ? .unauthorized : .unreachable
    }
}

/// One card link: the till's APDUs go to the card one at a time, answers go back.
/// APDU bytes are never logged; nothing here logs at all.
final class CardLink {
    let id: String
    private let socket: LinkSocket
    private let lock = NSLock()
    private var _cardGone = false

    private init(id: String, socket: LinkSocket) {
        self.id = id
        self.socket = socket
    }

    /// Waits for the till's `ready` frame and returns the open link.
    static func open(_ socket: LinkSocket, readyTimeout: TimeInterval = 8) async throws -> CardLink {
        let first: String?
        do {
            first = try await withTimeout(readyTimeout) { try await socket.receive() }
        } catch let e as LinkError {
            socket.close(); throw e
        } catch {
            socket.close(); throw error
        }
        guard let text = first,
              let o = LinkFrame.decode(text), o["type"] as? String == "ready",
              let id = o["link"] as? String, !id.isEmpty, id.count <= 64 else {
            socket.close()
            throw LinkError.badHandshake
        }
        return CardLink(id: id, socket: socket)
    }

    var cardGone: Bool { lock.lock(); defer { lock.unlock() }; return _cardGone }

    /// The NFC session ended or the tag left: every further APDU is answered `lost`.
    func markCardGone() { lock.lock(); _cardGone = true; lock.unlock() }

    /// Answers `apdu` frames from `card` until the till closes the link (or `close()` is called).
    func serve(card: CardChannel) async {
        while !Task.isCancelled {
            let text: String?
            do { text = try await socket.receive() } catch { break }
            guard let text else { break }
            guard let o = LinkFrame.decode(text), o["type"] as? String == "apdu",
                  let n = o["id"] as? NSNumber else { continue }
            let id = n.int64Value
            guard !cardGone, let hex = o["apdu"] as? String, let apdu = Hex.decode(hex), apdu.count >= 4 else {
                try? await socket.send(LinkFrame.lost(id))
                continue
            }
            do {
                let r = try await card.transmit(apdu)
                try? await socket.send(LinkFrame.rapdu(id, r))
            } catch {
                markCardGone()
                try? await socket.send(LinkFrame.lost(id))
            }
        }
    }

    func close() { socket.close() }
}

enum LinkFrame {
    static func decode(_ text: String) -> [String: Any]? {
        guard let d = text.data(using: .utf8) else { return nil }
        return (try? JSONSerialization.jsonObject(with: d)) as? [String: Any]
    }
    static func rapdu(_ id: Int64, _ r: Data) -> String { "{\"type\":\"rapdu\",\"id\":\(id),\"rapdu\":\"\(Hex.encode(r))\"}" }
    static func lost(_ id: Int64) -> String { "{\"type\":\"lost\",\"id\":\(id)}" }
}

func withTimeout<T>(_ seconds: TimeInterval, _ op: @escaping () async throws -> T) async throws -> T {
    try await withThrowingTaskGroup(of: T.self) { g in
        g.addTask { try await op() }
        g.addTask {
            try await Task.sleep(nanoseconds: UInt64(seconds * 1_000_000_000))
            throw LinkError.timeout
        }
        defer { g.cancelAll() }
        guard let r = try await g.next() else { throw LinkError.timeout }
        return r
    }
}

/// `GET /v1/card/link` over URLSession's WebSocket, with the bearer token.
final class URLSessionLinkSocket: NSObject, LinkSocket {
    private let session: URLSession
    private let task: URLSessionWebSocketTask

    init(url: URL, token: String?) {
        let cfg = URLSessionConfiguration.ephemeral
        cfg.timeoutIntervalForRequest = 130
        cfg.waitsForConnectivity = false
        session = URLSession(configuration: cfg)
        var req = URLRequest(url: url, timeoutInterval: 10)
        if let token { req.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization") }
        task = session.webSocketTask(with: req)
        task.maximumMessageSize = 1 << 20
        super.init()
        task.resume()
    }

    func send(_ text: String) async throws { try await task.send(.string(text)) }

    func receive() async throws -> String? {
        do {
            switch try await task.receive() {
            case .string(let s): return s
            case .data(let d): return String(data: d, encoding: .utf8)
            @unknown default: return nil
            }
        } catch {
            if task.closeCode != .invalid { return nil }
            throw LinkError.forStatus((task.response as? HTTPURLResponse)?.statusCode)
        }
    }

    func close() {
        task.cancel(with: .normalClosure, reason: nil)
        session.invalidateAndCancel()
    }
}
