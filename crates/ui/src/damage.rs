//! Damage relative to what the screen shows, not to the buffer being drawn.
//!
//! With two buffers in turn, the buffer being drawn holds the state from two
//! frames ago, while the compositor shows the other one. Damaging only what
//! this frame repainted leaves the other buffer's last change on screen (the
//! spike's bug).
//!
//! The invariant: every paint (`Wheel::paint_full`, `Wheel::paint_hover`)
//! leaves its buffer equal to a full paint of the new state, and the
//! rectangle it returns contains every pixel where that state differs from
//! the unlit base (the same wheel with no slice lit): the lit slice, or the
//! whole box for `paint_full`. Two buffers then differ only where one of
//! them differs from the base, so the buffer being attached differs from the
//! one on screen only within its own painted rectangle and the one painted
//! into the screen's buffer last: the damage is their union. Older frames do
//! not matter, so the damage does not grow with the history.
//!
//! The base is one `Wheel`. After a new `Wheel` (stage 1 -> stage 2) the
//! next paint of *each* buffer must be `paint_full` with the new one; until
//! then a buffer holds the old wheel and `paint_hover` would leave it so.

use crate::wheel::{BOX, Rect};

const FULL: Rect = Rect {
    x: 0,
    y: 0,
    w: BOX,
    h: BOX,
};

pub struct Screen {
    /// Per buffer: the rectangle of its last paint.
    last: [Rect; 2],
    /// Per buffer: never attached, content unknown.
    fresh: [bool; 2],
}

impl Screen {
    /// Buffer 0 is painted in full and on screen; buffer 1 is unpainted, so
    /// its first frame damages the whole box.
    ///
    /// Call order: paint buffer 0 with `paint_full` and attach it (the first
    /// frame, outside `Screen`). Then per frame: paint the free buffer
    /// (`paint_full` the first time buffer 1 is used, `paint_hover` after),
    /// attach it with the damage `commit` returns. Never `paint_hover` a
    /// buffer that was not painted in full with the current `Wheel`: after
    /// a new `Wheel`, the next paint of each buffer is `paint_full`.
    pub fn new() -> Screen {
        Screen {
            last: [FULL, Rect::EMPTY],
            fresh: [false, true],
        }
    }

    /// Damage to report if buffer `buf` (0 or 1), in which `painted` just
    /// changed, were attached now.
    ///
    /// # Panics
    /// If `buf` is not 0 or 1.
    pub fn damage_for(&self, buf: usize, painted: Rect) -> Rect {
        if self.fresh[buf] {
            return FULL;
        }
        painted.union(self.last[1 - buf])
    }

    /// Record that buffer `buf` was attached with `painted` changed since it
    /// was last painted; returns the damage to report.
    ///
    /// # Panics
    /// If `buf` is not 0 or 1.
    pub fn commit(&mut self, buf: usize, painted: Rect) -> Rect {
        let damage = self.damage_for(buf, painted);
        self.last[buf] = painted;
        self.fresh[buf] = false;
        damage
    }
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wheel::BOX;

    const FULL: Rect = Rect {
        x: 0,
        y: 0,
        w: BOX,
        h: BOX,
    };
    const fn r(x: u32) -> Rect {
        Rect {
            x,
            y: 0,
            w: 10,
            h: 10,
        }
    }

    #[test]
    fn new_has_buffer_0_on_screen_and_buffer_1_unpainted() {
        let s = Screen::new();
        assert_eq!(s.damage_for(0, Rect::EMPTY), Rect::EMPTY);
        assert_eq!(s.damage_for(0, r(5)), r(5));
        // Buffer 1 has never been shown: its first frame damages everything.
        assert_eq!(s.damage_for(1, Rect::EMPTY), FULL);
        assert_eq!(s.damage_for(1, r(5)), FULL);
    }

    /// Each frame damages what it painted plus what the previous frame
    /// painted (the screen shows that one), never the whole history.
    #[test]
    fn damage_is_this_frame_plus_the_one_on_screen() {
        let mut s = Screen::new();
        assert_eq!(s.commit(1, FULL), FULL); // first frame of buffer 1
        // Buffer 0 follows a full frame, so it is full once more ...
        assert_eq!(s.commit(0, r(0)), FULL);
        // ... then the history is forgotten: disjoint rects do not pile up.
        assert_eq!(s.commit(1, r(100)), r(100).union(r(0)));
        assert_eq!(s.commit(0, r(200)), r(200).union(r(100)));
        assert_eq!(s.commit(1, r(300)), r(300).union(r(200)));
        assert_eq!(s.commit(0, r(400)), r(400).union(r(300)));
        assert_eq!(s.commit(1, Rect::EMPTY), r(400));
        assert_eq!(s.commit(0, Rect::EMPTY), Rect::EMPTY);
    }

    /// The compositor may still hold the other buffer: the same one is drawn
    /// twice in a row.
    #[test]
    fn same_buffer_twice_stays_covered() {
        let mut s = Screen::new();
        s.commit(1, FULL);
        s.commit(0, r(0));
        s.commit(1, r(100));
        assert_eq!(s.commit(1, r(200)), r(200).union(r(0)));
        // The screen now shows buffer 1, lit within r(200) only (r(100) was
        // repainted unlit). Buffer 0's frame must cover its own paint and
        // r(200).
        let d = s.commit(0, r(300));
        assert!(d.contains(r(300)) && d.contains(r(200)), "{d:?}");
    }

    #[test]
    fn damage_for_does_not_change_the_state() {
        let mut s = Screen::new();
        s.commit(1, FULL);
        let a = s.damage_for(0, r(7));
        assert_eq!(a, s.damage_for(0, r(7)));
        assert_eq!(s.commit(0, r(7)), a);
    }
}
