import Network
import XCTest
@testable import FedimintCardPOS

/// A real WebSocket on loopback (Network.framework) standing in for the till's `/v1/card/link`,
/// so the app's URLSession socket, bearer header and frame handling are exercised end to end.
final class MockTill {
    let listener: NWListener
    private var authFetch: () -> String? = { nil }
    var authHeader: String? { authFetch() }
    private(set) var path: String?
    private(set) var received: [String] = []
    private var conn: NWConnection?
    let script: [String]
    let done = XCTestExpectation(description: "till finished")

    init(script: [String]) throws {
        self.script = script
        let ws = NWProtocolWebSocket.Options()
        var seen: String?
        ws.setClientRequestHandler(.main) { _, headers in
            seen = headers.first { $0.name.lowercased() == "authorization" }?.value
            return NWProtocolWebSocket.Response(status: .accept, subprotocol: nil)
        }
        let params = NWParameters.tcp
        params.defaultProtocolStack.applicationProtocols.insert(ws, at: 0)
        listener = try NWListener(using: params, on: .any)
        authFetch = { seen }
        listener.newConnectionHandler = { [weak self] c in self?.accept(c) }
    }

    func start() async -> UInt16 {
        await withCheckedContinuation { k in
            listener.stateUpdateHandler = { [weak self] s in
                if case .ready = s { k.resume(returning: self!.listener.port!.rawValue); self?.listener.stateUpdateHandler = nil }
            }
            listener.start(queue: .main)
        }
    }

    private func accept(_ c: NWConnection) {
        conn = c
        c.start(queue: .main)
        send(#"{"type":"ready","link":"a1b2c3d4e5f60718"}"#)
        step(0)
    }

    private func send(_ text: String) {
        let meta = NWProtocolWebSocket.Metadata(opcode: .text)
        let ctx = NWConnection.ContentContext(identifier: "t", metadata: [meta])
        conn?.send(content: Data(text.utf8), contentContext: ctx, isComplete: true, completion: .contentProcessed { _ in })
    }

    /// Sends script[i], waits for the phone's answer, then the next; closes at the end.
    private func step(_ i: Int) {
        guard i < script.count else {
            conn?.cancel()
            done.fulfill()
            return
        }
        send(script[i])
        conn?.receiveMessage { [weak self] data, _, _, _ in
            if let data { self?.received.append(String(decoding: data, as: UTF8.self)) }
            self?.step(i + 1)
        }
    }

    func stop() { conn?.cancel(); listener.cancel() }
}

final class LinkSocketIntegrationTests: XCTestCase {
    func testURLSessionLinkAgainstMockTill() async throws {
        let till = try MockTill(script: [#"{"type":"apdu","id":1,"apdu":"00a4040008f0464d4341524401"}"#,
                                         #"{"type":"apdu","id":2,"apdu":"80ca000000"}"#])
        defer { till.stop() }
        let port = await till.start()
        let pairing = TillPairing.parse("fedimintcard-till:http://127.0.0.1:\(port)?t=cafe01")!
        let card = MockCard()
        card.responses["00a4040008f0464d4341524401"] = Data([0x90, 0x00])
        card.failFrom = 2
        let link = try await CardLink.open(URLSessionLinkSocket(url: pairing.linkURL!, token: pairing.token))
        XCTAssertEqual(link.id, "a1b2c3d4e5f60718")
        let serve = Task { await link.serve(card: card) }
        await fulfillment(of: [till.done], timeout: 5)
        _ = await serve.value
        link.close()
        XCTAssertEqual(till.authHeader, "Bearer cafe01")
        XCTAssertEqual(till.received, [#"{"type":"rapdu","id":1,"rapdu":"9000"}"#, #"{"type":"lost","id":2}"#])
    }
}
