import Foundation
import SwiftUI

enum TillHealth: Equatable { case ok, offline, notPaired, refused }

/// Runs the flow's effects: the NFC session, the card link, the till calls and the timers.
@MainActor
final class PosController: ObservableObject {
    let flow = PosFlow()
    let store: PairingStore
    private lazy var client = TillClient(pairing: { [weak self] in self?.pairing }, transport: URLSessionTransport())
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

    private var failures = 0
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

    init(store: PairingStore = KeychainPairingStore()) {
        self.store = store
        pairing = store.load()
        health = pairing == nil ? .notPaired : .ok
    }

    // MARK: pairing

    func pair(_ p: TillPairing) {
        store.save(p)
        pairing = p
        failures = 0
        status = nil
        health = .ok
        pollNow()
    }

    func forgetTill() {
        store.clear()
        pairing = nil
        status = nil
        health = .notPaired
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
        pollTask?.cancel()
        pollTask = Task { [weak self] in
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
        if case .receipt = now { pollNow() }
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
        case .startCharge(let cid, let price, let ref, let pin):
            let linkId = link?.id
            Task { [weak self] in
                guard let self else { pin?.wipe(); return }
                let r = await self.client.charge(priceMsat: price, clientRef: ref, pin: pin, cardLink: linkId) // wipes pin
                switch r {
                case .success(let rc): self.endCardJob(message: "Paid \(formatSats(rc.priceMsat)) sats", ok: true)
                case .declined(let code, _, _) where code == .wrongPin: self.endCardJob(message: "Wrong PIN", ok: false)
                default: self.endCardJob(message: "Payment didn't go through", ok: false)
                }
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

    // MARK: card session + link

    private func beginCardSession(_ purpose: CardPurpose) {
        guard let p = pairing, let url = p.linkURL else {
            send(.linkFailed(.notPaired)); return
        }
        sessionGen += 1
        let gen = sessionGen
        reader.onCard = { [weak self] channel in
            Task { @MainActor in await self?.cardConnected(channel, gen: gen, url: url, token: p.token, purpose: purpose) }
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

    private func cardConnected(_ channel: CardChannel, gen: Int, url: URL, token: String?, purpose: CardPurpose) async {
        guard gen == sessionGen else { return }
        // The system's NFC sheet covers the lower half of the screen, so it carries the status too.
        switch purpose {
        case .charge: reader.update(alert: "Paying… keep the card on the phone")
        case .balance: reader.update(alert: "Reading… keep the card on the phone")
        case .topUp: reader.update(alert: "Loading… keep the card on the phone")
        }
        let l: CardLink
        do {
            l = try await CardLink.open(URLSessionLinkSocket(url: url, token: token))
        } catch {
            guard gen == sessionGen else { return }
            let te: TillError = (error as? LinkError) == .unauthorized ? .unauthorized : .unreachable
            reader.finish(error: te == .unauthorized ? "The till refused this phone" : "Can't reach the till")
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
        guard pairing != nil else { health = .notPaired; return }
        if case .signing = state { return }
        if case .topUpLoading = state { return }
        guard !pollInFlight else { return }
        pollInFlight = true
        Task { [weak self] in
            guard let self else { return }
            let r = await self.client.status()
            self.pollInFlight = false
            switch r {
            case .ok(let s):
                self.failures = 0; self.status = s; self.health = .ok
            case .err(.unauthorized):
                self.health = .refused
            case .err(.notPaired):
                self.health = .notPaired
            case .err:
                self.failures += 1
                if self.failures >= 2 { self.health = .offline }
            }
        }
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
