import SwiftUI

@main
struct FedimintCardPOSApp: App {
    @StateObject private var pos = PosController()
    @Environment(\.scenePhase) private var phase

    var body: some Scene {
        WindowGroup {
            RootView()
                .environmentObject(pos)
                .preferredColorScheme(.dark)
        }
        .onChange(of: phase) { _, p in
            switch p {
            case .active: pos.start(); UIApplication.shared.isIdleTimerDisabled = true
            case .background: pos.send(.backgrounded); pos.stop()
            default: break
            }
        }
    }
}

struct RootView: View {
    @EnvironmentObject var pos: PosController
    @Environment(\.scenePhase) private var phase
    @State private var showSettings = false

    private var pinShowing: Bool { if case .enteringPin = pos.state { return true }; return false }

    var body: some View {
        ZStack {
            AppBackground()
            screen
                .padding(.horizontal, 16)
                .padding(.vertical, 8)
            // The app-switcher snapshot is taken while inactive: never let it hold the PIN screen.
            if pinShowing && phase != .active {
                PrivacyCover()
            }
        }
        .sheet(isPresented: $showSettings) { SettingsView().environmentObject(pos) }
    }

    /// The four tap states share one TapScreen, so waiting to working is one view changing (the
    /// motion morphs, the card settles) rather than one screen replacing another.
    private var tap: (title: String, status: String, cancellable: Bool, working: Bool)? {
        switch pos.state {
        case .waitingForCard(let price, _, _): return ("\(formatSats(price)) sats", "Hold the card to the top of the phone", true, false)
        case .signing(let price, _, _, _): return ("\(formatSats(price)) sats", "Paying… keep the card on the phone", false, true)
        case .balanceWaiting: return ("Card balance", "Hold the card to the top of the phone", true, false)
        case .balanceReading: return ("Card balance", "Reading… keep the card on the phone", true, true)
        case .topUpWaitingForCard(let t): return ("\(formatSats(t.amountMsat)) sats", "Hold the card to load \(formatSats(t.amountMsat)) sats", true, false)
        case .topUpLoading(_, let t): return ("\(formatSats(t.amountMsat)) sats", "Loading… keep the card on the phone", false, true)
        default: return nil
        }
    }

    @ViewBuilder private var screen: some View {
        if let t = tap {
            TapScreen(title: t.title, status: t.status, cancellable: t.cancellable, working: t.working)
        } else {
            other
        }
    }

    @ViewBuilder private var other: some View {
        switch pos.state {
        case .entering: AmountScreen(openSettings: { showSettings = true })
        case .enteringPin(let price, _, let wrong, let left, _): PinScreen(priceMsat: price, wrongPin: wrong, triesLeft: left)
        case .waitingForCard, .signing, .balanceWaiting, .balanceReading, .topUpWaitingForCard, .topUpLoading: EmptyView()
        case .topUpEntering: TopUpScreen(creating: false)
        case .topUpCreating: TopUpScreen(creating: true)
        case .topUpInvoice(let t, _): InvoiceScreen(topUp: t)
        case .topUpLoaded(let msat, let txid): ReceiptScreen(headline: "Loaded \(formatSats(msat)) sats", txid: txid)
        case .topUpFailed(let f): TopUpErrorScreen(failure: f)
        case .receipt(let r): ReceiptScreen(receipt: r)
        case .error(let e): ErrorScreen(info: e)
        case .sales: SalesScreen()
        case .balanceShown(_, let live): BalanceScreen(liveMsat: live)
        case .balanceFailed(let p, _): BalanceErrorScreen(problem: p)
        }
    }
}

struct PrivacyCover: View {
    var body: some View {
        ZStack {
            AppBackground()
            BrandMark().frame(width: 100, height: 76)
        }
    }
}
