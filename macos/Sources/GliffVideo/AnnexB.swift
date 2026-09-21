// Annex B byte-stream helpers: split an access unit into NAL units and
// rewrite it in the length-prefixed (AVCC) form VideoToolbox expects.
// Mirrors crates/gliff-vk/src/h264/annexb.rs.

import Foundation

public enum NalType: UInt8 {
    case slice = 1
    case idr = 5
    case sei = 6
    case sps = 7
    case pps = 8
    case accessUnitDelimiter = 9
}

/// One NAL unit, from its header byte on (no start code).
public struct Nal {
    public let data: Data

    public var type: UInt8 { data[data.startIndex] & 0x1f }
    public var isParameterSet: Bool {
        type == NalType.sps.rawValue || type == NalType.pps.rawValue
    }
}

/// Split an Annex B stream at its start codes. Trailing zero bytes are never
/// part of a NAL unit: they are the leading zero of a 4-byte start code or
/// trailing_zero_8bits.
public func nalUnits(_ data: Data) -> [Nal] {
    let bytes = [UInt8](data)
    var starts: [Int] = []
    var i = 0
    while i + 3 <= bytes.count {
        if bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1 {
            starts.append(i + 3)
            i += 3
        } else {
            i += 1
        }
    }
    var units: [Nal] = []
    for (n, body) in starts.enumerated() {
        var end = n + 1 < starts.count ? starts[n + 1] - 3 : bytes.count
        while end > body && bytes[end - 1] == 0 {
            end -= 1
        }
        if end > body {
            units.append(Nal(data: Data(bytes[body..<end])))
        }
    }
    return units
}

/// An access unit split into what VideoToolbox needs: the parameter sets, if
/// the unit carries them, and the remaining NAL units as AVCC (each prefixed
/// with its 4-byte big-endian length).
public struct AccessUnit {
    public var sps: Data?
    public var pps: Data?
    public var avcc = Data()

    public init(annexB: Data) {
        for nal in nalUnits(annexB) {
            switch nal.type {
            case NalType.sps.rawValue: sps = nal.data
            case NalType.pps.rawValue: pps = nal.data
            case NalType.accessUnitDelimiter.rawValue: break
            default:
                var length = UInt32(nal.data.count).bigEndian
                avcc.append(Data(bytes: &length, count: 4))
                avcc.append(nal.data)
            }
        }
    }
}
