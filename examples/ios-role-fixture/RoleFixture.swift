import SwiftUI
import UIKit

/// The controls whose accessibility vocabulary decides a cell in glass's role matrix
/// (`glass_core::role_support::ROLE_SUPPORT`), built from stock UIKit classes. Reading this app
/// back through `idb ui describe-all` answers one question per control: what AX role string does
/// the Simulator report for it?
///
/// A role is marked unreachable in the matrix only where a control was watched to arrive
/// carrying no token for it — a stepper arriving as two buttons, a picker as a slider. This app
/// is how that is watched.
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
            segmented, stepper, progress, picker, alertButton, menuButton, table,
        ])
        stack.axis = .vertical
        stack.spacing = 8
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
        collectionView.accessibilityIdentifier = "the-collection"
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
            }
        }
    }
}

@main
final class AppDelegate: UIResponder, UIApplicationDelegate {
    var window: UIWindow?

    /// The tab named by a `--tab controls|collection|swiftui` launch argument, as an index into
    /// the tab controller. Anything else — absent, misspelled, out of range — is the first tab,
    /// so a typo shows a working screen rather than an empty one.
    private static func requestedTab() -> Int {
        let arguments = ProcessInfo.processInfo.arguments
        guard let flag = arguments.firstIndex(of: "--tab"), flag + 1 < arguments.count else {
            return 0
        }
        switch arguments[flag + 1] {
        case "collection": return 1
        case "swiftui": return 2
        default: return 0
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
