import SwiftUI

@main
struct GreeterApp: App {
    var body: some Scene {
        WindowGroup { ContentView() }
    }
}

struct ContentView: View {
    @State private var name = ""
    @State private var greeting = "Enter a name"

    var body: some View {
        VStack(spacing: 24) {
            TextField("Name", text: $name)
                .textFieldStyle(.roundedBorder)
                .accessibilityIdentifier("nameField")
            Button("Greet") {
                greeting = name.isEmpty ? "Enter a name" : "Hello, \(name)!"
            }
            .accessibilityIdentifier("greetButton")
            // No accessibilityIdentifier: glass surfaces an element's identifier as its
            // name on iOS, so leaving it off lets the label's *text* be what a snapshot
            // shows — which is what we verify after tapping Greet.
            Text(greeting)
                .font(.title2)
        }
        .padding()
    }
}
