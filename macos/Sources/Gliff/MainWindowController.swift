// The one window: a connection bar, the remote screen, and a status line.
// Owns the session, reconnects when it drops, keeps the remote sized to the
// window, bridges the clipboard, and decides where key events go.

import AppKit
import CGliff
import GliffInput
import GliffVideo

final class MainWindowController: NSWindowController, NSWindowDelegate, RemoteInput {
    private var options: Options
    private let recombiner: Recombiner
    private let video: VideoView
    private let hostField = NSTextField()
    private let modePopup = NSPopUpButton()
    private let connectButton = NSButton(title: "Connect", target: nil, action: nil)
    private let statsButton = NSButton()
    private let statusLabel = NSTextField(labelWithString: "Not connected")
    private let statsLabel = NSTextField(labelWithString: "")
    private let bar = NSStackView()

    private var session: Session?
    private var retries = 0
    private var reconnectTimer: Timer?
    private var resizeTimer: Timer?
    /// The size last asked of the server, so it is not asked twice.
    private var requestedSize = (width: 0, height: 0)
    private var clipboardTimer: Timer?
    private var pasteboardCount = NSPasteboard.general.changeCount
    /// Text we just put on the local clipboard from the remote, so it is not
    /// sent straight back.
    private var lastRemoteClip: String?
    private var keyMonitor: Any?
    private lazy var tap = ShortcutTap(
        handler: { [weak self] type, keyCode, flags in
            self?.tapped(type: type, keyCode: keyCode, flags: flags) ?? false
        },
        onFailure: { [weak self] in
            self?.statusLabel.stringValue = "System shortcut capture stopped working"
        }
    )

    static let maxRetries = 5
    static let captureShortcutsKey = "captureSystemShortcuts"

    var captureSystemShortcuts: Bool {
        get { UserDefaults.standard.bool(forKey: Self.captureShortcutsKey) }
        set {
            UserDefaults.standard.set(newValue, forKey: Self.captureShortcutsKey)
            updateTap()
        }
    }

    init(options: Options) throws {
        self.options = options
        recombiner = try Recombiner()
        video = try VideoView(device: recombiner.device)
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1280, height: 800),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered, defer: false
        )
        window.title = "Gliff"
        window.collectionBehavior = [.fullScreenPrimary]
        window.setFrameAutosaveName("Gliff")
        window.minSize = NSSize(width: 480, height: 320)
        super.init(window: window)
        window.delegate = self
        if let capture = options.captureSystemShortcuts {
            UserDefaults.standard.set(capture, forKey: Self.captureShortcutsKey)
        }
        buildContent(in: window)
        video.input = self
        video.isoKeyboard = Keyboard.isISO
        video.onResize = { [weak self] in self?.scheduleResize() }
        if let path = options.viewSnapshot {
            video.viewSnapshot = (options.snapshotFrame, { texture in
                do {
                    try Snapshot.write(texture, to: path)
                    FileHandle.standardError.write(Data("gliff: view snapshot \(texture.width)x\(texture.height) written to \(path)\n".utf8))
                } catch {
                    FileHandle.standardError.write(Data("gliff: view snapshot failed: \(error)\n".utf8))
                }
            })
        }
        installKeyMonitor()
        clipboardTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            self?.pollClipboard()
        }
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    private func buildContent(in window: NSWindow) {
        hostField.placeholderString = "user@host"
        hostField.stringValue = options.host ?? UserDefaults.standard.string(forKey: "lastHost") ?? ""
        hostField.target = self
        hostField.action = #selector(connectClicked)
        hostField.widthAnchor.constraint(greaterThanOrEqualToConstant: 220).isActive = true

        modePopup.addItems(withTitles: ["Mirror focused screen", "New headless screen"])
        if case .output(let name) = options.mode {
            modePopup.addItem(withTitle: "Mirror \(name)")
            modePopup.selectItem(at: 2)
        } else {
            modePopup.selectItem(at: options.mode == .headless ? 1 : 0)
        }

        connectButton.target = self
        connectButton.action = #selector(connectClicked)
        connectButton.keyEquivalent = "\r"

        statsButton.setButtonType(.pushOnPushOff)
        statsButton.bezelStyle = .texturedRounded
        statsButton.image = NSImage(systemSymbolName: "gauge.with.dots.needle.33percent", accessibilityDescription: "Stats")
        statsButton.toolTip = "Show stats"
        statsButton.target = self
        statsButton.action = #selector(toggleStats)

        bar.orientation = .horizontal
        bar.spacing = 8
        bar.edgeInsets = NSEdgeInsets(top: 8, left: 12, bottom: 8, right: 12)
        for v in [hostField, modePopup, connectButton, statsButton] as [NSView] {
            bar.addArrangedSubview(v)
        }
        hostField.setContentHuggingPriority(.defaultLow, for: .horizontal)

        statusLabel.lineBreakMode = .byTruncatingTail
        statusLabel.textColor = .secondaryLabelColor
        statusLabel.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        let statusRow = NSStackView(views: [statusLabel])
        statusRow.edgeInsets = NSEdgeInsets(top: 4, left: 12, bottom: 6, right: 12)

        statsLabel.font = .monospacedSystemFont(ofSize: 11, weight: .regular)
        statsLabel.textColor = .white
        statsLabel.drawsBackground = true
        statsLabel.backgroundColor = NSColor.black.withAlphaComponent(0.6)
        statsLabel.isHidden = true
        statsLabel.translatesAutoresizingMaskIntoConstraints = false
        video.addSubview(statsLabel)
        NSLayoutConstraint.activate([
            statsLabel.topAnchor.constraint(equalTo: video.topAnchor, constant: 8),
            statsLabel.leadingAnchor.constraint(equalTo: video.leadingAnchor, constant: 8),
        ])

        let stack = NSStackView(views: [bar, video, statusRow])
        stack.orientation = .vertical
        stack.spacing = 0
        stack.alignment = .leading
        stack.setHuggingPriority(.defaultLow, for: .vertical)
        for v in [bar, video, statusRow] as [NSView] {
            v.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
        }
        video.setContentHuggingPriority(.init(1), for: .vertical)
        video.setContentCompressionResistancePriority(.init(1), for: .vertical)
        window.contentView = stack
        window.initialFirstResponder = hostField
    }

    // MARK: Connecting

    @objc func connectClicked() {
        retries = 0
        connect()
    }

    @objc func disconnect() {
        reconnectTimer?.invalidate()
        session?.stop()
        session = nil
        video.clear()
        statusLabel.stringValue = "Disconnected"
    }

    private func connect() {
        reconnectTimer?.invalidate()
        session?.stop()
        session = nil
        video.clear()

        let target: SessionConfig.Target
        if let address = options.connect {
            target = .tcp(address)
            window?.title = "Gliff — \(address)"
        } else {
            let host = hostField.stringValue.trimmingCharacters(in: .whitespaces)
            guard !host.isEmpty else {
                statusLabel.stringValue = "Enter a host first"
                return
            }
            UserDefaults.standard.set(host, forKey: "lastHost")
            switch modePopup.indexOfSelectedItem {
            case 1: options.mode = .headless
            case 2: break // the named output from the command line
            default: options.mode = .focused
            }
            target = .ssh(host: host, serverBin: options.serverBin, serverArgs: options.serverArgs)
            window?.title = "Gliff — \(host)"
        }

        let (maxWidth, maxHeight) = Self.largestScreen()
        let config = SessionConfig(target: target, keymap: Keyboard.keymap(), maxWidth: maxWidth, maxHeight: maxHeight)
        requestedSize = (0, 0)
        statusLabel.stringValue = "Connecting…"
        session = Session(
            config: config, recombiner: recombiner, mailbox: video.mailbox,
            onStatus: { [weak self] in self?.handle($0) },
            onFrame: options.snapshot.map { path in
                let hook = Snapshot(path: path, frame: options.snapshotFrame)
                return { hook.offer($0) }
            }
        )
        if session?.isRunning != true {
            statusLabel.stringValue = "Could not start a session"
            session = nil
        }
    }

    /// The biggest screen in device pixels: the largest stream worth asking for.
    private static func largestScreen() -> (UInt32, UInt32) {
        var width = 1920.0
        var height = 1080.0
        for screen in NSScreen.screens {
            width = max(width, screen.frame.width * screen.backingScaleFactor)
            height = max(height, screen.frame.height * screen.backingScaleFactor)
        }
        return (UInt32(width), UInt32(height))
    }

    private func handle(_ status: SessionStatus) {
        switch status {
        case .connected(let width, let height, let scale):
            statusLabel.toolTip = nil
            if options.verbose, let window {
                let visible = window.occlusionState.contains(.visible)
                FileHandle.standardError.write(Data("gliff: window \(Int(window.frame.width))x\(Int(window.frame.height)) at \(window.backingScaleFactor)x, \(visible ? "visible" : "not visible") on \(window.screen?.localizedName ?? "no screen")\n".utf8))
            }
            video.streamSize = (width, height)
            video.remoteScale = scale
            retries = 0
            statusLabel.stringValue = "Connected — \(width)x\(height)" + (video.capturing ? " — \(captureHint)" : "")
            requestedSize = (0, 0)
            requestResize()
            typeTestTextOnce()
        case .stats(let fps, let mbit, let decodeMs):
            let line = String(format: "%.0f fps  %.1f Mbit/s  decode %.1f ms", fps, mbit, decodeMs)
            statsLabel.stringValue = line
            if options.verbose {
                FileHandle.standardError.write(Data("gliff: stats \(line)\n".utf8))
            }
        case .cursor(let width, let height, let hotX, let hotY, let bgra):
            video.remoteCursor = Self.cursor(width: width, height: height, hotX: hotX, hotY: hotY, bgra: bgra, scale: video.remoteScale)
        case .clipboard(let text):
            lastRemoteClip = text
            let pasteboard = NSPasteboard.general
            pasteboard.clearContents()
            pasteboard.setString(text, forType: .string)
            pasteboardCount = pasteboard.changeCount
            if options.verbose {
                FileHandle.standardError.write(Data("gliff: remote clipboard: \(text)\n".utf8))
            }
        case .log(let line):
            if options.verbose {
                FileHandle.standardError.write(Data("remote: \(line)\n".utf8))
            }
        case .error(let message):
            FileHandle.standardError.write(Data("gliff: \(message)\n".utf8))
            statusLabel.stringValue = Self.explain(message, host: hostField.stringValue)
            statusLabel.toolTip = message
            FileHandle.standardError.write(Data("gliff: status: \(statusLabel.stringValue)\n".utf8))
            // Retrying cannot fix a setup problem, only a dropped connection.
            if Self.isSetupProblem(message) {
                session?.stop()
                session = nil
            } else {
                scheduleReconnect()
            }
        case .closed:
            statusLabel.stringValue = "Disconnected"
            scheduleReconnect()
        }
    }

    /// ssh failures that need the user to change something, not a retry.
    private static let setupProblems = [
        "Host key verification failed", "Permission denied", "command not found",
        "Could not resolve hostname", "unsupported", "No such file",
    ]

    private static func isSetupProblem(_ message: String) -> Bool {
        setupProblems.contains { message.contains($0) }
    }

    /// Say what to do about the usual ways an ssh connection fails. The app
    /// runs ssh without a terminal, so ssh cannot ask about a new host key or
    /// a password itself.
    static func explain(_ message: String, host: String) -> String {
        let host = host.isEmpty ? "the host" : host
        if message.contains("Host key verification failed") {
            return "ssh does not know \(host)'s host key yet. Run `ssh \(host)` once in Terminal to check and accept it."
        }
        if message.contains("Permission denied") {
            return "\(host) refused this Mac's ssh key. Gliff needs key-based ssh login (there is no password prompt)."
        }
        if message.contains("command not found") || message.contains("No such file") {
            return "gliff-server is not installed or not on PATH on \(host) for ssh sessions."
        }
        if message.contains("Could not resolve hostname") {
            return "Cannot find \(host). Check the name, or use its IP address."
        }
        if message.contains("unsupported") {
            return "\(host) runs a gliff-server from a different version. Update both sides."
        }
        return "Error: \(message)"
    }

    private func scheduleReconnect() {
        session?.stop()
        session = nil
        retries += 1
        if retries > Self.maxRetries {
            statusLabel.stringValue += " — press Connect to retry"
            return
        }
        let attempt = retries
        reconnectTimer = Timer.scheduledTimer(withTimeInterval: 1.5, repeats: false) { [weak self] _ in
            self?.statusLabel.stringValue = "Reconnecting… (attempt \(attempt))"
            self?.connect()
        }
    }

    // MARK: Size

    private func scheduleResize() {
        resizeTimer?.invalidate()
        resizeTimer = Timer.scheduledTimer(withTimeInterval: 0.2, repeats: false) { [weak self] _ in
            self?.requestResize()
        }
    }

    /// Ask the server for a stream the size of the view, in device pixels.
    private func requestResize() {
        guard let session else { return }
        let (w, h) = video.pixelSize
        let size = (width: w & ~1, height: h & ~1)
        guard size.width >= 64, size.height >= 64,
              size != video.streamSize, size != requestedSize
        else { return }
        requestedSize = size
        session.resize(width: UInt32(size.width), height: UInt32(size.height), scale: Float(video.scale))
    }

    // MARK: Cursor

    private static func cursor(width: Int, height: Int, hotX: Int, hotY: Int, bgra: Data, scale: Double) -> NSCursor? {
        guard width > 0, height > 0, width <= 1024, height <= 1024, bgra.count >= width * height * 4 else { return nil }
        let visible = bgra.withUnsafeBytes { gliff_has_visible_shape($0.bindMemory(to: UInt8.self).baseAddress, bgra.count) }
        guard visible,
              let provider = CGDataProvider(data: bgra as CFData),
              let image = CGImage(
                  width: width, height: height, bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: width * 4,
                  space: CGColorSpaceCreateDeviceRGB(),
                  bitmapInfo: CGBitmapInfo(rawValue: CGBitmapInfo.byteOrder32Little.rawValue | CGImageAlphaInfo.first.rawValue),
                  provider: provider, decode: nil, shouldInterpolate: true, intent: .defaultIntent
              )
        else { return nil }
        // The image is in remote pixels; show it at the remote's logical size.
        // Hyprland sends the hotspot in logical units already.
        let size = NSSize(width: Double(width) / scale, height: Double(height) / scale)
        return NSCursor(image: NSImage(cgImage: image, size: size), hotSpot: NSPoint(x: hotX, y: hotY))
    }

    // MARK: Clipboard

    private func pollClipboard() {
        let pasteboard = NSPasteboard.general
        guard pasteboard.changeCount != pasteboardCount else { return }
        pasteboardCount = pasteboard.changeCount
        guard let text = pasteboard.string(forType: .string) else { return }
        // One-shot echo guard: skip only the value we just took from the
        // remote, so copying it again later still goes through.
        if text == lastRemoteClip {
            lastRemoteClip = nil
            return
        }
        session?.clipboard(text)
    }

    // MARK: Keyboard capture

    private var captureHint: String {
        "Shift+Esc releases the keyboard"
    }

    /// While the remote screen has the keyboard, every key event goes to it,
    /// including the Cmd chords the menu would otherwise take.
    private func installKeyMonitor() {
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .keyUp, .flagsChanged]) { [weak self] event in
            guard let self, video.capturing else { return event }
            if event.type == .keyDown, isReleaseHotkey(keyCode: event.keyCode, flags: event.modifierFlags) {
                releaseCapture()
                return nil
            }
            if event.type == .keyDown, event.isARepeat {
                return nil // the remote repeats keys itself
            }
            video.forwardKey(type: event.type, keyCode: event.keyCode, flags: UInt64(event.modifierFlags.rawValue))
            return nil
        }
    }

    private func isReleaseHotkey(keyCode: UInt16, flags: NSEvent.ModifierFlags) -> Bool {
        keyCode == 0x35 && flags.intersection([.shift, .command, .control, .option]) == .shift
    }

    private func releaseCapture() {
        window?.makeFirstResponder(hostField)
        statusLabel.stringValue = "Keyboard released — click the screen to capture it again"
    }

    /// Key events from the system-wide tap, while it runs.
    private func tapped(type: NSEvent.EventType, keyCode: UInt16, flags: UInt64) -> Bool {
        guard video.capturing, NSApp.isActive else { return false }
        if type == .keyDown, isReleaseHotkey(keyCode: keyCode, flags: NSEvent.ModifierFlags(rawValue: UInt(flags))) {
            releaseCapture()
            return true
        }
        video.forwardKey(type: type, keyCode: keyCode, flags: flags)
        return true
    }

    private func updateTap() {
        if captureSystemShortcuts, video.capturing, NSApp.isActive, ShortcutTap.isAllowed(prompt: true) {
            tap.start()
        } else {
            tap.stop()
        }
    }

    func captureChanged(_ captured: Bool) {
        if captured, session != nil {
            statusLabel.stringValue = "Keyboard captured — \(captureHint)"
        }
        // The responder change finishes after this returns.
        DispatchQueue.main.async { [weak self] in self?.updateTap() }
    }

    func windowDidResignKey(_ notification: Notification) {
        session?.releaseAllInput()
        tap.stop()
    }

    func windowDidBecomeKey(_ notification: Notification) {
        updateTap()
    }

    func windowWillEnterFullScreen(_ notification: Notification) {
        bar.isHidden = true
    }

    func windowWillExitFullScreen(_ notification: Notification) {
        bar.isHidden = false
    }

    func windowWillClose(_ notification: Notification) {
        disconnect()
        NSApp.terminate(nil)
    }

    @objc func toggleStats() {
        statsLabel.isHidden.toggle()
        statsButton.state = statsLabel.isHidden ? .off : .on
    }

    @objc func toggleCaptureShortcuts(_ sender: NSMenuItem) {
        captureSystemShortcuts.toggle()
        sender.state = captureSystemShortcuts ? .on : .off
    }

    // MARK: RemoteInput

    func key(_ code: UInt32, pressed: Bool) { session?.key(code, pressed: pressed) }
    func pointer(x: Double, y: Double) { session?.pointer(x: x, y: y) }
    func button(_ button: UInt32, pressed: Bool) { session?.button(button, pressed: pressed) }
    func axis(horizontal: Bool, value: Double, discrete: Int32?, stop: Bool) {
        session?.axis(horizontal: horizontal, value: value, discrete: discrete, stop: stop)
    }
    func releaseAllInput() { session?.releaseAllInput() }

    // MARK: Test hooks

    private var typedTestText = false

    /// `--type`: once streaming, point at the middle of the remote screen
    /// and type the text there.
    private func typeTestTextOnce() {
        guard let text = options.typeText, !typedTestText else { return }
        typedTestText = true
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { [weak self] in
            guard let self, let session else { return }
            let (w, h) = video.streamSize
            session.pointer(x: Double(w) / video.remoteScale / 2, y: Double(h) / video.remoteScale / 2)
            for (code, shift) in usKeystrokes(text) {
                if shift { session.key(Evdev.leftShift, pressed: true) }
                session.key(code, pressed: true)
                session.key(code, pressed: false)
                if shift { session.key(Evdev.leftShift, pressed: false) }
            }
            FileHandle.standardError.write(Data("gliff: typed \(text.debugDescription)\n".utf8))
        }
    }
}
