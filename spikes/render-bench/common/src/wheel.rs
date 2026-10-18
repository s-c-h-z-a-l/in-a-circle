//! Wheel geometry and palette shared by every backend, so they draw the same scene.

/// Side of the square region the wheel occupies, in pixels.
pub const BOX: u32 = 480;
pub const SEGMENTS: u32 = 8;
pub const R_OUT: f32 = 220.0;
pub const R_IN: f32 = 80.0;
/// Inner disc radius (the "cancel" hub).
pub const R_HUB: f32 = 64.0;
/// Half the gap between segments, measured along the arc, in pixels.
pub const GAP: f32 = 2.0;

/// Premultiplied RGBA.
pub const SEG: [f32; 4] = premul([0.15, 0.15, 0.18], 0.85);
pub const HOVER: [f32; 4] = premul([0.95, 0.75, 0.20], 0.95);
pub const HUB: [f32; 4] = premul([0.08, 0.08, 0.10], 0.90);

const fn premul(c: [f32; 3], a: f32) -> [f32; 4] {
    [c[0] * a, c[1] * a, c[2] * a, a]
}

/// Where the wheel sits on a surface of the given size: top-left of its box.
pub fn origin(width: u32, height: u32) -> (u32, u32) {
    (
        width.saturating_sub(BOX) / 2,
        height.saturating_sub(BOX) / 2,
    )
}

/// Hovered segment for a given animation frame (cycles through all of them,
/// so every frame actually changes pixels).
pub fn hover_for(frame: u32) -> u32 {
    frame % SEGMENTS
}
