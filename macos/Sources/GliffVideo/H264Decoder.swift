// One H.264 stream decoded by VideoToolbox into IOSurface-backed NV12 pixel
// buffers that Metal can read without a copy.
//
// Decoding is synchronous: with no asynchronous or temporal-processing flag,
// VideoToolbox calls the output handler before DecodeFrame returns. gliff's
// streams have no frame reordering, so each access unit gives at most one
// picture.

import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

public enum DecodeError: Error, CustomStringConvertible {
    case noParameterSets
    case status(String, OSStatus)
    case noImage

    public var description: String {
        switch self {
        case .noParameterSets: "no SPS/PPS seen before the first slice"
        case .status(let what, let code): "\(what) failed: OSStatus \(code)"
        case .noImage: "decoder returned no image"
        }
    }
}

public final class H264Decoder {
    private var format: CMVideoFormatDescription?
    private var session: VTDecompressionSession?
    private var sps: Data?
    private var pps: Data?

    public init() {}

    deinit {
        if let session {
            VTDecompressionSessionInvalidate(session)
        }
    }

    /// Decode one Annex B access unit. Returns nil when it produced no picture.
    public func decode(_ annexB: Data) throws -> CVPixelBuffer? {
        let unit = AccessUnit(annexB: annexB)
        if let sps = unit.sps, let pps = unit.pps, sps != self.sps || pps != self.pps {
            try configure(sps: sps, pps: pps)
        }
        guard let session, let format else {
            throw DecodeError.noParameterSets
        }
        if unit.avcc.isEmpty {
            return nil
        }
        let sample = try sampleBuffer(unit.avcc, format: format)

        var image: CVPixelBuffer?
        var decodeStatus = noErr
        let status = VTDecompressionSessionDecodeFrame(
            session, sampleBuffer: sample, flags: [], infoFlagsOut: nil
        ) { status, _, imageBuffer, _, _ in
            decodeStatus = status
            image = imageBuffer
        }
        guard status == noErr else { throw DecodeError.status("DecodeFrame", status) }
        guard decodeStatus == noErr else { throw DecodeError.status("decode", decodeStatus) }
        return image
    }

    /// (Re)create the session for new parameter sets. The stream's size comes
    /// from the SPS, cropping included.
    private func configure(sps: Data, pps: Data) throws {
        var newFormat: CMVideoFormatDescription?
        let status = sps.withUnsafeBytes { spsBytes in
            pps.withUnsafeBytes { ppsBytes in
                let pointers = [
                    spsBytes.bindMemory(to: UInt8.self).baseAddress!,
                    ppsBytes.bindMemory(to: UInt8.self).baseAddress!,
                ]
                let sizes = [sps.count, pps.count]
                return CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    allocator: kCFAllocatorDefault,
                    parameterSetCount: 2,
                    parameterSetPointers: pointers,
                    parameterSetSizes: sizes,
                    nalUnitHeaderLength: 4,
                    formatDescriptionOut: &newFormat
                )
            }
        }
        guard status == noErr, let newFormat else {
            throw DecodeError.status("format description", status)
        }

        if let session, VTDecompressionSessionCanAcceptFormatDescription(session, formatDescription: newFormat) {
            format = newFormat
        } else {
            if let session {
                VTDecompressionSessionInvalidate(session)
                self.session = nil
            }
            let spec: [CFString: Any] = [
                kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder: true
            ]
            let attributes: [CFString: Any] = [
                kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                kCVPixelBufferMetalCompatibilityKey: true,
                kCVPixelBufferIOSurfacePropertiesKey: [CFString: Any](),
            ]
            var newSession: VTDecompressionSession?
            let created = VTDecompressionSessionCreate(
                allocator: kCFAllocatorDefault,
                formatDescription: newFormat,
                decoderSpecification: spec as CFDictionary,
                imageBufferAttributes: attributes as CFDictionary,
                outputCallback: nil,
                decompressionSessionOut: &newSession
            )
            guard created == noErr, let newSession else {
                throw DecodeError.status("session create", created)
            }
            VTSessionSetProperty(newSession, key: kVTDecompressionPropertyKey_RealTime, value: kCFBooleanTrue)
            session = newSession
            format = newFormat
        }
        self.sps = sps
        self.pps = pps
    }

    private func sampleBuffer(_ avcc: Data, format: CMVideoFormatDescription) throws -> CMSampleBuffer {
        var block: CMBlockBuffer?
        var status = CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault,
            memoryBlock: nil,
            blockLength: avcc.count,
            blockAllocator: kCFAllocatorDefault,
            customBlockSource: nil,
            offsetToData: 0,
            dataLength: avcc.count,
            flags: kCMBlockBufferAssureMemoryNowFlag,
            blockBufferOut: &block
        )
        guard status == noErr, let block else { throw DecodeError.status("block buffer", status) }
        status = avcc.withUnsafeBytes { bytes in
            CMBlockBufferReplaceDataBytes(
                with: bytes.baseAddress!, blockBuffer: block, offsetIntoDestination: 0, dataLength: avcc.count
            )
        }
        guard status == noErr else { throw DecodeError.status("block copy", status) }

        var sample: CMSampleBuffer?
        var size = avcc.count
        status = CMSampleBufferCreateReady(
            allocator: kCFAllocatorDefault,
            dataBuffer: block,
            formatDescription: format,
            sampleCount: 1,
            sampleTimingEntryCount: 0,
            sampleTimingArray: nil,
            sampleSizeEntryCount: 1,
            sampleSizeArray: &size,
            sampleBufferOut: &sample
        )
        guard status == noErr, let sample else { throw DecodeError.status("sample buffer", status) }
        return sample
    }
}
