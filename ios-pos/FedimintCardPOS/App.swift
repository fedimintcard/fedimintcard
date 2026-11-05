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

    @ViewBuilder private var screen: some View {
        switch pos.state {
        case .entering: AmountScreen(openSettings: { showSettings = true })
        case .enteringPin(let price, _, let wrong, let left, _): PinScreen(priceMsat: price, wrongPin: wrong, triesLeft: left)
        case .waitingForCard(let price, _, _): TapScreen(title: "\(formatSats(price)) sats", status: "Hold the card to the top of the phone", cancellable: true)
        case .signing(let price, _, _, _):
            TapScreen(title: "\(formatSats(price)) sats", status: pos.elapsed == 0 ? "Card present" : "Signing… \(pos.elapsed)s", cancellable: false)
        case .balanceWaiting: TapScreen(title: "Card balance", status: "Hold the card to the phone", cancellable: true)
        case .balanceReading: TapScreen(title: "Card balance", status: "Reading card…", cancellable: true)
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
