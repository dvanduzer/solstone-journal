// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "parakeet-helper",
    platforms: [.macOS(.v14), .iOS("26.0")],
    products: [
        .executable(name: "parakeet-helper", targets: ["parakeet-helper"])
    ],
    dependencies: [
        .package(
            url: "https://github.com/FluidInference/FluidAudio.git",
            exact: "0.14.0"
        )
    ],
    targets: [
        .target(
            name: "InstalledModels",
            dependencies: [.product(name: "FluidAudio", package: "FluidAudio")]
        ),
        .executableTarget(
            name: "parakeet-helper",
            dependencies: [
                .product(name: "FluidAudio", package: "FluidAudio"),
                "InstalledModels"
            ]
        ),
        .testTarget(
            name: "parakeet-helperTests",
            dependencies: ["InstalledModels"]
        )
    ]
)
