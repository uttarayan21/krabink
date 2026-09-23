// Image assets for the ink shader: tip masks and paper grains as two
// `r8` texture arrays (masks clamped, grains repeating, both mipmapped),
// built from the PNGs bundled in the core plus the workspace's `assets`
// map. A style names its layer by asset id; an asset the arrays lack
// falls back to the shape mask or value noise, and a rebuild posts
// `InkAssets.didChange` so renderers restyle what they show.

import Metal
import KrabinkCore
import UIKit

@MainActor
final class InkAssets {
    static let shared = InkAssets()
    static let didChange = Notification.Name("InkAssets.didChange")

    static let maskSize = 256
    static let grainSize = 512

    let device: MTLDevice?
    private(set) var masks: MTLTexture?
    private(set) var grains: MTLTexture?
    let clampSampler: MTLSamplerState?
    let repeatSampler: MTLSamplerState?
    private(set) var maskLayers: [String: UInt32] = [:]
    private(set) var grainLayers: [String: UInt32] = [:]
    /// Every asset the arrays hold, bundled first.
    private(set) var assets: [AssetInfo] = []
    private(set) var generation = 0

    init(device: MTLDevice? = MTLCreateSystemDefaultDevice()) {
        self.device = device
        func sampler(_ address: MTLSamplerAddressMode) -> MTLSamplerState? {
            let d = MTLSamplerDescriptor()
            d.minFilter = .linear
            d.magFilter = .linear
            d.mipFilter = .linear
            d.sAddressMode = address
            d.tAddressMode = address
            return device?.makeSamplerState(descriptor: d)
        }
        clampSampler = sampler(.clampToEdge)
        repeatSampler = sampler(.repeat)
        rebuild(shared: [])
    }

    func maskLayer(_ id: String) -> UInt32? { maskLayers[id] }
    func grainLayer(_ id: String) -> UInt32? { grainLayers[id] }

    /// The workspace's assets as the core reports them.
    func setShared(_ shared: [AssetInfo]) {
        rebuild(shared: shared)
        NotificationCenter.default.post(name: Self.didChange, object: self)
    }

    private func rebuild(shared: [AssetInfo]) {
        assets = builtinAssets() + shared.filter { s in !builtinAssets().contains { $0.id == s.id } }
        var maskLayersNew: [String: UInt32] = [:]
        var grainLayersNew: [String: UInt32] = [:]
        var maskTexels: [[UInt8]] = []
        var grainTexels: [[UInt8]] = []
        for asset in assets {
            let size = asset.kind == .mask ? Self.maskSize : Self.grainSize
            guard let texels = Self.luma(png: asset.png, size: size) else {
                NSLog("ink asset %@ (%@) did not decode", asset.id, asset.name)
                continue
            }
            switch asset.kind {
            case .mask:
                maskLayersNew[asset.id] = UInt32(maskTexels.count)
                maskTexels.append(texels)
            case .grain:
                grainLayersNew[asset.id] = UInt32(grainTexels.count)
                grainTexels.append(texels)
            }
        }
        masks = makeArray(maskTexels, size: Self.maskSize)
        grains = makeArray(grainTexels, size: Self.grainSize)
        maskLayers = maskLayersNew
        grainLayers = grainLayersNew
        generation += 1
    }

    /// One `r8Unorm` array texture with mips; an empty set is one white
    /// layer so the binding is always valid.
    private func makeArray(_ layers: [[UInt8]], size: Int) -> MTLTexture? {
        guard let device else { return nil }
        let layers = layers.isEmpty ? [[UInt8](repeating: 255, count: size * size)] : layers
        let d = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: .r8Unorm, width: size, height: size, mipmapped: true)
        d.textureType = .type2DArray
        d.arrayLength = layers.count
        d.usage = .shaderRead
        guard let texture = device.makeTexture(descriptor: d) else { return nil }
        for (slice, texels) in layers.enumerated() {
            texels.withUnsafeBytes { bytes in
                guard let base = bytes.baseAddress else { return }
                texture.replace(
                    region: MTLRegionMake2D(0, 0, size, size), mipmapLevel: 0, slice: slice,
                    withBytes: base, bytesPerRow: size, bytesPerImage: size * size)
            }
        }
        if let queue = device.makeCommandQueue(), let command = queue.makeCommandBuffer(),
            let blit = command.makeBlitCommandEncoder()
        {
            blit.generateMipmaps(for: texture)
            blit.endEncoding()
            command.commit()
            command.waitUntilCompleted()
        }
        return texture
    }

    /// Decode a PNG and draw it greyscale at `size`².
    nonisolated static func luma(png: Data, size: Int) -> [UInt8]? {
        guard let image = UIImage(data: png)?.cgImage else { return nil }
        var texels = [UInt8](repeating: 0, count: size * size)
        let ok = texels.withUnsafeMutableBytes { bytes -> Bool in
            guard
                let context = CGContext(
                    data: bytes.baseAddress, width: size, height: size, bitsPerComponent: 8,
                    bytesPerRow: size, space: CGColorSpaceCreateDeviceGray(),
                    bitmapInfo: CGImageAlphaInfo.none.rawValue)
            else { return false }
            context.interpolationQuality = .high
            context.draw(image, in: CGRect(x: 0, y: 0, width: size, height: size))
            return true
        }
        return ok ? texels : nil
    }
}

/// Turning a picked picture into an asset the workspace accepts: greyscale
/// PNG, at most 64 KiB, shrunk until it fits.
enum AssetImport {
    static let limit = 64 * 1024

    static func greyscalePNG(_ image: UIImage) -> Data? {
        var side = 256
        while side >= 32 {
            guard let texels = luma(image, side), let png = png(texels, side: side) else { return nil }
            if png.count <= limit { return png }
            side /= 2
        }
        return nil
    }

    private static func luma(_ image: UIImage, _ size: Int) -> [UInt8]? {
        guard let cg = image.cgImage else { return nil }
        var texels = [UInt8](repeating: 0, count: size * size)
        let ok = texels.withUnsafeMutableBytes { bytes -> Bool in
            guard
                let context = CGContext(
                    data: bytes.baseAddress, width: size, height: size, bitsPerComponent: 8,
                    bytesPerRow: size, space: CGColorSpaceCreateDeviceGray(),
                    bitmapInfo: CGImageAlphaInfo.none.rawValue)
            else { return false }
            context.interpolationQuality = .high
            context.draw(cg, in: CGRect(x: 0, y: 0, width: size, height: size))
            return true
        }
        return ok ? texels : nil
    }

    private static func png(_ texels: [UInt8], side: Int) -> Data? {
        guard let provider = CGDataProvider(data: Data(texels) as CFData),
            let cg = CGImage(
                width: side, height: side, bitsPerComponent: 8, bitsPerPixel: 8, bytesPerRow: side,
                space: CGColorSpaceCreateDeviceGray(), bitmapInfo: CGBitmapInfo(rawValue: 0),
                provider: provider, decode: nil, shouldInterpolate: false, intent: .defaultIntent)
        else { return nil }
        return UIImage(cgImage: cg).pngData()
    }
}
