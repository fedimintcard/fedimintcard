import Foundation
import FedimintCardPosCore

// The phone as an offline till (docs/POS-OFFLINE.md): the POS core (host/pos-core, UniFFI) takes
// the card when the till cannot be reached, keeps what it took in its store, and sends it to the
// till once the till answers. This is the only file that imports the core; the rest of the app,
// and the tests, see it through `OfflineCore`.

// MARK: - The core, in the app's terms

/// The card as the core asks it: one APDU at a time, the calling thread waiting for the answer.
protocol BlockingCardChannel: AnyObject {
    /// The card's answer, its data then SW1 SW2. A throw is the card leaving the field.
    func transmit(_ apdu: Data) throws -> Data
}

/// How a charge on the phone ended.
enum CoreOutcome: Equatable {
    case paid(txid: String, priceMsat: Int64, tillMsat: Int64, overpayMsat: Int64, feesMsat: Int64,
              inputs: [Note], change: [Note], replayed: Bool)
    /// `code` is the till API's (docs/TILL-API.md).
    case refused(code: String, detail: String, suggestedMsat: Int64?, triesLeft: Int?)
    /// The core failed in a way it did not expect: the sale's outcome is unknown, as `undecided`.
    case threw(String)
}

/// What the phone's store holds (`PosCore.stats()`).
struct OfflineStats: Equatable {
    /// Taken offline and signed, not on the till yet: "N payments waiting to sync".
    var waiting = 0
    var waitingMsat: Int64 = 0
    /// Left signing by a card taken away mid-tap: that card's next tap here finishes each.
    var signing = 0
    /// Refused by the till at sync, kept until dismissed.
    var refused = 0
    /// On the till, kept for repeats of their sale.
    var synced = 0
}

/// A charge the till refused at sync: the card paid, the till did not take it.
struct RefusedOfflineCharge: Equatable, Identifiable {
    let txid: String
    let code: String
    let priceMsat: Int64
    let tsUnixMs: Int64
    var id: String { txid }
}

/// `PosCore.drain`: the body of `POST /v1/offline/charges`, the charges in it, the refused ones.
struct CoreDrain: Equatable {
    let body: String
    let txids: [String]
    let refused: [RefusedOfflineCharge]
}

/// `PosCore.mark_synced`: on the till now, refused for good, refused for now.
struct CoreSyncReport: Equatable {
    let synced: Int
    let refused: Int
    let retry: Int
}

/// The core threw (`PosCoreError`): its store, the till's answer, or a failure it did not expect.
struct CoreFailure: Error, Equatable, CustomStringConvertible {
    let description: String
}

/// The core's calls (`PosCore`, or a fake in tests). Each one blocks: `OfflinePhone` makes them on
/// its own serial queue, never on the main thread, where the NFC tag answers.
protocol OfflineCore: AnyObject {
    func phoneId() -> String
    func charge(card: BlockingCardChannel, kitJson: String, priceMsat: Int64, clientRef: String, pin: String) -> CoreOutcome
    func stats() throws -> OfflineStats
    func signingRefs() throws -> [String]
    func drain(max: Int) throws -> CoreDrain
    func markSynced(answerJson: String) throws -> CoreSyncReport
    func dismiss(txid: String) throws -> Bool
}

/// `OfflineCore` over the UniFFI core.
final class PosCoreAdapter: OfflineCore {
    private let core: PosCore

    private init(_ core: PosCore) { self.core = core }

    static func open(_ dir: URL) throws -> PosCoreAdapter {
        do { return PosCoreAdapter(try PosCore.open(dir: dir.path)) } catch { throw failure(error) }
    }

    private static func failure(_ e: Error) -> CoreFailure {
        switch e as? PosCoreError {
        case .Store(let m)?: return CoreFailure(description: "the offline store: \(m)")
        case .Answer(let m)?: return CoreFailure(description: "the till's answer: \(m)")
        case .Internal(let m)?: return CoreFailure(description: m)
        case nil: return CoreFailure(description: "\(e)")
        }
    }

    private static func msat(_ v: UInt64) -> Int64 { Int64(clamping: v) }
    private static func notes(_ s: [SlotMsat]) -> [Note] { s.map { Note(slot: Int($0.slot), msat: msat($0.msat)) } }

    /// The core's own check that the offline crypto runs on this device (a note signed, unblinded
    /// and verified, a BIP340 signature): false, never a trap, when it does not.
    static func selfCheck() -> Bool { FedimintCardPosCore.selfCheck() }

    func phoneId() -> String { core.phoneId() }

    func charge(card: BlockingCardChannel, kitJson: String, priceMsat: Int64, clientRef: String, pin: String) -> CoreOutcome {
        do {
            switch try core.charge(transport: CoreTransport(card), kitJson: kitJson, priceMsat: UInt64(max(0, priceMsat)),
                                   clientRef: clientRef, pin: pin) {
            case .paid(let txid, let price, let till, let overpay, let fees, _, let inputs, let change, let replayed):
                return .paid(txid: txid, priceMsat: Self.msat(price), tillMsat: Self.msat(till), overpayMsat: Self.msat(overpay),
                             feesMsat: Self.msat(fees), inputs: Self.notes(inputs), change: Self.notes(change), replayed: replayed)
            case .refused(let code, let detail, let suggested, _, _, let tries):
                return .refused(code: code, detail: detail, suggestedMsat: suggested.map(Self.msat), triesLeft: tries.map(Int.init))
            }
        } catch {
            return .threw(Self.failure(error).description)
        }
    }

    func stats() throws -> OfflineStats {
        do {
            let s = try core.stats()
            return OfflineStats(waiting: Int(s.waiting), waitingMsat: Self.msat(s.waitingMsat), signing: Int(s.signing),
                                refused: Int(s.refused), synced: Int(s.synced))
        } catch { throw Self.failure(error) }
    }

    func signingRefs() throws -> [String] {
        do { return try core.signingRefs() } catch { throw Self.failure(error) }
    }

    func drain(max: Int) throws -> CoreDrain {
        do {
            let d = try core.drain(max: UInt32(clamping: Swift.max(0, max)))
            return CoreDrain(body: d.body, txids: d.txids, refused: d.refused.map {
                RefusedOfflineCharge(txid: $0.txid, code: $0.code, priceMsat: Self.msat($0.priceMsat), tsUnixMs: Self.msat($0.tsUnixMs))
            })
        } catch { throw Self.failure(error) }
    }

    func markSynced(answerJson: String) throws -> CoreSyncReport {
        do {
            let r = try core.markSynced(answerJson: answerJson)
            return CoreSyncReport(synced: Int(r.synced), refused: Int(r.refused), retry: Int(r.retry))
        } catch { throw Self.failure(error) }
    }

    func dismiss(txid: String) throws -> Bool {
        do { return try core.dismiss(txid: txid) } catch { throw Self.failure(error) }
    }
}

/// The core's `ApduTransport` over the app's card: an error is the card leaving the field.
private final class CoreTransport: ApduTransport, @unchecked Sendable {
    private let card: BlockingCardChannel
    init(_ card: BlockingCardChannel) { self.card = card }

    func transmit(apdu: Data) throws -> Data {
        do { return try card.transmit(apdu) } catch { throw FedimintCardPosCore.TransportError.Lost(reason: "\(error)") }
    }
}

// MARK: - The card in the NFC session

/// The card in the NFC session, for the core. The core calls `transmit` on `OfflinePhone`'s queue
/// and waits there; the tag answers on the main queue (the reader's delegate queue), so a call on
/// the main thread is refused rather than left to wait forever. An APDU the card does not answer
/// in `timeout` is lost, and from `cancel()` on (the NFC session or the card job ended) every APDU
/// is. APDU bytes are never logged.
final class NFCApduTransport: BlockingCardChannel {
    struct Lost: Error, CustomStringConvertible { let description: String }

    static let apduTimeout: TimeInterval = 15

    private let channel: CardChannel
    private let timeout: TimeInterval
    private let lock = NSLock()
    private var cancelled = false
    private var waiting: DispatchSemaphore?

    init(_ channel: CardChannel, timeout: TimeInterval = NFCApduTransport.apduTimeout) {
        self.channel = channel
        self.timeout = timeout
    }

    var isCancelled: Bool { lock.lock(); defer { lock.unlock() }; return cancelled }

    /// Every APDU from now on is lost, the one waiting too.
    func cancel() {
        lock.lock()
        cancelled = true
        let w = waiting
        waiting = nil
        lock.unlock()
        w?.signal()
    }

    func transmit(_ apdu: Data) throws -> Data {
        guard !Thread.isMainThread else { throw Lost(description: "called on the main thread, where the card answers") }
        let done = DispatchSemaphore(value: 0)
        let answer = Answer()
        lock.lock()
        if cancelled { lock.unlock(); throw Lost(description: "the NFC session ended") }
        waiting = done
        lock.unlock()
        let channel = self.channel
        let task = Task { [weak self] in
            if self?.isCancelled ?? true {
                answer.set(.failure(Lost(description: "the NFC session ended")))
            } else {
                do { answer.set(.success(try await channel.transmit(apdu))) } catch { answer.set(.failure(error)) }
            }
            done.signal()
        }
        let timedOut = done.wait(timeout: .now() + timeout) == .timedOut
        lock.lock()
        if waiting === done { waiting = nil }
        lock.unlock()
        if timedOut {
            task.cancel()
            cancel()
            throw Lost(description: "the card did not answer in \(Int(timeout)) s")
        }
        switch answer.get() {
        case .success(let d)?: return d
        case .failure(let e)?: throw Lost(description: "\(e)")
        case nil: task.cancel(); throw Lost(description: "the NFC session ended")
        }
    }

    private final class Answer {
        private let lock = NSLock()
        private var value: Result<Data, Error>?
        func set(_ r: Result<Data, Error>) { lock.lock(); value = r; lock.unlock() }
        func get() -> Result<Data, Error>? { lock.lock(); defer { lock.unlock() }; return value }
    }
}

// MARK: - The store's directory

/// `Application Support/PosCore`: the core's store and the kit, in the complete-protection file
/// class (unreadable while the phone is locked) and out of iCloud and computer backups. A store
/// restored from a backup would take pool entries a second time, and the till would refuse those
/// payments (docs/POS-OFFLINE.md § The phone's store).
enum OfflineStore {
    struct Problem: Error, CustomStringConvertible { let description: String }

    static func defaultDirectory() throws -> URL {
        try FileManager.default.url(for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
            .appendingPathComponent("PosCore", isDirectory: true)
    }

    /// Makes `dir` if needed, sets both attributes, and reads both back: offline stays off when
    /// either does not hold.
    static func prepare(_ dir: URL) throws {
        let fm = FileManager.default
        try fm.createDirectory(at: dir, withIntermediateDirectories: true, attributes: [.protectionKey: FileProtectionType.complete])
        try fm.setAttributes([.protectionKey: FileProtectionType.complete], ofItemAtPath: dir.path)
        var u = dir
        var v = URLResourceValues()
        v.isExcludedFromBackup = true
        try u.setResourceValues(v)

        var check = URL(fileURLWithPath: dir.path, isDirectory: true)
        check.removeAllCachedResourceValues()
        guard try check.resourceValues(forKeys: [.isExcludedFromBackupKey]).isExcludedFromBackup == true else {
            throw Problem(description: "the offline store could not be kept out of backups")
        }
        try checkProtection(fm.attributesOfItem(atPath: dir.path)[.protectionKey] as? FileProtectionType, onSimulator: onSimulator)
    }

    /// The class read back from the directory: complete on a device. The simulator has no data
    /// protection, and reads back none.
    static func checkProtection(_ p: FileProtectionType?, onSimulator: Bool) throws {
        if p == .complete || (p == nil && onSimulator) { return }
        throw Problem(description: "the offline store's file protection is \(p?.rawValue ?? "none"), not complete")
    }

    static var onSimulator: Bool {
        #if targetEnvironment(simulator)
        return true
        #else
        return false
        #endif
    }
}

// MARK: - The kit

/// What the app reads of an offline kit (`GET /v1/offline/kit`); the core takes the whole answer.
struct KitInfo: Equatable {
    let kitId: String
    let phoneId: String
    let federationId: String
    let capMsat: Int64
    let capCount: Int
    let issuedAtMs: Int64
    let expiresAtMs: Int64
    /// Pool entries in the kit as issued; the core knows which it has used.
    let poolEntries: Int
    /// When this phone last had it from the till.
    let fetchedAtMs: Int64

    /// The till's `{"ok":true,"kit":{…}}`, or nil for anything else.
    static func parse(_ answer: Data, fetchedAtMs: Int64) -> KitInfo? {
        guard let o = try? JSONSerialization.jsonObject(with: answer) as? [String: Any], isTrue(o["ok"]),
              let k = o["kit"] as? [String: Any],
              let kitId = k["kit_id"] as? String, let phone = k["phone_id"] as? String, let fed = k["federation_id"] as? String,
              let capMsat = (k["cap_msat"] as? NSNumber)?.int64Value, let capCount = (k["cap_count"] as? NSNumber)?.intValue,
              let issued = (k["issued_at_ms"] as? NSNumber)?.int64Value, let expires = (k["expires_at_ms"] as? NSNumber)?.int64Value
        else { return nil }
        let pool = (k["pool"] as? [[String: Any]]) ?? []
        let entries = pool.reduce(0) { $0 + (($1["blinded"] as? [Any])?.count ?? 0) }
        return KitInfo(kitId: kitId, phoneId: phone, federationId: fed, capMsat: capMsat, capCount: capCount,
                       issuedAtMs: issued, expiresAtMs: expires, poolEntries: entries, fetchedAtMs: fetchedAtMs)
    }

    private static func isTrue(_ v: Any?) -> Bool {
        guard let n = v as? NSNumber, CFGetTypeID(n) == CFBooleanGetTypeID() else { return false }
        return n.boolValue
    }
}

/// Whether a new sale can be taken on the phone now.
enum OfflineAvailability: Equatable {
    case ready
    /// No kit yet, or offline is off on this phone (Settings says why).
    case noKit
    case kitExpired
    /// The phone's caps on what waits to sync.
    case capsReached
}

/// What the flow and the screens know of the phone's offline side. Immutable: `OfflinePhone`
/// makes a new one after each change.
struct OfflineSnapshot: Equatable {
    /// The store is open.
    var enabled = false
    /// Why offline is off, when it is.
    var disabledReason: String?
    var kit: KitInfo?
    var stats = OfflineStats()
    /// Sales retried on the phone only, never on the till's charge route: those a card left
    /// signing here (`signing_refs()`), and those whose last local charge was `undecided` or threw.
    var localOnlyRefs: Set<String> = []
    var refused: [RefusedOfflineCharge] = []
    /// Why the last sync stopped short (Settings' "Sync stopped" line); nil once a sync finds
    /// nothing waiting.
    var syncStopped: String?

    static let disabled = OfflineSnapshot()

    /// The core refuses all of these again, before the card hears anything.
    func availability(priceMsat: Int64, nowMs: Int64) -> OfflineAvailability {
        guard enabled, let kit else { return .noKit }
        if max(nowMs, kit.issuedAtMs) >= kit.expiresAtMs { return .kitExpired }
        if stats.waiting + stats.signing + 1 > kit.capCount || stats.waitingMsat + priceMsat > kit.capMsat { return .capsReached }
        return .ready
    }
}

// MARK: - The phone's offline side

/// The phone's store and kit, the local charge and the sync. Main actor; every core call runs on
/// one serial queue of its own.
@MainActor
final class OfflinePhone {
    /// A kept kit is fetched again once it is this old (the till answers the same one for an hour).
    static let kitRefreshAfterMs: Int64 = 10 * 60 * 1000
    /// After a fetch that failed (a till without the route, an `offline_not_ready`), the polls wait
    /// this long before the next: not every poll.
    static let kitRetryAfterMs: Int64 = 60 * 1000
    static let syncBatch = 50
    static let syncRounds = 20
    /// After a sync that stopped short (an HTTP failure, a round the till took none of, charges
    /// kept for their till), the syncs of the next minute return at once: the till's answer would
    /// not change sooner.
    static let syncRetryAfterMs: Int64 = 60 * 1000
    static let kitFile = "kit.json"

    private let queue = DispatchQueue(label: "org.fedimintcard.pos.offline")
    private let directory: () throws -> URL
    private let prepare: (URL) throws -> Void
    private let openCore: (URL) throws -> OfflineCore
    let clock: () -> Int64

    private var core: OfflineCore?
    private var dir: URL?
    private var phoneId = ""
    /// The open in progress: every caller of `start` waits for the same one.
    private var opening: Task<Void, Never>?
    private var disabledReason: String?
    private var kitAnswer: String?
    /// The till the kept kit came from.
    private var kitTill: String?
    private var kit: KitInfo?
    /// The till paired now (nil: none).
    private var currentTill: String?
    /// The till the kept kit came from: what the phone took with it is that till's.
    var kitTillURL: String? { kitTill }
    private var kitRefreshWanted = false
    /// When the last kit fetch failed, until one succeeds.
    private var kitFailedAtMs: Int64?
    /// Sales whose last local charge was `undecided` or threw: local only, with `signing_refs()`.
    private var heldRefs: Set<String> = []
    private var syncing = false
    /// Why the last sync stopped short, until one finds nothing waiting.
    private var syncStopped: String?
    /// No sync before this (the clock's ms), after one that stopped short.
    private var syncRetryAt: Int64 = 0

    private(set) var snapshot = OfflineSnapshot.disabled

    /// The store's directory, once open.
    var storeDirectory: URL? { dir }

    init(directory: @escaping () throws -> URL = OfflineStore.defaultDirectory,
         prepare: @escaping (URL) throws -> Void = OfflineStore.prepare,
         open: @escaping (URL) throws -> OfflineCore = { try PosCoreAdapter.open($0) },
         clock: @escaping () -> Int64 = { Int64(Date().timeIntervalSince1970 * 1000) }) {
        self.directory = directory
        self.prepare = prepare
        self.openCore = open
        self.clock = clock
    }

    private func onQueue<T>(_ work: @escaping () -> T) async -> T {
        await withCheckedContinuation { (k: CheckedContinuation<T, Never>) in
            queue.async { k.resume(returning: work()) }
        }
    }

    // MARK: opening

    /// Opens the store (once per process; called on every foreground, so a failure is tried again
    /// then) and reads the kept kit: dropped when it is another till's than the one paired.
    func start() async {
        guard core == nil else { return }
        if let o = opening { await o.value; return }
        let o = Task { await self.open() }
        opening = o
        await o.value
        opening = nil
    }

    private func open() async {
        let directory = self.directory, prepare = self.prepare, openCore = self.openCore
        let opened: Result<(URL, OfflineCore, String), Error> = await onQueue {
            Result {
                let dir = try directory()
                try prepare(dir)
                let core = try openCore(dir)
                return (dir, core, core.phoneId())
            }
        }
        switch opened {
        case .failure(let e):
            disabledReason = "\(e)"
            snapshot = OfflineSnapshot(enabled: false, disabledReason: disabledReason)
            return
        case .success(let (dir, core, phoneId)):
            self.dir = dir
            self.core = core
            self.phoneId = phoneId
            disabledReason = nil
        }
        await loadKit()
        if let cur = currentTill, let had = kitTill, had != cur { dropKit() }
        await refresh()
    }

    private var kitURL: URL? { dir?.appendingPathComponent(Self.kitFile) }

    private func loadKit() async {
        guard let url = kitURL else { return }
        let data: Data? = await onQueue { try? Data(contentsOf: url) }
        guard let data, let o = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let till = o["till"] as? String, let answer = o["answer"] as? String,
              let fetched = (o["fetched_at_ms"] as? NSNumber)?.int64Value,
              let info = KitInfo.parse(Data(answer.utf8), fetchedAtMs: fetched), info.phoneId == phoneId else { return }
        kit = info
        kitAnswer = answer
        kitTill = till
    }

    /// The till paired now (nil once it is forgotten). A kit from another till goes: its pool is
    /// that till's. A forgotten till's kit stays, for a held sale and for pairing it again.
    /// Payments waiting stay either way.
    func tillChanged(to baseURL: String?) {
        currentTill = baseURL
        guard let baseURL, let had = kitTill, had != baseURL else { return }
        dropKit()
    }

    private func dropKit() {
        kit = nil
        kitAnswer = nil
        kitTill = nil
        snapshot.kit = nil
        if let url = kitURL { queue.async { try? FileManager.default.removeItem(at: url) } }
    }

    // MARK: snapshot

    /// Reads the store again. `failClosed`: a sale to keep local-only when `signing_refs()` cannot
    /// be read (the last set is kept too).
    @discardableResult
    func refresh(failClosed: String? = nil) async -> OfflineSnapshot {
        guard let core else {
            snapshot = OfflineSnapshot(enabled: false, disabledReason: disabledReason)
            return snapshot
        }
        let (stats, refs, drained) = await onQueue { (Result { try core.stats() }, Result { try core.signingRefs() }, Result { try core.drain(max: 0) }) }
        var s = snapshot
        s.enabled = true
        s.disabledReason = nil
        s.kit = kit
        if case .success(let st) = stats { s.stats = st }
        switch refs {
        case .success(let r): s.localOnlyRefs = Set(r).union(heldRefs)
        case .failure:
            s.localOnlyRefs.formUnion(heldRefs)
            if let failClosed { s.localOnlyRefs.insert(failClosed) }
        }
        if case .success(let d) = drained { s.refused = d.refused }
        s.syncStopped = syncStopped
        snapshot = s
        return s
    }

    // MARK: kit

    /// Whether the status poll should fetch the kit: none kept, the kept one 10 minutes old, or a
    /// sync or a refusal asked for a new one.
    func kitRefreshDue() -> Bool {
        guard core != nil else { return false }
        let now = clock()
        if let failed = kitFailedAtMs, now - failed < Self.kitRetryAfterMs { return false }
        guard let kit else { return true }
        return kitRefreshWanted || now - kit.fetchedAtMs >= Self.kitRefreshAfterMs
    }

    /// Fetches the kit from the till `tillURL` (`client` asks that till only) and keeps it when it
    /// is `ok:true`, for this phone, of `federationId` (the till's status), and that till is still
    /// the one paired: anything else keeps the kit there was. Answers whether a new one is kept.
    @discardableResult
    func refreshKit(client: TillClient, federationId: String, tillURL: String) async -> Bool {
        guard core != nil, let url = kitURL, tillURL == currentTill else { return false }
        let fetched = await client.offlineKit(phoneId: phoneId)
        let now = clock()
        // The pairing may have changed while the till answered.
        guard tillURL == currentTill else { return false }
        guard case .ok(let data) = fetched, let text = String(data: data, encoding: .utf8), let info = KitInfo.parse(data, fetchedAtMs: now),
              info.phoneId == phoneId, info.federationId.lowercased() == federationId.lowercased() else {
            kitFailedAtMs = now
            return false
        }
        let file: [String: Any] = ["till": tillURL, "fetched_at_ms": now, "answer": text]
        let saved: Bool = await onQueue {
            guard let bytes = try? JSONSerialization.data(withJSONObject: file) else { return false }
            return (try? bytes.write(to: url, options: [.atomic, .completeFileProtection])) != nil
        }
        guard saved else { kitFailedAtMs = now; return false }
        guard tillURL == currentTill else { return false }
        kit = info
        kitAnswer = text
        kitTill = tillURL
        kitRefreshWanted = false
        kitFailedAtMs = nil
        snapshot.kit = info
        return true
    }

    // MARK: charge

    /// Takes `priceMsat` from the card for the sale `clientRef`, on the phone. `pin` is wiped
    /// before the core is called; it crosses to the core as a `String` for that one call.
    /// `renewKit`: fetches the kit from the till when it answers, for a held sale whose kit has
    /// expired (the core refuses an expired kit before it finishes anything).
    func charge(card: BlockingCardChannel, priceMsat: Int64, clientRef: String, pin: Pin?,
                renewKit: (() async -> Void)? = nil) async -> ChargeResult {
        let started = clock()
        guard let core else {
            pin?.wipe()
            return .declined(code: .offlineNotReady, detail: disabledReason ?? "offline is off on this phone", httpStatus: 0)
        }
        if snapshot.localOnlyRefs.contains(clientRef), let renewKit,
           kit.map({ max(started, $0.issuedAtMs) >= $0.expiresAtMs }) ?? true {
            await renewKit()
        }
        let kitJson = kitAnswer ?? ""
        let pinText = pin.map { p in p.withBytes { String(decoding: $0, as: UTF8.self) } } ?? ""
        pin?.wipe()
        let out = await onQueue { core.charge(card: card, kitJson: kitJson, priceMsat: priceMsat, clientRef: clientRef, pin: pinText) }
        switch out {
        case .threw: heldRefs.insert(clientRef)
        case .refused(let code, _, _, _) where code == ChargeErrorCode.undecided.rawValue: heldRefs.insert(clientRef)
        default: heldRefs.remove(clientRef)
        }
        if case .refused(let code, _, _, _) = out,
           [ChargeErrorCode.offlineKitExpired, .offlinePoolExhausted, .offlineCapReached].map(\.rawValue).contains(code) {
            kitRefreshWanted = true
        }
        await refresh(failClosed: clientRef)
        return Self.result(out, durationMs: clock() - started)
    }

    /// The core's answer as the till's would be.
    nonisolated static func result(_ out: CoreOutcome, durationMs: Int64) -> ChargeResult {
        switch out {
        case .paid(let txid, let price, let till, let overpay, let fees, let inputs, let change, _):
            return .success(ChargeReceipt(saleId: 0, txid: txid, priceMsat: price, receivedMsat: till, overpaidMsat: overpay,
                                          feesMsat: fees, inputs: inputs, change: change, collectionPending: true,
                                          warnings: [], durationMs: durationMs, offline: true))
        case .refused(let code, let detail, let suggested, let tries):
            var c = ChargeErrorCode.fromWire(code)
            // A code this app does not know is the card's generic error (docs/POS-OFFLINE.md).
            if c == .unknown { c = .cardError }
            let d = c == .wrongPin ? tries.map { "wrong PIN; \($0) tries left before the card blocks" } ?? detail : detail
            return .declined(code: c, detail: d, httpStatus: 0, suggestedMsat: suggested.flatMap { $0 > 0 ? $0 : nil })
        case .threw(let m):
            return .declined(code: .undecided, detail: "the phone's offline core failed: \(m)", httpStatus: 0)
        }
    }

    // MARK: sync

    struct SyncOutcome: Equatable {
        /// Charges the till took (`queued` or `already`).
        var synced = 0
        /// A drain found nothing waiting: the sync is done.
        var drainedEmpty = false
    }

    /// Sends what waits here to the till `tillURL` (`client` asks that till only): up to
    /// `syncRounds` rounds of `drain(50)`, its body to `POST /v1/offline/charges`, the till's 200
    /// answer to `mark_synced`, the refusals of a till that does not know the charges made
    /// retries first (`keepForTheirTill`). Done at an empty drain, which clears `syncStopped`;
    /// ends at another till paired meanwhile. Stops short at an HTTP failure, a round the till
    /// took none of, charges kept for their till or a store failure: `syncStopped` says why, and
    /// the syncs of the next minute return at once, draining and posting nothing. One at a time.
    func sync(client: TillClient, tillURL: String) async -> SyncOutcome {
        guard let core, !syncing, clock() >= syncRetryAt else { return SyncOutcome() }
        syncing = true
        defer { syncing = false }
        var out = SyncOutcome()
        var last: [String] = []
        rounds: for _ in 0..<Self.syncRounds {
            let d: CoreDrain
            switch await onQueue({ Result { try core.drain(max: Self.syncBatch) } }) {
            case .success(let drained): d = drained
            case .failure(let e): stopSync("\(e)"); break rounds
            }
            if d.txids.isEmpty {
                out.drainedEmpty = true
                syncStopped = nil
                break
            }
            if d.txids == last { stopSync(Self.noneTaken(d.txids.count)); break }
            last = d.txids
            // Another till paired meanwhile: nothing goes to it.
            guard tillURL == currentTill else { break }
            let answer: String
            switch await client.offlineChargesReply(d.body) {
            case .failure(let e): stopSync(Self.syncFailure(e)); break rounds
            case .success(let r) where r.status != 200: stopSync("the till refused the sync (HTTP \(r.status))"); break rounds
            case .success(let r): answer = String(decoding: r.body, as: UTF8.self)
            }
            let (forCore, kept) = Self.keepForTheirTill(answer)
            let report: CoreSyncReport
            switch await onQueue({ Result { try core.markSynced(answerJson: forCore) } }) {
            case .success(let r): report = r
            case .failure(let e): stopSync("\(e)"); break rounds
            }
            out.synced += report.synced
            // The next drain starts with them again, and this till's answer would not change.
            if kept > 0 {
                stopSync(kept == 1 ? "1 payment waits for the till it was taken for" : "\(kept) payments wait for the till they were taken for")
                break
            }
            if report.synced + report.refused == 0 { stopSync(Self.noneTaken(d.txids.count)); break }
        }
        if out.synced > 0 { kitRefreshWanted = true }
        await refresh()
        return out
    }

    /// A sync that stopped short: Settings says why, and none runs for a minute.
    private func stopSync(_ why: String) {
        syncStopped = why
        syncRetryAt = clock() + Self.syncRetryAfterMs
    }

    /// A round that moved nothing: every charge in it waits for a later sync, as the till asked.
    private static func noneTaken(_ count: Int) -> String { "the till took none of \(count) (it asked for them again later)" }

    /// Settings' words for a `POST /v1/offline/charges` that got no answer from the till.
    private static func syncFailure(_ e: TillError) -> String {
        switch e {
        case .unreachable: return "the till did not answer"
        case .timeout: return "the till took too long"
        case .interrupted: return "the connection broke off"
        case .badResponse(let detail): return detail
        case .notPaired: return "no till is paired"
        case .unauthorized: return "the till refused this phone"
        }
    }

    /// `answerJson`, the till's 200 to `POST /v1/offline/charges`, with each refusal of a till that
    /// does not know the charge made a retry, and how many it made one: `pool_entry_unknown`,
    /// `bad_note` and `bad_charge` with `retry` false or absent (pool entries it never issued to
    /// this phone, notes of another federation; docs/TILL-API.md). The core checked a charge's
    /// notes against the kit it was taken with, and its pool entries stay that kit's till's until
    /// their redeem window ends, so those refusals come from another till, paired since. The core
    /// would keep them refused for good; they wait instead, and sync once the phone is paired with
    /// their till again. `pool_entry_reused`, `pool_entry_expired` and `bad_signature` stay final.
    /// An answer that is not JSON, or has no `results`, goes to the core as it came (which refuses
    /// it), as does one with nothing to keep.
    nonisolated static func keepForTheirTill(_ answerJson: String) -> (String, Int) {
        let notTheirs: Set<String> = ["pool_entry_unknown", "bad_note", "bad_charge"]
        guard var answer = (try? JSONSerialization.jsonObject(with: Data(answerJson.utf8))) as? [String: Any],
              var results = answer["results"] as? [Any] else { return (answerJson, 0) }
        var kept = 0
        for (i, item) in results.enumerated() {
            guard var r = item as? [String: Any], (r["status"] as? String) == "refused",
                  let code = r["code"] as? String, notTheirs.contains(code), noRetry(r["retry"]) else { continue }
            r["retry"] = true
            results[i] = r
            kept += 1
        }
        guard kept > 0 else { return (answerJson, 0) }
        answer["results"] = results
        guard let data = try? JSONSerialization.data(withJSONObject: answer, options: [.withoutEscapingSlashes]) else { return (answerJson, 0) }
        return (String(decoding: data, as: UTF8.self), kept)
    }

    /// `retry` absent, or JSON `false`.
    private nonisolated static func noRetry(_ v: Any?) -> Bool {
        guard let v else { return true }
        guard let n = v as? NSNumber, CFGetTypeID(n) == CFBooleanGetTypeID() else { return false }
        return !n.boolValue
    }

    /// Deletes the record of a charge the till refused (the card paid; this is its only record).
    func dismiss(txid: String) async {
        guard let core else { return }
        _ = await onQueue { try? core.dismiss(txid: txid) }
        await refresh()
    }
}
