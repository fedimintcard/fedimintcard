import XCTest
@testable import FedimintCardPOS

/// A till-side WebSocket: frames queued up front, sends recorded, closes after the last one.
final class MockSocket: LinkSocket {
    private var incoming: [String?]
    private let lock = NSLock()
    private(set) var sent: [String] = []
    private(set) var closed = false
    var receiveError: Error?
    /// Hold `receive()` forever (for the ready timeout).
    var hang = false

    init(_ frames: [String]) { incoming = frames }

    func send(_ text: String) async throws { lock.lock(); sent.append(text); lock.unlock() }

    func receive() async throws -> String? {
        if hang { try await Task.sleep(nanoseconds: 60_000_000_000); return nil }
        if let e = receiveError { throw e }
        lock.lock(); defer { lock.unlock() }
        if closed || incoming.isEmpty { return nil }
        return incoming.removeFirst()
    }

    func close() { lock.lock(); closed = true; lock.unlock() }

    func sentObjects() -> [[String: Any]] { sent.compactMap { LinkFrame.decode($0) } }
}

final class MockCard: CardChannel {
    struct Gone: Error {}
    var responses: [String: Data] = [:]
    var failFrom: Int? // the n-th transmit (1-based) and later throw
    private(set) var calls: [Data] = []

    func transmit(_ apdu: Data) async throws -> Data {
        calls.append(apdu)
        if let f = failFrom, calls.count >= f { throw Gone() }
        return responses[Hex.encode(apdu)] ?? Data([0x6d, 0x00])
    }
}

final class CardLinkTests: XCTestCase {
    let ready = #"{"type":"ready","link":"0123456789abcdef"}"#
    let select = "00a4040008f0464d4341524401"

    func apdu(_ id: Int, _ hex: String) -> String { #"{"type":"apdu","id":\#(id),"apdu":"\#(hex)"}"# }

    func testOpenReadsReady() async throws {
        let link = try await CardLink.open(MockSocket([ready]))
        XCTAssertEqual(link.id, "0123456789abcdef")
    }

    func testOpenRejectsBadFirstFrame() async {
        for first in [#"{"type":"apdu","id":1,"apdu":"00"}"#, #"{"type":"ready"}"#, "not json", #"{"type":"ready","link":""}"#] {
            let s = MockSocket([first])
            do { _ = try await CardLink.open(s); XCTFail(first) } catch {
                XCTAssertEqual(error as? LinkError, .badHandshake, first)
                XCTAssertTrue(s.closed)
            }
        }
        let empty = MockSocket([])
        do { _ = try await CardLink.open(empty); XCTFail() } catch { XCTAssertEqual(error as? LinkError, .badHandshake) }
    }

    func testOpenPassesUnauthorized() async {
        let s = MockSocket([]); s.receiveError = LinkError.unauthorized
        do { _ = try await CardLink.open(s); XCTFail() } catch { XCTAssertEqual(error as? LinkError, .unauthorized) }
    }

    func testOpenTimesOutWithoutReady() async {
        let s = MockSocket([]); s.hang = true
        do { _ = try await CardLink.open(s, readyTimeout: 0.2); XCTFail() } catch { XCTAssertEqual(error as? LinkError, .timeout) }
    }

    func testAnswersApdusInOrder() async throws {
        let s = MockSocket([ready, apdu(1, select), apdu(2, "80ca000000")])
        let card = MockCard()
        card.responses[select] = Data([0x90, 0x00])
        card.responses["80ca000000"] = Data([0x01, 0x02, 0x90, 0x00])
        let link = try await CardLink.open(s)
        await link.serve(card: card)
        XCTAssertEqual(card.calls.map(Hex.encode), [select, "80ca000000"])
        let out = s.sentObjects()
        XCTAssertEqual(out.count, 2)
        XCTAssertEqual(out[0]["type"] as? String, "rapdu"); XCTAssertEqual((out[0]["id"] as? NSNumber)?.intValue, 1)
        XCTAssertEqual(out[0]["rapdu"] as? String, "9000")
        XCTAssertEqual(out[1]["rapdu"] as? String, "01029000"); XCTAssertEqual((out[1]["id"] as? NSNumber)?.intValue, 2)
    }

    func testCardLostAnswersLostThenEveryLaterApdu() async throws {
        let s = MockSocket([ready, apdu(1, select), apdu(2, select), apdu(3, select)])
        let card = MockCard(); card.responses[select] = Data([0x90, 0x00]); card.failFrom = 2
        let link = try await CardLink.open(s)
        await link.serve(card: card)
        let out = s.sentObjects()
        XCTAssertEqual(out.map { $0["type"] as? String }, ["rapdu", "lost", "lost"])
        XCTAssertEqual(out.map { ($0["id"] as? NSNumber)?.intValue }, [1, 2, 3])
        XCTAssertEqual(card.calls.count, 2, "no APDU goes to a card that has left")
        XCTAssertTrue(link.cardGone)
    }

    func testSessionEndMarksCardGone() async throws {
        let s = MockSocket([ready, apdu(5, select)])
        let card = MockCard()
        let link = try await CardLink.open(s)
        link.markCardGone()
        await link.serve(card: card)
        XCTAssertEqual(s.sent, [#"{"type":"lost","id":5}"#])
        XCTAssertTrue(card.calls.isEmpty)
    }

    func testMalformedApduIsLostAndUnknownFramesIgnored() async throws {
        let s = MockSocket([ready, #"{"type":"ping"}"#, "garbage", apdu(1, "zz"), apdu(2, "00a4"), apdu(3, select)])
        let card = MockCard(); card.responses[select] = Data([0x90, 0x00])
        let link = try await CardLink.open(s)
        await link.serve(card: card)
        XCTAssertEqual(s.sent, [#"{"type":"lost","id":1}"#, #"{"type":"lost","id":2}"#, #"{"type":"rapdu","id":3,"rapdu":"9000"}"#])
    }

    func testServeEndsWhenTillCloses() async throws {
        let s = MockSocket([ready])
        let link = try await CardLink.open(s)
        await link.serve(card: MockCard()) // returns: nothing left, socket closed
        XCTAssertTrue(s.sent.isEmpty)
    }
}

final class LinkStatusTests: XCTestCase {
    /// The link's HTTP answer when the WebSocket did not open: 401 and 403 are this phone refused
    /// (never taken on the phone); a 5xx or no answer is the till not serving.
    func testRefusalsAndTheTillNotServing() {
        XCTAssertEqual(LinkError.forStatus(401), .unauthorized)
        XCTAssertEqual(LinkError.forStatus(403), .unauthorized)
        for s: Int? in [nil, 500, 502, 503, 504, 404] { XCTAssertEqual(LinkError.forStatus(s), .unreachable, "\(String(describing: s))") }
    }
}
