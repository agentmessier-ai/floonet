import SwiftUI

@main
struct FloonetPanelApp: App {
    var body: some Scene {
        MenuBarExtra("Floonet", systemImage: "arrow.left.arrow.right") {
            ContentView()
        }
        .menuBarExtraStyle(.window)
    }
}
