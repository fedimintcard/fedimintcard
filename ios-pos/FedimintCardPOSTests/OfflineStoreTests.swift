import XCTest
@testable import FedimintCardPOS

/// The store's directory: out of backups, in the complete-protection class (docs/POS-OFFLINE.md
/// § The phone's store). The simulator has no data protection; a device must read back complete.
final class OfflineStoreTests: XCTestCase {
    private func tempDir() -> URL {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("store-\(UUID().uuidString)", isDirectory: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: dir) }
        return dir
    }

    func testPrepareMakesTheDirectoryAndReadsBothBack() throws {
        let dir = tempDir().appendingPathComponent("PosCore", isDirectory: true)
        try OfflineStore.prepare(dir)
        var isDir: ObjCBool = false
        XCTAssertTrue(FileManager.default.fileExists(atPath: dir.path, isDirectory: &isDir))
        XCTAssertTrue(isDir.boolValue)
        var check = URL(fileURLWithPath: dir.path, isDirectory: true)
        check.removeAllCachedResourceValues()
        XCTAssertEqual(try check.resourceValues(forKeys: [.isExcludedFromBackupKey]).isExcludedFromBackup, true)
        let p = try FileManager.default.attributesOfItem(atPath: dir.path)[.protectionKey] as? FileProtectionType
        if OfflineStore.onSimulator { XCTAssertTrue(p == nil || p == .complete) } else { XCTAssertEqual(p, .complete) }
        // Every launch prepares it again: an existing directory is fine.
        try OfflineStore.prepare(dir)
    }

    func testOnlyCompleteProtectionPassesOnADevice() {
        XCTAssertNoThrow(try OfflineStore.checkProtection(.complete, onSimulator: false))
        XCTAssertNoThrow(try OfflineStore.checkProtection(.complete, onSimulator: true))
        XCTAssertNoThrow(try OfflineStore.checkProtection(nil, onSimulator: true), "the simulator reads back no class")
        XCTAssertThrowsError(try OfflineStore.checkProtection(nil, onSimulator: false))
        XCTAssertThrowsError(try OfflineStore.checkProtection(.completeUntilFirstUserAuthentication, onSimulator: false))
        XCTAssertThrowsError(try OfflineStore.checkProtection(.completeUnlessOpen, onSimulator: true))
        XCTAssertThrowsError(try OfflineStore.checkProtection(FileProtectionType.none, onSimulator: false))
    }

    func testTheStoreIsUnderApplicationSupport() throws {
        let dir = try OfflineStore.defaultDirectory()
        XCTAssertEqual(dir.lastPathComponent, "PosCore")
        XCTAssertEqual(dir.deletingLastPathComponent().lastPathComponent, "Application Support")
    }
}
