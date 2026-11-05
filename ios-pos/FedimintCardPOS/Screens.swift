import SwiftUI

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
    @State private var pulse = false

    var body: some View {
        VStack(spacing: 0) {
            Text(title).font(Fig.extraBold(44)).kerning(-0.8).foregroundStyle(Palette.ink)
                .lineLimit(1).minimumScaleFactor(0.5).padding(.top, 32)
            Image(uiImage: UIImage(named: "card_front") ?? UIImage())
                .resizable().aspectRatio(contentMode: .fit)
                .frame(width: 280)
                .clipShape(RoundedRectangle(cornerRadius: 14))
                .overlay(RoundedRectangle(cornerRadius: 14).stroke(Palette.line, lineWidth: 1))
                .rotationEffect(.degrees(-6))
                .shadow(color: .black.opacity(0.35), radius: 12, y: 6)
                .padding(.top, 28)
                .accessibilityHidden(true)
            ZStack {
                Circle().fill(Color(hex: 0xd2bb6a, alpha: 0x22 / 255.0))
                    .overlay(Circle().stroke(Palette.gold, lineWidth: 4))
                    .frame(width: 160, height: 160)
                    .scaleEffect(pulse ? 1.1 : 0.7)
                    .opacity(pulse ? 0.35 : 1)
                    .animation(.linear(duration: 0.9).repeatForever(autoreverses: true), value: pulse)
            }
            .frame(maxHeight: .infinity)
            Text(status).font(Fig.extraBold(24)).foregroundStyle(Palette.ink).multilineTextAlignment(.center)
            Button("Cancel") { pos.send(.cancelPressed) }
                .buttonStyle(GhostButtonStyle()).padding(.top, 16)
                .opacity(cancellable ? 1 : 0).disabled(!cancellable)
        }
        .padding(.horizontal, 8)
        .onAppear { pulse = true }
    }
}

struct ReceiptScreen: View {
    @EnvironmentObject var pos: PosController
    let receipt: ChargeReceipt

    var body: some View {
        VStack(spacing: 0) {
            Spacer()
            VStack(spacing: 0) {
                ZStack {
                    Circle().fill(Color(hex: 0x4ade80, alpha: 0x22 / 255.0)).overlay(Circle().stroke(Palette.ok, lineWidth: 3))
                    Text("✓").font(.system(size: 64)).foregroundStyle(Palette.ok)
                }
                .frame(width: 104, height: 104)
                // The price only: change and sub-sat rounding stay in the till's sales record.
                Text("Paid \(formatSats(receipt.priceMsat)) sats").font(Fig.extraBold(44)).kerning(-0.8)
                    .foregroundStyle(Palette.ink).lineLimit(1).minimumScaleFactor(0.5).padding(.top, 24)
                Text(shortTxid(receipt.txid)).font(.system(size: 13, design: .monospaced)).foregroundStyle(Palette.dim).padding(.top, 12)
            }
            .padding(24)
            .frame(maxWidth: .infinity)
            .cardPanel()
            Spacer()
            Button("New sale") { pos.send(.newSalePressed) }.buttonStyle(GoldButtonStyle())
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
            Button("Done") { pos.send(.donePressed) }.buttonStyle(GoldButtonStyle())
        }
        .padding(8)
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
