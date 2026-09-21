// Decode streams recorded from gliff's Vulkan encoder and check the result
// against what gliff's Vulkan decoder and recombine produced for the same
// bytes. Record fixtures on a Linux machine with
//
//   gliff-probe roundtrip --width 1366 --height 768 --frames 8 --dump <dir>/dual-1366x768
//
// and point GLIFF_VT_FIXTURES at <dir>. H.264 decoding is bit-exact, so the
// only difference allowed is rounding in the colour conversion.

import CoreVideo
import Foundation
import Metal
import Testing

@testable import GliffVideo

let fixturesRoot = ProcessInfo.processInfo.environment["GLIFF_VT_FIXTURES"].map {
    URL(fileURLWithPath: $0)
}

func fixtureDirectories() -> [URL] {
    guard let root = fixturesRoot,
          let entries = try? FileManager.default.contentsOfDirectory(
              at: root, includingPropertiesForKeys: nil
          )
    else { return [] }
    return entries.filter { FileManager.default.fileExists(atPath: $0.appendingPathComponent("meta.txt").path) }
        .sorted { $0.lastPathComponent < $1.lastPathComponent }
}

struct Fixture {
    let width: Int
    let height: Int
    let dual: Bool
    /// Each frame's main and auxiliary access units, and whether it is a keyframe.
    let frames: [(main: Data, aux: Data, keyframe: Bool)]

    init(_ dir: URL) throws {
        let meta = try String(contentsOf: dir.appendingPathComponent("meta.txt"), encoding: .utf8)
            .split(separator: " ").compactMap { Int($0.trimmingCharacters(in: .whitespacesAndNewlines)) }
        width = meta[0]
        height = meta[1]
        dual = meta[2] != 0
        let main = try Data(contentsOf: dir.appendingPathComponent("main.h264"))
        let aux = try Data(contentsOf: dir.appendingPathComponent("aux.h264"))
        var mainAt = 0
        var auxAt = 0
        var frames: [(Data, Data, Bool)] = []
        for line in try String(contentsOf: dir.appendingPathComponent("frames.txt"), encoding: .utf8)
            .split(separator: "\n")
        {
            let f = line.split(separator: " ").map { Int($0)! }
            frames.append((main.subdata(in: mainAt..<mainAt + f[0]), aux.subdata(in: auxAt..<auxAt + f[1]), f[2] != 0))
            mainAt += f[0]
            auxAt += f[1]
        }
        self.frames = frames
    }
}

/// Compare the colour channels of two BGRA buffers.
func compare(_ a: [UInt8], _ b: [UInt8]) -> (maxDiff: Int, over1: Int, psnr: Double) {
    var maxDiff = 0
    var over1 = 0
    var squared = 0.0
    var n = 0
    for i in 0..<min(a.count, b.count) where i % 4 != 3 {
        let d = abs(Int(a[i]) - Int(b[i]))
        maxDiff = max(maxDiff, d)
        if d > 1 { over1 += 1 }
        squared += Double(d * d)
        n += 1
    }
    let mse = squared / Double(max(n, 1))
    return (maxDiff, over1, mse == 0 ? 99 : 10 * log10(255 * 255 / mse))
}

@Test(.enabled(if: !fixtureDirectories().isEmpty, "set GLIFF_VT_FIXTURES to a gliff-probe --dump directory"),
      arguments: fixtureDirectories())
func matchesVulkanDecode(_ dir: URL) throws {
    let fixture = try Fixture(dir)
    let main = H264Decoder()
    let aux = fixture.dual ? H264Decoder() : nil
    let recombiner = try Recombiner()
    var slowest = 0.0
    var total = 0.0

    for (i, frame) in fixture.frames.enumerated() {
        let start = Date()
        let mainPicture = try #require(try main.decode(frame.main), "frame \(i): no main picture")
        let auxPicture = try aux.map { try #require(try $0.decode(frame.aux), "frame \(i): no aux picture") }
        #expect(CVPixelBufferGetWidth(mainPicture) == fixture.width)
        #expect(CVPixelBufferGetHeight(mainPicture) == fixture.height)
        let texture = try recombiner.recombine(main: mainPicture, aux: auxPicture)
        let elapsed = Date().timeIntervalSince(start) * 1000
        slowest = max(slowest, elapsed)
        total += elapsed

        let expected = try [UInt8](Data(contentsOf: dir.appendingPathComponent("out\(i).bgra")))
        let got = Recombiner.bytes(of: texture)
        #expect(got.count == expected.count)
        let (maxDiff, over1, psnr) = compare(got, expected)
        print(String(format: "%@ frame %d key=%d: max diff %d, %d samples off by >1, PSNR %.1f dB, %.2f ms",
                     dir.lastPathComponent, i, frame.keyframe ? 1 : 0, maxDiff, over1, psnr, elapsed))
        #expect(maxDiff <= 1, "frame \(i) differs from the Vulkan decode by up to \(maxDiff)")
    }
    print(String(format: "%@: decode + recombine avg %.2f ms, worst %.2f ms",
                 dir.lastPathComponent, total / Double(fixture.frames.count), slowest))
}
