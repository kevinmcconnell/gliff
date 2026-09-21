// AVC444 recombine on the GPU: the main and auxiliary NV12 pictures from the
// two decoders become one full-chroma BGRA texture. A port of
// crates/gliff-vk/shaders/recombine.comp and common.glsl; with no auxiliary
// picture the main chroma is spread over each 2x2 block (Single420).

import CoreVideo
import Foundation
import Metal

public enum RecombineError: Error, CustomStringConvertible {
    case noDevice
    case texture(String)
    case sizeMismatch

    public var description: String {
        switch self {
        case .noDevice: "no Metal device"
        case .texture(let what): "cannot wrap \(what) as a Metal texture"
        case .sizeMismatch: "main and auxiliary pictures differ in size"
        }
    }
}

public final class Recombiner {
    public let device: MTLDevice
    private let queue: MTLCommandQueue
    private let pipeline: MTLComputePipelineState
    private let cache: CVMetalTextureCache

    public init(device: MTLDevice? = MTLCreateSystemDefaultDevice()) throws {
        guard let device, let queue = device.makeCommandQueue() else {
            throw RecombineError.noDevice
        }
        let options = MTLCompileOptions()
        // The constants must round exactly as the Vulkan shader's do.
        if #available(macOS 15.0, *) {
            options.mathMode = .safe
        } else {
            options.fastMathEnabled = false
        }
        let library = try device.makeLibrary(source: Self.source, options: options)
        let function = library.makeFunction(name: "recombine")!
        self.device = device
        self.queue = queue
        self.pipeline = try device.makeComputePipelineState(function: function)
        var cache: CVMetalTextureCache?
        CVMetalTextureCacheCreate(kCFAllocatorDefault, nil, device, nil, &cache)
        guard let cache else { throw RecombineError.noDevice }
        self.cache = cache
    }

    /// Recombine one frame into a new texture and wait for the GPU.
    public func recombine(main: CVPixelBuffer, aux: CVPixelBuffer?) throws -> MTLTexture {
        let output = try makeOutput(width: CVPixelBufferGetWidth(main), height: CVPixelBufferGetHeight(main))
        try recombine(main: main, aux: aux, into: output)
        return output
    }

    /// A texture the recombine can write and a renderer can sample.
    public func makeOutput(width: Int, height: Int) throws -> MTLTexture {
        let descriptor = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: .bgra8Unorm, width: width, height: height, mipmapped: false
        )
        descriptor.usage = [.shaderWrite, .shaderRead]
        descriptor.storageMode = .shared
        guard let output = device.makeTexture(descriptor: descriptor) else {
            throw RecombineError.texture("output")
        }
        return output
    }

    /// Recombine one frame into `output` and wait for the GPU. The plane
    /// textures (and the pixel buffers behind them) are held until the
    /// command buffer completes.
    public func recombine(main: CVPixelBuffer, aux: CVPixelBuffer?, into output: MTLTexture) throws {
        let width = CVPixelBufferGetWidth(main)
        let height = CVPixelBufferGetHeight(main)
        if let aux, CVPixelBufferGetWidth(aux) != width || CVPixelBufferGetHeight(aux) != height {
            throw RecombineError.sizeMismatch
        }
        guard output.width == width, output.height == height else {
            throw RecombineError.sizeMismatch
        }

        let planes = [
            try plane(main, 0, .r8Unorm, "main luma"),
            try plane(main, 1, .rg8Unorm, "main chroma"),
            try plane(aux ?? main, 0, .r8Unorm, "aux luma"),
            try plane(aux ?? main, 1, .rg8Unorm, "aux chroma"),
        ]
        guard let commands = queue.makeCommandBuffer(),
              let encoder = commands.makeComputeCommandEncoder()
        else {
            throw RecombineError.texture("command buffer")
        }

        var params: [Int32] = [Int32(width), Int32(height), aux == nil ? 0 : 1]
        encoder.setComputePipelineState(pipeline)
        for (index, texture) in planes.enumerated() {
            encoder.setTexture(CVMetalTextureGetTexture(texture), index: index)
        }
        encoder.setTexture(output, index: 4)
        encoder.setBytes(&params, length: MemoryLayout<Int32>.stride * params.count, index: 0)
        // One thread per 2x2 block, as in the Vulkan shader.
        let grid = MTLSize(width: (width + 1) / 2, height: (height + 1) / 2, depth: 1)
        encoder.dispatchThreads(grid, threadsPerThreadgroup: MTLSize(width: 16, height: 16, depth: 1))
        encoder.endEncoding()
        commands.addCompletedHandler { _ in _ = planes }
        commands.commit()
        commands.waitUntilCompleted()
        if let error = commands.error {
            throw error
        }
    }

    private func plane(
        _ buffer: CVPixelBuffer, _ index: Int, _ format: MTLPixelFormat, _ what: String
    ) throws -> CVMetalTexture {
        var texture: CVMetalTexture?
        let status = CVMetalTextureCacheCreateTextureFromImage(
            kCFAllocatorDefault, cache, buffer, nil, format,
            CVPixelBufferGetWidthOfPlane(buffer, index),
            CVPixelBufferGetHeightOfPlane(buffer, index),
            index, &texture
        )
        guard status == kCVReturnSuccess, let texture else {
            throw RecombineError.texture(what)
        }
        return texture
    }

    /// The texture's pixels as tightly packed BGRA bytes.
    public static func bytes(of texture: MTLTexture) -> [UInt8] {
        let rowBytes = texture.width * 4
        var out = [UInt8](repeating: 0, count: rowBytes * texture.height)
        texture.getBytes(
            &out, bytesPerRow: rowBytes,
            from: MTLRegionMake2D(0, 0, texture.width, texture.height), mipmapLevel: 0
        )
        return out
    }

    // BT.709 limited range, matching gliff-proto::color. The output is a
    // bgra8Unorm texture, so the kernel writes logical RGBA and Metal stores
    // it as B,G,R,A. (The Vulkan shader swizzles to .bgr because it views
    // its BGRA image as RGBA; doing that here would swap red and blue.)
    static let source = """
    #include <metal_stdlib>
    using namespace metal;

    struct Params { int width; int height; int has_aux; };

    static float3 to_rgb(float y8, float u8, float v8) {
        float y = y8 * 255.0 - 16.0;
        float cb = u8 * 255.0 - 128.0;
        float cr = v8 * 255.0 - 128.0;
        return clamp(float3(1.1644 * y + 1.7927 * cr,
                            1.1644 * y - 0.2132 * cb - 0.5329 * cr,
                            1.1644 * y + 2.1124 * cb) / 255.0, 0.0, 1.0);
    }

    kernel void recombine(texture2d<float, access::read> main_y [[texture(0)]],
                          texture2d<float, access::read> main_uv [[texture(1)]],
                          texture2d<float, access::read> aux_y [[texture(2)]],
                          texture2d<float, access::read> aux_uv [[texture(3)]],
                          texture2d<float, access::write> dst [[texture(4)]],
                          constant Params &p [[buffer(0)]],
                          uint2 gid [[thread_position_in_grid]]) {
        int bx = int(gid.x), by = int(gid.y);
        int x = 2 * bx, y = 2 * by;
        if (x >= p.width || y >= p.height) return;
        float y00 = main_y.read(uint2(x, y)).r;
        float y10 = main_y.read(uint2(x + 1, y)).r;
        float y01 = main_y.read(uint2(x, y + 1)).r;
        float y11 = main_y.read(uint2(x + 1, y + 1)).r;
        float2 c00 = main_uv.read(uint2(bx, by)).rg;
        float2 c10 = c00, c01 = c00, c11 = c00;
        if (p.has_aux != 0) {
            int half_h = p.height / 2;
            c10 = aux_uv.read(uint2(bx, by)).rg;
            c01 = float2(aux_y.read(uint2(x, by)).r, aux_y.read(uint2(x, half_h + by)).r);
            c11 = float2(aux_y.read(uint2(x + 1, by)).r, aux_y.read(uint2(x + 1, half_h + by)).r);
        }
        dst.write(float4(to_rgb(y00, c00.r, c00.g), 1.0), uint2(x, y));
        dst.write(float4(to_rgb(y10, c10.r, c10.g), 1.0), uint2(x + 1, y));
        dst.write(float4(to_rgb(y01, c01.r, c01.g), 1.0), uint2(x, y + 1));
        dst.write(float4(to_rgb(y11, c11.r, c11.g), 1.0), uint2(x + 1, y + 1));
    }
    """
}
