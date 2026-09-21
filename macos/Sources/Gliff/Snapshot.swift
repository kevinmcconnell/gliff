// `--snapshot`: write one decoded frame to a PNG, so a test can check what
// the client decoded without screen-recording permission.

import Foundation
import GliffVideo
import ImageIO
import Metal
import UniformTypeIdentifiers

final class Snapshot {
    private let path: String
    private let frame: Int
    private var seen = 0

    init(path: String, frame: Int) {
        self.path = path
        self.frame = frame
    }

    /// Called for every decoded frame, on the session's worker thread.
    func offer(_ decoded: Frame) {
        seen += 1
        guard seen == frame else { return }
        let texture = decoded.texture
        do {
            try Self.write(texture, to: path)
            FileHandle.standardError.write(Data("gliff: snapshot \(texture.width)x\(texture.height) written to \(path)\n".utf8))
        } catch {
            FileHandle.standardError.write(Data("gliff: snapshot failed: \(error)\n".utf8))
        }
    }

    static func write(_ texture: MTLTexture, to path: String) throws {
        let bytes = Recombiner.bytes(of: texture)
        guard let provider = CGDataProvider(data: Data(bytes) as CFData),
              let image = CGImage(
                  width: texture.width, height: texture.height, bitsPerComponent: 8, bitsPerPixel: 32,
                  bytesPerRow: texture.width * 4, space: CGColorSpaceCreateDeviceRGB(),
                  bitmapInfo: CGBitmapInfo(rawValue: CGBitmapInfo.byteOrder32Little.rawValue | CGImageAlphaInfo.noneSkipFirst.rawValue),
                  provider: provider, decode: nil, shouldInterpolate: false, intent: .defaultIntent
              ),
              let destination = CGImageDestinationCreateWithURL(
                  URL(fileURLWithPath: path) as CFURL, UTType.png.identifier as CFString, 1, nil
              )
        else { throw OptionError("cannot encode a PNG") }
        CGImageDestinationAddImage(destination, image, nil)
        guard CGImageDestinationFinalize(destination) else { throw OptionError("cannot write \(path)") }
    }
}
