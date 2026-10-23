//! Damage relative to what the screen shows, not to the buffer being drawn.
//!
//! With two buffers in turn, the buffer being drawn holds the state from two
//! frames ago, while the compositor shows the other one. Damaging only what
//! this frame repainted leaves the other buffer's last change on screen (the
//! spike's bug). `Screen` tracks, per buffer, the region where it may differ
//! from the screen, and adds it to the damage of every commit.

use crate::wheel::Rect;

/// Which of two buffers shows what. Starts with both buffers and the screen
/// identical (e.g. after painting both fully).
pub struct Screen {
    /// Per buffer: where it may differ from the screen.
    stale: [Rect; 2],
}

impl Screen {
    pub fn new() -> Screen {
        Screen {
            stale: [Rect::EMPTY; 2],
        }
    }

    /// Damage to report if buffer `buf` (0 or 1), in which `painted` just
    /// changed, were attached now.
    pub fn damage_for(&self, buf: usize, painted: Rect) -> Rect {
        self.stale[buf].union(painted)
    }

    /// Record that buffer `buf` was attached with `painted` changed since it
    /// was last attached; returns the damage to report. The screen then
    /// equals `buf`, and the other buffer is stale wherever the screen moved.
    pub fn commit(&mut self, buf: usize, painted: Rect) -> Rect {
        let damage = self.damage_for(buf, painted);
        self.stale[buf] = Rect::EMPTY;
        self.stale[1 - buf] = self.stale[1 - buf].union(damage);
        damage
    }
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}
