import CoreText
import SwiftUI
import UIKit

extension Color {
    init(hex: UInt32, alpha: Double = 1) {
        self.init(.sRGB, red: Double((hex >> 16) & 0xff) / 255, green: Double((hex >> 8) & 0xff) / 255,
                  blue: Double(hex & 0xff) / 255, opacity: alpha)
    }
}

/// The Android app's palette (res/values/colors.xml).
enum Palette {
    static let night = Color(hex: 0x1a3272)
    static let navy = Color(hex: 0x22408a)
    static let ink = Color(hex: 0xeef3ff)
    static let mist = Color(hex: 0xb9c8ee)
    static let dim = Color(hex: 0xa9b8e2)
    static let line = Color(hex: 0xd6e2ff, alpha: 0x38 / 255.0)
    static let cardFill = Color(hex: 0x0a163c, alpha: 0x52 / 255.0)
    static let gold = Color(hex: 0xd2bb6a)
    static let goldHi = Color(hex: 0xf4e9b0)
    static let goldDeep = Color(hex: 0xa88a3c)
    static let ok = Color(hex: 0x4ade80)
    static let bad = Color(hex: 0xf87171)
}

/// Figtree, the variable font bundled from the Android app, at a given weight.
enum Fig {
    private static let baseName: String? = {
        guard let url = Bundle.main.url(forResource: "Figtree", withExtension: "ttf"),
              let descs = CTFontManagerCreateFontDescriptorsFromURL(url as CFURL) as? [CTFontDescriptor],
              let d = descs.first else { return nil }
        return CTFontDescriptorCopyAttribute(d, kCTFontNameAttribute) as? String
    }()

    static func font(_ size: CGFloat, _ weight: CGFloat = 400) -> Font {
        guard let name = baseName else { return .system(size: size, weight: weight >= 700 ? .heavy : weight >= 500 ? .medium : .regular) }
        let wght: UInt32 = 0x7767_6874 // 'wght'
        let desc = UIFontDescriptor(fontAttributes: [
            .name: name,
            UIFontDescriptor.AttributeName(rawValue: kCTFontVariationAttribute as String): [wght: weight],
        ])
        return Font(UIFont(descriptor: desc, size: size))
    }

    static func regular(_ s: CGFloat) -> Font { font(s, 400) }
    static func medium(_ s: CGFloat) -> Font { font(s, 500) }
    static func extraBold(_ s: CGFloat) -> Font { font(s, 800) }
}

struct AppBackground: View {
    var body: some View {
        ZStack {
            LinearGradient(colors: [Color(hex: 0x2a4fae), Color(hex: 0x1f3d8c), Color(hex: 0x1a3272)],
                           startPoint: .top, endPoint: .bottom)
            GeometryReader { g in
                RadialGradient(colors: [Color(hex: 0x4f7fe6, alpha: 0.4), Color(hex: 0x4f7fe6, alpha: 0)],
                               center: UnitPoint(x: 0.9, y: 0), startRadius: 0,
                               endRadius: max(g.size.width, g.size.height) * 0.75)
            }
        }
        .ignoresSafeArea()
    }
}

/// The brand mark (res/drawable/ic_mark.xml): a gold card with the federation heptagon.
struct BrandMark: View {
    var body: some View {
        Canvas { ctx, size in
            let sx = size.width / 100, sy = size.height / 76
            let t = CGAffineTransform(scaleX: sx, y: sy)
            let card = Path(roundedRect: CGRect(x: 1, y: 1, width: 98, height: 74), cornerRadius: 13).applying(t)
            ctx.clip(to: card)
            ctx.fill(card, with: .linearGradient(
                Gradient(stops: [.init(color: Palette.goldHi, location: 0), .init(color: Palette.gold, location: 0.45),
                                 .init(color: Palette.goldDeep, location: 1)]),
                startPoint: .zero, endPoint: CGPoint(x: size.width, y: size.height)))
            let rawPts: [(CGFloat, CGFloat)] = [(50, 23), (61.73, 28.65), (64.62, 41.34), (56.51, 51.51), (43.49, 51.51), (35.38, 41.34), (38.27, 28.65)]
            let pts = rawPts.map { CGPoint(x: $0.0, y: $0.1) }
            let rawFar: [(CGFloat, CGFloat)] = [(50, -52), (120.36, -18.11), (137.74, 58.03), (89.05, 119.09), (10.95, 119.09), (-37.74, 58.03), (-20.36, -18.11)]
            let far = rawFar.map { CGPoint(x: $0.0, y: $0.1) }
            var p = Path()
            p.addLines(pts); p.closeSubpath()
            for (a, b) in zip(pts, far) { p.move(to: a); p.addLine(to: b) }
            ctx.stroke(p.applying(t), with: .color(Palette.night),
                       style: StrokeStyle(lineWidth: 2.8 * sx, lineCap: .round, lineJoin: .round))
        }
    }
}

struct GoldButtonStyle: ButtonStyle {
    var enabled = true
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(Fig.extraBold(20))
            .foregroundStyle(Palette.night)
            .frame(maxWidth: .infinity, minHeight: 64)
            .background(RoundedRectangle(cornerRadius: 14).fill(configuration.isPressed ? Palette.goldDeep : Palette.gold))
            .opacity(enabled ? 1 : 0.4)
    }
}

struct OutlineButtonStyle: ButtonStyle {
    var color = Palette.goldHi
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(Fig.medium(16))
            .foregroundStyle(color)
            .frame(maxWidth: .infinity, minHeight: 52)
            .background(RoundedRectangle(cornerRadius: 14).fill(configuration.isPressed ? Color.white.opacity(0.08) : .clear))
            .overlay(RoundedRectangle(cornerRadius: 14).stroke(color, lineWidth: 1.5))
    }
}

struct GhostButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(Fig.medium(16))
            .foregroundStyle(Palette.goldHi)
            .frame(minHeight: 44)
            .padding(.horizontal, 12)
            .opacity(configuration.isPressed ? 0.6 : 1)
    }
}

struct KeyButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(Fig.medium(32))
            .foregroundStyle(Palette.ink)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(RoundedRectangle(cornerRadius: 14).fill(configuration.isPressed ? Color(hex: 0xeef3ff, alpha: 0.2) : Palette.cardFill))
            .overlay(RoundedRectangle(cornerRadius: 14).stroke(Palette.line, lineWidth: 1))
    }
}

struct CardPanel: ViewModifier {
    func body(content: Content) -> some View {
        content
            .background(RoundedRectangle(cornerRadius: 14).fill(Palette.cardFill))
            .overlay(RoundedRectangle(cornerRadius: 14).stroke(Palette.line, lineWidth: 1))
    }
}

extension View {
    func cardPanel() -> some View { modifier(CardPanel()) }
}
