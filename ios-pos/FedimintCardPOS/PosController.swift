import Foundation
import SwiftUI

enum TillHealth: Equatable { case ok, offline, notPaired, refused }

/// Runs the flow's effects: the NFC session, the card link, the till calls, the phone's offline
/// side (docs/POS-OFFLINE.md) and the timers.
@MainActor
final class PosController: ObservableObject {
    let flow = PosFlow()
    let store: PairingStore
    let offline: OfflinePhone
    private let transport: HTTPTransport = URLSessionTransport()
    private lazy var client = TillClient(pairing: { [weak self] in self?.pairing }, transport: transport)
    private let reader = NFCCardReader()

    @Published private(set) var state: PosState = .entering
    @Published private(set) var amountDisplay = "0"
    @Published private(set) var chargeable = false
    @Published private(set) var topUpDisplay = "0"
    @Published private(set) var topUpChargeable = false
    /// The clock the invoice's time left is shown against; ticks while an invoice is on screen.
    @Published private(set) var nowMs: Int64 = PosController.clockMs()
    @Published private(set) var pinLength = 0
    @Published private(set) var pinComplete = false
    @Published private(set) var status: TillStatus?
    @Published private(set) var health: TillHealth = .ok
    @Published private(set) var sales: TillResult<SalesList>?
    @Published private(set) var balanceCountdown = PosFlow.balanceShownSeconds
    @Published private(set) var elapsed = 0
    @Published private(set) var pairing: TillPairing?
    /// The phone's offline side as the screens show it. After a sync it is published with the
    /// till's status read again, so the till's queue and the phone's move together.
    @Published private(set) var phone = OfflineSnapshot.disabled

    private var failures = 0
    /// Send what waits on the phone at the next poll that reaches the till: set on foreground, on
    /// every receipt and on reconnect; cleared only by a sync that finds nothing waiting.
    private var syncWanted = true
    private var pollTask: Task<Void, Never>?
    private var salesPollTask: Task<Void, Never>?
    private var countdownTask: Task<Void, Never>?
    private var tickTask: Task<Void, Never>?
    private var invoiceTask: Task<Void, Never>?

    static func clockMs() -> Int64 { Int64(Date().timeIntervalSince1970 * 1000) }
    static let invoicePollNanos: UInt64 = 1_500_000_000

    // The card job in progress.
    private var sessionGen = 0
    private var link: CardLink?
    private var serveTask: Task<Void, Never>?
    /// The card in this NFC session, for a charge the phone takes itself.
    private var cardChannel: CardChannel?
    private var localTransport: NFCApduTransport?

    /// `offline`: the phone's offline side (nil: the app's own store).
    init(store: PairingStore = KeychainPairingStore(), offline: OfflinePhone? = nil) {
        self.store = store
        self.offline = offline ?? OfflinePhone()
        pairing = store.load()
        health = pairing == nil ? .notPaired : .ok
        self.offline.tillChanged(to: pairing?.baseURL)
    }

    // MARK: pairing

    /// Payments the phone holds for the paired till: waiting to sync, or left signing by a card.
    var paymentsOnPhone: Int { phone.stats.waiting + phone.stats.signing }

    /// Pairing `p` leaves what the phone holds for another till unsyncable: ask first.
    func pairingStrandsPayments(_ p: TillPairing) -> Bool {
        strandsPayments(onPhone: paymentsOnPhone, pairedURL: pairing?.baseURL, kitTillURL: offline.kitTillURL, newURL: p.baseURL)
    }

    func pair(_ p: TillPairing) {
        // First: a kit from another till never outlives the pairing it came with.
        offline.tillChanged(to: p.baseURL)
        store.save(p)
        pairing = p
        failures = 0
        status = nil
        health = .ok
        syncWanted = true
        phone = offline.snapshot
        pollNow()
    }

    /// The kit stays (it is this till's): a held sale can still be finished, and pairing this till
    /// again goes on with it.
    func forgetTill() {
        offline.tillChanged(to: nil)
        store.clear()
        pairing = nil
        status = nil
        health = .notPaired
    }

    /// Deletes the record of a charge the till refused at sync.
    func dismissRefused(_ txid: String) {
        Task { [weak self] in
            guard let self else { return }
            await self.offline.dismiss(txid: txid)
            self.phone = self.offline.snapshot
        }
    }

    // MARK: keys

    func key(_ k: Int?) {
        let pad: AmountPad
        switch state {
        case .entering: pad = flow.pad
        case .topUpEntering: pad = flow.topUpPad
        default: return
        }
        switch k { case nil: pad.backspace(); case .some(-1): pad.clear(); case let d?: pad.press(d) }
        syncPads()
    }

    /// PIN digits go to the pad only while the PIN screen shows; nothing about them is logged.
    func pinKey(_ k: Int?) {
        guard case .enteringPin = state else { return }
        switch k { case nil: flow.pinPad.backspace(); case .some(-1): flow.pinPad.clear(); case let d?: flow.pinPad.press(d) }
        syncPads()
    }

    private func syncPads() {
        amountDisplay = flow.pad.display
        chargeable = flow.pad.isChargeable
        topUpDisplay = flow.topUpPad.display
        topUpChargeable = flow.topUpPad.isChargeable
        pinLength = flow.pinPad.length
        pinComplete = flow.pinPad.isComplete
    }

    // MARK: lifecycle

    func start() {
        syncPads()
        syncWanted = true
        pollTask?.cancel()
        pollTask = Task { [weak self] in
            // The store opens once per process; a failure is tried again at the next foreground.
            await self?.offline.start()
            if let self { self.phone = self.offline.snapshot }
            while !Task.isCancelled {
                self?.pollNow()
                try? await Task.sleep(nanoseconds: 5_000_000_000)
            }
        }
        if flow.unresolvedRef != nil { scheduleSalesPoll() }
    }

    func stop() {
        pollTask?.cancel(); pollTask = nil
        salesPollTask?.cancel(); salesPollTask = nil
    }

    // MARK: dispatch

    func send(_ e: PosEvent) {
        flow.offline = offline.snapshot
        let before = flow.state
        let refBefore = flow.unresolvedRef
        let effects = flow.dispatch(e)
        let now = flow.state
        state = now
        syncPads()
        if case .signing = now, !isSigning(before) { startTick() }
        if case .sales = now, !isSales(before) { loadSales() }
        let refNow = flow.unresolvedRef
        if refNow == nil { salesPollTask?.cancel(); salesPollTask = nil } else if refNow != refBefore { scheduleSalesPoll() }
        // After every charge (the till's or the phone's), sync what waits here.
        if case .receipt = now { syncWanted = true; pollNow() }
        if case .topUpLoaded = now { pollNow() }
        if case .topUpInvoice(let t, _) = now {
            if case .topUpInvoice(let b, _) = before, b.topupId == t.topupId {} else { watchInvoice(t) }
        }
        // The waiting top-up arriving changes the state, not the read: the countdown keeps going.
        if case .balanceShown(let rid, _, _) = now {
            if case .balanceShown(let was, _, _) = before, was == rid {} else { startCountdown(rid) }
        }
        for fx in effects { run(fx) }
    }

    private func isSigning(_ s: PosState) -> Bool { if case .signing = s { return true }; return false }
    private func isSales(_ s: PosState) -> Bool { if case .sales = s { return true }; return false }

    private func run(_ fx: PosEffect) {
        switch fx {
        case .startCardSession(let purpose): beginCardSession(purpose)
        case .endCardSession: endCardJob(message: nil, ok: false, cancel: true)
        case .failCardSession(let message): endCardJob(message: message, ok: false)
        case .startCharge(let cid, let price, let ref, let pin):
            let linkId = link?.id
            Task { [weak self] in
                guard let self else { pin?.wipe(); return }
                let r = await self.client.charge(priceMsat: price, clientRef: ref, pin: pin, cardLink: linkId) // wipes pin
                self.finishCharge(r)
                self.send(.chargeDone(chargeId: cid, result: r))
            }
        case .startLocalCharge(let cid, let price, let ref, let pin):
            guard let channel = cardChannel else {
                pin?.wipe()
                Task { [weak self] in
                    self?.send(.chargeDone(chargeId: cid, result: .declined(code: .noCard, detail: "no card in the NFC session", httpStatus: 0)))
                }
                return
            }
            let card = NFCApduTransport(channel)
            localTransport = card
            let till = pairing
            Task { [weak self] in
                guard let self else { pin?.wipe(); return }
                let r = await self.offline.charge(card: card, priceMsat: price, clientRef: ref, pin: pin) { [weak self] in
                    await self?.renewKit(till)
                } // wipes pin
                if self.localTransport === card { self.localTransport = nil }
                self.finishCharge(r)
                self.phone = self.offline.snapshot
                self.send(.chargeDone(chargeId: cid, result: r))
            }
        case .createTopUp(let cid, let msat, let ref, let card):
            Task { [weak self] in
                guard let self else { return }
                let r = await self.client.createTopUp(amountMsat: msat, clientRef: ref, shortId: card)
                self.send(.topUpCreated(createId: cid, result: r))
            }
        case .fetchWaiting(let rid, let sid):
            Task { [weak self] in
                guard let self else { return }
                let r = await self.client.waitingTopUps(shortId: sid)
                self.send(.waitingFetched(readId: rid, topUps: r))
            }
        case .loadTopUp(let lid, let tid):
            let linkId = link?.id
            Task { [weak self] in
                guard let self else { return }
                let r = await self.client.loadTopUp(tid, cardLink: linkId)
                if case .ok(let st) = r, st.state == "loaded" { self.endCardJob(message: "Loaded \(formatSats(st.amountMsat)) sats", ok: true) }
                else { self.endCardJob(message: "Not loaded", ok: false) }
                self.send(.topUpLoadDone(loadId: lid, result: r))
            }
        case .startBalanceRead(let rid):
            let linkId = link?.id
            Task { [weak self] in
                guard let self else { return }
                let r = await self.client.cardBalance(cardLink: linkId)
                if case .read(let b) = r { self.endCardJob(message: "\(formatSats(b.liveMsat)) sats on this card", ok: true) }
                else { self.endCardJob(message: "Couldn't read the card", ok: false) }
                self.send(.balanceDone(readId: rid, result: r))
            }
        }
    }

    /// The NFC sheet's last word on a charge, the till's or the phone's ("Paid offline · N sats").
    private func finishCharge(_ r: ChargeResult) {
        switch r {
        case .success(let rc): endCardJob(message: receiptHeadline(rc), ok: true)
        case .declined(let code, _, _, _) where code == .wrongPin: endCardJob(message: "Wrong PIN", ok: false)
        default: endCardJob(message: "Payment didn't go through", ok: false)
        }
    }

    /// A client that asks the till `p` only, whatever is paired by the time it asks.
    private func client(for p: TillPairing) -> TillClient { TillClient(pairing: { p }, transport: transport) }

    /// A held sale's kit has expired: a new one, if the till answers now.
    private func renewKit(_ p: TillPairing?) async {
        guard let p else { return }
        let c = client(for: p)
        guard case .ok(let s) = await c.status() else { return }
        await offline.refreshKit(client: c, federationId: s.federationId, tillURL: p.baseURL)
    }

    // MARK: card session + link

    private func beginCardSession(_ purpose: CardPurpose) {
        let url = pairing?.linkURL
        // A sale held on the phone is taken there with no link; every other card job needs the till.
        guard purpose == .chargeLocal || url != nil else {
            send(.linkFailed(.notPaired)); return
        }
        sessionGen += 1
        let gen = sessionGen
        let token = pairing?.token
        reader.onCard = { [weak self] channel in
            Task { @MainActor in await self?.cardConnected(channel, gen: gen, url: url, token: token, purpose: purpose) }
        }
        reader.onEnd = { [weak self] hadCard in
            Task { @MainActor in self?.sessionEnded(gen: gen, hadCard: hadCard) }
        }
        if case .topUpWaitingForCard(let t) = flow.state {
            reader.begin(alert: "Hold the card to the top of the phone to load \(formatSats(t.amountMsat)) sats")
        } else {
            reader.begin(alert: "Hold the card to the top of the phone")
        }
    }

    private func waitingForCard() -> Bool {
        switch flow.state { case .waitingForCard, .balanceWaiting, .topUpWaitingForCard: return true; default: return false }
    }

    private func cardConnected(_ channel: CardChannel, gen: Int, url: URL?, token: String?, purpose: CardPurpose) async {
        guard gen == sessionGen else { return }
        // The system's NFC sheet covers the lower half of the screen, so it carries the status too.
        switch purpose {
        case .charge, .chargeLocal: reader.update(alert: "Paying… keep the card on the phone")
        case .balance: reader.update(alert: "Reading… keep the card on the phone")
        case .topUp: reader.update(alert: "Loading… keep the card on the phone")
        }
        // Kept for a charge the phone takes itself: a held sale, or a till out of reach.
        cardChannel = channel
        if purpose == .chargeLocal {
            if waitingForCard() { send(.cardPresent) }
            return
        }
        guard let url else { return }
        let l: CardLink
        do {
            l = try await CardLink.open(URLSessionLinkSocket(url: url, token: token))
        } catch {
            guard gen == sessionGen else { return }
            let te: TillError = (error as? LinkError) == .unauthorized ? .unauthorized : .unreachable
            // A charge keeps the NFC session open: the flow takes the card on the phone, or ends it
            // with the message. A balance read or a top-up ends it here.
            if purpose != .charge {
                reader.finish(error: te == .unauthorized ? "The till refused this phone" : "Can't reach the till")
            }
            send(.linkFailed(te))
            return
        }
        guard gen == sessionGen, waitingForCard() else { l.close(); return }
        link = l
        serveTask = Task.detached { await l.serve(card: channel) }
        send(.cardPresent)
    }

    private func sessionEnded(gen: Int, hadCard: Bool) {
        guard gen == sessionGen else { return }
        link?.markCardGone()
        // A charge on the phone answers no_card, or undecided once the card has signed.
        localTransport?.cancel()
        if link == nil && waitingForCard() { send(.cardSessionEnded) }
    }

    /// Ends the job's NFC session and link. `message` shows on the NFC sheet as it closes.
    private func endCardJob(message: String?, ok: Bool, cancel: Bool = false) {
        sessionGen += 1
        link?.markCardGone()
        link?.close()
        link = nil
        serveTask?.cancel()
        serveTask = nil
        localTransport?.cancel()
        localTransport = nil
        cardChannel = nil
        if cancel || message == nil { reader.cancel() }
        else if ok { reader.finish(success: message!) }
        else { reader.finish(error: message!) }
    }

    // MARK: timers and polls

    private func startTick() {
        elapsed = 0
        tickTask?.cancel()
        tickTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 1_000_000_000)
                guard let self, case .signing = self.state else { return }
                self.elapsed += 1
            }
        }
    }

    /// While `t`'s invoice is on screen: the clock every second for the time left, and the till
    /// asked every 1.5 s whether it is paid. Stops when another screen (or invoice) takes over.
    private func watchInvoice(_ t: TopUp) {
        invoiceTask?.cancel()
        nowMs = Self.clockMs()
        invoiceTask = Task { [weak self] in
            var sincePoll: UInt64 = 0
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 500_000_000)
                sincePoll += 500_000_000
                guard let self, case .topUpInvoice(let cur, _) = self.state, cur.topupId == t.topupId else { return }
                self.nowMs = Self.clockMs()
                if sincePoll >= Self.invoicePollNanos {
                    sincePoll = 0
                    let r = await self.client.topUpStatus(t.topupId)
                    guard case .topUpInvoice(let still, _) = self.state, still.topupId == t.topupId else { return }
                    self.send(.topUpPolled(topupId: t.topupId, result: r))
                }
            }
        }
    }

    private func startCountdown(_ rid: Int) {
        balanceCountdown = PosFlow.balanceShownSeconds
        countdownTask?.cancel()
        countdownTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 1_000_000_000)
                guard let self, case .balanceShown(let cur, _, _) = self.state, cur == rid else { return }
                self.balanceCountdown -= 1
                if self.balanceCountdown <= 0 { self.send(.balanceExpired(readId: rid)); return }
            }
        }
    }

    private var pollInFlight = false

    func pollNow() {
        guard let till = pairing else { health = .notPaired; return }
        if case .signing = state { return }
        if case .topUpLoading = state { return }
        guard !pollInFlight else { return }
        pollInFlight = true
        Task { [weak self] in
            guard let self else { return }
            let r = await self.client.status()
            switch r {
            case .ok(let s):
                // Back after the till was out of reach: sync what the phone took meanwhile.
                if self.failures > 0 || self.health != .ok { self.syncWanted = true }
                self.failures = 0; self.status = s; self.health = .ok
                await self.offlineWork(status: s, till: till)
            case .err(.unauthorized):
                self.health = .refused
            case .err(.notPaired):
                self.health = .notPaired
            case .err:
                self.failures += 1
                if self.failures >= 2 { self.health = .offline }
            }
            self.pollInFlight = false
        }
    }

    /// Off a status poll that reached the till `till`: the kit when it is due, then what waits on
    /// the phone, both with that till only. After a sync the status is read again and published
    /// with the phone's snapshot.
    private func offlineWork(status s: TillStatus, till: TillPairing) async {
        let c = client(for: till)
        if offline.kitRefreshDue() {
            await offline.refreshKit(client: c, federationId: s.federationId, tillURL: till.baseURL)
        }
        guard syncWanted else { phone = offline.snapshot; return }
        let out = await offline.sync(client: c, tillURL: till.baseURL)
        if out.drainedEmpty { syncWanted = false }
        guard out.synced > 0 else { phone = offline.snapshot; return }
        // A sync that synced asks for a new kit.
        if offline.kitRefreshDue() {
            await offline.refreshKit(client: c, federationId: s.federationId, tillURL: till.baseURL)
        }
        // The till holds them now: its count and the phone's move together, or not at all.
        guard case .ok(let fresh) = await c.status(), pairing == till else { return }
        status = fresh
        phone = offline.snapshot
    }

    /// While a sale's outcome is unknown, look for its client_ref in the sales ledger every 3 s.
    private func scheduleSalesPoll() {
        salesPollTask?.cancel()
        salesPollTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 3_000_000_000)
                guard let self, !Task.isCancelled, let ref = self.flow.unresolvedRef else { return }
                if case .ok(let list) = await self.client.sales(), !Task.isCancelled,
                   let hit = list.sales.first(where: { $0.clientRef == ref }), self.flow.unresolvedRef == ref {
                    self.send(.saleFound(hit))
                    return
                }
            }
        }
    }

    func loadSales() {
        sales = nil
        Task { [weak self] in
            guard let self else { return }
            self.sales = await self.client.sales()
        }
    }
}
