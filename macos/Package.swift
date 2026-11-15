// swift-tools-version: 6.3
import PackageDescription

let package = Package(
    name: "Mimir",
    platforms: [
        .macOS(.v14)
    ],
    products: [
        .library(name: "MimirCore", targets: ["MimirCore"]),
        .executable(name: "Mimir", targets: ["MimirApp"])
    ],
    targets: [
        .target(
            name: "MimirCore",
            path: "Sources/MimirCore"
        ),
        .executableTarget(
            name: "MimirApp",
            dependencies: ["MimirCore"],
            path: "Sources/MimirApp"
        ),
        .executableTarget(
            name: "MimirCoreTests",
            dependencies: ["MimirCore"],
            path: "Tests/MimirCoreTests"
        )
    ]
)
