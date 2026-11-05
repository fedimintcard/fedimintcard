import Foundation

/// Keypad entry in whole sats.
final class AmountPad {
    static let maxDigits = 9
    private var digits = ""

    func press(_ d: Int) {
        guard (0...9).contains(d) else { return }
        var next = digits + String(d)
        while next.hasPrefix("0") { next.removeFirst() }
        guard next.count <= AmountPad.maxDigits else { return }
        digits = next
    }

    func backspace() { if !digits.isEmpty { digits.removeLast() } }
    func clear() { digits = "" }

    var sats: Int64 { Int64(digits) ?? 0 }
    var msat: Int64 { satsToMsat(sats) }
    var isChargeable: Bool { sats > 0 }
    var display: String { formatSats(msat) }
}

/// The holder's PIN, held only as bytes this object owns. `wipe()` zeroes them; the PIN never
/// becomes a `String`, is never in `description`, and is only ever copied into the charge body.
final class Pin: CustomStringConvertible, CustomDebugStringConvertible {
    private var bytes: [UInt8]

    init(bytes: [UInt8]) { self.bytes = bytes }

    var isWiped: Bool { bytes.isEmpty }
    var count: Int { bytes.count }

    /// Runs `block` on the ASCII digits. Do not keep the buffer.
    func withBytes<T>(_ block: ([UInt8]) -> T) -> T { block(bytes) }

    func wipe() {
        for i in bytes.indices { bytes[i] = 0 }
        bytes = []
    }

    deinit { wipe() }

    var description: String { "Pin(****)" }
    var debugDescription: String { "Pin(****)" }
}

/// Masked PIN entry for the holder: digits only, `min` to `max` of them.
final class PinPad: CustomStringConvertible {
    /// The applet's and the till's bounds (host/card PIN_MIN, PIN_MAX).
    static let min = 4
    static let max = 8

    private var buf = [UInt8](repeating: 0, count: PinPad.max)
    private(set) var length = 0

    func press(_ d: Int) {
        guard (0...9).contains(d), length < PinPad.max else { return }
        buf[length] = UInt8(48 + d)
        length += 1
    }

    func backspace() {
        guard length > 0 else { return }
        length -= 1
        buf[length] = 0
    }

    func clear() {
        for i in buf.indices { buf[i] = 0 }
        length = 0
    }

    var isComplete: Bool { (PinPad.min...PinPad.max).contains(length) }

    /// One dot per digit typed; never the digits.
    var masked: String { String(repeating: "●", count: length) }

    /// Moves the digits out into a `Pin` and clears the pad, or nil while too short.
    func take() -> Pin? {
        guard isComplete else { return nil }
        let p = Pin(bytes: Array(buf[0..<length]))
        clear()
        return p
    }

    /// For tests only: whether any non-zero byte is left in the buffer.
    var bufferIsZeroed: Bool { buf.allSatisfy { $0 == 0 } }

    var description: String { "PinPad(length=\(length))" }
}
