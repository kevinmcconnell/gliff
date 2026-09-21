import CoreVideo
import Foundation
import Testing

@testable import GliffVideo

/// The x264 stream the Rust header parser is tested against.
let x264Fixture = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
    .deletingLastPathComponent()
    .appendingPathComponent("crates/gliff-vk/src/h264/testdata/x264-64x64.264")

@Test func splitsNalUnits() {
    let stream = Data([0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9])
    let units = nalUnits(stream)
    #expect(units.map(\.type) == [7, 8, 5])
    #expect(units.map(\.data.count) == [3, 2, 3])
}

@Test func accessUnitBecomesAvcc() {
    let stream = Data([0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9])
    let unit = AccessUnit(annexB: stream)
    #expect(unit.sps == Data([0x67, 1, 2]))
    #expect(unit.pps == Data([0x68, 3]))
    #expect(unit.avcc == Data([0, 0, 0, 3, 0x65, 9, 9]))
}

@Test func decodesX264Stream() throws {
    let units = nalUnits(try Data(contentsOf: x264Fixture))
    #expect(units.first?.type == NalType.sps.rawValue)

    // Feed the parameter sets with the first slice, then one slice at a time.
    let decoder = H264Decoder()
    let startCode = Data([0, 0, 0, 1])
    var pending = Data()
    var pictures = 0
    for nal in units {
        pending.append(startCode + nal.data)
        guard nal.type == NalType.slice.rawValue || nal.type == NalType.idr.rawValue else { continue }
        if let picture = try decoder.decode(pending) {
            #expect(CVPixelBufferGetWidth(picture) == 64)
            #expect(CVPixelBufferGetHeight(picture) == 64)
            pictures += 1
        }
        pending = Data()
    }
    #expect(pictures > 0)
}
