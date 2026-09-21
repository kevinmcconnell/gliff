import AppKit
import CGliff

final class AppDelegate: NSObject, NSApplicationDelegate {
    private let options: Options
    private var controller: MainWindowController?

    init(options: Options) {
        self.options = options
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        gliff_init_logging()
        NSApp.mainMenu = buildMenu()
        do {
            let controller = try MainWindowController(options: options)
            self.controller = controller
            controller.window?.center()
            controller.showWindow(nil)
            NSApp.activate(ignoringOtherApps: true)
            if options.host != nil || options.connect != nil {
                controller.connectClicked()
            }
        } catch {
            FileHandle.standardError.write(Data("gliff: cannot start: \(error)\n".utf8))
            let alert = NSAlert()
            alert.messageText = "Gliff cannot start"
            alert.informativeText = "\(error)"
            alert.runModal()
            NSApp.terminate(nil)
        }
        if let seconds = options.exitAfter {
            Timer.scheduledTimer(withTimeInterval: seconds, repeats: false) { _ in
                NSApp.terminate(nil)
            }
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        controller?.disconnect()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }

    private func buildMenu() -> NSMenu {
        let main = NSMenu()
        func submenu(_ title: String, _ items: [NSMenuItem]) {
            let item = NSMenuItem(title: title, action: nil, keyEquivalent: "")
            let menu = NSMenu(title: title)
            items.forEach(menu.addItem)
            item.submenu = menu
            main.addItem(item)
        }
        func item(_ title: String, _ action: Selector?, _ key: String = "", _ mods: NSEvent.ModifierFlags = .command) -> NSMenuItem {
            let item = NSMenuItem(title: title, action: action, keyEquivalent: key)
            item.keyEquivalentModifierMask = mods
            return item
        }

        submenu("Gliff", [
            item("About Gliff", #selector(NSApplication.orderFrontStandardAboutPanel(_:))),
            .separator(),
            item("Hide Gliff", #selector(NSApplication.hide(_:)), "h"),
            item("Hide Others", #selector(NSApplication.hideOtherApplications(_:)), "h", [.command, .option]),
            item("Show All", #selector(NSApplication.unhideAllApplications(_:))),
            .separator(),
            item("Quit Gliff", #selector(NSApplication.terminate(_:)), "q"),
        ])
        submenu("Edit", [
            item("Cut", #selector(NSText.cut(_:)), "x"),
            item("Copy", #selector(NSText.copy(_:)), "c"),
            item("Paste", #selector(NSText.paste(_:)), "v"),
            item("Select All", #selector(NSText.selectAll(_:)), "a"),
        ])
        let capture = item("Capture System Shortcuts", #selector(MainWindowController.toggleCaptureShortcuts(_:)))
        capture.state = UserDefaults.standard.bool(forKey: MainWindowController.captureShortcutsKey) ? .on : .off
        submenu("Connection", [
            item("Connect", #selector(MainWindowController.connectClicked), "k"),
            item("Disconnect", #selector(MainWindowController.disconnect), "d", [.command, .shift]),
            .separator(),
            capture,
        ])
        submenu("View", [
            item("Show Stats", #selector(MainWindowController.toggleStats), "s", [.command, .option]),
            item("Enter Full Screen", #selector(NSWindow.toggleFullScreen(_:)), "f", [.command, .control]),
        ])
        submenu("Window", [
            item("Minimize", #selector(NSWindow.performMiniaturize(_:)), "m"),
        ])
        return main
    }
}
