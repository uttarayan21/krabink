// Metal ink renderer. Every stroke on screen — committed, remote wet, the
// local live stroke and its predicted tail — is an indexed triangle mesh
// from the Rust core (`elementMesh` / `wetMesh` / `pointsMesh`), drawn with
// one flat-colour pipeline under 4x MSAA. The view is demand-driven: it
// redraws only when ink or the viewport changes, so an idle canvas costs
// no GPU time.
//
// Committed ink is batched: all strokes share one vertex/index buffer with
// the colour stored per vertex, so a page of thousands of strokes is one
// draw call. The batch is rebuilt (CPU concat + one upload) when strokes
// are added, removed, reordered or re-tessellated for a new zoom bucket;
// none of that happens per frame. Wet and live strokes change every event
// and stay as their own small buffers.
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

/// Mirrors `Uniforms` in Shaders.metal (float2, float2).
private struct Uniforms {
    var scale: SIMD2<Float>
    var translate: SIMD2<Float>
}

/// Mirrors `VertexIn` in Shaders.metal: float2 position, uchar4 colour
/// (RGBA, normalised in the shader). 12 bytes.
struct InkVertex {
    var x: Float
    var y: Float
    /// R in the lowest byte, A in the highest (little-endian uchar4).
    var color: UInt32

    static let stride = MemoryLayout<InkVertex>.stride

    /// 0xRRGGBBAA → in-memory uchar4.
    static func vertexColor(_ packed: UInt32) -> UInt32 {
        let r = (packed >> 24) & 0xff
        let g = (packed >> 16) & 0xff
        let b = (packed >> 8) & 0xff
        let a = packed & 0xff
        return r | (g << 8) | (b << 16) | (a << 24)
    }

    static var descriptor: MTLVertexDescriptor {
        let d = MTLVertexDescriptor()
        d.attributes[0].format = .float2
        d.attributes[0].offset = 0
        d.attributes[0].bufferIndex = 0
        d.attributes[1].format = .uchar4Normalized
        d.attributes[1].offset = 8
        d.attributes[1].bufferIndex = 0
        d.layouts[0].stride = stride
        return d
    }
}

/// CPU-side interleaved geometry for one or many strokes.
struct InkGeometry {
    var vertices: [InkVertex] = []
    var indices: [UInt32] = []

    var isEmpty: Bool { indices.count < 3 }

    init() {}

    init(_ mesh: IndexedMesh, color: UInt32) {
        append(mesh, color: color)
    }

    /// Append a mesh, offsetting its indices past the vertices so far.
    mutating func append(_ mesh: IndexedMesh, color: UInt32) {
        guard mesh.indices.count >= 3, mesh.positions.count >= 6 else { return }
        let base = UInt32(vertices.count)
        let packed = InkVertex.vertexColor(color)
        vertices.reserveCapacity(vertices.count + mesh.positions.count / 2)
        var i = 0
        while i + 1 < mesh.positions.count {
            vertices.append(InkVertex(x: mesh.positions[i], y: mesh.positions[i + 1], color: packed))
            i += 2
        }
        indices.reserveCapacity(indices.count + mesh.indices.count)
        for index in mesh.indices { indices.append(base + index) }
    }
}

/// Geometry uploaded to the GPU. Buffers grow and are reused across
/// uploads, so a batch that changes often does not churn allocations.
final class GPUGeometry {
    private let device: MTLDevice
    private(set) var vertices: MTLBuffer?
    private(set) var indices: MTLBuffer?
    private(set) var indexCount = 0

    init(device: MTLDevice) {
        self.device = device
    }

    var isEmpty: Bool { indexCount < 3 || vertices == nil || indices == nil }

    func upload(_ geometry: InkGeometry) {
        indexCount = geometry.indices.count
        guard !geometry.isEmpty else { return }
        vertices = Self.write(
            geometry.vertices, into: vertices, device: device, stride: InkVertex.stride)
        indices = Self.write(
            geometry.indices, into: indices, device: device, stride: MemoryLayout<UInt32>.stride)
    }

    private static func write<T>(
        _ items: [T], into existing: MTLBuffer?, device: MTLDevice, stride: Int
    ) -> MTLBuffer? {
        let length = items.count * stride
        let buffer: MTLBuffer?
        if let existing, existing.length >= length {
            buffer = existing
        } else {
            // Grow geometrically so a stroke-by-stroke append settles fast.
            let capacity = max(length, (existing?.length ?? 0) * 2)
            buffer = device.makeBuffer(length: capacity, options: .storageModeShared)
        }
        guard let buffer else { return nil }
        items.withUnsafeBytes { bytes in
            guard let base = bytes.baseAddress else { return }
            buffer.contents().copyMemory(from: base, byteCount: length)
        }
        return buffer
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

@MainActor
final class InkRenderer: NSObject, MTKViewDelegate {
    let device: MTLDevice
    private let queue: MTLCommandQueue
    private let pipeline: MTLRenderPipelineState
    private weak var view: MTKView?

    private struct Committed {
        let element: Element
        var z: Int
        var mesh: IndexedMesh
    }

    private struct Wet {
        let tool: Tool
        let color: UInt32
        let baseWidth: Float
        var points: [WetPoint] = []
        let geometry: GPUGeometry
    }

    private var committed: [String: Committed] = [:]
    /// Committed ids in draw order (CRDT z).
    private var order: [String] = []
    /// One buffer for all committed ink; rebuilt when `batchDirty`.
    private let batch: GPUGeometry
    private var batchDirty = false
    private var wet: [String: Wet] = [:]
    /// Remote wet ids in arrival order, drawn above committed ink.
    private var wetOrder: [String] = []
    private let local: GPUGeometry
    private var hasLocal = false
    /// Committed meshes are built for this zoom bucket; a bucket change
    /// re-tessellates them lazily on the next draw.
    private var meshBucket: CGFloat = 1
    private var committedStale = false
    private var loggedDrawable = false

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
        descriptor.vertexDescriptor = InkVertex.descriptor
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
        batch = GPUGeometry(device: device)
        local = GPUGeometry(device: device)
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

    /// Show a committed element, stroke or shape (idempotent; re-show only
    /// updates z).
    func show(_ element: Element, z: Int) {
        let id = element.id
        if committed[id] != nil {
            if committed[id]?.z != z {
                committed[id]?.z = z
                resort()
            }
            return
        }
        let mesh = elementMesh(element: element, tolerance: tolerance)
        committed[id] = Committed(element: element, z: z, mesh: mesh)
        inkBounds = inkBounds.union(mesh.bounds)
        resort()
    }

    func show(_ stroke: Stroke, z: Int) {
        show(.stroke(stroke), z: z)
    }

    func remove(_ id: String) {
        guard committed.removeValue(forKey: id) != nil else { return }
        order.removeAll { $0 == id }
        batchDirty = true
        needsDisplay()
    }

    func removeAll() {
        committed = [:]
        order = []
        batchDirty = true
        wet = [:]
        wetOrder = []
        hasLocal = false
        inkBounds = .null
        needsDisplay()
    }

    private func resort() {
        order = committed.keys.sorted { (committed[$0]?.z ?? 0) < (committed[$1]?.z ?? 0) }
        batchDirty = true
        needsDisplay()
    }

    private func retessellateCommitted() {
        meshBucket = Self.bucket(for: viewport.zoom)
        committedStale = false
        for id in order {
            guard let entry = committed[id] else { continue }
            committed[id]?.mesh = elementMesh(element: entry.element, tolerance: tolerance)
        }
        batchDirty = true
    }

    private func rebuildBatch() {
        batchDirty = false
        var geometry = InkGeometry()
        for id in order {
            guard let entry = committed[id] else { continue }
            geometry.append(entry.mesh, color: entry.element.color)
        }
        batch.upload(geometry)
    }

    // MARK: remote wet ink

    func wetBegin(_ id: String, tool: Tool, color: UInt32, baseWidth: Float) {
        if wet[id] == nil { wetOrder.append(id) }
        wet[id] = Wet(
            tool: tool, color: color, baseWidth: baseWidth, geometry: GPUGeometry(device: device))
        needsDisplay()
    }

    func wetAppend(_ id: String, _ points: [WetPoint]) {
        guard var entry = wet[id] else { return }
        entry.points.append(contentsOf: points)
        let mesh = wetMesh(
            points: entry.points, tool: entry.tool, baseWidth: entry.baseWidth, tolerance: tolerance)
        entry.geometry.upload(InkGeometry(mesh, color: entry.color))
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
        let mesh = pointsMesh(points: points, tool: tool, baseWidth: baseWidth, tolerance: tolerance)
        local.upload(InkGeometry(mesh, color: color))
        hasLocal = true
        needsDisplay()
    }

    /// Replace the live stroke's ink with a snapped shape's outline (the
    /// draw-and-hold preview), in the stroke's tool, colour and width.
    func setLocalShape(_ shape: Shape, tool: Tool, color: UInt32, baseWidth: Float) {
        let mesh = shapeOutlineMesh(
            shape: shape, tool: tool, baseWidth: baseWidth, tolerance: tolerance)
        local.upload(InkGeometry(mesh, color: color))
        hasLocal = true
        needsDisplay()
    }

    func clearLocal() {
        guard hasLocal else { return }
        hasLocal = false
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
        if committedStale { retessellateCommitted() }
        if batchDirty { rebuildBatch() }
        guard
            let drawable = view.currentDrawable,
            let pass = view.currentRenderPassDescriptor,
            let command = queue.makeCommandBuffer(),
            let encoder = command.makeRenderCommandEncoder(descriptor: pass)
        else { return }
        if !loggedDrawable {
            loggedDrawable = true
            // NSLog: reaches `devicectl --console` on a device, unlike os_log.
            NSLog(
                "ink view %.0fx%.0fpt drawable %.0fx%.0fpx scale %.2f msaa %d",
                view.bounds.width, view.bounds.height, view.drawableSize.width,
                view.drawableSize.height, view.contentScaleFactor, view.sampleCount)
        }

        let size = viewport.size
        guard size.width > 0, size.height > 0 else {
            encoder.endEncoding()
            command.present(drawable)
            command.commit()
            return
        }
        // canvas → clip: x' = (x·zoom − off.x) / w · 2 − 1, y flipped.
        let zoom = Float(viewport.zoom)
        var uniforms = Uniforms(
            scale: SIMD2(2 * zoom / Float(size.width), -2 * zoom / Float(size.height)),
            translate: SIMD2(
                -2 * Float(viewport.offset.x) / Float(size.width) - 1,
                2 * Float(viewport.offset.y) / Float(size.height) + 1))

        encoder.setRenderPipelineState(pipeline)
        encoder.setVertexBytes(&uniforms, length: MemoryLayout<Uniforms>.stride, index: 1)
        func encode(_ geometry: GPUGeometry) {
            guard !geometry.isEmpty, let vertices = geometry.vertices, let indices = geometry.indices
            else { return }
            encoder.setVertexBuffer(vertices, offset: 0, index: 0)
            encoder.drawIndexedPrimitives(
                type: .triangle, indexCount: geometry.indexCount, indexType: .uint32,
                indexBuffer: indices, indexBufferOffset: 0)
        }
        encode(batch)
        for id in wetOrder {
            guard let entry = wet[id] else { continue }
            encode(entry.geometry)
        }
        if hasLocal { encode(local) }
        encoder.endEncoding()
        command.present(drawable)
        command.commit()
    }
}
