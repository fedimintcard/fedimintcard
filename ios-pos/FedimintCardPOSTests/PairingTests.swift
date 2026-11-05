import XCTest
@testable import FedimintCardPOS

final class PairingTests: XCTestCase {
    let token = String(repeating: "ab", count: 32)

    func testParsesTillPairingString() {
        let p = TillPairing.parse("fedimintcard-till:http://192.168.1.20:9877?t=\(token)")
        XCTAssertEqual(p, TillPairing(baseURL: "http://192.168.1.20:9877", token: token))
        XCTAssertEqual(p?.linkURL?.absoluteString, "ws://192.168.1.20:9877/v1/card/link")
    }

    func testTrimsWhitespaceAndSchemeCase() {
        let p = TillPairing.parse("  FEDIMINTCARD-TILL:http://10.0.0.5:9877?t=\(token)\n")
        XCTAssertEqual(p?.baseURL, "http://10.0.0.5:9877")
        XCTAssertEqual(p?.token, token)
    }

    func testTypedURLWithoutToken() {
        XCTAssertEqual(TillPairing.parse("http://10.0.0.5:9877/"), TillPairing(baseURL: "http://10.0.0.5:9877", token: nil))
        XCTAssertEqual(TillPairing.parse("10.0.0.5:9877"), TillPairing(baseURL: "http://10.0.0.5:9877", token: nil))
    }

    func testHttpsMapsToWss() {
        XCTAssertEqual(TillPairing.parse("fedimintcard-till:https://till.local?t=abc")?.linkURL?.absoluteString,
                       "wss://till.local/v1/card/link")
    }

    func testRejectsGarbage() {
        for s in ["", "fedimintcard-till:", "hello world", "ftp://1.2.3.4:9877?t=ab",
                  "fedimintcard-till:http://1.2.3.4:9877?t=", "fedimintcard-till:http://1.2.3.4:9877?x=ab",
                  "fedimintcard-till:http://1.2.3.4:9877?t=ab&t=cd", "fedimintcard-till:http://1.2.3.4:9877/v1?t=ab",
                  "fedimintcard-till:http://u:p@1.2.3.4:9877?t=ab", "fedimintcard-till:http://1.2.3.4:9877?t=a%20b",
                  "fedimintcard-till:http://1.2.3.4:9877#frag"] {
            XCTAssertNil(TillPairing.parse(s), s)
        }
    }

    func testMemoryStoreRoundTrip() {
        let s = MemoryPairingStore()
        let p = TillPairing(baseURL: "http://1.2.3.4:9877", token: token)
        s.save(p); XCTAssertEqual(s.load(), p)
        s.clear(); XCTAssertNil(s.load())
    }
}
