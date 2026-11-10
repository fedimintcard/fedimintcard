import Foundation
import Security

/// Where the till is and the bearer token it handed out (docs/POS-LINK.md, "Pairing and auth").
struct TillPairing: Equatable {
    /// Base URL with no trailing slash and no query, e.g. `http://192.168.1.20:9877`.
    let baseURL: String
    /// The till's POS token; nil only for a typed URL with no token.
    let token: String?

    static let scheme = "fedimintcard-till:"

    /// Parses the till's pairing text `fedimintcard-till:http://<ip>:9877?t=<token>`, or a typed
    /// `http://host:port` (optionally with `?t=<token>`). Returns nil for anything else.
    static func parse(_ input: String) -> TillPairing? {
        var s = input.trimmingCharacters(in: .whitespacesAndNewlines)
        if s.lowercased().hasPrefix(scheme) { s = String(s.dropFirst(scheme.count)) }
        guard !s.isEmpty else { return nil }
        if !s.contains("://") { s = "http://" + s }
        guard let c = URLComponents(string: s),
              let sch = c.scheme?.lowercased(), sch == "http" || sch == "https",
              let host = c.host, !host.isEmpty,
              c.user == nil, c.password == nil, c.fragment == nil else { return nil }
        let path = c.path.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        guard path.isEmpty else { return nil }
        var token: String? = nil
        if let items = c.queryItems {
            // Only `t` is defined; anything else means this is not a pairing string.
            guard items.allSatisfy({ $0.name == "t" }), items.count == 1,
                  let t = items[0].value, isToken(t) else { return nil }
            token = t
        } else if c.query != nil {
            return nil
        }
        var base = "\(sch)://\(host.contains(":") ? "[\(host)]" : host)"
        if let port = c.port { base += ":\(port)" }
        return TillPairing(baseURL: base, token: token)
    }

    private static func isToken(_ t: String) -> Bool {
        !t.isEmpty && t.count <= 256 && t.unicodeScalars.allSatisfy { CharacterSet.alphanumerics.contains($0) && $0.isASCII }
    }

    /// The card link's WebSocket URL: `ws(s)://host:port/v1/card/link`.
    var linkURL: URL? {
        guard var c = URLComponents(string: baseURL) else { return nil }
        let secure = c.scheme == "https"
        c.scheme = secure ? "wss" : "ws"
        c.path = "/v1/card/link"
        return c.url
    }
}

/// Whether pairing `newURL` leaves payments the phone holds (`onPhone`, waiting to sync or held for
/// their cards) with no till to sync to: they are the till paired now's or, with none paired, the
/// till the kit came from. Another till refuses them (docs/POS-OFFLINE.md).
func strandsPayments(onPhone: Int, pairedURL: String?, kitTillURL: String?, newURL: String) -> Bool {
    guard onPhone > 0, let holder = pairedURL ?? kitTillURL else { return false }
    return holder != newURL
}

/// Persists the pairing. The app uses the Keychain; tests use a dictionary.
protocol PairingStore: AnyObject {
    func load() -> TillPairing?
    func save(_ p: TillPairing)
    func clear()
}

final class MemoryPairingStore: PairingStore {
    var value: TillPairing?
    init(_ v: TillPairing? = nil) { value = v }
    func load() -> TillPairing? { value }
    func save(_ p: TillPairing) { value = p }
    func clear() { value = nil }
}

/// URL and token in one generic-password item, readable only on this device after first unlock.
final class KeychainPairingStore: PairingStore {
    private let service = "org.fedimintcard.pos.till"
    private let account = "pairing"

    private var query: [String: Any] {
        [kSecClass as String: kSecClassGenericPassword,
         kSecAttrService as String: service,
         kSecAttrAccount as String: account]
    }

    func load() -> TillPairing? {
        var q = query
        q[kSecReturnData as String] = true
        q[kSecMatchLimit as String] = kSecMatchLimitOne
        var out: CFTypeRef?
        guard SecItemCopyMatching(q as CFDictionary, &out) == errSecSuccess,
              let data = out as? Data,
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: String],
              let url = obj["url"] else { return nil }
        return TillPairing(baseURL: url, token: obj["token"])
    }

    func save(_ p: TillPairing) {
        var obj = ["url": p.baseURL]
        if let t = p.token { obj["token"] = t }
        guard let data = try? JSONSerialization.data(withJSONObject: obj) else { return }
        SecItemDelete(query as CFDictionary)
        var q = query
        q[kSecValueData as String] = data
        q[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        SecItemAdd(q as CFDictionary, nil)
    }

    func clear() { SecItemDelete(query as CFDictionary) }
}
