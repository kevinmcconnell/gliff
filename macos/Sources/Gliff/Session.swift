// One connection to a gliff server: the Rust session core (libgliff_ffi)
// runs the protocol on its own thread and calls back here to decode each
// frame and to report status.
//
// Threading: `configure` and `decode` run on the session's worker thread,
// which owns the decoders. Status reports hop to the main thread, and are
// dropped once the session has been stopped, so a stale report from an old
// session never reaches the UI.

import AppKit
import CGliff
import GliffVideo

struct SessionConfig {
    enum Target {
        case ssh(host: String, serverBin: String, serverArgs: [String])
        case tcp(String)
    }

    var target: Target
    var keymap: String
    var maxWidth: UInt32
    var maxHeight: UInt32
}

enum SessionStatus {
    case connected(width: Int, height: Int, scale: Double)
    case stats(fps: Float, mbit: Float, decodeMs: Float)
    case cursor(width: Int, height: Int, hotX: Int, hotY: Int, bgra: Data)
    case clipboard(String)
    case log(String)
    case error(String)
    case closed
}

final class Session {
    private var handle: OpaquePointer?
    private let recombiner: Recombiner
    private let mailbox: FrameMailbox
    private let onStatus: (SessionStatus) -> Void
    private let onFrame: ((Frame) -> Void)?
    /// Owned by the worker thread.
    private var pipeline: FramePipeline?
    /// Read and written on the main thread only.
    private var stopped = false

    /// Start a session. `onStatus` runs on the main thread; `onFrame`, when
    /// given, runs on the worker thread for every decoded frame (for
    /// snapshots).
    init(
        config: SessionConfig, recombiner: Recombiner, mailbox: FrameMailbox,
        onStatus: @escaping (SessionStatus) -> Void, onFrame: ((Frame) -> Void)? = nil
    ) {
        self.recombiner = recombiner
        self.mailbox = mailbox
        self.onStatus = onStatus
        self.onFrame = onFrame

        let callbacks = GliffCallbacks(
            ctx: Unmanaged.passRetained(self).toOpaque(),
            configure: { ctx, width, height, dual in
                Session.from(ctx).configure(width: Int(width), height: Int(height), dual: dual)
            },
            decode: { ctx, main, mainLen, aux, auxLen in
                Session.from(ctx).decode(
                    main: Data(bytesNoCopy: UnsafeMutableRawPointer(mutating: main!), count: mainLen, deallocator: .none),
                    aux: auxLen > 0
                        ? Data(bytesNoCopy: UnsafeMutableRawPointer(mutating: aux!), count: auxLen, deallocator: .none)
                        : Data()
                )
            },
            status: { ctx, status in
                Session.from(ctx).report(status!.pointee)
            }
        )

        var owned: [UnsafeMutablePointer<CChar>] = []
        func cString(_ s: String) -> UnsafePointer<CChar> {
            let p = strdup(s)!
            owned.append(p)
            return UnsafePointer(p)
        }
        defer { owned.forEach { free($0) } }

        var c = GliffConfig()
        c.keymap = cString(config.keymap)
        c.max_width = config.maxWidth
        c.max_height = config.maxHeight
        switch config.target {
        case .tcp(let address):
            c.tcp = cString(address)
            handle = gliff_session_start(&c, callbacks)
        case .ssh(let host, let serverBin, let serverArgs):
            c.host = cString(host)
            c.server_bin = cString(serverBin)
            var args: [UnsafePointer<CChar>?] = serverArgs.map { cString($0) }
            args.withUnsafeMutableBufferPointer { buffer in
                c.server_args = UnsafePointer(buffer.baseAddress)
                c.server_args_len = buffer.count
                handle = gliff_session_start(&c, callbacks)
            }
        }
        if handle == nil {
            Unmanaged<Session>.fromOpaque(callbacks.ctx).release()
        }
    }

    var isRunning: Bool { handle != nil && !stopped }

    /// Stop and wait for the worker. Main thread only.
    func stop() {
        guard let handle, !stopped else { return }
        stopped = true
        gliff_release_all_input(handle)
        gliff_session_stop(handle)
        self.handle = nil
        // The worker is gone, so its reference to us can go too.
        Unmanaged.passUnretained(self).release()
    }

    // MARK: Input, from the main thread

    @discardableResult
    func key(_ code: UInt32, pressed: Bool) -> Bool {
        guard let handle, !stopped else { return false }
        return gliff_send_key(handle, code, pressed)
    }

    func pointer(x: Double, y: Double) {
        guard let handle, !stopped else { return }
        gliff_send_pointer_motion(handle, x, y)
    }

    func button(_ button: UInt32, pressed: Bool) {
        guard let handle, !stopped else { return }
        gliff_send_pointer_button(handle, button, pressed)
    }

    func axis(horizontal: Bool, value: Double, discrete: Int32?, stop: Bool) {
        guard let handle, !stopped else { return }
        gliff_send_axis(handle, horizontal, value, discrete != nil, discrete ?? 0, stop)
    }

    func releaseAllInput() {
        guard let handle, !stopped else { return }
        gliff_release_all_input(handle)
    }

    func resize(width: UInt32, height: UInt32, scale: Float) {
        guard let handle, !stopped else { return }
        gliff_send_resize(handle, width, height, scale)
    }

    func clipboard(_ text: String) {
        guard let handle, !stopped else { return }
        var bytes = Array(text.utf8)
        gliff_send_clipboard_text(handle, &bytes, bytes.count)
    }

    // MARK: Callbacks, on the worker thread

    private static func from(_ ctx: UnsafeMutableRawPointer?) -> Session {
        Unmanaged<Session>.fromOpaque(ctx!).takeUnretainedValue()
    }

    private func configure(width: Int, height: Int, dual: Bool) -> Bool {
        pipeline = nil
        do {
            pipeline = try FramePipeline(recombiner: recombiner, width: width, height: height, dual: dual)
            return true
        } catch {
            log("cannot create a \(width)x\(height) decoder: \(error)")
            return false
        }
    }

    private func decode(main: Data, aux: Data) -> Int32 {
        guard let pipeline else { return GLIFF_DECODE_ERROR }
        do {
            guard let frame = try pipeline.decode(main: main, aux: aux) else {
                return GLIFF_DECODE_NONE
            }
            onFrame?(frame)
            mailbox.post(frame)
            return GLIFF_DECODE_SHOWN
        } catch {
            log("decode failed: \(error)")
            return GLIFF_DECODE_ERROR
        }
    }

    private func report(_ s: GliffStatus) {
        let data = s.data.map { Data(bytes: $0, count: s.len) } ?? Data()
        let text = String(decoding: data, as: UTF8.self)
        let status: SessionStatus
        switch s.kind {
        case GLIFF_STATUS_KIND_CONNECTED:
            status = .connected(width: Int(s.width), height: Int(s.height), scale: Double(s.scale_milli) / 1000)
        case GLIFF_STATUS_KIND_STATS:
            status = .stats(fps: s.fps, mbit: s.mbit, decodeMs: s.decode_ms)
        case GLIFF_STATUS_KIND_CURSOR:
            status = .cursor(width: Int(s.width), height: Int(s.height), hotX: Int(s.hot_x), hotY: Int(s.hot_y), bgra: data)
        case GLIFF_STATUS_KIND_CLIPBOARD:
            status = .clipboard(text)
        case GLIFF_STATUS_KIND_LOG:
            status = .log(text)
        case GLIFF_STATUS_KIND_ERROR:
            status = .error(text)
        default:
            status = .closed
        }
        DispatchQueue.main.async { [self] in
            if !stopped {
                onStatus(status)
            }
        }
    }

    private func log(_ message: String) {
        FileHandle.standardError.write(Data("gliff: \(message)\n".utf8))
    }
}
