//! The budget meter of one read or write. Every charge comes BEFORE the
//! allocation it pays for; the first failure is sticky, later charges fail
//! without spending, and the builders stop at their next loop test.

use crate::{LimitError, Limits};

pub(crate) struct Meter {
    limits: Limits,
    nodes: usize,
    text: usize,
    err: Option<LimitError>,
}

impl Meter {
    pub(crate) fn new(limits: Limits) -> Self {
        Meter {
            limits,
            nodes: 0,
            text: 0,
            err: None,
        }
    }

    pub(crate) fn failed(&self) -> bool {
        self.err.is_some()
    }

    pub(crate) fn result(&self) -> Result<(), LimitError> {
        self.err.map_or(Ok(()), Err)
    }

    fn fail(&mut self, e: LimitError) {
        self.err.get_or_insert(e);
    }

    /// Records an allocation failure (the first failure stays).
    pub(crate) fn fail_allocation(&mut self) {
        self.fail(LimitError::Allocation);
    }

    /// Charges one node.
    pub(crate) fn node(&mut self) -> bool {
        self.nodes(1)
    }

    pub(crate) fn nodes(&mut self, n: usize) -> bool {
        if self.failed() {
            return false;
        }
        match self.nodes.checked_add(n) {
            Some(t) if t <= self.limits.nodes => {
                self.nodes = t;
                true
            }
            _ => {
                self.fail(LimitError::Exceeded("nodes"));
                false
            }
        }
    }

    /// Charges `n` owned text bytes.
    pub(crate) fn text(&mut self, n: usize) -> bool {
        if self.failed() {
            return false;
        }
        match self.text.checked_add(n) {
            Some(t) if t <= self.limits.text_bytes => {
                self.text = t;
                true
            }
            _ => {
                self.fail(LimitError::Exceeded("owned text"));
                false
            }
        }
    }

    /// Owned copy of `s`, charged first and reserved fallibly. On failure
    /// the empty string comes back and the meter is failed.
    pub(crate) fn own(&mut self, s: &str) -> String {
        let mut out = String::new();
        if self.text(s.len()) {
            self.reserve_str(&mut out, s.len());
            if !self.failed() {
                out.push_str(s);
            }
        }
        out
    }

    /// Fallible reservation for `n` more bytes in `out`.
    pub(crate) fn reserve_str(&mut self, out: &mut String, n: usize) {
        if out.try_reserve(n).is_err() {
            self.fail(LimitError::Allocation);
        }
    }

    /// Fallible push (room for one more element is reserved first).
    pub(crate) fn push<T>(&mut self, v: &mut Vec<T>, item: T) {
        if self.failed() {
            return;
        }
        if v.try_reserve(1).is_err() {
            self.fail(LimitError::Allocation);
            return;
        }
        v.push(item);
    }
}

/// The output budget of a writer: a sink that counts every byte before it
/// is appended and reserves fallibly.
pub(crate) struct Sink {
    limit: usize,
    err: Option<LimitError>,
}

impl Sink {
    pub(crate) fn new(limits: Limits) -> Self {
        Sink {
            limit: limits.output_bytes,
            err: None,
        }
    }

    pub(crate) fn failed(&self) -> bool {
        self.err.is_some()
    }

    pub(crate) fn result(&self) -> Result<(), LimitError> {
        self.err.map_or(Ok(()), Err)
    }

    /// Appends `s` to `out` unless the result would pass the budget (`out`
    /// is one piece; a piece is never longer than the final output, a
    /// nested block's text is copied into its parent's, so each piece is
    /// held to the whole budget).
    pub(crate) fn push_str(&mut self, out: &mut String, s: &str) {
        if self.room(out, s.len()) {
            out.push_str(s);
        }
    }

    pub(crate) fn push(&mut self, out: &mut String, c: char) {
        if self.room(out, c.len_utf8()) {
            out.push(c);
        }
    }

    /// Whether `n` more bytes fit in `out`; on success the room is
    /// reserved.
    pub(crate) fn room(&mut self, out: &mut String, n: usize) -> bool {
        if self.err.is_some() {
            return false;
        }
        match out.len().checked_add(n) {
            Some(t) if t <= self.limit => {}
            _ => {
                self.err = Some(LimitError::Exceeded("output"));
                return false;
            }
        }
        if out.try_reserve(n).is_err() {
            self.err = Some(LimitError::Allocation);
            return false;
        }
        true
    }
}
