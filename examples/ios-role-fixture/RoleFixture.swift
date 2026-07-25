import SwiftUI
import UIKit

/// The controls whose accessibility vocabulary decides a cell in glass's role matrix
/// (`glass_core::role_support::ROLE_SUPPORT`), built from stock UIKit classes. Reading this app
/// back through `idb ui describe-all --nested --json` — the format glass itself reads — answers
/// one question per control: what AX role string does the Simulator report for it?
///
/// Where a control exists to look at, that reading is what decides whether its cell is a gap or
/// unreachable: a stepper arriving as two buttons, a picker as a slider. It bounds the claim to
/// what idb exposes, which may be narrower than what UIKit publishes to VoiceOver.
final class ControlsViewController: UIViewController, UITableViewDataSource, UITableViewDelegate,
    UIPickerViewDataSource, UIPickerViewDelegate
{
    private let rows = ["Row one", "Row two", "Row three"]

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .systemBackground
        title = "Controls"

        let table = UITableView(frame: .zero, style: .insetGrouped)
        table.dataSource = self
        table.delegate = self
        table.accessibilityIdentifier = "the-table"

        let segmented = UISegmentedControl(items: ["Alpha", "Beta", "Gamma"])
        segmented.selectedSegmentIndex = 0
        segmented.accessibilityIdentifier = "the-segmented"

        let stepper = UIStepper()
        stepper.accessibilityIdentifier = "the-stepper"

        let progress = UIProgressView(progressViewStyle: .default)
        progress.progress = 0.4
        progress.accessibilityIdentifier = "the-progress"

        let picker = UIPickerView()
        picker.dataSource = self
        picker.delegate = self
        picker.accessibilityIdentifier = "the-picker"

        let sheetButton = UIButton(type: .system)
        sheetButton.setTitle("action sheet", for: .normal)
        sheetButton.accessibilityIdentifier = "the-sheet-button"
        sheetButton.addTarget(self, action: #selector(showSheet), for: .touchUpInside)

        let alertButton = UIButton(type: .system)
        alertButton.setTitle("alert dialog", for: .normal)
        alertButton.accessibilityIdentifier = "the-alert-button"
        alertButton.addTarget(self, action: #selector(showAlert), for: .touchUpInside)

        let menuButton = UIButton(type: .system)
        menuButton.setTitle("pull-down menu", for: .normal)
        menuButton.accessibilityIdentifier = "the-menu-button"
        menuButton.showsMenuAsPrimaryAction = true
        menuButton.menu = UIMenu(
            title: "Menu title",
            children: [
                UIAction(title: "Menu item one") { _ in },
                UIAction(title: "Menu item two") { _ in },
            ])

        let stack = UIStackView(arrangedSubviews: [
            segmented, stepper, progress, picker, sheetButton, alertButton, menuButton, table,
        ])
        stack.axis = .vertical
        stack.spacing = 8
        // Each screen names itself in the tree: the tab bar's items are not elements, so a
        // reading otherwise carries nothing saying which screen produced it.
        stack.accessibilityIdentifier = "screen-controls"
        stack.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 8),
            stack.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 8),
            stack.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -8),
            stack.bottomAnchor.constraint(equalTo: view.safeAreaLayoutGuide.bottomAnchor),
            picker.heightAnchor.constraint(equalToConstant: 100),
        ])
    }

    /// Presented without animation so a describe right after the tap reads the settled tree.
    @objc private func showAlert() {
        let alert = UIAlertController(
            title: "Dialog title", message: "Dialog message", preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default))
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel))
        present(alert, animated: false)
    }

    /// An action sheet as well as an alert: they are separate presentation styles, and one
    /// carrying no container token says nothing about the other.
    @objc private func showSheet() {
        let sheet = UIAlertController(
            title: "Sheet title", message: "Sheet message", preferredStyle: .actionSheet)
        sheet.addAction(UIAlertAction(title: "Sheet one", style: .default))
        sheet.addAction(UIAlertAction(title: "Cancel", style: .cancel))
        present(sheet, animated: false)
    }

    func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int {
        rows.count
    }

    func tableView(_ tableView: UITableView, cellForRowAt indexPath: IndexPath) -> UITableViewCell {
        let cell = UITableViewCell(style: .default, reuseIdentifier: nil)
        cell.textLabel?.text = rows[indexPath.row]
        // Row 0 is exposed as its own accessibility element while the rest keep UIKit's default,
        // so the tree shows whether an explicitly-exposed cell reports a different token.
        if indexPath.row == 0 {
            cell.isAccessibilityElement = true
            cell.accessibilityLabel = rows[indexPath.row]
        }
        return cell
    }

    func numberOfComponents(in pickerView: UIPickerView) -> Int { 1 }

    func pickerView(_ pickerView: UIPickerView, numberOfRowsInComponent component: Int) -> Int { 3 }

    func pickerView(_ pickerView: UIPickerView, titleForRow row: Int, forComponent component: Int)
        -> String?
    { "Pick \(row)" }
}

/// A collection view, whose cells are each their own accessibility element.
final class CollectionViewController: UICollectionViewController {
    init() {
        let layout = UICollectionViewFlowLayout()
        layout.itemSize = CGSize(width: 100, height: 100)
        super.init(collectionViewLayout: layout)
    }

    required init?(coder: NSCoder) { fatalError("not loaded from a nib") }

    override func viewDidLoad() {
        super.viewDidLoad()
        title = "Collection"
        collectionView.backgroundColor = .systemBackground
        collectionView.accessibilityIdentifier = "screen-collection"
        collectionView.register(UICollectionViewCell.self, forCellWithReuseIdentifier: "cell")
    }

    override func collectionView(
        _ collectionView: UICollectionView, numberOfItemsInSection section: Int
    ) -> Int { 4 }

    override func collectionView(
        _ collectionView: UICollectionView, cellForItemAt indexPath: IndexPath
    ) -> UICollectionViewCell {
        let cell = collectionView.dequeueReusableCell(withReuseIdentifier: "cell", for: indexPath)
        cell.backgroundColor = .secondarySystemBackground
        cell.isAccessibilityElement = true
        cell.accessibilityLabel = "Item \(indexPath.item)"
        return cell
    }
}

/// The same list and picker concepts expressed in SwiftUI rather than UIKit, since a SwiftUI
/// container could report a token its UIKit equivalent does not.
struct SwiftUIScreen: View {
    @State private var pick = 0

    var body: some View {
        List {
            Section("Section header") {
                Text("SwiftUI row one")
                Text("SwiftUI row two")
                Picker("Pick one", selection: $pick) {
                    Text("One").tag(0)
                    Text("Two").tag(1)
                }
                .pickerStyle(.inline)
                Picker("Pick menu", selection: $pick) {
                    Text("Menu one").tag(0)
                    Text("Menu two").tag(1)
                }
                .pickerStyle(.menu)
            }
        }
        .accessibilityIdentifier("screen-swiftui")
    }
}

@main
final class AppDelegate: UIResponder, UIApplicationDelegate {
    var window: UIWindow?

    /// The screen to show, as an index into the tab controller.
    ///
    /// Named by `ROLE_FIXTURE_TAB=controls|collection|swiftui` in the environment, or by a
    /// `--tab=<name>` launch argument. Either reaches the app through glass — `AppSpec::env`
    /// arrives as `SIMCTL_CHILD_*`, and `AppSpec::run`'s tail is forwarded to `simctl launch` —
    /// or through `simctl` driven by hand.
    ///
    /// Only the joined `--tab=<name>` form works: `simctl launch` forwards it, but drops the
    /// value of a separated `--tab <name>`, which would leave this reading a flag with nothing
    /// after it. That case is fatal here rather than silently first-tab, as is a name it does not
    /// recognize. This app exists to put one specific screen in front of a reader whose tree
    /// becomes evidence, and a tree carries no marker of which screen produced it — so falling
    /// back quietly would hand them a plausible reading of the wrong screen.
    private static func requestedTab() -> Int {
        let arguments = ProcessInfo.processInfo.arguments
        var requested = ProcessInfo.processInfo.environment["ROLE_FIXTURE_TAB"]
        if let inline = arguments.first(where: { $0.hasPrefix("--tab=") }) {
            requested = String(inline.dropFirst("--tab=".count))
        } else if arguments.contains("--tab") {
            fatalError("--tab needs its value joined: --tab=controls|collection|swiftui")
        }
        guard let name = requested else { return 0 }
        switch name {
        case "controls": return 0
        case "collection": return 1
        case "swiftui": return 2
        default:
            fatalError("unknown tab '\(name)' — use controls, collection or swiftui")
        }
    }

    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions options: [UIApplication.LaunchOptionsKey: Any]?
    ) -> Bool {
        let controls = UINavigationController(rootViewController: ControlsViewController())
        controls.tabBarItem = UITabBarItem(title: "Controls", image: nil, tag: 0)
        let collection = CollectionViewController()
        collection.tabBarItem = UITabBarItem(title: "Collection", image: nil, tag: 1)
        let swiftui = UIHostingController(rootView: SwiftUIScreen())
        swiftui.tabBarItem = UITabBarItem(title: "SwiftUI", image: nil, tag: 2)

        // The tab bar's items are not exposed as accessibility elements and a synthetic tap on
        // one does not switch tabs, so the screen to show is chosen at launch instead:
        // `simctl launch booted tech.fixedwidth.glassrolefixture --tab collection`.
        let tabs = UITabBarController()
        tabs.viewControllers = [controls, collection, swiftui]
        tabs.selectedIndex = Self.requestedTab()

        let window = UIWindow(frame: UIScreen.main.bounds)
        window.rootViewController = tabs
        window.makeKeyAndVisible()
        self.window = window
        return true
    }
}
