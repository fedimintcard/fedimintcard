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

/// What the receipt and the NFC sheet say for a paid sale: the price only, and "Paid offline" for a
/// charge the till took with no registry or federation reachable.
func receiptHeadline(_ receipt: ChargeReceipt) -> String {
    receipt.offline
        ? "Paid offline · \(formatSats(receipt.priceMsat)) sats"
        : "Paid \(formatSats(receipt.priceMsat)) sats"
}

/// The line under a card's payable balance for the notes the till holds back from it (ENG-867,
/// `stranded_msat` and `stranded_until_ms` in docs/TILL-API.md), or nil when none is held: no line
/// at all. "N sats on hold until D" while `untilMs` is ahead of the phone's clock `nowMs`; "N sats
/// on hold" when there is no date to show (nil, or not ahead of the clock).
///
/// D is the first whole day in `timeZone` the notes can come back: the local day that starts at or
/// after `untilMs`. A hold that ends at 15:00 on Dec 21 names Dec 22, and one that ends exactly at
/// the midnight that starts Dec 21 names Dec 21: the line never promises the money a day early.
func onHoldText(strandedMsat: Int64, untilMs: Int64?, nowMs: Int64, timeZone: TimeZone) -> String? {
    guard strandedMsat > 0 else { return nil }
    let held = "\(formatSats(strandedMsat)) sats on hold"
    guard let untilMs, untilMs > nowMs, let day = firstWholeDay(untilMs, timeZone) else { return held }
    let f = DateFormatter()
    f.locale = Locale(identifier: "en_US_POSIX") // the app's copy is English whatever the phone's language
    f.calendar = Calendar(identifier: .gregorian)
    f.timeZone = timeZone
    f.dateFormat = "MMM d"
    return "\(held) until \(f.string(from: day))"
}

/// The first local day in `timeZone` that starts at or after `ms`, as the moment it starts.
private func firstWholeDay(_ ms: Int64, _ timeZone: TimeZone) -> Date? {
    var cal = Calendar(identifier: .gregorian)
    cal.timeZone = timeZone
    let start = cal.startOfDay(for: Date(timeIntervalSince1970: TimeInterval(ms) / 1000))
    // Compared in whole milliseconds, as the till sends them.
    if Int64((start.timeIntervalSince1970 * 1000).rounded()) == ms { return start }
    return cal.date(byAdding: .day, value: 1, to: start)
}

/// What the NFC sheet and VoiceOver say of a balance read: the payable balance, then `onHold`
/// (`onHoldText`) when there is one.
func balanceSpoken(liveMsat: Int64, onHold: String?) -> String {
    "\(formatSats(liveMsat)) sats on this card" + (onHold.map { ". \($0)" } ?? "")
}

/// "N payments waiting to sync" for the till's offline queue; nil when nothing is waiting.
func waitingToSyncText(_ queued: Int64) -> String? {
    queued > 0 ? "\(queued) \(queued == 1 ? "payment" : "payments") waiting to sync" : nil
}

/// The home screen's count: the till's offline queue (when its status is known) and this phone's.
func waitingToSyncText(till: TillStatus?, phone: OfflineStats) -> String? {
    waitingToSyncText((till?.offlineQueued ?? 0) + Int64(phone.waiting))
}

/// The home screen's line for sales a card left signing on this phone (`stats().signing`): only
/// that card's next tap here finishes each. Nil when none.
func heldOnPhoneText(_ count: Int) -> String? {
    switch count {
    case ..<1: return nil
    case 1: return "1 payment is waiting for its card: tap that card to finish it."
    default: return "\(count) payments are waiting for their cards: tap those cards to finish them."
    }
}

/// A duration in the largest whole unit: "45 s", "12 min", "5 h", "6 days".
func durationText(_ ms: Int64) -> String {
    let s = max(0, ms) / 1000
    if s < 60 { return "\(s) s" }
    if s < 3600 { return "\(s / 60) min" }
    if s < 2 * 86_400 { return "\(s / 3600) h" }
    return "\(s / 86_400) days"
}

/// Settings' line for the offline kit: when the phone last had it from the till, and how long it
/// is good for.
func kitAgeText(fetchedAtMs: Int64, expiresAtMs: Int64, nowMs: Int64) -> String {
    let fetched = "Kit fetched \(durationText(nowMs - fetchedAtMs)) ago"
    return nowMs >= expiresAtMs ? fetched + " · expired" : fetched + " · expires in \(durationText(expiresAtMs - nowMs))"
}

/// Settings' row for a charge the till refused at sync: the card paid, the till did not take it.
func refusedChargeText(priceMsat: Int64, code: String) -> String {
    "\(formatSats(priceMsat)) sats · \(code.isEmpty ? "refused" : code): the till refused it after the card paid."
}

/// The confirm before re-pairing or forgetting a till with payments this phone still holds.
func unpairWarningText(_ count: Int) -> String {
    count == 1
        ? "1 payment is waiting to sync with this till. It stays on this phone and can't be synced to another till."
        : "\(count) payments are waiting to sync with this till. They stay on this phone and can't be synced to another till."
}

/// Settings' offline lines: why offline is off, or the kit (its age and expiry, the phone's caps),
/// what the store holds and why the last sync stopped short. When `till` (the till's last status)
/// reports `offline.double_spent`, its count follows the phone's, offline on or off. Each one is a
/// sale already answered as paid offline that the registry later found spent elsewhere: a loss the
/// till keeps for the operator, so the line names it as one.
func offlinePanelLines(_ phone: OfflineSnapshot, nowMs: Int64, till: TillStatus? = nil) -> [String] {
    let doubleSpent = till?.offlineDoubleSpent.map { "Offline payments lost to double spends: \($0)" }
    guard phone.enabled else {
        return ["Offline payments are off on this phone" + (phone.disabledReason.map { ": \($0)" } ?? "."), doubleSpent].compactMap { $0 }
    }
    var lines: [String] = []
    if let kit = phone.kit {
        lines.append(kitAgeText(fetchedAtMs: kit.fetchedAtMs, expiresAtMs: kit.expiresAtMs, nowMs: nowMs))
        lines.append("Caps: \(kit.capCount) payments or \(formatSats(kit.capMsat)) sats waiting to sync")
    } else {
        lines.append("No offline kit yet. Reach the till once while online.")
    }
    let s = phone.stats
    lines.append("\(s.waiting) waiting to sync · \(s.signing) held for their cards · \(s.synced) synced")
    if let doubleSpent { lines.append(doubleSpent) }
    if let why = phone.syncStopped { lines.append("Sync stopped: \(why)") }
    return lines
}
