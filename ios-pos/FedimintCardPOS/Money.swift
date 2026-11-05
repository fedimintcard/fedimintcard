import Foundation

func satsToMsat(_ sats: Int64) -> Int64 { sats * 1000 }

private func group(_ n: Int64) -> String {
    let s = String(n)
    var out = ""
    for (i, c) in s.enumerated() {
        if i > 0 && (s.count - i) % 3 == 0 { out.append(",") }
        out.append(c)
    }
    return out
}

/// Whole sats with thousands separators; up to 3 decimals only when msat is not a whole sat.
func formatSats(_ msat: Int64) -> String {
    let neg = msat < 0
    let a = neg ? -msat : msat
    let frac = a % 1000
    let whole = group(a / 1000)
    var f = ""
    if frac != 0 {
        var d = String(frac)
        while d.count < 3 { d = "0" + d }
        while d.hasSuffix("0") { d.removeLast() }
        f = "." + d
    }
    return (neg ? "-" : "") + whole + f
}

func shortTxid(_ txid: String) -> String {
    guard txid.count > 13 else { return txid }
    return String(txid.prefix(8)) + "…" + String(txid.suffix(4))
}

enum Hex {
    static func encode(_ d: Data) -> String { d.map { String(format: "%02x", $0) }.joined() }

    static func decode(_ s: String) -> Data? {
        let chars = Array(s.utf8)
        guard chars.count % 2 == 0 else { return nil }
        var out = Data(capacity: chars.count / 2)
        var i = 0
        while i < chars.count {
            guard let hi = nibble(chars[i]), let lo = nibble(chars[i + 1]) else { return nil }
            out.append(hi << 4 | lo)
            i += 2
        }
        return out
    }

    private static func nibble(_ c: UInt8) -> UInt8? {
        switch c {
        case 48...57: return c - 48
        case 97...102: return c - 87
        case 65...70: return c - 55
        default: return nil
        }
    }
}
