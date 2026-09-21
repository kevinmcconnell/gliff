import Testing

@testable import GliffInput

@Test func everyMacKeyMapsToADistinctEvdevCode() {
    for iso in [false, true] {
        var seen: [UInt32: UInt16] = [:]
        for key in ansiTable.keys {
            let code = evdevCode(forMacKey: key, iso: iso)!
            #expect(seen[code] == nil, "kVK \(key) and \(seen[code] ?? 0) both map to \(code) (iso: \(iso))")
            seen[code] = key
        }
    }
}

@Test func letterRowsFollowPcPositions() {
    // kVK_ANSI_A, kVK_ANSI_Q, kVK_ANSI_Z → KEY_A, KEY_Q, KEY_Z
    #expect(evdevCode(forMacKey: 0x00, iso: false) == 30)
    #expect(evdevCode(forMacKey: 0x0C, iso: false) == 16)
    #expect(evdevCode(forMacKey: 0x06, iso: false) == 44)
    #expect(evdevCode(forMacKey: 0x24, iso: false) == 28) // Return
    #expect(evdevCode(forMacKey: 0x3F, iso: false) == nil) // Fn
}

@Test func isoKeyboardsSwapSectionAndGrave() {
    #expect(evdevCode(forMacKey: 0x32, iso: false) == Evdev.grave)
    #expect(evdevCode(forMacKey: 0x0A, iso: true) == Evdev.grave)
    #expect(evdevCode(forMacKey: 0x32, iso: true) == Evdev.key102nd)
}

@Test func modifiersUseTheirSideBits() {
    // Left Command down: NSEventModifierFlagCommand plus NX_DEVICELCMDKEYMASK.
    #expect(modifierChange(keyCode: 0x37, flags: 0x10_0008, iso: false) == .press(Evdev.leftMeta))
    // Right Command released while the left one is still held.
    #expect(modifierChange(keyCode: 0x36, flags: 0x10_0008, iso: false) == .release(Evdev.rightMeta))
    #expect(modifierChange(keyCode: 0x3A, flags: 0x8_0020, iso: false) == .press(Evdev.leftAlt))
    #expect(modifierChange(keyCode: 0x39, flags: 0x1_0000, iso: false) == .tap(Evdev.capsLock))
    #expect(modifierChange(keyCode: 0x3F, flags: 0x80_0000, iso: false) == nil)
}

@Test func buttonsMapToEvdev() {
    #expect(evdevButton(0) == 0x110)
    #expect(evdevButton(1) == 0x111)
    #expect(evdevButton(2) == 0x112)
}

@Test func layoutsBecomeRmlvoNames() {
    #expect(keymapNames(forInputSource: "com.apple.keylayout.Danish") == "rmlvo:layout=dk;variant=")
    #expect(keymapNames(forInputSource: "com.apple.keylayout.Dvorak") == "rmlvo:layout=us;variant=dvorak")
    #expect(keymapNames(forInputSource: "com.apple.inputmethod.Kotoeri") == "rmlvo:layout=us;variant=")
}

@Test func typesUsText() {
    let strokes = usKeystrokes("aA1!\n")
    #expect(strokes.map(\.code) == [30, 30, 2, 2, 28])
    #expect(strokes.map(\.shift) == [false, true, false, true, false])
}
