// Swift smoke test for the pendant-ffi bindings (run via swift-smoke.sh).
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
    let strokeId = try note.beginStroke(sketch: sketch, tool: .pen, color: 0x1E3C_C8FF, baseWidth: 3.0)
    try note.appendPoints(stroke: strokeId, seq: 1, points: [WetPoint(x: 0, y: 0, force: 0.5)])
    try note.finishStroke(
        sketch: sketch,
        stroke: Stroke(
            id: strokeId, tool: .pen, color: 0x1E3C_C8FF, baseWidth: 3.0,
            kind: .polylineSample,
            points: (0..<16).map {
                StrokePoint(x: Float($0) * 3, y: Float($0), force: 0.5, tMs: UInt32($0) * 8, tilt: nil)
            },
            createdMs: 1))

    guard try note.text() == "# hello from swift" else { fatalError("text mismatch") }
    guard try note.strokes(sketch: sketch).count == 1 else { fatalError("stroke missing") }
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
    guard strokes.count == 1, strokes[0].id == ids.stroke, strokes[0].points.count == 16 else {
        fatalError("persisted stroke mismatch")
    }
    print("swift smoke OK — note \(ids.note), device \(core.deviceId())")
}

let dir = FileManager.default.temporaryDirectory
    .appendingPathComponent("pendant-smoke-\(UUID().uuidString)").path
let ids = try createAndEdit(dir: dir)
try verifyPersisted(dir: dir, ids: ids)
