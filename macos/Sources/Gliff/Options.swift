// Command-line options. The app is normally started from Finder with none;
// these let a terminal (or a test) connect straight away.

import Foundation

struct Options {
    enum Mode: Equatable {
        /// Mirror the remote's focused screen.
        case focused
        /// Mirror a named remote output.
        case output(String)
        /// A private remote screen sized to this window.
        case headless
    }

    var host: String?
    /// Dev: a `gliff-server --listen` address.
    var connect: String?
    var serverBin = "gliff-server"
    var mode = Mode.focused
    var lowBandwidth = false
    var captureSystemShortcuts: Bool?
    var verbose = false

    // Test hooks.
    /// Write the Nth decoded frame to this PNG.
    var snapshot: String?
    var snapshotFrame = 30
    /// Write what the view drew for its Nth frame to this PNG.
    var viewSnapshot: String?
    /// Quit after this many seconds.
    var exitAfter: Double?
    /// Once streaming, type this text on the remote (US layout, ASCII).
    var typeText: String?

    var serverArgs: [String] {
        var args: [String]
        switch mode {
        case .focused: args = ["--output", "auto"]
        case .output(let name): args = ["--output", name]
        case .headless: args = ["--headless"]
        }
        if lowBandwidth {
            args.append("--low-bandwidth")
        }
        return args
    }

    static let usage = """
    usage: Gliff [options] [user@host]

      --connect ADDR           dev: connect to gliff-server --listen ADDR
      --server-bin PATH        remote gliff-server path (default: on PATH)
      --headless               a private remote screen sized to this window
      --output NAME            mirror the named remote output
      --low-bandwidth          one 4:2:0 stream instead of 4:4:4
      --capture-shortcuts      also capture Cmd-Tab and other system keys
      --verbose                print stats and server logs to stderr
    """

    static func parse(_ arguments: [String]) throws -> Options {
        var o = Options()
        var args = arguments[...]
        func value(_ flag: String) throws -> String {
            guard let v = args.popFirst() else { throw OptionError("\(flag) needs a value") }
            return v
        }
        while let arg = args.popFirst() {
            switch arg {
            case "--connect": o.connect = try value(arg)
            case "--server-bin": o.serverBin = try value(arg)
            case "--headless": o.mode = .headless
            case "--output": o.mode = .output(try value(arg))
            case "--low-bandwidth": o.lowBandwidth = true
            case "--capture-shortcuts": o.captureSystemShortcuts = true
            case "--verbose": o.verbose = true
            case "--snapshot": o.snapshot = try value(arg)
            case "--snapshot-frame": o.snapshotFrame = Int(try value(arg)) ?? 30
            case "--view-snapshot": o.viewSnapshot = try value(arg)
            case "--exit-after": o.exitAfter = Double(try value(arg))
            case "--type": o.typeText = try value(arg)
            case "-h", "--help": throw OptionError(usage)
            // Finder and Xcode pass these; they are not ours.
            case _ where arg.hasPrefix("-psn_") || arg.hasPrefix("-NS") || arg.hasPrefix("-Apple"):
                _ = arg.hasPrefix("-psn_") ? nil : args.popFirst()
            case _ where arg.hasPrefix("-"): throw OptionError("unknown option \(arg)\n\n\(usage)")
            default: o.host = arg
            }
        }
        return o
    }
}

struct OptionError: Error, CustomStringConvertible {
    let description: String
    init(_ description: String) { self.description = description }
}
