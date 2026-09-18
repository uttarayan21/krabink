// Metal ink renderer. Every stroke on screen — committed, remote wet, the
// local live stroke and its predicted tail, strokes settling after pen-up —
// is an `InkMesh` from the Rust core drawn under 4x MSAA into an
// sRGB-encoded framebuffer with linear, premultiplied blending. The view
// is demand-driven: it redraws only when ink or the viewport changes.
//
// Geometry rides exactly as the core emits it (position, uv, opacity per
// vertex) plus a per-vertex stroke index; everything per stroke — linear
// colour, edge mask, grain, depth slot, blend and overlap — sits in a
// `StrokeStyle` array. Committed ink is one batch: one vertex/index/style
// upload, drawn as runs that split only where the pipeline state changes
// (Normal vs Multiply blend, Accumulate vs Discard overlap), in z order.
//
// Write-once ink (`Overlap::Discard`, the marker): every stroke gets a
// depth slot strictly monotone in draw order and writes depth. A Discard
// run compares `.less`, so a stroke's own later fragments at a sample are
// rejected and it never darkens where it crosses itself; an Accumulate
// run compares `.lessEqual` and layers. Later strokes are strictly nearer
// and blend over either. MSAA keeps depth per sample, so edges stay
// antialiased and shared triangle edges give neither double hits nor seams.
//
// Coordinates: the core is canvas space (points, y down). `Viewport` maps
// that onto the view through the scroll view's zoom and content offset,
// so the Metal view stays pinned to the screen at any zoom.

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

/// Mirrors `Uniforms` in Shaders.metal.
private struct Uniforms {
    var scale: SIMD2<Float>
    var translate: SIMD2<Float>
    var zoom: Float
    var pad: Float = 0

    /// canvas → clip: x' = (x·zoom − off.x) / w · 2 − 1, y flipped.
    init(_ viewport: Viewport) {
        let zoom = Float(viewport.zoom)
        let w = Float(viewport.size.width)
        let h = Float(viewport.size.height)
        scale = SIMD2(2 * zoom / w, -2 * zoom / h)
        translate = SIMD2(
            -2 * Float(viewport.offset.x) / w - 1,
            2 * Float(viewport.offset.y) / h + 1)
        self.zoom = zoom
    }
}

/// Mirrors `StrokeStyle` in Shaders.metal: 64 bytes, the same on the
/// desktop's WGSL.
struct StrokeStyle {
    /// Linear RGBA, straight alpha; the brush opacity is folded in.
    var color: SIMD4<Float>
    /// aspect, corner, hardness, 0
    var mask: SIMD4<Float>
    /// scale, strength, seed, 0
    var grain: SIMD4<Float>
    /// NDC z slot, strictly monotone in draw order.
    var depth: Float
    var flags: UInt32
    var maskLayer: UInt32 = 0
    var grainLayer: UInt32 = 0

    static let maskEdge: UInt32 = 3
    /// Grain kind 1: value noise seeded by `grainLayer`.
    static let grainNoise: UInt32 = 4
    /// Grain follows the stroke's own uv rather than the canvas.
    static let grainStroke: UInt32 = 16
    static let multiply: UInt32 = 32
    static let discard: UInt32 = 64
    static let stride = MemoryLayout<StrokeStyle>.stride

    init(_ style: InkStyle, depth: Float) {
        let c = Self.linearColor(style.color)
        color = SIMD4(c.x, c.y, c.z, c.w * style.opacity)
        mask = SIMD4(1, 0, style.hardness, 0)
        self.depth = depth
        var flags: UInt32 = 0
        if style.hardness < 1 { flags |= Self.maskEdge }
        if style.blend == .multiply { flags |= Self.multiply }
        if style.overlap == .discard { flags |= Self.discard }
        if let g = style.grain {
            grain = SIMD4(g.scale, g.strength, 0, 0)
            grainLayer = g.seed
            flags |= Self.grainNoise
            if g.mapping == .stroke { flags |= Self.grainStroke }
        } else {
            grain = .zero
        }
        self.flags = flags
    }

    var combo: InkCombo {
        InkCombo(multiply: flags & Self.multiply != 0, discard: flags & Self.discard != 0)
    }

    /// sRGB byte (low 8 bits) → linear.
    static func linearByte(_ byte: UInt32) -> Float {
        let c = Float(byte & 0xff) / 255
        return c <= 0.04045 ? c / 12.92 : powf((c + 0.055) / 1.055, 2.4)
    }

    /// 0xRRGGBBAA → linear RGBA.
    static func linearColor(_ packed: UInt32) -> SIMD4<Float> {
        SIMD4(
            linearByte(packed >> 24), linearByte(packed >> 16), linearByte(packed >> 8),
            Float(packed & 0xff) / 255)
    }
}

/// The pipeline state a run of ink shares.
struct InkCombo: Hashable {
    let multiply: Bool
    let discard: Bool
}

/// A contiguous range of the index buffer drawn with one pipeline state.
struct InkRun {
    let combo: InkCombo
    let indexOffset: Int
    var indexCount: Int
}

/// CPU-side geometry for one or many strokes, laid out for upload as-is.
struct InkGeometry {
    /// Floats per vertex in `vertices` (`InkMesh` contract).
    static let vertexFloats = 5
    var vertices: [Float] = []
    var strokeIndex: [UInt32] = []
    var indices: [UInt32] = []
    var styles: [StrokeStyle] = []
    var runs: [InkRun] = []

    var vertexCount: Int { vertices.count / Self.vertexFloats }
    var isEmpty: Bool { indices.count < 3 }

    init() {}

    init(_ mesh: InkMesh, depth: Float) {
        append(mesh, depth: depth)
    }

    /// Append a mesh as one stroke at `depth`, offsetting its indices past
    /// the vertices so far. Extends the last run when the pipeline state
    /// matches, so consecutive same-brush strokes are one draw. The mesh
    /// arrives as little-endian bytes; they are copied, not decoded.
    mutating func append(_ mesh: InkMesh, depth: Float) {
        let count = Int(mesh.vertexCount)
        let indexCount = Int(mesh.indexCount)
        guard indexCount >= 3, count >= 3,
              mesh.vertices.count == count * Self.vertexFloats * 4,
              mesh.indices.count == indexCount * 4
        else { return }
        let base = UInt32(vertexCount)
        let stroke = UInt32(styles.count)
        let style = StrokeStyle(mesh.style, depth: depth)
        styles.append(style)
        vertices.append(contentsOf: mesh.vertices.floats)
        strokeIndex.append(contentsOf: repeatElement(stroke, count: count))
        let start = indices.count
        if base == 0 {
            indices.append(contentsOf: mesh.indices.uint32s)
        } else {
            indices.reserveCapacity(indices.count + indexCount)
            for index in mesh.indices.uint32s { indices.append(base + index) }
        }
        if let last = runs.last, last.combo == style.combo {
            runs[runs.count - 1].indexCount += indexCount
        } else {
            runs.append(InkRun(combo: style.combo, indexOffset: start, indexCount: indexCount))
        }
    }
}

extension Data {
    /// Little-endian `Float`s, one copy (arm64 is little-endian).
    var floats: [Float] {
        [Float](unsafeUninitializedCapacity: count / 4) { buffer, filled in
            filled = copyBytes(to: buffer) / 4
        }
    }

    var uint32s: [UInt32] {
        [UInt32](unsafeUninitializedCapacity: count / 4) { buffer, filled in
            filled = copyBytes(to: buffer) / 4
        }
    }
}

/// Geometry uploaded to the GPU. Buffers grow and are reused across
/// uploads, so a batch that changes often does not churn allocations.
final class GPUGeometry {
    private let device: MTLDevice
    private(set) var vertices: MTLBuffer?
    private(set) var strokeIndex: MTLBuffer?
    private(set) var indices: MTLBuffer?
    private(set) var styles: MTLBuffer?
    private(set) var runs: [InkRun] = []

    init(device: MTLDevice) {
        self.device = device
    }

    var isEmpty: Bool {
        runs.isEmpty || vertices == nil || strokeIndex == nil || indices == nil || styles == nil
    }

    func upload(_ geometry: InkGeometry) {
        runs = geometry.runs
        guard !geometry.isEmpty else {
            runs = []
            return
        }
        vertices = Self.write(geometry.vertices, into: vertices, device: device)
        strokeIndex = Self.write(geometry.strokeIndex, into: strokeIndex, device: device)
        indices = Self.write(geometry.indices, into: indices, device: device)
        styles = Self.write(geometry.styles, into: styles, device: device)
    }

    private static func write<T>(_ items: [T], into existing: MTLBuffer?, device: MTLDevice) -> MTLBuffer? {
        let length = items.count * MemoryLayout<T>.stride
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

extension InkMesh {
    /// Axis-aligned bounds in canvas units; `.null` when empty.
    var bounds: CGRect {
        var minX = Float.infinity
        var minY = Float.infinity
        var maxX = -Float.infinity
        var maxY = -Float.infinity
        let floats = vertices.floats
        var i = 0
        while i + 1 < floats.count {
            minX = min(minX, floats[i])
            maxX = max(maxX, floats[i])
            minY = min(minY, floats[i + 1])
            maxY = max(maxY, floats[i + 1])
            i += InkGeometry.vertexFloats
        }
        guard minX <= maxX else { return .null }
        return CGRect(
            x: CGFloat(minX), y: CGFloat(minY),
            width: CGFloat(maxX - minX), height: CGFloat(maxY - minY))
    }
}

@MainActor
final class InkRenderer: NSObject, MTKViewDelegate {
    let device: MTLDevice
    private let queue: MTLCommandQueue
    private let psoNormal: MTLRenderPipelineState
    private let psoMultiply: MTLRenderPipelineState
    private let depthAccumulate: MTLDepthStencilState
    private let depthDiscard: MTLDepthStencilState
    private weak var view: MTKView?

    private struct Committed {
        let element: Element
        var z: Int
        var mesh: InkMesh
    }

    private struct Wet {
        let brush: BrushRef
        let color: UInt32
        var points: [StrokePoint] = []
        var end: StrokeEnd = .live
        let geometry: GPUGeometry
    }

    private var committed: [String: Committed] = [:]
    /// Committed ids in draw order (CRDT z).
    private var order: [String] = []
    /// One upload for all committed ink; rebuilt when `batchDirty`.
    private let batch: GPUGeometry
    private var batchDirty = false
    private var wet: [String: Wet] = [:]
    /// Remote wet ids in arrival order, drawn above committed ink.
    private var wetOrder: [String] = []
    /// The local live stroke.
    private let local: GPUGeometry
    /// The Pencil Pro hover preview: one dab above everything.
    private let hover: GPUGeometry
    private var hasHover = false
    private var localGeometry: InkGeometry?
    private var hasLocal = false
    /// Local strokes past pen-up whose commit is held for estimated-property
    /// updates; drawn until `show` replaces them.
    private var locals: [String: GPUGeometry] = [:]
    private var localsOrder: [String] = []
    private var settled = 0
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
    /// sRGB-encoded so blending happens in linear light, as on the desktop.
    static let colorFormat: MTLPixelFormat = .bgra8Unorm_srgb
    static let depthFormat: MTLPixelFormat = .depth32Float

    /// Depth slots: committed stroke `k` of `n` in draw order. Later
    /// strokes are nearer; everything stays inside (0.2, 1).
    static func committedDepth(_ k: Int, of n: Int) -> Float {
        0.2 + 0.8 * Float(n - k) / Float(n + 1)
    }
    /// Remote wet stroke `j` of `w`, above every committed stroke.
    static func wetDepth(_ j: Int, of w: Int) -> Float {
        0.1 + 0.001 * Float(w - j)
    }
    static let liveDepth: Float = 0.01
    static let hoverDepth: Float = 0.005

    init?(view: MTKView) {
        view.colorPixelFormat = Self.colorFormat
        view.depthStencilPixelFormat = Self.depthFormat
        view.clearDepth = 1
        view.sampleCount = Self.sampleCount
        guard
            let device = view.device ?? MTLCreateSystemDefaultDevice(),
            let queue = device.makeCommandQueue(),
            let pipelines = Self.makePipelines(device: device),
            let depthStates = Self.makeDepthStates(device: device)
        else { return nil }
        precondition(inkVertexFloats() == UInt32(InkGeometry.vertexFloats), "InkMesh vertex layout changed")
        self.device = device
        self.queue = queue
        (psoNormal, psoMultiply) = pipelines
        (depthAccumulate, depthDiscard) = depthStates
        self.view = view
        batch = GPUGeometry(device: device)
        local = GPUGeometry(device: device)
        hover = GPUGeometry(device: device)
        super.init()
    }

    /// Mirrors `VertexIn` in Shaders.metal: buffer 0 is the core's
    /// `InkMesh.vertices` verbatim (float2 position, float2 uv, float
    /// opacity; 20 bytes), buffer 3 the per-vertex stroke index.
    private static var vertexDescriptor: MTLVertexDescriptor {
        let d = MTLVertexDescriptor()
        d.attributes[0].format = .float2
        d.attributes[0].offset = 0
        d.attributes[0].bufferIndex = 0
        d.attributes[1].format = .float2
        d.attributes[1].offset = 8
        d.attributes[1].bufferIndex = 0
        d.attributes[2].format = .float
        d.attributes[2].offset = 16
        d.attributes[2].bufferIndex = 0
        d.layouts[0].stride = InkGeometry.vertexFloats * MemoryLayout<Float>.stride
        d.attributes[3].format = .uint
        d.attributes[3].offset = 0
        d.attributes[3].bufferIndex = 3
        d.layouts[3].stride = MemoryLayout<UInt32>.stride
        return d
    }

    /// Normal and Multiply pipelines; both premultiplied.
    private static func makePipelines(device: MTLDevice) -> (MTLRenderPipelineState, MTLRenderPipelineState)? {
        guard
            let library = device.makeDefaultLibrary(),
            let vertex = library.makeFunction(name: "ink_vertex"),
            let fragment = library.makeFunction(name: "ink_fragment")
        else { return nil }
        func make(multiply: Bool) -> MTLRenderPipelineState? {
            let descriptor = MTLRenderPipelineDescriptor()
            descriptor.label = multiply ? "ink multiply" : "ink normal"
            descriptor.vertexFunction = vertex
            descriptor.fragmentFunction = fragment
            descriptor.vertexDescriptor = vertexDescriptor
            descriptor.rasterSampleCount = sampleCount
            descriptor.depthAttachmentPixelFormat = depthFormat
            let target = descriptor.colorAttachments[0]!
            target.pixelFormat = colorFormat
            target.isBlendingEnabled = true
            // Normal: out = src + dst·(1 − a). Multiply: out = lerp(dst, c·dst, a)
            // = c·a·dst + dst·(1 − a), the classic highlighter.
            target.sourceRGBBlendFactor = multiply ? .destinationColor : .one
            target.destinationRGBBlendFactor = .oneMinusSourceAlpha
            target.sourceAlphaBlendFactor = .one
            target.destinationAlphaBlendFactor = .oneMinusSourceAlpha
            return try? device.makeRenderPipelineState(descriptor: descriptor)
        }
        guard let normal = make(multiply: false), let multiply = make(multiply: true) else { return nil }
        return (normal, multiply)
    }

    /// Accumulate (`.lessEqual`) and Discard (`.less`), both writing depth.
    private static func makeDepthStates(device: MTLDevice) -> (MTLDepthStencilState, MTLDepthStencilState)? {
        func make(_ compare: MTLCompareFunction) -> MTLDepthStencilState? {
            let d = MTLDepthStencilDescriptor()
            d.depthCompareFunction = compare
            d.isDepthWriteEnabled = true
            return device.makeDepthStencilState(descriptor: d)
        }
        guard let accumulate = make(.lessEqual), let discard = make(.less) else { return nil }
        return (accumulate, discard)
    }

    /// The clear colour for a UIKit background: the framebuffer is sRGB,
    /// Metal takes clear values in linear light.
    static func clearColor(for color: UIColor, trait: UITraitCollection) -> MTLClearColor {
        var r: CGFloat = 0
        var g: CGFloat = 0
        var b: CGFloat = 0
        var a: CGFloat = 0
        color.resolvedColor(with: trait).getRed(&r, green: &g, blue: &b, alpha: &a)
        func linear(_ c: CGFloat) -> Double {
            let c = Double(c)
            return c <= 0.04045 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4)
        }
        return MTLClearColor(red: linear(r), green: linear(g), blue: linear(b), alpha: 1)
    }

    // MARK: tolerance

    /// Zoom rounded up to a power of two: meshes are rebuilt when the
    /// bucket changes, not on every pinch frame.
    private static func bucket(for zoom: CGFloat) -> CGFloat {
        guard zoom > 1 else { return 1 }
        return pow(2, ceil(log2(zoom)))
    }

    /// Flattening tolerance for the current zoom bucket, canvas units.
    var tolerance: Float {
        Float(CGFloat(defaultTolerance()) / meshBucket)
    }

    // MARK: committed ink

    /// Show a committed element, stroke or shape (idempotent; re-show only
    /// updates z). Replaces the settling local copy of the same id.
    func show(_ element: Element, z: Int) {
        let id = element.id
        if locals.removeValue(forKey: id) != nil { localsOrder.removeAll { $0 == id } }
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
        localGeometry = nil
        locals = [:]
        localsOrder = []
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
        for (k, id) in order.enumerated() {
            guard let entry = committed[id] else { continue }
            geometry.append(entry.mesh, depth: Self.committedDepth(k, of: order.count))
        }
        batch.upload(geometry)
    }

    // MARK: remote wet ink

    func wetBegin(_ id: String, brush: BrushRef, color: UInt32) {
        if wet[id] == nil { wetOrder.append(id) }
        wet[id] = Wet(brush: brush, color: color, geometry: GPUGeometry(device: device))
        needsDisplay()
    }

    /// Stored points the sender emitted; the mesh is the sender's own
    /// (same core, same points), so the commit changes nothing on screen.
    func wetAppend(_ id: String, _ points: [StrokePoint]) {
        guard wet[id] != nil else { return }
        wet[id]?.points.append(contentsOf: points)
        remeshWet(id)
    }

    /// The sender's pen-up: `tail` is what its `finish` added.
    func wetEnd(_ id: String, tail: [StrokePoint]) {
        guard wet[id] != nil else { return }
        wet[id]?.points.append(contentsOf: tail)
        wet[id]?.end = .complete
        remeshWet(id)
    }

    private func remeshWet(_ id: String) {
        guard let entry = wet[id], let j = wetOrder.firstIndex(of: id) else { return }
        let mesh = pointsMesh(
            points: entry.points, brush: entry.brush, color: entry.color, end: entry.end,
            tolerance: tolerance)
        entry.geometry.upload(InkGeometry(mesh, depth: Self.wetDepth(j, of: wetOrder.count)))
        needsDisplay()
    }

    func wetRemove(_ id: String) {
        guard wet.removeValue(forKey: id) != nil else { return }
        wetOrder.removeAll { $0 == id }
        needsDisplay()
    }

    func hasWet(_ id: String) -> Bool { wet[id] != nil }

    // MARK: local live stroke

    /// Replace the live stroke's ink with a mesh the modeler built
    /// (`BrushModeler.liveMesh` at `tolerance`).
    func setLocal(mesh: InkMesh) {
        setLocal(mesh)
    }

    /// Replace the live stroke's ink with a snapped shape's outline (the
    /// draw-and-hold preview), in the stroke's brush and colour.
    func setLocalShape(_ shape: Shape, brush: BrushRef, color: UInt32) {
        setLocal(shapeOutlineMesh(shape: shape, brush: brush, color: color, tolerance: tolerance))
    }

    private func setLocal(_ mesh: InkMesh) {
        let geometry = InkGeometry(mesh, depth: Self.liveDepth)
        local.upload(geometry)
        localGeometry = geometry
        hasLocal = true
        needsDisplay()
    }

    func clearLocal() {
        guard hasLocal else { return }
        hasLocal = false
        localGeometry = nil
        needsDisplay()
    }

    /// Pen is up but the commit waits for estimated-property updates: keep
    /// the live ink on screen under `id` until `show` lands the element.
    func settleLocal(as id: String) {
        guard hasLocal, var geometry = localGeometry else { return }
        hasLocal = false
        localGeometry = nil
        // Distinct slots so two settling markers do not discard each other.
        settled = (settled + 1) % 40
        let depth = 0.05 - 0.001 * Float(settled)
        for i in geometry.styles.indices { geometry.styles[i].depth = depth }
        let gpu = GPUGeometry(device: device)
        gpu.upload(geometry)
        if locals[id] == nil { localsOrder.append(id) }
        locals[id] = gpu
        needsDisplay()
    }

    func dropLocal(_ id: String) {
        guard locals.removeValue(forKey: id) != nil else { return }
        localsOrder.removeAll { $0 == id }
        needsDisplay()
    }

    // MARK: hover preview

    /// Show the tip the pen would leave where it hovers (`hoverDabMesh`,
    /// colour alpha already reduced); replaces the previous dab.
    func setHover(mesh: InkMesh) {
        let geometry = InkGeometry(mesh, depth: Self.hoverDepth)
        hover.upload(geometry)
        hasHover = !geometry.isEmpty
        needsDisplay()
    }

    func clearHover() {
        guard hasHover else { return }
        hasHover = false
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
                "ink view %.0fx%.0fpt drawable %.0fx%.0fpx scale %.2f msaa %d srgb %d depth %d",
                view.bounds.width, view.bounds.height, view.drawableSize.width,
                view.drawableSize.height, view.contentScaleFactor, view.sampleCount,
                view.colorPixelFormat == Self.colorFormat ? 1 : 0,
                view.depthStencilPixelFormat == Self.depthFormat ? 1 : 0)
        }
        let size = viewport.size
        if size.width > 0, size.height > 0 {
            var uniforms = Uniforms(viewport)
            encode(encoder, uniforms: &uniforms) { draw in
                draw(batch)
                for id in wetOrder {
                    guard let entry = wet[id] else { continue }
                    draw(entry.geometry)
                }
                for id in localsOrder {
                    guard let entry = locals[id] else { continue }
                    draw(entry)
                }
                if hasLocal { draw(local) }
                if hasHover { draw(hover) }
            }
        }
        encoder.endEncoding()
        command.present(drawable)
        command.commit()
    }

    /// Bind the uniforms once, then hand the body a `draw` that encodes a
    /// geometry's runs, switching pipeline and depth state per run.
    private func encode(
        _ encoder: MTLRenderCommandEncoder, uniforms: inout Uniforms,
        _ body: ((GPUGeometry) -> Void) -> Void
    ) {
        encoder.setVertexBytes(&uniforms, length: MemoryLayout<Uniforms>.stride, index: 1)
        encoder.setFragmentBytes(&uniforms, length: MemoryLayout<Uniforms>.stride, index: 1)
        body { geometry in
            guard
                !geometry.isEmpty, let vertices = geometry.vertices,
                let strokeIndex = geometry.strokeIndex, let indices = geometry.indices,
                let styles = geometry.styles
            else { return }
            encoder.setVertexBuffer(vertices, offset: 0, index: 0)
            encoder.setVertexBuffer(styles, offset: 0, index: 2)
            encoder.setVertexBuffer(strokeIndex, offset: 0, index: 3)
            encoder.setFragmentBuffer(styles, offset: 0, index: 0)
            for run in geometry.runs {
                encoder.setRenderPipelineState(run.combo.multiply ? psoMultiply : psoNormal)
                encoder.setDepthStencilState(run.combo.discard ? depthDiscard : depthAccumulate)
                encoder.drawIndexedPrimitives(
                    type: .triangle, indexCount: run.indexCount, indexType: .uint32,
                    indexBuffer: indices,
                    indexBufferOffset: run.indexOffset * MemoryLayout<UInt32>.stride)
            }
        }
    }

    // MARK: thumbnails

    /// Render committed elements offscreen through the same pipeline the
    /// canvas uses (masks, multiply, write-once ink included), at most
    /// `maxSide` points on the long edge, 2x pixel density. `nil` when
    /// there is no ink.
    func renderThumbnail(elements: [Element], maxSide: CGFloat, background: UIColor, trait: UITraitCollection) -> UIImage? {
        let tolerance = defaultTolerance()
        var geometry = InkGeometry()
        var bounds = CGRect.null
        for (k, element) in elements.enumerated() {
            let mesh = elementMesh(element: element, tolerance: tolerance)
            bounds = bounds.union(mesh.bounds)
            geometry.append(mesh, depth: Self.committedDepth(k, of: elements.count))
        }
        guard !geometry.isEmpty, !bounds.isNull else { return nil }
        bounds = bounds.insetBy(dx: -8, dy: -8)
        let scale = max(0.1, min(1, maxSide / max(bounds.width, bounds.height, 1)))
        let pixelScale: CGFloat = 2
        let points = CGSize(width: bounds.width * scale, height: bounds.height * scale)
        let width = max(1, Int((points.width * pixelScale).rounded(.up)))
        let height = max(1, Int((points.height * pixelScale).rounded(.up)))

        let color = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: Self.colorFormat, width: width, height: height, mipmapped: false)
        color.textureType = .type2DMultisample
        color.sampleCount = Self.sampleCount
        color.usage = .renderTarget
        color.storageMode = .private
        let resolve = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: Self.colorFormat, width: width, height: height, mipmapped: false)
        resolve.usage = .renderTarget
        resolve.storageMode = .shared
        let depth = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: Self.depthFormat, width: width, height: height, mipmapped: false)
        depth.textureType = .type2DMultisample
        depth.sampleCount = Self.sampleCount
        depth.usage = .renderTarget
        depth.storageMode = .private
        guard
            let colorTexture = device.makeTexture(descriptor: color),
            let resolveTexture = device.makeTexture(descriptor: resolve),
            let depthTexture = device.makeTexture(descriptor: depth),
            let command = queue.makeCommandBuffer()
        else { return nil }

        let pass = MTLRenderPassDescriptor()
        pass.colorAttachments[0].texture = colorTexture
        pass.colorAttachments[0].resolveTexture = resolveTexture
        pass.colorAttachments[0].loadAction = .clear
        pass.colorAttachments[0].storeAction = .multisampleResolve
        pass.colorAttachments[0].clearColor = Self.clearColor(for: background, trait: trait)
        pass.depthAttachment.texture = depthTexture
        pass.depthAttachment.loadAction = .clear
        pass.depthAttachment.clearDepth = 1
        pass.depthAttachment.storeAction = .dontCare
        guard let encoder = command.makeRenderCommandEncoder(descriptor: pass) else { return nil }
        let gpu = GPUGeometry(device: device)
        gpu.upload(geometry)
        var uniforms = Uniforms(
            Viewport(
                zoom: scale,
                offset: CGPoint(x: bounds.minX * scale, y: bounds.minY * scale),
                size: points))
        encode(encoder, uniforms: &uniforms) { draw in draw(gpu) }
        encoder.endEncoding()
        command.commit()
        command.waitUntilCompleted()

        let bytesPerRow = width * 4
        var pixels = [UInt8](repeating: 0, count: bytesPerRow * height)
        pixels.withUnsafeMutableBytes { bytes in
            guard let base = bytes.baseAddress else { return }
            resolveTexture.getBytes(
                base, bytesPerRow: bytesPerRow,
                from: MTLRegionMake2D(0, 0, width, height), mipmapLevel: 0)
        }
        let info = CGBitmapInfo(
            rawValue: CGImageAlphaInfo.premultipliedFirst.rawValue | CGBitmapInfo.byteOrder32Little.rawValue)
        guard
            let provider = CGDataProvider(data: Data(pixels) as CFData),
            let image = CGImage(
                width: width, height: height, bitsPerComponent: 8, bitsPerPixel: 32,
                bytesPerRow: bytesPerRow, space: CGColorSpace(name: CGColorSpace.sRGB)!,
                bitmapInfo: info, provider: provider, decode: nil, shouldInterpolate: true,
                intent: .defaultIntent)
        else { return nil }
        return UIImage(cgImage: image, scale: pixelScale, orientation: .up)
    }
}
