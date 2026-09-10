// Metal ink renderer. Every stroke on screen — committed, remote wet, the
// local live stroke and its predicted tail — is an indexed triangle mesh
// from the Rust core (`strokeMesh` / `wetMesh` / `pointsMesh`), drawn with
// one flat-colour pipeline under 4x MSAA. The view is demand-driven: it
// redraws only when ink or the viewport changes, so an idle canvas costs
// no GPU time.
//
// Coordinates: the core is canvas space (points, y down). `Viewport` maps
// that onto the view through the scroll view's zoom and content offset,
// so the Metal view stays pinned to the screen at any zoom and never needs
// a texture the size of the canvas.

import Metal
import MetalKit
import PendantCore
import UIKit
import simd

/// Where the canvas sits on screen: canvas point `p` lands at
/// `p * zoom - offset`, in view points.
struct Viewport: Equatable {
    var zoom: CGFloat = 1
    var offset: CGPoint = .zero
    /// View bounds, points.
    var size: CGSize = .zero
}

/// Mirrors `Uniforms` in Shaders.metal (float2, float2, float4).
private struct Uniforms {
    var scale: SIMD2<Float>
    var translate: SIMD2<Float>
    var color: SIMD4<Float>
}

/// One mesh uploaded to the GPU. `nil` for empty meshes (Metal rejects
/// zero-length buffers).
final class GPUMesh {
    let positions: MTLBuffer
    let indices: MTLBuffer
    let indexCount: Int
    let bounds: CGRect

    init?(device: MTLDevice, mesh: IndexedMesh) {
        guard mesh.indices.count >= 3, mesh.positions.count >= 6 else { return nil }
        guard
            let positions = mesh.positions.withUnsafeBytes({
                device.makeBuffer(bytes: $0.baseAddress!, length: $0.count, options: .storageModeShared)
            }),
            let indices = mesh.indices.withUnsafeBytes({
                device.makeBuffer(bytes: $0.baseAddress!, length: $0.count, options: .storageModeShared)
            })
        else { return nil }
        self.positions = positions
        self.indices = indices
        indexCount = mesh.indices.count
        bounds = mesh.bounds
    }
}

extension IndexedMesh {
    /// Axis-aligned bounds in canvas units; `.null` when empty.
    var bounds: CGRect {
        var minX = Float.infinity
        var minY = Float.infinity
        var maxX = -Float.infinity
        var maxY = -Float.infinity
        var i = 0
        while i + 1 < positions.count {
            minX = min(minX, positions[i])
            maxX = max(maxX, positions[i])
            minY = min(minY, positions[i + 1])
            maxY = max(maxY, positions[i + 1])
            i += 2
        }
        guard minX <= maxX else { return .null }
        return CGRect(
            x: CGFloat(minX), y: CGFloat(minY),
            width: CGFloat(maxX - minX), height: CGFloat(maxY - minY))
    }

    /// The triangles as one CGPath (one closed subpath each) for
    /// CoreGraphics fills, e.g. thumbnails. Every triangle is wound the
    /// same way so a non-zero fill of a self-overlapping stroke adds up
    /// instead of cancelling into holes. Fill with antialiasing off:
    /// adjacent antialiased triangles leave hairline seams.
    var cgPath: CGPath {
        let path = CGMutablePath()
        var i = 0
        while i + 2 < indices.count {
            let ia = Int(indices[i]) * 2
            let ib = Int(indices[i + 1]) * 2
            let ic = Int(indices[i + 2]) * 2
            guard max(ia, ib, ic) + 1 < positions.count else { break }
            let a = CGPoint(x: CGFloat(positions[ia]), y: CGFloat(positions[ia + 1]))
            var b = CGPoint(x: CGFloat(positions[ib]), y: CGFloat(positions[ib + 1]))
            var c = CGPoint(x: CGFloat(positions[ic]), y: CGFloat(positions[ic + 1]))
            let twiceArea = (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
            if twiceArea < 0 { swap(&b, &c) }
            path.move(to: a)
            path.addLine(to: b)
            path.addLine(to: c)
            path.closeSubpath()
            i += 3
        }
        return path
    }
}

/// Unpack 0xRRGGBBAA into a shader colour.
private func shaderColor(_ packed: UInt32) -> SIMD4<Float> {
    SIMD4(
        Float((packed >> 24) & 0xff) / 255,
        Float((packed >> 16) & 0xff) / 255,
        Float((packed >> 8) & 0xff) / 255,
        Float(packed & 0xff) / 255)
}

@MainActor
final class InkRenderer: NSObject, MTKViewDelegate {
    let device: MTLDevice
    private let queue: MTLCommandQueue
    private let pipeline: MTLRenderPipelineState
    private weak var view: MTKView?

    private struct Committed {
        let stroke: Stroke
        let color: SIMD4<Float>
        var z: Int
        var mesh: GPUMesh?
    }

    private struct Wet {
        let tool: Tool
        let color: SIMD4<Float>
        let baseWidth: Float
        var points: [WetPoint] = []
        var mesh: GPUMesh?
    }

    private struct Local {
        let color: SIMD4<Float>
        let mesh: GPUMesh?
    }

    private var committed: [String: Committed] = [:]
    /// Committed ids in draw order (CRDT z).
    private var order: [String] = []
    private var wet: [String: Wet] = [:]
    /// Remote wet ids in arrival order, drawn above committed ink.
    private var wetOrder: [String] = []
    private var local: Local?
    /// Committed meshes are built for this zoom bucket; a bucket change
    /// rebuilds them lazily on the next draw.
    private var meshBucket: CGFloat = 1
    private var committedStale = false

    /// Union of committed ink bounds, canvas units; drives canvas growth.
    private(set) var inkBounds = CGRect.null

    var viewport = Viewport() {
        didSet {
            guard viewport != oldValue else { return }
            if Self.bucket(for: viewport.zoom) != meshBucket { committedStale = true }
            needsDisplay()
        }
    }

    static let sampleCount = 4

    init?(view: MTKView) {
        guard
            let device = view.device ?? MTLCreateSystemDefaultDevice(),
            let queue = device.makeCommandQueue(),
            let library = device.makeDefaultLibrary(),
            let vertex = library.makeFunction(name: "ink_vertex"),
            let fragment = library.makeFunction(name: "ink_fragment")
        else { return nil }
        let descriptor = MTLRenderPipelineDescriptor()
        descriptor.vertexFunction = vertex
        descriptor.fragmentFunction = fragment
        descriptor.rasterSampleCount = Self.sampleCount
        let target = descriptor.colorAttachments[0]!
        target.pixelFormat = view.colorPixelFormat
        // Straight-alpha blending so the marker's translucent ink layers.
        target.isBlendingEnabled = true
        target.sourceRGBBlendFactor = .sourceAlpha
        target.destinationRGBBlendFactor = .oneMinusSourceAlpha
        target.sourceAlphaBlendFactor = .one
        target.destinationAlphaBlendFactor = .oneMinusSourceAlpha
        guard let pipeline = try? device.makeRenderPipelineState(descriptor: descriptor) else {
            return nil
        }
        self.device = device
        self.queue = queue
        self.pipeline = pipeline
        self.view = view
        super.init()
    }

    // MARK: tolerance

    /// Zoom rounded up to a power of two: meshes are rebuilt when the
    /// bucket changes, not on every pinch frame.
    private static func bucket(for zoom: CGFloat) -> CGFloat {
        guard zoom > 1 else { return 1 }
        return pow(2, ceil(log2(zoom)))
    }

    private var tolerance: Float {
        Float(CGFloat(defaultTolerance()) / meshBucket)
    }

    // MARK: committed ink

    /// Show a committed stroke (idempotent; re-show only updates z).
    func show(_ stroke: Stroke, z: Int) {
        if committed[stroke.id] != nil {
            committed[stroke.id]?.z = z
            resort()
            return
        }
        let mesh = GPUMesh(device: device, mesh: strokeMesh(stroke: stroke, tolerance: tolerance))
        committed[stroke.id] = Committed(
            stroke: stroke, color: shaderColor(stroke.color), z: z, mesh: mesh)
        if let box = mesh?.bounds { inkBounds = inkBounds.union(box) }
        resort()
        needsDisplay()
    }

    func remove(_ id: String) {
        guard committed.removeValue(forKey: id) != nil else { return }
        order.removeAll { $0 == id }
        needsDisplay()
    }

    func removeAll() {
        committed = [:]
        order = []
        wet = [:]
        wetOrder = []
        local = nil
        inkBounds = .null
        needsDisplay()
    }

    private func resort() {
        order = committed.keys.sorted { (committed[$0]?.z ?? 0) < (committed[$1]?.z ?? 0) }
    }

    private func rebuildCommitted() {
        meshBucket = Self.bucket(for: viewport.zoom)
        committedStale = false
        for id in order {
            guard let entry = committed[id] else { continue }
            committed[id]?.mesh = GPUMesh(
                device: device, mesh: strokeMesh(stroke: entry.stroke, tolerance: tolerance))
        }
    }

    // MARK: remote wet ink

    func wetBegin(_ id: String, tool: Tool, color: UInt32, baseWidth: Float) {
        if wet[id] == nil { wetOrder.append(id) }
        wet[id] = Wet(tool: tool, color: shaderColor(color), baseWidth: baseWidth)
        needsDisplay()
    }

    func wetAppend(_ id: String, _ points: [WetPoint]) {
        guard var entry = wet[id] else { return }
        entry.points.append(contentsOf: points)
        entry.mesh = GPUMesh(
            device: device,
            mesh: wetMesh(
                points: entry.points, tool: entry.tool, baseWidth: entry.baseWidth,
                tolerance: tolerance))
        wet[id] = entry
        needsDisplay()
    }

    func wetRemove(_ id: String) {
        guard wet.removeValue(forKey: id) != nil else { return }
        wetOrder.removeAll { $0 == id }
        needsDisplay()
    }

    func hasWet(_ id: String) -> Bool { wet[id] != nil }

    // MARK: local live stroke

    /// Replace the live stroke's ink: the modeler's points plus its
    /// predicted tail, re-tessellated whole (well under a millisecond for
    /// thousands of points).
    func setLocal(points: [StrokePoint], tool: Tool, color: UInt32, baseWidth: Float) {
        local = Local(
            color: shaderColor(color),
            mesh: GPUMesh(
                device: device,
                mesh: pointsMesh(points: points, tool: tool, baseWidth: baseWidth, tolerance: tolerance)))
        needsDisplay()
    }

    func clearLocal() {
        guard local != nil else { return }
        local = nil
        needsDisplay()
    }

    // MARK: drawing

    func needsDisplay() {
        view?.setNeedsDisplay()
    }

    nonisolated func mtkView(_ view: MTKView, drawableSizeWillChange size: CGSize) {
        MainActor.assumeIsolated { needsDisplay() }
    }

    nonisolated func draw(in view: MTKView) {
        MainActor.assumeIsolated { render(in: view) }
    }

    private func render(in view: MTKView) {
        guard
            let drawable = view.currentDrawable,
            let pass = view.currentRenderPassDescriptor,
            let command = queue.makeCommandBuffer(),
            let encoder = command.makeRenderCommandEncoder(descriptor: pass)
        else { return }
        if committedStale { rebuildCommitted() }

        let size = viewport.size
        guard size.width > 0, size.height > 0 else {
            encoder.endEncoding()
            command.present(drawable)
            command.commit()
            return
        }
        // canvas → clip: x' = (x·zoom − off.x) / w · 2 − 1, y flipped.
        let zoom = Float(viewport.zoom)
        let scale = SIMD2(2 * zoom / Float(size.width), -2 * zoom / Float(size.height))
        let translate = SIMD2(
            -2 * Float(viewport.offset.x) / Float(size.width) - 1,
            2 * Float(viewport.offset.y) / Float(size.height) + 1)

        encoder.setRenderPipelineState(pipeline)
        func encode(_ mesh: GPUMesh?, _ color: SIMD4<Float>) {
            guard let mesh else { return }
            var uniforms = Uniforms(scale: scale, translate: translate, color: color)
            encoder.setVertexBuffer(mesh.positions, offset: 0, index: 0)
            encoder.setVertexBytes(&uniforms, length: MemoryLayout<Uniforms>.stride, index: 1)
            encoder.drawIndexedPrimitives(
                type: .triangle, indexCount: mesh.indexCount, indexType: .uint32,
                indexBuffer: mesh.indices, indexBufferOffset: 0)
        }
        for id in order {
            guard let entry = committed[id] else { continue }
            encode(entry.mesh, entry.color)
        }
        for id in wetOrder {
            guard let entry = wet[id] else { continue }
            encode(entry.mesh, entry.color)
        }
        if let local { encode(local.mesh, local.color) }
        encoder.endEncoding()
        command.present(drawable)
        command.commit()
    }
}
