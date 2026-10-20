// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "FloonetPanel",
    platforms: [.macOS(.v14)],
    targets: [
        // No linked libraries. It used to link sqlite3, because the panel
        // opened ~/.teleport/teleport.db itself; it now speaks to the daemon
        // over ~/.teleport/tpd.sock and Network.framework comes from the SDK.
        .executableTarget(name: "FloonetPanel")
    ]
)
