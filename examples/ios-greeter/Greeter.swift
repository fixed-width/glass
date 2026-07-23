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
            Text(greeting)
                .font(.title2)
                .accessibilityIdentifier("greeting")
        }
        .padding()
    }
}
