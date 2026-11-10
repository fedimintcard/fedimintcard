import XCTest
@testable import FedimintCardPOS

/// The real core (host/pos-core, from the XCFramework) behind `PosCoreAdapter`: the library and
/// its bindings match (UniFFI would stop the app otherwise), the crypto runs on this target, the
/// store opens, and the card's APDUs travel through the app's transport and back.
final class PosCoreAdapterTests: XCTestCase {
    /// The card, recording each APDU; from `lostAt` on (counted from 0) it has left the field.
    final class RecordingCard: BlockingCardChannel {
        struct Gone: Error {}
        private(set) var apdus: [Data] = []
        private(set) var onMain: [Bool] = []
        let lostAt: Int?
        init(lostAt: Int? = nil) { self.lostAt = lostAt }

        func transmit(_ apdu: Data) throws -> Data {
            apdus.append(apdu)
            onMain.append(Thread.isMainThread)
            if let lostAt, apdus.count > lostAt { throw Gone() }
            return Data([0x6d, 0x00])
        }
    }

    private func store(phoneId: String? = nil) throws -> URL {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("poscore-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        if let phoneId { try Data(phoneId.utf8).write(to: dir.appendingPathComponent("phone-id")) }
        addTeardownBlock { try? FileManager.default.removeItem(at: dir) }
        return dir
    }

    func testTheCoreRunsHere() {
        XCTAssertTrue(PosCoreAdapter.selfCheck())
    }

    func testAnEmptyStoreOpensAndAnswers() throws {
        let dir = try store()
        let core = try PosCoreAdapter.open(dir)
        XCTAssertEqual(core.phoneId().count, 32)
        XCTAssertEqual(try core.stats(), OfflineStats())
        XCTAssertEqual(try core.signingRefs(), [])
        let d = try core.drain(max: 50)
        XCTAssertEqual(d.txids, [])
        XCTAssertEqual(d.refused, [])
        let body = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(d.body.utf8)) as? [String: Any])
        XCTAssertEqual(body["phone_id"] as? String, core.phoneId())
        XCTAssertEqual((body["charges"] as? [Any])?.count, 0)
        XCTAssertFalse(try core.dismiss(txid: "00"))
        XCTAssertThrowsError(try core.markSynced(answerJson: "{"))
        XCTAssertEqual(try core.markSynced(answerJson: #"{"ok":true,"results":[]}"#), CoreSyncReport(synced: 0, refused: 0, retry: 0))
        XCTAssertThrowsError(try PosCoreAdapter.open(dir), "one PosCore per store")
    }

    func testAKitTheCoreCannotUseIsRefusedBeforeTheCard() throws {
        let core = try PosCoreAdapter.open(try store())
        let card = RecordingCard()
        let out = core.charge(card: card, kitJson: "", priceMsat: 5000, clientRef: "a", pin: "1234")
        guard case .refused("offline_not_ready", _, _, _) = out else { return XCTFail("\(out)") }
        // The fixture was issued to another phone than this store's.
        let other = core.charge(card: card, kitJson: PosCoreFixture.kit, priceMsat: 5000, clientRef: "b", pin: "1234")
        guard case .refused("offline_not_ready", _, _, _) = other else { return XCTFail("\(other)") }
        XCTAssertTrue(card.apdus.isEmpty)
    }

    func testTheCardLeavingIsNoCardAndNothingIsTaken() throws {
        let core = try PosCoreAdapter.open(try store(phoneId: PosCoreFixture.phoneId))
        XCTAssertEqual(core.phoneId(), PosCoreFixture.phoneId)
        let card = RecordingCard(lostAt: 0)
        let out = core.charge(card: card, kitJson: PosCoreFixture.kit, priceMsat: 5000, clientRef: "sale-1", pin: "1234")
        guard case .refused("no_card", _, nil, nil) = out else { return XCTFail("\(out)") }
        XCTAssertEqual(card.apdus.map(Hex.encode), ["00a4040008f0464d434152440100"], "the applet is selected first, then the card is gone")
        XCTAssertEqual(try core.stats(), OfflineStats())
        XCTAssertEqual(try core.signingRefs(), [])
        // The PIN's shape is checked before the card is asked anything.
        let short = RecordingCard()
        let bad = core.charge(card: short, kitJson: PosCoreFixture.kit, priceMsat: 5000, clientRef: "sale-2", pin: "12")
        guard case .refused("bad_request", _, _, _) = bad else { return XCTFail("\(bad)") }
        XCTAssertTrue(short.apdus.isEmpty)
    }

    @MainActor
    func testOfflinePhoneTakesTheCardThroughTheCoreOffTheMainThread() async throws {
        let dir = try store(phoneId: PosCoreFixture.phoneId)
        let phone = OfflinePhone(directory: { dir }, prepare: { _ in }, open: { try PosCoreAdapter.open($0) })
        phone.tillChanged(to: Offline.base)
        await phone.start()
        XCTAssertTrue(phone.snapshot.enabled)
        let till = FakeTill()
        till.answer("/v1/offline/kit", 200, PosCoreFixture.kit)
        let client = TillClient(pairing: { TillPairing(baseURL: Offline.base, token: "tok") }, transport: till)
        let kept = await phone.refreshKit(client: client, federationId: String(repeating: "00", count: 32), tillURL: Offline.base)
        XCTAssertTrue(kept)
        let card = RecordingCard(lostAt: 0)
        let pin = Pin(bytes: Array("1234".utf8))
        let r = await phone.charge(card: card, priceMsat: 5000, clientRef: "sale-1", pin: pin)
        guard case .declined(.noCard, _, 0, nil) = r else { return XCTFail("\(r)") }
        XCTAssertTrue(pin.isWiped)
        XCTAssertEqual(card.apdus.count, 1)
        XCTAssertEqual(card.onMain, [false], "the core runs on the phone's own queue, never where the tag answers")
        XCTAssertEqual(phone.snapshot.stats, OfflineStats())
        XCTAssertTrue(phone.snapshot.localOnlyRefs.isEmpty, "the card left before it signed: nothing held")
    }
}
