// swift-tools-version: 6.3
import PackageDescription

let package = Package(
    name: "Mimir",
    platforms: [
        .macOS(.v14)
    ],
    products: [
        .library(name: "MimirCore", targets: ["MimirCore"]),
        .executable(name: "mimir-sc-tap", targets: ["MimirTap"])
    ],
    targets: [
        .target(
            name: "MimirCore",
            path: "Sources/MimirCore"
        ),
        .executableTarget(
            name: "MimirTap",
            dependencies: ["MimirCore"],
            path: "Sources/MimirTap"
        ),
        .executableTarget(
            name: "MimirCoreTests",
            dependencies: ["MimirCore"],
            path: "Tests/MimirCoreTests"
        )
    ]
)
