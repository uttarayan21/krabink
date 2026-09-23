// Swift smoke test for the krabink-ffi bindings (run via swift-smoke.sh).
// Exercises the full local surface: create note, edit text, commit a stroke,
// then reopen the store in a fresh Core and verify everything persisted.

import Foundation

// Scoped so the first Core deallocates (releasing the redb lock) before the
// reopen below.
func createAndEdit(dir: String) throws -> (note: String, sketch: String, stroke: String) {
    let core = try Core(dataDir: dir)
    let note = try core.createNote(title: "smoke")
    try note.applyTextEdit(at: 0, del: 0, insert: "# hello from swift")

    let sketch = try note.createSketch()
    let strokeId = try note.beginStroke(sketch: sketch, tool: .pen, color: 0x1E3C_C8FF, baseWidth: 3.0, spec: nil)

    // Model raw touches the way the iPad canvas will: push, predict, finish.
    let modeler = BrushModeler(tool: .pen, size: 3.0)
    let live = modeler.push(samples: (0..<16).map {
        RawSample(x: Float($0) * 3, y: Float($0), force: 0.5, tMs: 1000 + Double($0) * 8, tilt: nil,
                  estimationId: nil, expectsUpdate: false)
    })
    guard !live.isEmpty, live.allSatisfy({ $0.size == nil }) else { fatalError("modeler emitted no ink") }
    let tail = modeler.predict(samples: [RawSample(x: 60, y: 20, force: 0.5, tMs: 1200, tilt: nil,
                                                   estimationId: nil, expectsUpdate: false)])
    guard modeler.points() == live, tail.count == 1 else { fatalError("predict mutated the modeler") }
    try note.appendPoints(stroke: strokeId, seq: 1, points: live)
    let points = modeler.finish()
    let brush = BrushRef(tool: .pen, baseWidth: 3.0)
    let mesh = pointsMesh(points: points, brush: brush, color: 0x1E3C_C8FF, end: .complete, tolerance: defaultTolerance())
    guard mesh.indexCount >= 3, mesh.indexCount % 3 == 0, mesh.indices.count == Int(mesh.indexCount) * 4
    else { fatalError("empty mesh") }
    guard mesh.vertices.count == Int(mesh.vertexCount) * Int(inkVertexFloats()) * 4, mesh.style.color == 0x1E3C_C8FF else {
        fatalError("mesh layout")
    }

    let stroke = Stroke(
        id: strokeId, tool: .pen, color: 0x1E3C_C8FF, baseWidth: 3.0,
        kind: .polylineSample, points: points, createdMs: 1)
    guard strokeMesh(stroke: stroke, tolerance: defaultTolerance()) == mesh else {
        fatalError("live and committed meshes differ")
    }
    try note.finishStroke(sketch: sketch, stroke: stroke, tail: Array(points[live.count...]))

    // Shape recognition: a held rough rectangle snaps and commits as a shape.
    let rectModeler = BrushModeler(tool: .pen, size: 3.0)
    let corners: [(Float, Float)] = [(0, 0), (120, 1), (119, 80), (1, 79), (0, 2)]
    var rectSamples: [RawSample] = []
    for i in 0..<(corners.count - 1) {
        let (ax, ay) = corners[i], (bx, by) = corners[i + 1]
        for k in 0..<30 {
            let t = Float(k) / 30
            rectSamples.append(RawSample(
                x: ax + (bx - ax) * t, y: ay + (by - ay) * t, force: 0.6,
                tMs: Double(rectSamples.count) * 8, tilt: nil, estimationId: nil, expectsUpdate: false))
        }
    }
    _ = rectModeler.push(samples: rectSamples)
    guard let snapped = recognizeShape(points: rectModeler.points(), holdRadius: 4.5),
          case .rect = snapped.shape else { fatalError("rectangle not recognised") }
    let shapeId = try note.beginStroke(sketch: sketch, tool: .pen, color: 0xFF00_00FF, baseWidth: 3.0, spec: nil)
    let shape = ShapeElement(
        id: shapeId, shape: snapped.shape, tool: .pen, color: 0xFF00_00FF, width: 3.0,
        start: nil, end: nil, createdMs: 2)
    let preview = shapeOutlineMesh(shape: snapped.shape, brush: brush, color: 0xFF00_00FF, tolerance: defaultTolerance())
    guard elementMesh(element: .shape(shape), tolerance: defaultTolerance()) == preview else {
        fatalError("shape preview and committed meshes differ")
    }
    try note.finishShape(sketch: sketch, shape: shape)

    // Bundled image assets: content-hashed PNGs, the chalk brush samples two of them.
    let assets = builtinAssets()
    guard assets.count == 2, assets.allSatisfy({ $0.png.count <= 64 * 1024 }) else { fatalError("assets") }
    let chalkSpec = builtinBrushes().first { $0.id == "builtin:chalk" }!.spec
    guard brushAssets(spec: chalkSpec).count == 2 else { fatalError("chalk assets") }
    let chalkStyle = pointsMesh(
        points: [StrokePoint(x: 0, y: 0, force: 0.5, tMs: 0, tilt: nil, size: nil)],
        brush: BrushRef(tool: .pencil, baseWidth: 8, custom: CustomBrush(id: "builtin:chalk", spec: chalkSpec)),
        color: 0xFF, end: .complete, tolerance: defaultTolerance()
    ).style
    guard case .image = chalkStyle.mask, chalkStyle.grain?.image != nil else { fatalError("chalk style: \(chalkStyle)") }
    let plain = brushWithMask(spec: brushWithGrainImage(spec: chalkSpec, asset: nil), asset: nil)
    guard brushAssets(spec: plain).isEmpty else { fatalError("asset removal") }
    let paperId = try core.putAsset(name: "paper again", kind: .grain, png: assets[0].png)
    guard paperId == assets[0].id else { fatalError("asset id is the content hash") }

    // A bundled stamped brush: dabs masked in the shader, spec snapshotted on the stroke.
    let builtins = builtinBrushes()
    guard let crayon = builtins.first(where: { $0.id == "builtin:crayon" }) else { fatalError("no crayon") }
    let crayonRef = BrushRef(
        tool: .pencil, baseWidth: 9.0, custom: CustomBrush(id: crayon.id, spec: crayon.spec))
    let crayonId = try note.beginStroke(
        sketch: sketch, tool: .pencil, color: 0xC800_00FF, baseWidth: 9.0, spec: crayon.spec)
    let crayonModeler = BrushModeler.forBrush(brush: crayonRef)
    _ = crayonModeler.push(samples: (0..<24).map {
        RawSample(x: Float($0) * 6, y: 40 + Float($0 % 3), force: 0.6, tMs: 2000 + Double($0) * 8, tilt: nil,
                  estimationId: nil, expectsUpdate: false)
    })
    let crayonPoints = crayonModeler.finish()
    let dabs = pointsMesh(points: crayonPoints, brush: crayonRef, color: 0xC800_00FF, end: .complete,
                          tolerance: defaultTolerance())
    guard dabs.zoomIndependent, dabs.indexCount % 6 == 0, dabs.indexCount >= 6 * 20,
          case .shape = dabs.style.mask
    else { fatalError("crayon did not stamp: \(dabs.indexCount) indices, \(dabs.style.mask)") }
    let crayonStroke = Stroke(
        id: crayonId, tool: .pencil, color: 0xC800_00FF, baseWidth: 9.0, kind: .polylineSample,
        points: crayonPoints, createdMs: 3, brush: CustomBrush(id: crayon.id, spec: crayon.spec))
    guard strokeMesh(stroke: crayonStroke, tolerance: defaultTolerance()).indexCount == dabs.indexCount else {
        fatalError("committed crayon lost its spec")
    }
    try note.finishStroke(sketch: sketch, stroke: crayonStroke, tail: crayonPoints)

    guard try note.text() == "# hello from swift" else { fatalError("text mismatch") }
    guard try note.strokes(sketch: sketch).count == 2 else { fatalError("stroke missing") }
    let elements = try note.elements(sketch: sketch)
    guard elements.count == 3, case .shape(let stored) = elements[1], stored.id == shapeId else {
        fatalError("shape missing from elements")
    }
    guard core.listNotes().first?.title == "smoke" else { fatalError("registry mismatch") }
    return (note.id(), sketch, strokeId)
}

func verifyPersisted(dir: String, ids: (note: String, sketch: String, stroke: String)) throws {
    let core = try Core(dataDir: dir)
    let notes = core.listNotes()
    guard notes.count == 1, notes[0].title == "smoke" else { fatalError("persisted registry mismatch") }

    let note = try core.openNote(id: ids.note)
    guard try note.text() == "# hello from swift" else { fatalError("persisted text mismatch") }
    let strokes = try note.strokes(sketch: ids.sketch)
    guard strokes.count == 2, strokes[0].id == ids.stroke, strokes[0].points.count > 1 else {
        fatalError("persisted stroke mismatch")
    }
    guard strokes[0].brush == nil, strokes[1].brush?.id == "builtin:crayon" else {
        fatalError("persisted brush mismatch: \(String(describing: strokes[1].brush?.id))")
    }
    guard try note.elements(sketch: ids.sketch).count == 3 else { fatalError("persisted shape mismatch") }
    print("swift smoke OK — note \(ids.note), device \(core.deviceId())")
}

let dir = FileManager.default.temporaryDirectory
    .appendingPathComponent("krabink-smoke-\(UUID().uuidString)").path
let ids = try createAndEdit(dir: dir)
try verifyPersisted(dir: dir, ids: ids)
