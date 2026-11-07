import SwiftUI
import CoreImage.CIFilterBuiltins

private func keyLabel(_ k: Int?) -> String {
    switch k { case nil: return "⌫"; case .some(-1): return "C"; case let d?: return String(d) }
}

/// 3×4 keypad: 1–9, then C, 0, ⌫.
struct Keypad: View {
    let onKey: (Int?) -> Void
    private let rows: [[Int?]] = [[1, 2, 3], [4, 5, 6], [7, 8, 9], [-1, 0, nil]]
    var body: some View {
        VStack(spacing: 10) {
            ForEach(0..<4, id: \.self) { r in
                HStack(spacing: 10) {
                    ForEach(0..<3, id: \.self) { c in
                        let k = rows[r][c]
                        Button(keyLabel(k)) { onKey(k) }
                            .buttonStyle(KeyButtonStyle())
                            .accessibilityLabel(k == nil ? "Delete" : k == -1 ? "Clear" : keyLabel(k))
                    }
                }
            }
        }
    }
}

struct AmountScreen: View {
    @EnvironmentObject var pos: PosController
    let openSettings: () -> Void

    private var problem: String? {
        switch pos.health {
        case .ok: return nil
        case .offline: return "Till offline"
        case .notPaired: return "No till paired"
        case .refused: return "Till refused this phone"
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 0) {
                HStack(spacing: 8) {
                    BrandMark().frame(width: 34, height: 26)
                    HStack(spacing: 4) {
                        Text("fedimint").font(Fig.extraBold(20)).kerning(-0.4).foregroundStyle(Palette.ink)
                        Text("card").font(Fig.regular(20)).foregroundStyle(Palette.mist)
                    }
                }
                .accessibilityElement(children: .ignore)
                .accessibilityLabel("fedimint card")
                Spacer()
                Button("SALES") { pos.send(.salesOpened) }
                    .font(Fig.medium(13)).kerning(1.5).foregroundStyle(Palette.goldHi)
                    .frame(minWidth: 48, minHeight: 44).padding(.horizontal, 8)
                Button(action: openSettings) {
                    Image(systemName: "gearshape").font(.system(size: 18, weight: .medium))
                }
                .foregroundStyle(Palette.goldHi).frame(minWidth: 48, minHeight: 44)
                .accessibilityLabel("Settings")
            }
            Text(pos.status?.federationName ?? "Fedimint Card POS")
                .font(Fig.extraBold(22)).kerning(-0.4).foregroundStyle(Palette.ink).lineLimit(1)
                .padding(.top, 16)
                .onLongPressGesture(perform: openSettings)
            Text(pos.status.map { "Till balance: \(formatSats($0.walletBalanceMsat)) sats" } ?? " ")
                .font(Fig.regular(14)).foregroundStyle(Palette.dim).padding(.top, 2)
            if let waiting = pos.status.flatMap({ waitingToSyncText($0.offlineQueued) }) {
                Text(waiting).font(Fig.regular(14)).foregroundStyle(Palette.dim).padding(.top, 1)
                    .accessibilityIdentifier("waiting-to-sync")
            }
            if let problem {
                Button(action: openSettings) {
                    Text(problem.uppercased()).font(Fig.medium(11)).kerning(1.3)
                        .foregroundStyle(Palette.bad)
                        .padding(.horizontal, 12).frame(minHeight: 32)
                        .background(Capsule().fill(Color(hex: 0xf87171, alpha: 0x26 / 255.0)))
                        .overlay(Capsule().stroke(Palette.bad, lineWidth: 1))
                }
                .padding(.top, 10)
                .accessibilityLabel("\(problem). Open settings")
            }
            Text("\(pos.amountDisplay) sats")
                .font(Fig.extraBold(64)).kerning(-1.2).foregroundStyle(Palette.ink)
                .minimumScaleFactor(0.4).lineLimit(1)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .layoutPriority(1)
            Keypad { pos.key($0) }
                .frame(maxHeight: 380)
            Button(pos.chargeable ? "Charge \(pos.amountDisplay) sats" : "Charge") { pos.send(.chargePressed) }
                .buttonStyle(GoldButtonStyle(enabled: pos.chargeable))
                .disabled(!pos.chargeable)
                .padding(.top, 8)
            Button("Check card balance") { pos.send(.cardBalanceOpened) }
                .buttonStyle(OutlineButtonStyle())
                .padding(.top, 10)
        }
    }
}

/// Holder PIN: dots only, the digits never reach a view.
struct PinScreen: View {
    @EnvironmentObject var pos: PosController
    let priceMsat: Int64
    let wrongPin: Bool
    let triesLeft: Int?

    var body: some View {
        VStack(spacing: 0) {
            Text("Pay \(formatSats(priceMsat)) sats").font(Fig.regular(20)).foregroundStyle(Palette.mist).padding(.top, 16)
            Text("Card holder: enter your PIN").font(Fig.extraBold(28)).kerning(-0.5).foregroundStyle(Palette.ink)
                .multilineTextAlignment(.center).padding(.top, 8)
            if wrongPin {
                Text(wrongPinCopy(triesLeft)).font(Fig.regular(16)).foregroundStyle(Palette.bad).padding(.top, 8)
            }
            Text(String(repeating: "●", count: pos.pinLength))
                .font(Fig.extraBold(48)).kerning(14).foregroundStyle(Palette.ink)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .accessibilityLabel("\(pos.pinLength) digits entered")
            Keypad { pos.pinKey($0) }.frame(maxHeight: 380)
            Button("Pay") { pos.send(.pinConfirmed) }
                .buttonStyle(GoldButtonStyle(enabled: pos.pinComplete))
                .disabled(!pos.pinComplete)
                .padding(.top, 8)
            Button("Cancel") { pos.send(.cancelPressed) }.buttonStyle(GhostButtonStyle()).padding(.top, 4)
        }
    }
}

struct TapScreen: View {
    @EnvironmentObject var pos: PosController
    let title: String
    let status: String
    let cancellable: Bool
    /// The card is on the phone and the till is working with it (signing, reading).
    var working: Bool = false

    // Everything sits in the top half: in every tap state the system's NFC sheet covers the
    // lower half, and it brings its own Cancel and tap picture, so neither is drawn here.
    var body: some View {
        GeometryReader { geo in
            VStack(spacing: 0) {
                Text(title).font(Fig.extraBold(44)).kerning(-0.8).foregroundStyle(Palette.ink)
                    .lineLimit(1).minimumScaleFactor(0.5).padding(.top, 24)
                TapMotion(working: working)
                    .frame(width: 200, height: 200)
                    .padding(.top, 8)
                    .accessibilityHidden(true)
                Text(status).font(Fig.extraBold(22)).foregroundStyle(Palette.ink).multilineTextAlignment(.center)
                    .contentTransition(.opacity)
                    .animation(.easeOut(duration: 0.2), value: status)
                    .padding(.top, 4)
                    .accessibilityAddTraits(.updatesFrequently)
            }
            .frame(width: geo.size.width, height: geo.size.height * 0.5, alignment: .top)
        }
        .padding(.horizontal, 8)
    }
}

/// The tap screen's motion, the same drawing as Android's TapMotionView. Waiting: contactless
/// rings leave the centre one after another (2.4 s a ring). Working: the rings gather into a ring
/// and a gold arc runs round it, its length breathing, so a 4 to 6 second signature reads as
/// steady progress. The switch morphs over 0.42 s. With Reduce Motion, each mode is one still frame.
struct TapMotion: View {
    let working: Bool
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var start = Date()
    @State private var changedAt = Date.distantPast
    @State private var from: Double = 0

    var body: some View {
        TimelineView(.animation(paused: reduceMotion)) { tl in
            Canvas { ctx, size in draw(&ctx, size, tl.date) }
        }
        .onChange(of: working) { _, now in
            from = blend(at: Date(), target: now ? 0 : 1)
            changedAt = Date()
        }
    }

    /// 0 = waiting, 1 = working, eased (standard decelerate) from where the last switch left it.
    private func blend(at now: Date, target: Double? = nil) -> Double {
        let to = target ?? (working ? 1 : 0)
        if reduceMotion { return to }
        let p = min(1, max(0, now.timeIntervalSince(changedAt) / 0.42))
        let e = UnitCurve.bezier(startControlPoint: UnitPoint(x: 0.2, y: 0), endControlPoint: UnitPoint(x: 0, y: 1)).value(at: p)
        return from + (to - from) * e
    }

    private func draw(_ ctx: inout GraphicsContext, _ size: CGSize, _ now: Date) {
        let c = CGPoint(x: size.width / 2, y: size.height / 2)
        let r = min(size.width, size.height) * 0.36
        let still = reduceMotion
        let ms = still ? 0 : now.timeIntervalSince(start) * 1000
        let b = blend(at: now)
        let waiting = 1 - b, working = b

        func circle(_ radius: Double) -> Path {
            Path(ellipseIn: CGRect(x: c.x - radius, y: c.y - radius, width: radius * 2, height: radius * 2))
        }

        if waiting > 0.01 {
            let period = 2400.0
            for i in 0..<3 {
                let p = still ? 0.35 + Double(i) * 0.2 : (ms + Double(i) * period / 3).truncatingRemainder(dividingBy: period) / period
                let eased = 1 - pow(1 - p, 3)
                ctx.stroke(circle(r * (0.30 + 0.70 * eased)),
                           with: .color(Palette.goldHi.opacity(0.6 * pow(1 - p, 1.6) * waiting)), lineWidth: 2)
            }
            let breath = still ? 0 : sin(2 * .pi * ms / 2400)
            ctx.fill(circle(7 + 1.5 * breath), with: .color(Palette.gold.opacity(waiting)))
        }

        if working > 0.01 {
            let ringR = r * (0.55 + 0.30 * working)
            let glow = still ? 1 : 0.85 + 0.15 * sin(2 * .pi * ms / 1600)
            ctx.fill(circle(ringR), with: .radialGradient(
                Gradient(colors: [Palette.gold.opacity(70 / 255 * working * glow), Palette.gold.opacity(0)]),
                center: c, startRadius: 0, endRadius: ringR))
            ctx.stroke(circle(ringR), with: .color(Palette.ink.opacity(0.14 * working)), lineWidth: 3)

            // The head turns once every 1.2 s; the arc's length breathes between 40° and 150°.
            let head = still ? 300 : ms.truncatingRemainder(dividingBy: 1200) / 1200 * 360
            let sweep = still ? 120 : 95 + 55 * sin(2 * .pi * ms / 1800)
            let startDeg = head - sweep - 90
            var arc = Path()
            arc.addArc(center: c, radius: ringR, startAngle: .degrees(startDeg), endAngle: .degrees(startDeg + sweep), clockwise: false)
            ctx.stroke(arc, with: .conicGradient(
                Gradient(stops: [.init(color: Palette.goldHi.opacity(0), location: 0),
                                 .init(color: Palette.goldHi.opacity(working), location: sweep / 360),
                                 .init(color: Palette.goldHi.opacity(0), location: min(1, sweep / 360 + 0.001))]),
                center: c, angle: .degrees(startDeg)),
                style: StrokeStyle(lineWidth: 4, lineCap: .round))
        }
    }
}

struct ReceiptScreen: View {
    @EnvironmentObject var pos: PosController
    let headline: String
    let txid: String
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var landed = false
    @State private var risen = false

    var body: some View {
        VStack(spacing: 0) {
            Spacer()
            VStack(spacing: 0) {
                ZStack {
                    Circle().fill(Color(hex: 0x4ade80, alpha: 0x22 / 255.0)).overlay(Circle().stroke(Palette.ok, lineWidth: 3))
                    Text("✓").font(.system(size: 64)).foregroundStyle(Palette.ok)
                }
                .frame(width: 104, height: 104)
                .scaleEffect(landed ? 1 : 0.4).opacity(landed ? 1 : 0)
                Text(headline).font(Fig.extraBold(44)).kerning(-0.8)
                    .foregroundStyle(Palette.ink).lineLimit(1).minimumScaleFactor(0.5).padding(.top, 24)
                    .opacity(risen ? 1 : 0).offset(y: risen ? 0 : 12)
                Text(shortTxid(txid)).font(.system(size: 13, design: .monospaced)).foregroundStyle(Palette.dim).padding(.top, 12)
            }
            .padding(24)
            .frame(maxWidth: .infinity)
            .cardPanel()
            Spacer()
            Button("New sale") { pos.send(.newSalePressed) }.buttonStyle(GoldButtonStyle())
        }
        .padding(8)
        // The tick lands with a small overshoot, then the amount rises in under it.
        .onAppear {
            if reduceMotion { landed = true; risen = true; return }
            withAnimation(.spring(response: 0.42, dampingFraction: 0.58)) { landed = true }
            withAnimation(.timingCurve(0.2, 0, 0, 1, duration: 0.32).delay(0.14)) { risen = true }
        }
    }
}

extension ReceiptScreen {
    /// A sale: the price only. Change and sub-sat rounding stay in the till's sales record.
    init(receipt: ChargeReceipt) { self.init(headline: receiptHeadline(receipt), txid: receipt.txid) }
}

/// The top-up amount: the amount keypad again, with "Create invoice".
struct TopUpScreen: View {
    @EnvironmentObject var pos: PosController
    let creating: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("Top up").font(Fig.extraBold(28)).kerning(-0.4).foregroundStyle(Palette.goldHi).padding(.top, 16)
            Text("\(pos.topUpDisplay) sats")
                .font(Fig.extraBold(64)).kerning(-1.2).foregroundStyle(Palette.ink)
                .minimumScaleFactor(0.4).lineLimit(1)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .layoutPriority(1)
            Keypad { pos.key($0) }
                .frame(maxHeight: 380)
                .disabled(creating)
            let ok = pos.topUpChargeable && !creating
            Button(creating ? "Creating invoice…" : "Create invoice") { pos.send(.createInvoicePressed) }
                .buttonStyle(GoldButtonStyle(enabled: ok))
                .disabled(!ok)
                .padding(.top, 8)
            Button("Cancel") { pos.send(.cancelPressed) }
                .buttonStyle(OutlineButtonStyle())
                .disabled(creating)
                .padding(.top, 10)
        }
    }
}

/// The invoice the holder pays from their own wallet.
struct InvoiceScreen: View {
    @EnvironmentObject var pos: PosController
    let topUp: TopUp

    var body: some View {
        VStack(spacing: 0) {
            Text("Top up \(formatSats(topUp.amountMsat)) sats").font(Fig.extraBold(40)).kerning(-0.8)
                .foregroundStyle(Palette.ink).lineLimit(1).minimumScaleFactor(0.5).padding(.top, 24)
            Spacer()
            InvoiceQR(text: invoiceQrText(topUp.invoice))
                .padding(18)
                .background(RoundedRectangle(cornerRadius: 20).fill(Color.white))
                .accessibilityLabel("Lightning invoice QR code")
            Text("Scan with a Lightning wallet").font(Fig.regular(20)).foregroundStyle(Palette.mist).padding(.top, 20)
            Text("Expires in \(timeLeft(expiresAtMs: topUp.expiresAtMs, nowMs: pos.nowMs))")
                .font(Fig.regular(15)).foregroundStyle(Palette.dim).monospacedDigit().padding(.top, 6)
            Spacer()
            Button("Cancel") { pos.send(.cancelPressed) }.buttonStyle(OutlineButtonStyle())
        }
        .padding(8)
    }
}

/// A crisp QR: CoreImage draws one pixel per module, scaled without smoothing.
struct InvoiceQR: View {
    let text: String

    var body: some View {
        if let img = Self.image(text) {
            Image(uiImage: img).interpolation(.none).resizable().aspectRatio(1, contentMode: .fit)
        } else {
            Color.white.aspectRatio(1, contentMode: .fit)
        }
    }

    static func image(_ text: String) -> UIImage? {
        let f = CIFilter.qrCodeGenerator()
        f.message = Data(text.utf8)
        f.correctionLevel = "M"
        guard let out = f.outputImage else { return nil }
        // Navy on white, as the Android POS draws it.
        let colored = out.applyingFilter("CIFalseColor", parameters: [
            "inputColor0": CIColor(red: 0x0a / 255.0, green: 0x16 / 255.0, blue: 0x3c / 255.0),
            "inputColor1": CIColor(red: 1, green: 1, blue: 1),
        ])
        let scaled = colored.transformed(by: CGAffineTransform(scaleX: 12, y: 12))
        guard let cg = CIContext().createCGImage(scaled, from: scaled.extent) else { return nil }
        return UIImage(cgImage: cg)
    }
}

/// A failed top-up step.
struct TopUpErrorScreen: View {
    @EnvironmentObject var pos: PosController
    let failure: TopUpFailure

    private var retryLabel: String? {
        switch failure.problem {
        case .failed, .notPaired, .noRoom: return nil
        case .expired: return "New invoice"
        default: return failure.topUp == nil ? "Try again" : "Tap again"
        }
    }

    var body: some View {
        let (title, detail) = topUpErrorCopy(failure.problem, paid: failure.paid, detail: failure.detail)
        VStack(spacing: 10) {
            Spacer()
            Text(title).font(Fig.extraBold(28)).foregroundStyle(Palette.bad).multilineTextAlignment(.center)
            if !detail.isEmpty {
                Text(detail).font(Fig.regular(16)).foregroundStyle(Palette.mist).multilineTextAlignment(.center).padding(.top, 2)
            }
            Spacer()
            if let retryLabel {
                Button(retryLabel) { pos.send(.retryPressed) }.buttonStyle(GoldButtonStyle())
            }
            Button("Done") { pos.send(.donePressed) }.buttonStyle(OutlineButtonStyle())
        }
        .padding(8)
    }
}

struct ErrorScreen: View {
    @EnvironmentObject var pos: PosController
    let info: PosErrorInfo
    @State private var confirmAbandon = false

    var body: some View {
        let (title, detail) = errorCopy(info.kind, info.detail)
        let unresolved = info.unresolvedRef != nil
        VStack(spacing: 10) {
            Spacer()
            Text(title).font(Fig.extraBold(28)).foregroundStyle(Palette.bad).multilineTextAlignment(.center)
            if !detail.isEmpty {
                Text(detail).font(Fig.regular(16)).foregroundStyle(Palette.mist).multilineTextAlignment(.center).padding(.top, 2)
            }
            Spacer()
            if info.kind.retryable || unresolved {
                Button(unresolved ? "Try again" : "Retry") { pos.send(.retryPressed) }.buttonStyle(GoldButtonStyle())
            }
            if unresolved {
                Button("Check sales") { pos.send(.salesOpened) }.buttonStyle(OutlineButtonStyle())
                Button("Abandon sale") { confirmAbandon = true }.buttonStyle(OutlineButtonStyle(color: Palette.bad))
            } else {
                Button("Back") { pos.send(.backPressed) }.buttonStyle(OutlineButtonStyle())
            }
        }
        .padding(8)
        .alert("Abandon sale?", isPresented: $confirmAbandon) {
            Button("Abandon", role: .destructive) { pos.send(.abandonPressed) }
            Button("Keep", role: .cancel) {}
        } message: {
            Text("If the payment did go through, the customer will have paid without a receipt. Check Sales first.")
        }
    }
}

/// A failed balance read: Try again and Done, nothing about sales.
struct BalanceErrorScreen: View {
    @EnvironmentObject var pos: PosController
    let problem: BalanceProblem

    var body: some View {
        let (title, detail) = balanceErrorCopy(problem)
        VStack(spacing: 10) {
            Spacer()
            Text(title).font(Fig.extraBold(28)).foregroundStyle(Palette.bad).multilineTextAlignment(.center)
            Text(detail).font(Fig.regular(16)).foregroundStyle(Palette.mist).multilineTextAlignment(.center).padding(.top, 2)
            Spacer()
            Button("Try again") { pos.send(.retryPressed) }.buttonStyle(GoldButtonStyle())
            Button("Done") { pos.send(.donePressed) }.buttonStyle(OutlineButtonStyle())
        }
        .padding(8)
    }
}

/// Shown to the holder, so money only, in plain words.
struct BalanceScreen: View {
    @EnvironmentObject var pos: PosController
    let liveMsat: Int64
    /// A paid top-up the till holds for this card; arrives after the balance.
    var waiting: WaitingTopUp? = nil

    var body: some View {
        VStack(spacing: 0) {
            Spacer()
            VStack(spacing: 0) {
                Text("CARD BALANCE").font(Fig.medium(11)).kerning(1.3).foregroundStyle(Palette.mist)
                    .padding(.horizontal, 10).padding(.vertical, 4)
                    .background(Capsule().fill(Palette.cardFill)).overlay(Capsule().stroke(Palette.line, lineWidth: 1))
                Text("\(formatSats(liveMsat)) sats").font(Fig.extraBold(64)).kerning(-1.2).foregroundStyle(Palette.goldHi)
                    .lineLimit(1).minimumScaleFactor(0.4).frame(height: 96).padding(.top, 20)
                Text("on this card").font(Fig.regular(20)).foregroundStyle(Palette.mist).padding(.top, 4)
            }
            .padding(.vertical, 36).padding(.horizontal, 20)
            .frame(maxWidth: .infinity)
            .cardPanel()
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("Card balance: \(formatSats(liveMsat)) sats on this card")
            Spacer()
            Text("Back to the amount in \(pos.balanceCountdown) s").font(Fig.regular(14)).foregroundStyle(Palette.dim).padding(.bottom, 12)
            if let w = waiting {
                Button("Load waiting top-up · \(formatSats(w.amountMsat)) sats") { pos.send(.loadWaitingPressed) }
                    .buttonStyle(GoldButtonStyle())
                    .padding(.bottom, 12)
                    .transition(.opacity)
            }
            HStack(spacing: 12) {
                Button("Charge card") { pos.send(.chargeCardPressed) }.buttonStyle(OutlineButtonStyle())
                Button("Top up card") { pos.send(.topUpPressed) }.buttonStyle(GoldButtonStyle())
            }
            Button("Done") { pos.send(.donePressed) }.buttonStyle(GhostButtonStyle()).padding(.top, 4)
        }
        .padding(8)
        .animation(.easeOut(duration: 0.22), value: waiting)
    }
}

struct SalesScreen: View {
    @EnvironmentObject var pos: PosController
    private static let fmt: DateFormatter = { let f = DateFormatter(); f.dateFormat = "HH:mm:ss"; return f }()

    private var header: String {
        switch pos.sales {
        case nil: return "Loading…"
        case .err?: return "Could not load sales"
        case .ok(let l)?: return "\(l.count) \(l.count == 1 ? "sale" : "sales") · \(formatSats(l.totalPriceMsat)) sats"
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Button("Back") { pos.send(.backPressed) }.buttonStyle(GhostButtonStyle())
                Spacer()
                Text(header).font(Fig.extraBold(16)).foregroundStyle(Palette.goldHi)
            }
            ScrollView {
                LazyVStack(spacing: 8) {
                    if case .ok(let l)? = pos.sales {
                        ForEach(l.sales.reversed()) { s in
                            HStack(alignment: .firstTextBaseline) {
                                VStack(alignment: .leading, spacing: 2) {
                                    Text("\(formatSats(s.priceMsat)) sats").font(Fig.extraBold(18)).foregroundStyle(Palette.ink)
                                    Text("received \(formatSats(s.receivedMsat))").font(Fig.regular(13)).foregroundStyle(Palette.dim)
                                }
                                Spacer()
                                VStack(alignment: .trailing, spacing: 2) {
                                    Text(Self.fmt.string(from: Date(timeIntervalSince1970: Double(s.tsUnixMs) / 1000)))
                                        .font(Fig.medium(14)).foregroundStyle(Palette.mist)
                                    Text(shortTxid(s.txid)).font(.system(size: 12, design: .monospaced)).foregroundStyle(Palette.dim)
                                }
                            }
                            .padding(14)
                            .cardPanel()
                        }
                    }
                }
                .padding(.top, 8)
            }
            .refreshable { pos.loadSales() }
        }
    }
}
