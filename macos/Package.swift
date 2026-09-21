// swift-tools-version:5.10
import PackageDescription

let package = Package(
    name: "Gliff",
    platforms: [.macOS(.v14)],
    targets: [
        // VideoToolbox decode and the Metal AVC444 recombine.
        .target(name: "GliffVideo"),
        .testTarget(name: "GliffVideoTests", dependencies: ["GliffVideo"]),
    ]
)
