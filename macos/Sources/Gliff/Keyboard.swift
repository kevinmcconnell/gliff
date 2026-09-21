// The local keyboard, as the server needs to know it.

import Carbon
import GliffInput

enum Keyboard {
    /// The current layout as `rmlvo:` names for the server's keymap.
    static func keymap() -> String {
        guard let source = TISCopyCurrentKeyboardLayoutInputSource()?.takeRetainedValue(),
              let raw = TISGetInputSourceProperty(source, kTISPropertyInputSourceID)
        else { return keymapNames(forInputSource: "") }
        let id = Unmanaged<CFString>.fromOpaque(raw).takeUnretainedValue() as String
        return keymapNames(forInputSource: id)
    }

    /// True for an ISO keyboard, whose section and grave keys report
    /// swapped codes.
    static var isISO: Bool {
        KBGetLayoutType(Int16(LMGetKbdType())) == kKeyboardISO
    }
}
