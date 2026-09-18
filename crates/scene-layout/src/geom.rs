//! Canvas-space geometry. Everything layout emits is in canvas pixels,
//! origin top-left — the same space Skia draws in.

/// Intrinsic or computed content size in canvas pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Size {
    pub w: f64,
    pub h: f64,
}

impl Size {
    pub const ZERO: Size = Size { w: 0.0, h: 0.0 };

    pub fn is_empty(self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }
}

/// A placed box: `x,y` is the top-left corner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn center(self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

/// How an element's pixels fill its rect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// Scale to fill the rect, cropping overflow — b-roll semantics.
    Cover,
    /// The rect IS the content — text, boards.
    Content,
}

/// Per-frame presentation offset, applied about the rect's centre.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    pub dx: f64,
    pub dy: f64,
    pub scale: f64,
}

impl Transform {
    pub const IDENTITY: Transform = Transform {
        dx: 0.0,
        dy: 0.0,
        scale: 1.0,
    };

    pub fn is_identity(self) -> bool {
        self == Transform::IDENTITY
    }
}
