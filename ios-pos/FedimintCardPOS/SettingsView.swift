import AVFoundation
import SwiftUI

struct SettingsView: View {
    @EnvironmentObject var pos: PosController
    @Environment(\.dismiss) private var dismiss
    @State private var scanning = false
    @State private var text = ""
    @State private var message: String?
    /// A pairing waiting for "Unpair anyway?": the phone holds payments for another till.
    @State private var pendingPair: TillPairing?
    @State private var confirmForget = false
    /// A refused charge waiting for "Delete this record?".
    @State private var dismissing: RefusedOfflineCharge?

    var body: some View {
        ZStack {
            AppBackground()
            VStack(alignment: .leading, spacing: 14) {
                HStack {
                    Text("Settings").font(Fig.extraBold(28)).foregroundStyle(Palette.ink)
                    Spacer()
                    Button("Done") { dismiss() }.buttonStyle(GhostButtonStyle())
                }
                ScrollView {
                    VStack(alignment: .leading, spacing: 14) {
                        VStack(alignment: .leading, spacing: 4) {
                            Text("TILL").font(Fig.medium(11)).kerning(1.3).foregroundStyle(Palette.dim)
                            Text(pos.pairing?.baseURL ?? "Not paired").font(Fig.medium(18)).foregroundStyle(Palette.ink)
                            if let p = pos.pairing {
                                Text(p.token == nil ? "No token: the till will refuse this phone over the network" : "Paired with a token")
                                    .font(Fig.regular(14)).foregroundStyle(p.token == nil ? Palette.bad : Palette.mist)
                            }
                        }
                        .padding(14).frame(maxWidth: .infinity, alignment: .leading).cardPanel()

                        OfflinePanel(phone: pos.phone, till: pos.status) { dismissing = $0 }

                        if scanning {
                            QRScannerView { code in
                                scanning = false
                                apply(code)
                            }
                            .frame(height: 300)
                            .clipShape(RoundedRectangle(cornerRadius: 14))
                            Button("Stop scanning") { scanning = false }.buttonStyle(OutlineButtonStyle())
                        } else {
                            Button("Pair with till") { message = nil; scanning = true }.buttonStyle(GoldButtonStyle())
                        }

                        Text("Or paste the pairing text the till printed:").font(Fig.regular(14)).foregroundStyle(Palette.mist)
                        TextField("", text: $text, prompt: Text("fedimintcard-till:http://…").foregroundStyle(Palette.dim))
                            .font(Fig.regular(16)).foregroundStyle(Palette.ink)
                            .textInputAutocapitalization(.never).autocorrectionDisabled().keyboardType(.URL)
                            .padding(12).cardPanel()
                        HStack(spacing: 10) {
                            Button("Paste") { if let s = UIPasteboard.general.string { text = s } }.buttonStyle(OutlineButtonStyle())
                            Button("Save") { apply(text) }.buttonStyle(OutlineButtonStyle())
                        }
                        if let message {
                            Text(message).font(Fig.regular(15)).foregroundStyle(message.hasPrefix("Paired") ? Palette.ok : Palette.bad)
                        }
                        if pos.pairing != nil {
                            Button("Forget till") {
                                if pos.paymentsOnPhone > 0 { confirmForget = true } else { pos.forgetTill(); message = nil }
                            }
                            .buttonStyle(OutlineButtonStyle(color: Palette.bad))
                            .padding(.top, 10)
                        }
                    }
                }
            }
            .padding(16)
        }
        .preferredColorScheme(.dark)
        // Never a silent drop: the payments stay on the phone, and the operator is told they can't
        // reach another till.
        .alert("Unpair anyway?", isPresented: Binding(get: { pendingPair != nil || confirmForget },
                                                     set: { if !$0 { pendingPair = nil; confirmForget = false } })) {
            Button("Unpair", role: .destructive) {
                if let p = pendingPair { pair(p) } else { pos.forgetTill(); message = nil }
                pendingPair = nil
                confirmForget = false
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text(unpairWarningText(pos.paymentsOnPhone))
        }
        .alert("Delete this record?", isPresented: Binding(get: { dismissing != nil }, set: { if !$0 { dismissing = nil } })) {
            Button("Delete", role: .destructive) {
                if let r = dismissing { pos.dismissRefused(r.txid) }
                dismissing = nil
            }
            Button("Keep", role: .cancel) {}
        } message: {
            Text("The card was charged; this deletes the only record of it.")
        }
    }

    private func apply(_ s: String) {
        guard let p = TillPairing.parse(s) else {
            message = "That isn't a till pairing code. It starts with fedimintcard-till:"
            return
        }
        if pos.pairingStrandsPayments(p) { pendingPair = p; return }
        pair(p)
    }

    private func pair(_ p: TillPairing) {
        pos.pair(p)
        text = ""
        message = "Paired with \(p.baseURL)"
    }
}

/// The phone's offline side: the kit's age, expiry and caps, what the store holds (or why offline
/// is off), the double spends the till counts (when its status reports them), why the last sync
/// stopped short, and the charges the till refused, each with Dismiss.
struct OfflinePanel: View {
    let phone: OfflineSnapshot
    /// The till's last status, for its `offline.double_spent` count; nil before one is read.
    let till: TillStatus?
    let onDismiss: (RefusedOfflineCharge) -> Void

    var body: some View {
        TimelineView(.periodic(from: .now, by: 30)) { tl in
            VStack(alignment: .leading, spacing: 4) {
                Text("OFFLINE").font(Fig.medium(11)).kerning(1.3).foregroundStyle(Palette.dim)
                let lines = offlinePanelLines(phone, nowMs: Int64(tl.date.timeIntervalSince1970 * 1000), till: till)
                // Offline off: the first line (why) in the fault color, the till's count under it plain.
                ForEach(Array(lines.enumerated()), id: \.offset) { i, line in
                    Text(line).font(Fig.regular(14)).foregroundStyle(phone.enabled || i > 0 ? Palette.mist : Palette.bad)
                }
                ForEach(phone.refused) { r in
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Text(refusedChargeText(priceMsat: r.priceMsat, code: r.code))
                            .font(Fig.regular(14)).foregroundStyle(Palette.bad)
                        Spacer()
                        Button("Dismiss") { onDismiss(r) }.buttonStyle(GhostButtonStyle())
                    }
                    .accessibilityIdentifier("refused-\(r.txid)")
                }
            }
            .padding(14).frame(maxWidth: .infinity, alignment: .leading).cardPanel()
        }
    }
}

/// Camera QR scanner; reports the first QR text it reads.
struct QRScannerView: UIViewControllerRepresentable {
    let onCode: (String) -> Void

    func makeUIViewController(context: Context) -> ScannerController {
        let c = ScannerController()
        c.onCode = onCode
        return c
    }

    func updateUIViewController(_ vc: ScannerController, context: Context) {}

    final class ScannerController: UIViewController, AVCaptureMetadataOutputObjectsDelegate {
        var onCode: ((String) -> Void)?
        private let session = AVCaptureSession()
        private var preview: AVCaptureVideoPreviewLayer?
        private var done = false

        override func viewDidLoad() {
            super.viewDidLoad()
            view.backgroundColor = .black
            AVCaptureDevice.requestAccess(for: .video) { ok in
                DispatchQueue.main.async { ok ? self.setup() : self.denied() }
            }
        }

        private func denied() {
            let l = UILabel()
            l.text = "Camera access is off. Allow it in Settings, or paste the pairing text."
            l.textColor = .white; l.numberOfLines = 0; l.textAlignment = .center
            l.frame = view.bounds.insetBy(dx: 16, dy: 16)
            l.autoresizingMask = [.flexibleWidth, .flexibleHeight]
            view.addSubview(l)
        }

        private func setup() {
            guard let dev = AVCaptureDevice.default(for: .video), let input = try? AVCaptureDeviceInput(device: dev),
                  session.canAddInput(input) else { denied(); return }
            session.addInput(input)
            let out = AVCaptureMetadataOutput()
            guard session.canAddOutput(out) else { return }
            session.addOutput(out)
            out.setMetadataObjectsDelegate(self, queue: .main)
            out.metadataObjectTypes = [.qr]
            let p = AVCaptureVideoPreviewLayer(session: session)
            p.videoGravity = .resizeAspectFill
            p.frame = view.bounds
            view.layer.addSublayer(p)
            preview = p
            let s = session
            DispatchQueue.global(qos: .userInitiated).async { s.startRunning() }
        }

        override func viewDidLayoutSubviews() {
            super.viewDidLayoutSubviews()
            preview?.frame = view.bounds
        }

        override func viewWillDisappear(_ animated: Bool) {
            super.viewWillDisappear(animated)
            let s = session
            DispatchQueue.global(qos: .userInitiated).async { s.stopRunning() }
        }

        func metadataOutput(_ output: AVCaptureMetadataOutput, didOutput objects: [AVMetadataObject], from connection: AVCaptureConnection) {
            guard !done, let o = objects.first as? AVMetadataMachineReadableCodeObject, let s = o.stringValue else { return }
            done = true
            let session = self.session
            DispatchQueue.global(qos: .userInitiated).async { session.stopRunning() }
            onCode?(s)
        }
    }
}
