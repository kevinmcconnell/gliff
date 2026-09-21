// The remote screen: a CAMetalLayer that draws the latest decoded frame at
// one stream pixel per device pixel (shrunk to fit, never enlarged), and
// the view that turns local mouse and keyboard events into remote input.

import AppKit
import CGliff
import GliffInput
import GliffVideo
import Metal
import QuartzCore

/// Hands decoded frames from the session's worker thread to the view,
/// latest-wins: a frame that arrives before the previous one was drawn
/// replaces it.
final class FrameMailbox {
    private let lock = NSLock()
    private var pending: Frame?
    private var scheduled = false
    /// Called on the main thread when a frame is waiting.
    var onFrame: (() -> Void)?

    func post(_ frame: Frame) {
        let schedule = lock.withLock {
            pending = frame
            defer { scheduled = true }
            return !scheduled
        }
        if schedule {
            DispatchQueue.main.async { [self] in onFrame?() }
        }
    }

    func take() -> Frame? {
        lock.withLock {
            scheduled = false
            defer { pending = nil }
            return pending
        }
    }
}

/// Remote input, as the view produces it.
protocol RemoteInput: AnyObject {
    func key(_ code: UInt32, pressed: Bool)
    func pointer(x: Double, y: Double)
    func button(_ button: UInt32, pressed: Bool)
    func axis(horizontal: Bool, value: Double, discrete: Int32?, stop: Bool)
    func releaseAllInput()
    /// The view took or lost the keyboard.
    func captureChanged(_ captured: Bool)
}

final class VideoView: NSView {
    let mailbox = FrameMailbox()
    weak var input: RemoteInput?
    /// Called when the view's size in device pixels changes.
    var onResize: (() -> Void)?

    /// The stream being shown, in physical pixels, and the remote output's
    /// scale; pointer coordinates are sent in stream pixels / scale.
    var streamSize = (width: 0, height: 0)
    var remoteScale = 1.0

    var remoteCursor: NSCursor? {
        didSet { window?.invalidateCursorRects(for: self) }
    }

    /// True on Apple ISO keyboards, whose two keys left of 1 and right of
    /// the left Shift report swapped codes.
    var isoKeyboard = false

    private let device: MTLDevice
    private let queue: MTLCommandQueue
    private let pipeline: MTLRenderPipelineState
    private let sampler: MTLSamplerState
    private var current: Frame?
    /// Test hook: after this many frames are drawn, also render the view into
    /// an offscreen texture and hand it over.
    var viewSnapshot: (frame: Int, write: (MTLTexture) -> Void)?
    private var drawnFrames = 0
    private var metalLayer: CAMetalLayer { layer as! CAMetalLayer }

    init(device: MTLDevice) throws {
        self.device = device
        queue = device.makeCommandQueue()!
        let library = try device.makeLibrary(source: Self.shader, options: nil)
        let descriptor = MTLRenderPipelineDescriptor()
        descriptor.vertexFunction = library.makeFunction(name: "frame_vertex")
        descriptor.fragmentFunction = library.makeFunction(name: "frame_fragment")
        descriptor.colorAttachments[0].pixelFormat = .bgra8Unorm
        pipeline = try device.makeRenderPipelineState(descriptor: descriptor)
        let samplerDescriptor = MTLSamplerDescriptor()
        samplerDescriptor.minFilter = .linear
        samplerDescriptor.magFilter = .linear
        sampler = device.makeSamplerState(descriptor: samplerDescriptor)!
        super.init(frame: .zero)
        wantsLayer = true
        layerContentsRedrawPolicy = .never
        mailbox.onFrame = { [weak self] in self?.showPending() }
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override func makeBackingLayer() -> CALayer {
        let layer = CAMetalLayer()
        layer.device = device
        layer.pixelFormat = .bgra8Unorm
        layer.framebufferOnly = true
        layer.isOpaque = true
        layer.backgroundColor = NSColor.black.cgColor
        return layer
    }

    override var isFlipped: Bool { true }
    override var acceptsFirstResponder: Bool { true }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    /// Device pixels per point.
    var scale: Double { Double(window?.backingScaleFactor ?? NSScreen.main?.backingScaleFactor ?? 1) }

    /// The view's size in device pixels.
    var pixelSize: (width: Int, height: Int) {
        (Int(bounds.width * scale), Int(bounds.height * scale))
    }

    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        updateDrawableSize()
    }

    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        updateDrawableSize()
    }

    private func updateDrawableSize() {
        metalLayer.contentsScale = scale
        let size = CGSize(width: max(1, bounds.width * scale), height: max(1, bounds.height * scale))
        if metalLayer.drawableSize != size {
            metalLayer.drawableSize = size
            draw()
            onResize?()
        }
    }

    // MARK: Drawing

    private func showPending() {
        guard let frame = mailbox.take() else { return }
        current = frame
        draw()
    }

    /// Forget the last frame (a new session starts blank).
    func clear() {
        current = nil
        draw()
    }

    private func draw() {
        guard window != nil, let drawable = metalLayer.nextDrawable(),
              let commands = queue.makeCommandBuffer()
        else { return }
        let frame = current
        encode(frame, into: drawable.texture, commands)
        commands.present(drawable)
        if frame != nil {
            drawnFrames += 1
            if let snapshot = viewSnapshot, drawnFrames == snapshot.frame, let target = offscreen(like: drawable.texture) {
                encode(frame, into: target, commands)
                commands.addCompletedHandler { _ in snapshot.write(target) }
            }
        }
        // The frame's texture goes back to the decoder's pool only once the
        // GPU has finished reading it.
        commands.addCompletedHandler { _ in _ = frame }
        commands.commit()
    }

    /// Draw `frame` letterboxed into `target`, clearing the rest to black.
    private func encode(_ frame: Frame?, into target: MTLTexture, _ commands: MTLCommandBuffer) {
        let pass = MTLRenderPassDescriptor()
        pass.colorAttachments[0].texture = target
        pass.colorAttachments[0].loadAction = .clear
        pass.colorAttachments[0].clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)
        pass.colorAttachments[0].storeAction = .store
        guard let encoder = commands.makeRenderCommandEncoder(descriptor: pass) else { return }
        if let frame {
            let r = gliff_frame_rect(
                UInt32(frame.texture.width), UInt32(frame.texture.height), scale,
                bounds.width, bounds.height
            )
            // Snap to whole device pixels so a 1:1 frame samples texel centres.
            let s = scale
            encoder.setViewport(MTLViewport(
                originX: (r.x * s).rounded(), originY: (r.y * s).rounded(),
                width: (r.width * s).rounded(), height: (r.height * s).rounded(),
                znear: 0, zfar: 1
            ))
            encoder.setRenderPipelineState(pipeline)
            encoder.setFragmentTexture(frame.texture, index: 0)
            encoder.setFragmentSamplerState(sampler, index: 0)
            encoder.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4)
        }
        encoder.endEncoding()
    }

    private func offscreen(like texture: MTLTexture) -> MTLTexture? {
        let descriptor = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: texture.pixelFormat, width: texture.width, height: texture.height, mipmapped: false
        )
        descriptor.usage = [.renderTarget, .shaderRead]
        descriptor.storageMode = .shared
        return device.makeTexture(descriptor: descriptor)
    }

    static let shader = """
    #include <metal_stdlib>
    using namespace metal;

    struct Out { float4 position [[position]]; float2 uv; };

    vertex Out frame_vertex(uint id [[vertex_id]]) {
        float2 uv = float2(id & 1, id >> 1);
        Out out;
        out.position = float4(uv.x * 2 - 1, 1 - uv.y * 2, 0, 1);
        out.uv = uv;
        return out;
    }

    fragment float4 frame_fragment(Out in [[stage_in]], texture2d<float> tex [[texture(0)]],
                          sampler s [[sampler(0)]]) {
        return tex.sample(s, in.uv);
    }
    """

    // MARK: Pointer

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        trackingAreas.forEach(removeTrackingArea)
        addTrackingArea(NSTrackingArea(
            rect: .zero,
            options: [.mouseMoved, .activeInKeyWindow, .inVisibleRect, .cursorUpdate],
            owner: self
        ))
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: remoteCursor ?? .arrow)
    }

    override func cursorUpdate(with event: NSEvent) {
        (remoteCursor ?? .arrow).set()
    }

    /// A local point to the remote output's logical coordinates.
    private func remotePoint(_ event: NSEvent) -> (Double, Double) {
        let p = convert(event.locationInWindow, from: nil)
        var x = 0.0
        var y = 0.0
        gliff_to_remote(
            UInt32(streamSize.width), UInt32(streamSize.height), scale,
            bounds.width, bounds.height, remoteScale, p.x, p.y, &x, &y
        )
        return (x, y)
    }

    private func move(_ event: NSEvent) {
        let (x, y) = remotePoint(event)
        input?.pointer(x: x, y: y)
    }

    override func mouseMoved(with event: NSEvent) { move(event) }
    override func mouseDragged(with event: NSEvent) { move(event) }
    override func rightMouseDragged(with event: NSEvent) { move(event) }
    override func otherMouseDragged(with event: NSEvent) { move(event) }

    private func press(_ event: NSEvent, _ pressed: Bool) {
        if pressed, window?.firstResponder !== self {
            window?.makeFirstResponder(self)
        }
        move(event)
        input?.button(evdevButton(event.buttonNumber), pressed: pressed)
    }

    override func mouseDown(with event: NSEvent) { press(event, true) }
    override func mouseUp(with event: NSEvent) { press(event, false) }
    override func rightMouseDown(with event: NSEvent) { press(event, true) }
    override func rightMouseUp(with event: NSEvent) { press(event, false) }
    override func otherMouseDown(with event: NSEvent) { press(event, true) }
    override func otherMouseUp(with event: NSEvent) { press(event, false) }

    override func scrollWheel(with event: NSEvent) {
        // macOS has already applied the natural-scrolling preference, so the
        // content moves the way the deltas say; the remote's positive axis
        // scrolls down and right, the opposite sign.
        let ended = event.phase == .ended || event.phase == .cancelled || event.momentumPhase == .ended
        if event.hasPreciseScrollingDeltas {
            // A trackpad or Magic Mouse: continuous, in points.
            if event.scrollingDeltaY != 0 {
                input?.axis(horizontal: false, value: -event.scrollingDeltaY, discrete: nil, stop: false)
            }
            if event.scrollingDeltaX != 0 {
                input?.axis(horizontal: true, value: -event.scrollingDeltaX, discrete: nil, stop: false)
            }
            if ended {
                input?.axis(horizontal: false, value: 0, discrete: nil, stop: true)
            }
        } else {
            // A wheel: whole notches, 15 units each as on Linux.
            for (horizontal, delta) in [(false, event.scrollingDeltaY), (true, event.scrollingDeltaX)] where delta != 0 {
                let notches = -delta.rounded(.awayFromZero)
                input?.axis(horizontal: horizontal, value: notches * 15, discrete: Int32(notches), stop: false)
            }
        }
    }

    // MARK: Keyboard

    override func becomeFirstResponder() -> Bool {
        input?.captureChanged(true)
        return true
    }

    override func resignFirstResponder() -> Bool {
        input?.releaseAllInput()
        input?.captureChanged(false)
        return true
    }

    /// Whether key events go to the remote right now.
    var capturing: Bool {
        window?.isKeyWindow == true && window?.firstResponder === self
    }

    /// Forward one key event: a key press or release, or a modifier change.
    /// `flags` is the raw modifier state after the event.
    func forwardKey(type: NSEvent.EventType, keyCode: UInt16, flags: UInt64) {
        switch type {
        case .keyDown, .keyUp:
            if let code = evdevCode(forMacKey: keyCode, iso: isoKeyboard) {
                input?.key(code, pressed: type == .keyDown)
            }
        case .flagsChanged:
            switch modifierChange(keyCode: keyCode, flags: flags, iso: isoKeyboard) {
            case .press(let code): input?.key(code, pressed: true)
            case .release(let code): input?.key(code, pressed: false)
            case .tap(let code):
                input?.key(code, pressed: true)
                input?.key(code, pressed: false)
            case nil: break
            }
        default:
            break
        }
    }
}
