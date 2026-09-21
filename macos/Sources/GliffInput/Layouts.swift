// The Mac's keyboard layout as xkb names, for the server to compile its
// keymap from. A Mac cannot produce xkb keymap text itself, so it sends
// `rmlvo:` names instead (see hypr-input's keymap module).

/// xkb layout and variant for common macOS input source IDs
/// (kTISPropertyInputSourceID).
let layouts: [String: (layout: String, variant: String)] = [
    "com.apple.keylayout.US": ("us", ""),
    "com.apple.keylayout.ABC": ("us", ""),
    "com.apple.keylayout.USExtended": ("us", ""),
    "com.apple.keylayout.USInternational-PC": ("us", "intl"),
    "com.apple.keylayout.Dvorak": ("us", "dvorak"),
    "com.apple.keylayout.Colemak": ("us", "colemak"),
    "com.apple.keylayout.British": ("gb", ""),
    "com.apple.keylayout.British-PC": ("gb", ""),
    "com.apple.keylayout.Irish": ("ie", ""),
    "com.apple.keylayout.Canadian": ("ca", "eng"),
    "com.apple.keylayout.Canadian-CSA": ("ca", ""),
    "com.apple.keylayout.Australian": ("au", ""),
    "com.apple.keylayout.Danish": ("dk", ""),
    "com.apple.keylayout.Norwegian": ("no", ""),
    "com.apple.keylayout.Swedish": ("se", ""),
    "com.apple.keylayout.Swedish-Pro": ("se", ""),
    "com.apple.keylayout.Finnish": ("fi", ""),
    "com.apple.keylayout.Icelandic": ("is", ""),
    "com.apple.keylayout.German": ("de", ""),
    "com.apple.keylayout.Austrian": ("at", ""),
    "com.apple.keylayout.SwissGerman": ("ch", ""),
    "com.apple.keylayout.SwissFrench": ("ch", "fr"),
    "com.apple.keylayout.French": ("fr", "mac"),
    "com.apple.keylayout.French-PC": ("fr", ""),
    "com.apple.keylayout.Belgian": ("be", ""),
    "com.apple.keylayout.Dutch": ("nl", ""),
    "com.apple.keylayout.Spanish": ("es", ""),
    "com.apple.keylayout.Spanish-ISO": ("es", ""),
    "com.apple.keylayout.Italian": ("it", ""),
    "com.apple.keylayout.Italian-Pro": ("it", ""),
    "com.apple.keylayout.Portuguese": ("pt", ""),
    "com.apple.keylayout.Brazilian": ("br", ""),
    "com.apple.keylayout.Brazilian-ABNT2": ("br", ""),
    "com.apple.keylayout.Polish": ("pl", ""),
    "com.apple.keylayout.PolishPro": ("pl", ""),
    "com.apple.keylayout.Czech": ("cz", ""),
    "com.apple.keylayout.Czech-QWERTY": ("cz", "qwerty"),
    "com.apple.keylayout.Hungarian": ("hu", ""),
    "com.apple.keylayout.Turkish": ("tr", ""),
    "com.apple.keylayout.Turkish-QWERTY-PC": ("tr", ""),
    "com.apple.keylayout.Greek": ("gr", ""),
    "com.apple.keylayout.Russian": ("ru", ""),
    "com.apple.keylayout.Russian-Phonetic": ("ru", "phonetic"),
    "com.apple.keylayout.Ukrainian": ("ua", ""),
    "com.apple.keylayout.Hebrew": ("il", ""),
    "com.apple.keylayout.Estonian": ("ee", ""),
    "com.apple.keylayout.Latvian": ("lv", ""),
    "com.apple.keylayout.Lithuanian": ("lt", ""),
    "com.apple.keylayout.Slovenian": ("si", ""),
    "com.apple.keylayout.Croatian": ("hr", ""),
    "com.apple.keylayout.Romanian": ("ro", ""),
]

/// The `rmlvo:` keymap string for a macOS input source ID. Unknown layouts
/// (and input methods such as Japanese kana, which have no xkb layout) fall
/// back to US.
public func keymapNames(forInputSource id: String) -> String {
    let (layout, variant) = layouts[id] ?? ("us", "")
    return "rmlvo:layout=\(layout);variant=\(variant)"
}
