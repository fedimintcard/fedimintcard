// swift-tools-version:5.9
// The POS core (host/pos-core, docs/POS-OFFLINE.md) as a Swift package: the static libraries in
// FedimintCardPosCoreFFI.xcframework and the generated bindings in Sources/FedimintCardPosCore.
// ios-pos/scripts/build-poscore.sh builds both in one run (UniFFI stops the app when a library and
// its bindings come from different builds); neither is in git. A module of its own keeps the
// bindings' names (TransportError, PhoneStats) apart from the app's.
import Foundation
import PackageDescription

// Xcode resolves the package before any build phase runs, so this is where a checkout with no
// core built is told what to do.
let here = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
let built = ["FedimintCardPosCoreFFI.xcframework", "Sources/FedimintCardPosCore/FedimintCardPosCore.swift"]
    .allSatisfy { FileManager.default.fileExists(atPath: here.appendingPathComponent($0).path) }
if !built {
    fatalError("the POS core is not built: run ios-pos/scripts/build-poscore.sh")
}

let package = Package(
    name: "FedimintCardPosCore",
    platforms: [.iOS("26.5")],
    products: [
        .library(name: "FedimintCardPosCore", targets: ["FedimintCardPosCore"]),
    ],
    targets: [
        .binaryTarget(name: "FedimintCardPosCoreFFI", path: "FedimintCardPosCoreFFI.xcframework"),
        .target(name: "FedimintCardPosCore", dependencies: ["FedimintCardPosCoreFFI"], path: "Sources/FedimintCardPosCore"),
    ]
)
