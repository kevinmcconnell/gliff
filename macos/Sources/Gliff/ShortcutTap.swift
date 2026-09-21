// Capturing system shortcuts. A window never sees Cmd-Tab, Cmd-Space or the
// Mission Control keys: the system takes them first. A session event tap,
// active only while the remote screen has the keyboard, sees every key
// event before the system does, forwards it, and swallows it. The tap needs
// the Accessibility permission, so it is opt-in.

import AppKit
import ApplicationServices

final class ShortcutTap {
    /// Called on the main thread for each key event the tap takes. Return
    /// true to swallow the event.
    private let handler: (NSEvent.EventType, UInt16, UInt64) -> Bool
    /// Called when the tap stops working and cannot be revived.
    private let onFailure: () -> Void
    private var tap: CFMachPort?
    private var source: CFRunLoopSource?

    init(handler: @escaping (NSEvent.EventType, UInt16, UInt64) -> Bool, onFailure: @escaping () -> Void) {
        self.handler = handler
        self.onFailure = onFailure
    }

    /// Whether this app may tap events. With `prompt`, macOS asks the user
    /// (once) to allow it in System Settings.
    static func isAllowed(prompt: Bool) -> Bool {
        let options = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: prompt] as CFDictionary
        return AXIsProcessTrustedWithOptions(options)
    }

    var isActive: Bool { tap != nil }

    /// Start tapping. Returns false if the tap could not be created.
    @discardableResult
    func start() -> Bool {
        if tap != nil { return true }
        let mask = (1 << CGEventType.keyDown.rawValue) | (1 << CGEventType.keyUp.rawValue)
            | (1 << CGEventType.flagsChanged.rawValue)
        guard let tap = CGEvent.tapCreate(
            tap: .cgSessionEventTap, place: .headInsertEventTap, options: .defaultTap,
            eventsOfInterest: CGEventMask(mask),
            callback: { _, type, event, ctx in
                let this = Unmanaged<ShortcutTap>.fromOpaque(ctx!).takeUnretainedValue()
                return this.handle(type, event)
            },
            userInfo: Unmanaged.passUnretained(self).toOpaque()
        ) else { return false }
        let source = CFMachPortCreateRunLoopSource(kCFAllocatorDefault, tap, 0)
        CFRunLoopAddSource(CFRunLoopGetMain(), source, .commonModes)
        CGEvent.tapEnable(tap: tap, enable: true)
        self.tap = tap
        self.source = source
        return true
    }

    func stop() {
        guard let tap else { return }
        CGEvent.tapEnable(tap: tap, enable: false)
        if let source {
            CFRunLoopRemoveSource(CFRunLoopGetMain(), source, .commonModes)
        }
        CFMachPortInvalidate(tap)
        self.tap = nil
        source = nil
    }

    deinit {
        stop()
    }

    private func handle(_ type: CGEventType, _ event: CGEvent) -> Unmanaged<CGEvent>? {
        switch type {
        case .tapDisabledByTimeout, .tapDisabledByUserInput:
            // macOS turns off a tap it thinks is too slow. Turn it back on;
            // if that fails, give up and let events flow normally again.
            if let tap {
                CGEvent.tapEnable(tap: tap, enable: true)
                if !CGEvent.tapIsEnabled(tap: tap) {
                    stop()
                    onFailure()
                }
            }
            return Unmanaged.passUnretained(event)
        case .keyDown, .keyUp, .flagsChanged:
            let nsType: NSEvent.EventType = type == .keyDown ? .keyDown : type == .keyUp ? .keyUp : .flagsChanged
            let keyCode = UInt16(event.getIntegerValueField(.keyboardEventKeycode))
            return handler(nsType, keyCode, event.flags.rawValue) ? nil : Unmanaged.passUnretained(event)
        default:
            return Unmanaged.passUnretained(event)
        }
    }
}
