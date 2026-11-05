import CoreNFC
import Foundation

/// The card on an ISO 7816 tag in a reader session.
final class ISO7816Channel: CardChannel {
    private let tag: NFCISO7816Tag
    init(tag: NFCISO7816Tag) { self.tag = tag }

    enum Failure: Error { case badAPDU }

    func transmit(_ apdu: Data) async throws -> Data {
        guard let cmd = NFCISO7816APDU(data: apdu) else { throw Failure.badAPDU }
        let (data, sw1, sw2) = try await tag.sendCommand(apdu: cmd)
        var out = data
        out.append(sw1)
        out.append(sw2)
        return out
    }
}

/// One `NFCTagReaderSession` per card job. Callbacks arrive on the main queue.
final class NFCCardReader: NSObject, NFCTagReaderSessionDelegate {
    static var isAvailable: Bool { NFCTagReaderSession.readingAvailable }

    private var session: NFCTagReaderSession?
    private var connected = false
    /// A card is connected; called once per session.
    var onCard: ((CardChannel) -> Void)?
    /// The session is over. `hadCard`: a card was connected during it.
    var onEnd: ((_ hadCard: Bool) -> Void)?

    func begin(alert: String) {
        connected = false
        guard let s = NFCTagReaderSession(pollingOption: .iso14443, delegate: self, queue: .main) else {
            onEnd?(false); return
        }
        s.alertMessage = alert
        session = s
        s.begin()
    }

    func update(alert: String) { session?.alertMessage = alert }

    func finish(success message: String) {
        guard let s = session else { return }
        s.alertMessage = message
        s.invalidate()
        session = nil
    }

    func finish(error message: String) {
        guard let s = session else { return }
        s.invalidate(errorMessage: message)
        session = nil
    }

    func cancel() {
        session?.invalidate()
        session = nil
    }

    func tagReaderSessionDidBecomeActive(_ session: NFCTagReaderSession) {}

    func tagReaderSession(_ session: NFCTagReaderSession, didInvalidateWithError error: Error) {
        let had = connected
        if self.session === session { self.session = nil }
        connected = false
        onEnd?(had)
    }

    func tagReaderSession(_ session: NFCTagReaderSession, didDetect tags: [NFCTag]) {
        guard !connected else { return }
        guard let first = tags.first(where: { if case .iso7816 = $0 { return true }; return false }),
              case .iso7816(let iso) = first else {
            session.alertMessage = "That isn't a Fedimint card. Hold the card to the top of the phone."
            session.restartPolling()
            return
        }
        session.connect(to: first) { [weak self] err in
            guard let self else { return }
            if err != nil {
                session.alertMessage = "Hold the card still against the top of the phone"
                session.restartPolling()
                return
            }
            self.connected = true
            self.onCard?(ISO7816Channel(tag: iso))
        }
    }
}
