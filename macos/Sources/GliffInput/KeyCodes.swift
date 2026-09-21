// macOS virtual key codes (kVK_*, which name physical positions on an
// ANSI keyboard) to the Linux evdev codes the server's virtual keyboard
// takes. The server applies the keymap, so this only has to say which
// physical key moved.

/// Linux evdev key codes used below (linux/input-event-codes.h).
public enum Evdev {
    public static let esc: UInt32 = 1
    public static let grave: UInt32 = 41
    public static let leftShift: UInt32 = 42
    public static let rightShift: UInt32 = 54
    public static let leftCtrl: UInt32 = 29
    public static let rightCtrl: UInt32 = 97
    public static let leftAlt: UInt32 = 56
    public static let rightAlt: UInt32 = 100
    public static let leftMeta: UInt32 = 125
    public static let rightMeta: UInt32 = 126
    public static let capsLock: UInt32 = 58
    public static let key102nd: UInt32 = 86

    public static let buttonLeft: UInt32 = 0x110
    public static let buttonRight: UInt32 = 0x111
    public static let buttonMiddle: UInt32 = 0x112
    public static let buttonSide: UInt32 = 0x113
    public static let buttonExtra: UInt32 = 0x114
}

/// The key left of 1 and the key right of the left Shift on an ISO
/// keyboard. Apple's ISO keyboards report the first as kVK_ISO_Section and
/// the second as kVK_ANSI_Grave, the reverse of their PC positions.
let isoSection: UInt16 = 0x0A
let ansiGrave: UInt16 = 0x32

/// Every mapped key, by kVK code, for an ANSI keyboard.
let ansiTable: [UInt16: UInt32] = [
    0x00: 30, 0x01: 31, 0x02: 32, 0x03: 33, 0x04: 35, 0x05: 34, // A S D F H G
    0x06: 44, 0x07: 45, 0x08: 46, 0x09: 47, 0x0B: 48, // Z X C V B
    0x0C: 16, 0x0D: 17, 0x0E: 18, 0x0F: 19, 0x10: 21, 0x11: 20, // Q W E R Y T
    0x12: 2, 0x13: 3, 0x14: 4, 0x15: 5, 0x16: 7, 0x17: 6, // 1 2 3 4 6 5
    0x18: 13, 0x19: 10, 0x1A: 8, 0x1B: 12, 0x1C: 9, 0x1D: 11, // = 9 7 - 8 0
    0x1E: 27, 0x1F: 24, 0x20: 22, 0x21: 26, 0x22: 23, 0x23: 25, // ] O U [ I P
    0x24: 28, 0x25: 38, 0x26: 36, 0x27: 40, 0x28: 37, 0x29: 39, // Return L J ' K ;
    0x2A: 43, 0x2B: 51, 0x2C: 53, 0x2D: 49, 0x2E: 50, 0x2F: 52, // \ , / N M .
    0x30: 15, 0x31: 57, 0x32: 41, 0x33: 14, 0x35: 1, // Tab Space ` Backspace Esc
    0x0A: 86, // ISO section, placed as on a PC ISO keyboard
    0x36: Evdev.rightMeta, 0x37: Evdev.leftMeta, // Command
    0x38: Evdev.leftShift, 0x3C: Evdev.rightShift,
    0x39: Evdev.capsLock,
    0x3A: Evdev.leftAlt, 0x3D: Evdev.rightAlt, // Option
    0x3B: Evdev.leftCtrl, 0x3E: Evdev.rightCtrl,
    0x7A: 59, 0x78: 60, 0x63: 61, 0x76: 62, 0x60: 63, 0x61: 64, // F1-F6
    0x62: 65, 0x64: 66, 0x65: 67, 0x6D: 68, 0x67: 87, 0x6F: 88, // F7-F12
    0x69: 183, 0x6B: 184, 0x71: 185, 0x6A: 186, 0x40: 187, // F13-F17
    0x4F: 188, 0x50: 189, 0x5A: 190, // F18-F20
    0x72: 110, 0x73: 102, 0x74: 104, 0x75: 111, 0x77: 107, 0x79: 109, // Help(Insert) Home PgUp Del End PgDn
    0x7B: 105, 0x7C: 106, 0x7D: 108, 0x7E: 103, // Left Right Down Up
    0x41: 83, 0x43: 55, 0x45: 78, 0x47: 69, 0x4B: 98, 0x4C: 96, // KP . * + Clear/NumLock / Enter
    0x4E: 74, 0x51: 117, 0x52: 82, 0x53: 79, 0x54: 80, 0x55: 81, // KP - = 0 1 2 3
    0x56: 75, 0x57: 76, 0x58: 77, 0x59: 71, 0x5B: 72, 0x5C: 73, // KP 4 5 6 7 8 9
    0x48: 115, 0x49: 114, 0x4A: 113, // Volume up, down, mute
    0x6E: 127, // Context menu (Compose)
    0x5D: 124, 0x5E: 89, 0x5F: 95, 0x66: 94, 0x68: 92, // JIS Yen, Ro, KP comma, Eisu, Kana
]

/// The evdev code for a macOS key code, or nil for keys the server has no
/// use for (Fn). `iso` swaps the two keys Apple's ISO keyboards report
/// swapped.
public func evdevCode(forMacKey keyCode: UInt16, iso: Bool) -> UInt32? {
    if iso {
        if keyCode == isoSection { return Evdev.grave }
        if keyCode == ansiGrave { return Evdev.key102nd }
    }
    return ansiTable[keyCode]
}

/// The device-dependent modifier bits (NX_DEVICE*KEYMASK) that say which
/// side of a modifier is down, in `NSEvent.modifierFlags.rawValue` and
/// `CGEventFlags.rawValue`.
let modifierBits: [UInt16: UInt64] = [
    0x3B: 0x0000_0001, // left Control
    0x38: 0x0000_0002, // left Shift
    0x3C: 0x0000_0004, // right Shift
    0x37: 0x0000_0008, // left Command
    0x36: 0x0000_0010, // right Command
    0x3A: 0x0000_0020, // left Option
    0x3D: 0x0000_0040, // right Option
    0x3E: 0x0000_2000, // right Control
]

/// What a modifier change (a flagsChanged event) means for the remote.
public enum ModifierChange: Equatable {
    case press(UInt32)
    case release(UInt32)
    /// Caps Lock reports each toggle once, so it is a press and a release.
    case tap(UInt32)
}

/// Interpret a flagsChanged event: the key that changed and the flags after
/// the change. Nil for keys that are not forwarded (Fn).
public func modifierChange(keyCode: UInt16, flags: UInt64, iso: Bool) -> ModifierChange? {
    guard let code = evdevCode(forMacKey: keyCode, iso: iso) else { return nil }
    if code == Evdev.capsLock {
        return .tap(code)
    }
    guard let bit = modifierBits[keyCode] else { return nil }
    return flags & bit != 0 ? .press(code) : .release(code)
}

/// Mouse button number (NSEvent.buttonNumber) to evdev BTN_*.
public func evdevButton(_ number: Int) -> UInt32 {
    switch number {
    case 0: Evdev.buttonLeft
    case 1: Evdev.buttonRight
    case 2: Evdev.buttonMiddle
    case 3: Evdev.buttonSide
    case 4: Evdev.buttonExtra
    default: Evdev.buttonMiddle
    }
}

/// Keystrokes that type `text` on a US layout: evdev codes, and whether
/// Shift is held. Characters with no US key are skipped. For tests.
public func usKeystrokes(_ text: String) -> [(code: UInt32, shift: Bool)] {
    let plain = Array("1234567890-=qwertyuiop[]asdfghjkl;'`\\zxcvbnm,./ \n\t")
    let shifted = Array("!@#$%^&*()_+QWERTYUIOP{}ASDFGHJKL:\"~|ZXCVBNM<>?")
    let codes: [UInt32] = [
        2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13,
        16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
        30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 43,
        44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 57, 28, 15,
    ]
    return text.compactMap { c in
        if let i = plain.firstIndex(of: c) { return (codes[i], false) }
        if let i = shifted.firstIndex(of: c) { return (codes[i], true) }
        return nil
    }
}
