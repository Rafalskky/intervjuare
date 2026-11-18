// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "IntervjuareMacOSBridge",
    platforms: [.macOS(.v13)],
    products: [
        .executable(name: "intervjuare-macos-bridge", targets: ["IntervjuareMacOSBridge"])
    ],
    targets: [
        .executableTarget(name: "IntervjuareMacOSBridge")
    ]
)
