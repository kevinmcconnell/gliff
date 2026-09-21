// A stream's whole client-side video path: two decoders (one for Single420)
// and the recombine, writing into a small pool of output textures that the
// display hands back when it is done with them.

import CoreVideo
import Foundation
import Metal

/// A decoded frame. Its texture returns to the pool when the last reference
/// goes, so the display keeps it exactly as long as it still needs it.
public final class Frame {
    public let texture: MTLTexture
    private let pool: TexturePool

    init(texture: MTLTexture, pool: TexturePool) {
        self.texture = texture
        self.pool = pool
    }

    deinit {
        pool.give(back: texture)
    }
}

final class TexturePool {
    private let lock = NSLock()
    private var free: [MTLTexture]

    init(_ textures: [MTLTexture]) {
        free = textures
    }

    func take() -> MTLTexture? {
        lock.withLock { free.popLast() }
    }

    func give(back texture: MTLTexture) {
        lock.withLock { free.append(texture) }
    }
}

/// What the two concurrent decodes produced.
private final class DecodeResults: @unchecked Sendable {
    private let lock = NSLock()
    private(set) var pictures: [CVPixelBuffer?] = [nil, nil]
    private(set) var failure: Error?

    func set(_ stream: Int, _ picture: CVPixelBuffer?) {
        lock.withLock { pictures[stream] = picture }
    }

    func fail(_ error: Error) {
        lock.withLock { failure = error }
    }
}

public final class FramePipeline {
    public let width: Int
    public let height: Int
    public let dual: Bool
    private let recombiner: Recombiner
    private let main = H264Decoder()
    private let aux: H264Decoder?
    private let pool: TexturePool

    /// Frames in flight at once: the one on screen, one waiting to be drawn,
    /// and one being decoded, plus a spare.
    static let poolSize = 4

    public init(recombiner: Recombiner, width: Int, height: Int, dual: Bool) throws {
        self.recombiner = recombiner
        self.width = width
        self.height = height
        self.dual = dual
        aux = dual ? H264Decoder() : nil
        pool = TexturePool(try (0..<Self.poolSize).map { _ in
            try recombiner.makeOutput(width: width, height: height)
        })
    }

    /// Decode one frame. Returns nil when the access units gave no picture,
    /// or when every output texture is still in use (the display is behind;
    /// latest-wins drops this frame rather than waiting).
    public func decode(main mainUnit: Data, aux auxUnit: Data) throws -> Frame? {
        // The two streams are independent, so decode them at the same time.
        let results = DecodeResults()
        DispatchQueue.concurrentPerform(iterations: aux == nil ? 1 : 2) { stream in
            do {
                let picture = stream == 0 ? try main.decode(mainUnit) : try aux!.decode(auxUnit)
                results.set(stream, picture)
            } catch {
                results.fail(error)
            }
        }
        if let failure = results.failure {
            throw failure
        }
        let mainPicture = results.pictures[0]
        let auxPicture = results.pictures[1]
        guard let mainPicture, !dual || auxPicture != nil else {
            return nil
        }
        guard let texture = pool.take() else {
            return nil
        }
        let frame = Frame(texture: texture, pool: pool)
        try recombiner.recombine(main: mainPicture, aux: auxPicture, into: texture)
        return frame
    }
}
