import XCTest
@testable import FedimintCardPOS

final class PadTests: XCTestCase {
    func testAmountPad() {
        let p = AmountPad()
        XCTAssertFalse(p.isChargeable)
        p.press(0); p.press(0); XCTAssertEqual(p.sats, 0)
        p.press(1); p.press(2); p.press(3); p.press(4)
        XCTAssertEqual(p.display, "1,234"); XCTAssertEqual(p.msat, 1_234_000)
        p.backspace(); XCTAssertEqual(p.sats, 123)
        for _ in 0..<20 { p.press(9) }
        XCTAssertEqual(String(p.sats).count, AmountPad.maxDigits)
        p.press(10); p.press(-3)
        p.clear(); XCTAssertEqual(p.sats, 0); XCTAssertEqual(p.display, "0")
    }

    func testPinPadBounds() {
        let p = PinPad()
        for d in [1, 2, 3] { p.press(d) }
        XCTAssertFalse(p.isComplete); XCTAssertNil(p.take())
        p.press(4); XCTAssertTrue(p.isComplete)
        for d in [5, 6, 7, 8, 9, 0] { p.press(d) }
        XCTAssertEqual(p.length, PinPad.max)
        p.press(11); p.press(-1); XCTAssertEqual(p.length, 8)
        XCTAssertEqual(p.masked, "●●●●●●●●")
        p.backspace(); XCTAssertEqual(p.length, 7)
    }

    func testPinNeverInDescriptions() {
        let p = PinPad()
        for d in [1, 3, 5, 7] { p.press(d) }
        XCTAssertFalse(p.description.contains("1357"))
        XCTAssertFalse(p.masked.contains("1"))
        let pin = p.take()!
        XCTAssertEqual("\(pin)", "Pin(****)")
        XCTAssertEqual(String(reflecting: pin), "Pin(****)")
    }

    func testTakeMovesDigitsAndWipesPad() {
        let p = PinPad()
        for d in [4, 3, 2, 1] { p.press(d) }
        let pin = p.take()!
        XCTAssertEqual(p.length, 0)
        XCTAssertTrue(p.bufferIsZeroed)
        XCTAssertEqual(pin.withBytes { String(decoding: $0, as: UTF8.self) }, "4321")
        pin.wipe()
        XCTAssertTrue(pin.isWiped)
        XCTAssertEqual(pin.withBytes { $0.count }, 0)
    }

    func testClearAndBackspaceZeroBuffer() {
        let p = PinPad()
        for d in [9, 9, 9, 9, 9] { p.press(d) }
        p.backspace(); p.backspace(); p.backspace(); p.backspace(); p.backspace()
        XCTAssertTrue(p.bufferIsZeroed)
        for d in [9, 9, 9, 9] { p.press(d) }
        p.clear()
        XCTAssertTrue(p.bufferIsZeroed)
    }

    func testMoney() {
        XCTAssertEqual(formatSats(0), "0")
        XCTAssertEqual(formatSats(1_000), "1")
        XCTAssertEqual(formatSats(1_234_567_000), "1,234,567")
        XCTAssertEqual(formatSats(1_500), "1.5")
        XCTAssertEqual(formatSats(1_001), "1.001")
        XCTAssertEqual(formatSats(-2_000), "-2")
        XCTAssertEqual(shortTxid("0123456789abcdef0123"), "01234567…0123")
        XCTAssertEqual(shortTxid("short"), "short")
        XCTAssertEqual(Hex.decode("00A4ff"), Data([0x00, 0xa4, 0xff]))
        XCTAssertNil(Hex.decode("abc")); XCTAssertNil(Hex.decode("zz"))
        XCTAssertEqual(Hex.encode(Data([0x90, 0x00])), "9000")
    }
}
