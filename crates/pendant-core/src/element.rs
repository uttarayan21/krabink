//! What a sketch is made of: freehand strokes and snapped shapes, one
//! z-ordered list. Everything a renderer needs (tool, colour, width, the
//! polyline to stroke) is reachable through [`Element`] without matching on
//! the variant, so the ink pipeline draws both alike.

use crate::geom::Ink;
use crate::ids::ElementId;
use crate::shape::Shape;
use crate::stroke::{Rgba, Stroke, StrokePoint, Tool};

/// How a shape is inked: the same tool, colour and width a stroke carries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Style {
    pub tool: Tool,
    pub color: Rgba,
    /// Full ink width in canvas units.
    pub width: f32,
}

/// An arrow or line end attached to another element, Excalidraw style:
/// `fixed_point` is the attachment in the target's unit square (0..=1 on
/// each axis), `gap` the distance kept from its outline. Never written by
/// v1 clients, always parsed, so bindings can land without a migration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Binding {
    pub element: ElementId,
    pub fixed_point: [f32; 2],
    pub gap: f32,
}

/// A recognised shape as stored.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapeElement {
    pub id: ElementId,
    pub shape: Shape,
    pub style: Style,
    /// Binding of the `a` end (lines and arrows only).
    pub start: Option<Binding>,
    /// Binding of the `b` end (lines and arrows only).
    pub end: Option<Binding>,
    /// Unix millis at creation.
    pub created_ms: u64,
}

/// One entry of a sketch's z-ordered element list.
#[derive(Debug, Clone, PartialEq)]
pub enum Element {
    Stroke(Stroke),
    Shape(ShapeElement),
}

impl Element {
    pub fn id(&self) -> ElementId {
        match self {
            Self::Stroke(s) => s.id,
            Self::Shape(s) => s.id,
        }
    }

    pub fn tool(&self) -> Tool {
        match self {
            Self::Stroke(s) => s.tool,
            Self::Shape(s) => s.style.tool,
        }
    }

    pub fn color(&self) -> Rgba {
        match self {
            Self::Stroke(s) => s.color,
            Self::Shape(s) => s.style.color,
        }
    }

    /// Full ink width in canvas units.
    pub fn base_width(&self) -> f32 {
        match self {
            Self::Stroke(s) => s.base_width,
            Self::Shape(s) => s.style.width,
        }
    }

    pub fn created_ms(&self) -> u64 {
        match self {
            Self::Stroke(s) => s.created_ms,
            Self::Shape(s) => s.created_ms,
        }
    }

    /// The polyline to stroke: a stroke's flattened points or the shape's
    /// [`Shape::outline`]. Feed to [`Ink::mesh`] from [`Self::ink`].
    pub fn outline(&self) -> Vec<StrokePoint> {
        match self {
            Self::Stroke(s) => s.flatten(),
            Self::Shape(s) => s.shape.outline(),
        }
    }

    /// How this element is inked.
    pub fn ink(&self) -> Ink<'_> {
        match self {
            Self::Stroke(s) => s.ink(),
            Self::Shape(s) => {
                Ink::preset(s.style.tool, s.style.color, s.style.width).with_seed(s.id.seed())
            }
        }
    }
}

impl From<Stroke> for Element {
    fn from(s: Stroke) -> Self {
        Self::Stroke(s)
    }
}

impl From<ShapeElement> for Element {
    fn from(s: ShapeElement) -> Self {
        Self::Shape(s)
    }
}
