//! The wheel's state machine: pure, no syscalls, driven by `surface`.
//!
//! Two levels. Level 1 shows categories, level 2 the targets (presets) of
//! the chosen one; `Done` follows. The way in depends on the early read of
//! the drag's `text/uri-list` (started at `enter`):
//!
//! - list in before the drop: the categories are on the wheel while the
//!   drag is over it; only a lit one accepts; dropping on it shows its
//!   targets (no second read);
//! - no list (no `text/uri-list`, an error, or the drop came first): `Drop
//!   here`, hub and every slice a target; the drop is read, the categories
//!   appear, a click on a lit one shows its targets.
//!
//! Every way into `Done` emits exactly one `Close` or one `Run`, and nothing
//! after it, so the surface is torn down on every path.
//!
//! Coordinates are surface-local: the overlay covers the output, so they are
//! output-local too. The wheel is placed once, at the first position known.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use engine::catalog::Catalog;
use engine::menu::Tally;
use ui::wheel::{Hit, Slice, SliceState, Wheel, place};

/// Life of each stage: hotkey to drop, then drop to choice (and, after a
/// fallback drop, category to choice). The surface's own deadline and the
/// `alarm` backstop both use it.
pub const STAGE: Duration = Duration::from_secs(30);
/// A choosing click closer than this to the wheel's replacement is ignored.
pub const SWAP_GUARD: Duration = Duration::from_millis(200);
pub const KEY_ESC: u32 = 1;
pub const BTN_LEFT: u32 = 0x110;
pub const URI_LIST: &str = "text/uri-list";

pub const DROP_HERE: &str = "Drop here";
pub const NOTHING: &str = "Nothing to do";
/// The hub text for a drop list past the byte or URI limit. Short on
/// purpose: "too many files (max 1024)" is 184 px, the hub shows 108
/// (`HUB_TEXT_W`) and would cut it to "too many file…"; this one is 101 px
/// (a test keeps it whole and in the atlas). The limit is in the docs.
pub const TOO_MANY: &str = "too many files";
/// The read's error for a list past `uri::MAX_BYTES`: it ends in the same
/// hub text as a list with too many URIs.
pub const LIST_TOO_BIG: &str = "URI list over the byte limit";
/// Stage 2 before the catalog was ever cached: nothing can be offered.
pub const NO_ACTIONS: &str = "No actions yet";

#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// The overlay's size (the output's).
    Configured(u32, u32),
    /// The pointer entered or moved (no drag).
    PointerAt(f64, f64),
    /// A button press.
    Click(u32 /* button */),
    /// A key press.
    Key(u32),
    DragEnter {
        x: f64,
        y: f64,
        mimes: Vec<String>,
    },
    DragMotion(f64, f64),
    DragLeave,
    Drop,
    /// The dropped `text/uri-list`, or why it could not be read.
    DropData(Result<Vec<u8>, String>),
    /// The early read's `text/uri-list` (started at `enter`), or why it
    /// could not be read, tagged with the `ReadEarly` tag it was started
    /// with; one that is not the latest is ignored. Not used after the
    /// drop.
    DragList(u32, Result<Vec<u8>, String>),
    /// The stage's deadline passed.
    Deadline,
    /// The compositor closed the surface or the connection failed.
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Show `Machine::wheel()` at `Machine::origin()` with
    /// `Machine::hover()`: the wheel was just placed or replaced. A buffer
    /// painted with an older `Machine::generation()` needs `paint_full`.
    Draw,
    /// Only the highlighted slice changed.
    Hover(Option<u8>),
    /// Over a target: accept `text/uri-list`, copy only. Else `accept(null)`
    /// and `set_actions(none)`.
    Accept(bool),
    /// Start receiving the offer's `text/uri-list` now, before the drop,
    /// and feed back `DragList` with the same tag (the machine's, bumped on
    /// every `enter`; the surface only echoes it). A new one replaces the
    /// earlier read.
    ReadEarly(u32),
    /// The drop was taken without a second read (the early list was in):
    /// the surface ends the dropped offer now. `finish` only if the drop's
    /// action is copy (else a protocol error), then release the keep and
    /// destroy the offer. Comes first among the drop's effects.
    EndDrop,
    /// Receive the drop's `text/uri-list` and feed back `DropData`; only
    /// when there is no early list.
    ReadDrop,
    /// Re-arm the deadline (surface and `alarm`) to this from now.
    Arm(Duration),
    /// Tear down and exit, no action.
    Close,
    /// Tear down, then run preset `.1` on the files.
    Run(PathList, String /* preset id */),
}

/// One level-1 slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupChoice {
    pub label: String,
    /// The category's id, for `TargetsFn`.
    pub group: String,
    /// False: drawn dim, not a target.
    pub lit: bool,
}

/// The dropped files, held once: levels 1 and 2, the click's effect and the
/// machine share one list (a clone is a count, not a copy of up to a
/// mebibyte of paths). Once the machine is dropped the list is unique, and
/// `Rc::try_unwrap` hands the vector to the run without a copy.
pub type PathList = Rc<Vec<PathBuf>>;

/// Level 1, made from the early or the dropped list.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupMenu {
    pub paths: PathList,
    /// By wheel slice index: the compass, 0 = north, clockwise.
    pub slices: [Option<GroupChoice>; 8],
    /// The source: a name, or the count.
    pub hub: String,
    /// What level 2 is built from.
    pub tally: Tally,
    /// The catalog level 1 was built from, kept so level 2 offers the same
    /// presets (one snapshot, loaded once).
    pub catalog: Option<Catalog>,
}

/// One level-2 slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub label: String,
    pub preset: String,
    /// False: drawn dim, a click does nothing.
    pub applicable: bool,
}

/// Level 2, the targets of one category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    pub paths: PathList,
    /// By wheel slice index: the compass, 0 = north, clockwise.
    pub slices: [Option<Choice>; 8],
    pub hub: String,
}

/// Builds level 1 from a `text/uri-list`. The real one classifies files;
/// tests pass a pure one.
pub type GroupsFn = fn(&[u8]) -> GroupMenu;

/// Builds level 2 for one category of level 1.
pub type TargetsFn = fn(&GroupMenu, &str) -> Menu;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the surface needs only the effects")
)]
pub enum Stage {
    Waiting,
    /// Level 1 after a fallback drop: click a category.
    Grouping,
    Choosing,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Stage 1, nothing known yet.
    Pending,
    /// The pointer came without a drag: `Nothing to do`.
    NoDrag,
    /// A drag is over us; whether we last accepted it.
    Dragging {
        accepted: Option<bool>,
    },
    /// Dropped without an early list, waiting for the data.
    Reading,
    /// Level 1 after the fallback read: a click on a lit category.
    Grouping,
    Choosing,
    Done,
}

/// The early read of the drag's list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Early {
    /// Not started (no `text/uri-list` on offer).
    Off,
    Pending,
    /// In: level 1 is on the wheel.
    Ready,
    /// Failed: `Drop here` stays.
    Failed,
}

pub struct Machine {
    phase: Phase,
    early: Early,
    /// Tag of the latest `enter`'s early read.
    early_seq: u32,
    /// The latest time `on_at` was given, if ever.
    now: Option<Duration>,
    /// When the wheel was last replaced.
    swapped_at: Option<Duration>,
    groups_fn: GroupsFn,
    targets_fn: TargetsFn,
    output: Option<(u32, u32)>,
    /// Last known pointer position.
    cursor: Option<(f64, f64)>,
    /// Top-left of the wheel, once placed.
    origin: Option<(i32, i32)>,
    hover: Option<u8>,
    wheel: Wheel,
    hub: String,
    /// Level 1, once the list is known.
    groups: Option<GroupMenu>,
    /// Level 2, once a category is chosen.
    menu: Option<Menu>,
    /// Whether the drag offers `text/uri-list`.
    uris: bool,
    generation: u32,
}

impl Machine {
    pub fn new() -> Machine {
        Machine::with_menus(crate::group_menu, crate::target_menu)
    }

    pub fn with_menus(groups_fn: GroupsFn, targets_fn: TargetsFn) -> Machine {
        Machine {
            phase: Phase::Pending,
            early: Early::Off,
            early_seq: 0,
            now: None,
            swapped_at: None,
            groups_fn,
            targets_fn,
            output: None,
            cursor: None,
            origin: None,
            hover: None,
            wheel: stage1(DROP_HERE),
            hub: DROP_HERE.into(),
            groups: None,
            menu: None,
            uris: false,
            generation: 0,
        }
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the surface needs only the effects")
    )]
    pub fn stage(&self) -> Stage {
        match self.phase {
            Phase::Pending | Phase::NoDrag | Phase::Dragging { .. } | Phase::Reading => {
                Stage::Waiting
            }
            Phase::Grouping => Stage::Grouping,
            Phase::Choosing => Stage::Choosing,
            Phase::Done => Stage::Done,
        }
    }

    pub fn wheel(&self) -> &Wheel {
        &self.wheel
    }

    /// The hub's text before fitting.
    #[cfg_attr(not(test), expect(dead_code, reason = "the wheel holds it fitted"))]
    pub fn hub(&self) -> &str {
        &self.hub
    }

    pub fn origin(&self) -> Option<(i32, i32)> {
        self.origin
    }

    pub fn hover(&self) -> Option<u8> {
        self.hover
    }

    /// Bumped whenever `wheel()` is replaced: a buffer painted with an
    /// older generation needs `paint_full`.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// `on` at monotonic time `now` (any origin): a click soon after the
    /// wheel was replaced is ignored. Without times (`on`) no click is.
    pub fn on_at(&mut self, now: Duration, i: Input) -> Vec<Effect> {
        self.now = Some(now);
        self.on(i)
    }

    pub fn on(&mut self, i: Input) -> Vec<Effect> {
        let mut fx = Vec::new();
        if self.phase == Phase::Done {
            return fx;
        }
        match i {
            Input::Configured(w, h) => {
                self.output = Some((w, h));
                self.settle(&mut fx, false);
                // A drag that came first can be hit now.
                self.update_accept(&mut fx);
            }
            Input::PointerAt(x, y) => {
                self.cursor = Some((x, y));
                let redraw = self.phase == Phase::Pending;
                if redraw {
                    self.phase = Phase::NoDrag;
                    self.set_wheel(stage1(NOTHING), NOTHING.into());
                }
                self.settle(&mut fx, redraw);
            }
            Input::Click(button) => match self.phase {
                Phase::Pending | Phase::NoDrag => self.close(&mut fx),
                Phase::Grouping => self.click_group(&mut fx, button),
                Phase::Choosing => self.click(&mut fx, button),
                // No clicks reach us during a drag; after the drop, one
                // before the data arrives (at most 500 ms) is not a choice.
                _ => {}
            },
            Input::Key(KEY_ESC) => self.close(&mut fx),
            Input::Key(_) => {}
            Input::DragEnter { x, y, mimes } => match self.phase {
                Phase::Pending | Phase::NoDrag | Phase::Dragging { .. } => {
                    self.cursor = Some((x, y));
                    self.uris = mimes.iter().any(|m| m == URI_LIST);
                    // A new offer forgets the old one's list.
                    let redraw = self.phase == Phase::NoDrag || self.early == Early::Ready;
                    if redraw {
                        self.set_wheel(stage1(DROP_HERE), DROP_HERE.into());
                    }
                    self.groups = None;
                    self.early_seq = self.early_seq.wrapping_add(1);
                    self.early = if self.uris {
                        fx.push(Effect::ReadEarly(self.early_seq));
                        Early::Pending
                    } else {
                        Early::Off
                    };
                    self.phase = Phase::Dragging { accepted: None };
                    self.settle(&mut fx, redraw);
                    self.update_accept(&mut fx);
                }
                // Done with dragging: refuse another one.
                _ => fx.push(Effect::Accept(false)),
            },
            Input::DragMotion(x, y) => {
                if let Phase::Dragging { .. } = self.phase {
                    self.cursor = Some((x, y));
                    self.settle(&mut fx, false);
                    self.update_accept(&mut fx);
                }
            }
            Input::DragLeave => {
                if let Phase::Dragging { .. } = self.phase {
                    self.close(&mut fx);
                }
            }
            Input::DragList(seq, r) => {
                if let (Phase::Dragging { .. }, Early::Pending) = (self.phase, self.early)
                    && seq == self.early_seq
                {
                    if self.show_list(r) {
                        self.early = Early::Ready;
                        self.settle(&mut fx, true);
                        self.update_accept(&mut fx);
                    } else {
                        self.early = Early::Failed;
                    }
                }
            }
            Input::Drop => match self.phase {
                Phase::Dragging {
                    accepted: Some(true),
                } if self.early == Early::Ready => match self.hit_group() {
                    // The list is in: no second read.
                    Some(group) => {
                        fx.extend([Effect::EndDrop, Effect::Arm(STAGE)]);
                        self.show_targets(&group);
                        self.settle(&mut fx, true);
                    }
                    None => self.close(&mut fx),
                },
                Phase::Dragging {
                    accepted: Some(true),
                } => {
                    self.phase = Phase::Reading;
                    fx.extend([Effect::Arm(STAGE), Effect::ReadDrop]);
                }
                // Dropped off target: the compositor cancels the drag.
                Phase::Dragging { .. } => self.close(&mut fx),
                _ => {}
            },
            Input::DropData(r) => {
                if self.phase == Phase::Reading {
                    if self.show_list(r) {
                        self.phase = Phase::Grouping;
                        self.settle(&mut fx, true);
                    } else {
                        self.close(&mut fx);
                    }
                }
            }
            Input::Deadline | Input::Closed => self.close(&mut fx),
        }
        fx
    }

    fn close(&mut self, fx: &mut Vec<Effect>) {
        self.phase = Phase::Done;
        fx.push(Effect::Close);
    }

    fn set_wheel(&mut self, wheel: Wheel, hub: String) {
        self.wheel = wheel;
        self.hub = hub;
        self.swapped_at = self.now;
        self.generation = self.generation.wrapping_add(1);
    }

    /// What is under the pointer; `Outside` while unplaced.
    fn hit(&self) -> Hit {
        match (self.cursor, self.origin) {
            (Some((x, y)), Some((ox, oy))) => self.wheel.hit(x - f64::from(ox), y - f64::from(oy)),
            _ => Hit::Outside,
        }
    }

    /// Whether the early list was accepted and level 1 is built from it.
    pub fn early_ready(&self) -> bool {
        self.early == Early::Ready
    }

    /// Level 1 from a read's result; false if the list could not be read.
    /// A list past the limits shows `TOO_MANY` with nothing lit (fail
    /// closed: no truncated selection), like an empty one shows `Nothing
    /// to do`.
    fn show_list(&mut self, r: Result<Vec<u8>, String>) -> bool {
        match r {
            Ok(bytes) if !crate::uri::too_many(&bytes) => self.show_groups(&bytes),
            Ok(_) => self.show_refused(),
            Err(e) if e == LIST_TOO_BIG => self.show_refused(),
            Err(_) => return false,
        }
        true
    }

    /// Level 1 with nothing on offer and `TOO_MANY` in the hub.
    fn show_refused(&mut self) {
        let gm = GroupMenu {
            paths: PathList::default(),
            slices: Default::default(),
            hub: TOO_MANY.into(),
            tally: Tally::default(),
            catalog: None,
        };
        self.set_wheel(group_wheel(&gm), gm.hub.clone());
        self.groups = Some(gm);
    }

    /// Level 1 from the list, onto the wheel.
    fn show_groups(&mut self, list: &[u8]) {
        let gm = (self.groups_fn)(list);
        self.set_wheel(group_wheel(&gm), gm.hub.clone());
        self.groups = Some(gm);
    }

    /// Level 2 for `group`, onto the wheel.
    fn show_targets(&mut self, group: &str) {
        let Some(gm) = &self.groups else {
            debug_assert!(false, "targets of {group} without level 1");
            return;
        };
        let menu = (self.targets_fn)(gm, group);
        self.set_wheel(stage2(&menu), menu.hub.clone());
        self.menu = Some(menu);
        self.phase = Phase::Choosing;
    }

    /// The lit category under the pointer on level 1, if that is showing.
    fn hit_group(&self) -> Option<String> {
        let Hit::Slice(k) = self.hit() else {
            return None;
        };
        let g = self.groups.as_ref()?.slices[usize::from(k)].as_ref()?;
        g.lit.then(|| g.group.clone())
    }

    /// Whether level 1 is on the wheel.
    fn level1(&self) -> bool {
        match self.phase {
            Phase::Grouping => true,
            Phase::Dragging { .. } => self.early == Early::Ready,
            _ => false,
        }
    }

    /// The slice to light: a lit category (level 1) or an applicable target
    /// (level 2) under the pointer.
    fn lit(&self) -> Option<u8> {
        let Hit::Slice(k) = self.hit() else {
            return None;
        };
        let lit = if self.level1() {
            self.hit_group().is_some()
        } else {
            self.menu.as_ref().is_some_and(|m| {
                self.phase == Phase::Choosing
                    && m.slices[usize::from(k)]
                        .as_ref()
                        .is_some_and(|c| c.applicable)
            })
        };
        lit.then_some(k)
    }

    /// Place the wheel once size and a finite position are known, then emit
    /// `Draw` (first placement or `redraw`) or a hover change.
    fn settle(&mut self, fx: &mut Vec<Effect>, redraw: bool) {
        let mut placed_now = false;
        if self.origin.is_none()
            && let (Some((w, h)), Some((x, y))) = (self.output, self.cursor)
            && x.is_finite()
            && y.is_finite()
        {
            // Saturating casts: any finite position is fine.
            self.origin = Some(place((x as i32, y as i32), (w, h)));
            placed_now = true;
        }
        if self.origin.is_none() {
            return;
        }
        let lit = self.lit();
        if placed_now || redraw {
            self.hover = lit;
            fx.push(Effect::Draw);
        } else if lit != self.hover {
            self.hover = lit;
            fx.push(Effect::Hover(lit));
        }
    }

    /// With a URI list on offer: over a lit category (level 1), else over
    /// the wheel (`Drop here`, hub or any slice): accept.
    fn update_accept(&mut self, fx: &mut Vec<Effect>) {
        let Phase::Dragging { accepted } = self.phase else {
            return;
        };
        let want = self.uris
            && if self.level1() {
                self.hit_group().is_some()
            } else {
                self.hit() != Hit::Outside
            };
        if accepted != Some(want) {
            self.phase = Phase::Dragging {
                accepted: Some(want),
            };
            fx.push(Effect::Accept(want));
        }
    }

    /// A choosing click this soon after the wheel was replaced is the
    /// second half of a double click (or the user did not see the new
    /// wheel): not a choice.
    fn too_soon(&self) -> bool {
        match (self.now, self.swapped_at) {
            (Some(now), Some(at)) => now.saturating_sub(at) < SWAP_GUARD,
            _ => false,
        }
    }

    /// Level 1 after a fallback drop: a lit category opens its targets.
    fn click_group(&mut self, fx: &mut Vec<Effect>, button: u32) {
        let Some(gm) = &self.groups else {
            return self.close(fx);
        };
        let any_lit = gm.slices.iter().flatten().any(|g| g.lit);
        match self.hit() {
            Hit::Outside => self.close(fx),
            // Nothing to do or to choose: any click closes.
            _ if gm.paths.is_empty() || !any_lit => self.close(fx),
            Hit::Slice(_) if button == BTN_LEFT && !self.too_soon() => {
                if let Some(group) = self.hit_group() {
                    fx.push(Effect::Arm(STAGE));
                    self.show_targets(&group);
                    self.settle(fx, true);
                }
            }
            _ => {}
        }
    }

    fn click(&mut self, fx: &mut Vec<Effect>, button: u32) {
        let Some(menu) = &self.menu else {
            return self.close(fx);
        };
        match self.hit() {
            Hit::Outside => self.close(fx),
            // Nothing to do: any click closes.
            _ if menu.paths.is_empty() => self.close(fx),
            Hit::Slice(k) if button == BTN_LEFT && !self.too_soon() => {
                if let Some(c) = &menu.slices[usize::from(k)]
                    && c.applicable
                {
                    let run = Effect::Run(Rc::clone(&menu.paths), c.preset.clone());
                    self.phase = Phase::Done;
                    fx.push(run);
                }
            }
            _ => {}
        }
    }
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

/// Stage 1 (or `Nothing to do`): empty slices, `hub` in the middle.
fn stage1(hub: &str) -> Wheel {
    Wheel::new(
        std::array::from_fn(|_| Slice {
            label: "",
            state: SliceState::Empty,
        }),
        hub,
    )
}

/// Level 1: the categories, lit ones normal, others dim; the source in the
/// hub.
fn group_wheel(gm: &GroupMenu) -> Wheel {
    let slices = std::array::from_fn(|k| match &gm.slices[k] {
        Some(g) => Slice {
            label: &g.label,
            state: if g.lit {
                SliceState::Normal
            } else {
                SliceState::Dim
            },
        },
        None => Slice {
            label: "",
            state: SliceState::Empty,
        },
    });
    Wheel::new(slices, &gm.hub)
}

/// Level 2: the menu's slices, applicable ones normal, others dim.
fn stage2(menu: &Menu) -> Wheel {
    let slices = std::array::from_fn(|k| match &menu.slices[k] {
        Some(c) => Slice {
            label: &c.label,
            state: if c.applicable {
                SliceState::Normal
            } else {
                SliceState::Dim
            },
        },
        None => Slice {
            label: "",
            state: SliceState::Empty,
        },
    });
    Wheel::new(slices, &menu.hub)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ui::wheel::BOX;

    const W: u32 = 1920;
    const H: u32 = 1080;
    /// The first position: the wheel's center (not near an edge).
    const AT: (f64, f64) = (960.0, 540.0);

    /// A point `r` from the center at slice `k`'s middle angle.
    fn slice(k: u8, r: f64) -> (f64, f64) {
        let a = f64::from(k) * std::f64::consts::FRAC_PI_4 - std::f64::consts::FRAC_PI_2;
        (AT.0 + r * a.cos(), AT.1 + r * a.sin())
    }
    const HUB: (f64, f64) = AT;
    const OUT: (f64, f64) = (AT.0 + 300.0, AT.1);

    fn group(label: &str, id: &str, lit: bool) -> Option<GroupChoice> {
        Some(GroupChoice {
            label: label.into(),
            group: id.into(),
            lit,
        })
    }

    /// Empty: nothing. Else one file; category `g0` at 0 is lit, `g4` at 4
    /// is dim.
    fn fake_groups(bytes: &[u8]) -> GroupMenu {
        let mut slices: [Option<GroupChoice>; 8] = Default::default();
        if bytes.is_empty() {
            return GroupMenu {
                paths: PathList::default(),
                slices,
                hub: NOTHING.into(),
                tally: Tally::default(),
                catalog: None,
            };
        }
        slices[0] = group("Zero", "g0", true);
        slices[4] = group("Four", "g4", false);
        GroupMenu {
            paths: vec![PathBuf::from("/x")].into(),
            slices,
            hub: "x".into(),
            tally: Tally::default(),
            catalog: None,
        }
    }

    /// `g0`: slice 0 applies (`p0`), slice 4 does not.
    fn fake_targets(gm: &GroupMenu, group: &str) -> Menu {
        let mut slices: [Option<Choice>; 8] = Default::default();
        if group == "g0" {
            slices[0] = Some(Choice {
                label: "Zero".into(),
                preset: "p0".into(),
                applicable: true,
            });
            slices[4] = Some(Choice {
                label: "Four".into(),
                preset: "p4".into(),
                applicable: false,
            });
        }
        Menu {
            paths: Rc::clone(&gm.paths),
            slices,
            hub: "Zero".into(),
        }
    }

    /// Two files; categories: 3 lit (`g3`), 1 dim (`g1`). `g3` runs `p3`
    /// at 3 and has a dim `p1` at 1.
    fn two_groups(_: &[u8]) -> GroupMenu {
        let mut slices: [Option<GroupChoice>; 8] = Default::default();
        slices[3] = group("Three", "g3", true);
        slices[1] = group("One", "g1", false);
        GroupMenu {
            paths: vec![PathBuf::from("/a"), PathBuf::from("/b c")].into(),
            slices,
            hub: "2 files".into(),
            tally: Tally::default(),
            catalog: None,
        }
    }

    fn two_targets(gm: &GroupMenu, _: &str) -> Menu {
        let mut slices: [Option<Choice>; 8] = Default::default();
        slices[3] = Some(Choice {
            label: "Three".into(),
            preset: "p3".into(),
            applicable: true,
        });
        slices[1] = Some(Choice {
            label: "One".into(),
            preset: "p1".into(),
            applicable: false,
        });
        Menu {
            paths: Rc::clone(&gm.paths),
            slices,
            hub: "Three".into(),
        }
    }

    fn fake_machine() -> Machine {
        Machine::with_menus(fake_groups, fake_targets)
    }

    fn uris() -> Vec<String> {
        vec!["text/plain".into(), URI_LIST.into()]
    }

    fn enter(p: (f64, f64)) -> Input {
        Input::DragEnter {
            x: p.0,
            y: p.1,
            mimes: uris(),
        }
    }

    /// The early list of the first (or `seq`th) `enter`.
    fn list() -> Input {
        list_n(1)
    }

    fn list_n(seq: u32) -> Input {
        Input::DragList(seq, Ok(b"a".to_vec()))
    }

    /// A group menu with files, none of its categories lit.
    fn dim_groups(_: &[u8]) -> GroupMenu {
        let mut gm = fake_groups(b"a");
        gm.slices[0] = group("Zero", "g0", false);
        gm
    }

    fn feed(m: &mut Machine, inputs: impl IntoIterator<Item = Input>) -> Vec<Effect> {
        inputs.into_iter().flat_map(|i| m.on(i)).collect()
    }

    /// Dragging, the early read pending: `Drop here`.
    fn dragging() -> Machine {
        let mut m = fake_machine();
        feed(&mut m, [Input::Configured(W, H), enter(HUB)]);
        m
    }

    /// Dragging with the early list in: the categories.
    fn listed() -> Machine {
        let mut m = dragging();
        feed(&mut m, [list()]);
        m
    }

    /// Dropped without an early list; the read is out.
    fn reading() -> Machine {
        let mut m = dragging();
        feed(&mut m, [Input::Drop]);
        m
    }

    /// The fallback's categories, to be clicked.
    fn grouping() -> Machine {
        let mut m = reading();
        feed(&mut m, [Input::DropData(Ok(b"a".to_vec()))]);
        m
    }

    /// Dropped on the lit category: the targets.
    fn choosing() -> Machine {
        let mut m = listed();
        let s = slice(0, 150.0);
        feed(&mut m, [Input::DragMotion(s.0, s.1), Input::Drop]);
        assert_eq!(m.stage(), Stage::Choosing);
        m
    }

    fn no_drag() -> Machine {
        let mut m = fake_machine();
        feed(
            &mut m,
            [Input::Configured(W, H), Input::PointerAt(HUB.0, HUB.1)],
        );
        m
    }

    /// One machine per non-final phase.
    fn every_stage() -> Vec<(&'static str, Machine)> {
        vec![
            ("pending", fake_machine()),
            ("no drag", no_drag()),
            ("dragging", dragging()),
            ("listed", listed()),
            ("reading", reading()),
            ("grouping", grouping()),
            ("choosing", choosing()),
        ]
    }

    #[test]
    fn stage_is_30_s() {
        assert_eq!(STAGE, Duration::from_secs(30));
    }

    #[test]
    fn early_list_shows_groups_then_drop_shows_targets() {
        let mut m = fake_machine();
        assert_eq!((m.stage(), m.hub()), (Stage::Waiting, DROP_HERE));
        assert_eq!(m.on(Input::Configured(W, H)), []);
        assert_eq!(m.origin(), None);
        // The early read starts first; the wheel is painted meanwhile.
        assert_eq!(
            m.on(enter(HUB)),
            [Effect::ReadEarly(1), Effect::Draw, Effect::Accept(true)]
        );
        let half = (BOX / 2) as i32;
        assert_eq!(m.origin(), Some((960 - half, 540 - half)));
        let s = slice(0, 150.0);
        assert_eq!(m.on(Input::DragMotion(s.0, s.1)), []);
        // The list arrives with the pointer on the lit category.
        assert_eq!(m.on(list()), [Effect::Draw]);
        assert_eq!((m.stage(), m.hub()), (Stage::Waiting, "x"));
        assert_eq!(m.hover(), Some(0));
        let d = slice(4, 150.0);
        assert_eq!(
            m.on(Input::DragMotion(d.0, d.1)),
            [Effect::Hover(None), Effect::Accept(false)]
        );
        assert_eq!(
            m.on(Input::DragMotion(s.0, s.1)),
            [Effect::Hover(Some(0)), Effect::Accept(true)]
        );
        // The drop reads nothing more: the targets of the category.
        // ... and the surface ends the dropped offer (finish, destroy).
        assert_eq!(
            m.on(Input::Drop),
            [Effect::EndDrop, Effect::Arm(STAGE), Effect::Draw]
        );
        assert_eq!((m.stage(), m.hub()), (Stage::Choosing, "Zero"));
        assert_eq!(m.hover(), Some(0));
        // The compositor's leave right after the drop does not close.
        assert_eq!(m.on(Input::DragLeave), []);
        assert_eq!(
            m.on(Input::Click(BTN_LEFT)),
            [Effect::Run(vec![PathBuf::from("/x")].into(), "p0".into())]
        );
        assert_eq!(m.stage(), Stage::Done);
        assert_eq!(m.on(Input::Deadline), []);
        assert_eq!(m.on(Input::Click(BTN_LEFT)), []);
    }

    #[test]
    fn drop_without_early_list_falls_back_to_drop_here_then_groups_by_click() {
        let mut m = fake_machine();
        m.on(Input::Configured(W, H));
        // No URI list on offer: no early read, `Drop here` accepts nothing.
        let e = m.on(Input::DragEnter {
            x: HUB.0,
            y: HUB.1,
            mimes: vec!["text/plain".into()],
        });
        assert_eq!(e, [Effect::Draw, Effect::Accept(false)]);
        // With one: the read starts, the drop beats the list.
        let mut m = dragging();
        assert_eq!((m.stage(), m.hub()), (Stage::Waiting, DROP_HERE));
        assert_eq!(m.on(Input::Drop), [Effect::Arm(STAGE), Effect::ReadDrop]);
        // The early list that comes late is not used.
        assert_eq!(m.on(list()), []);
        assert_eq!(m.on(Input::DragLeave), []);
        assert_eq!(m.on(Input::DropData(Ok(b"a".to_vec()))), [Effect::Draw]);
        assert_eq!((m.stage(), m.hub()), (Stage::Grouping, "x"));
        // The hub, a dim and an empty category do nothing on a click.
        assert_eq!(m.on(Input::Click(BTN_LEFT)), [], "hub");
        for k in [4, 2] {
            let s = slice(k, 150.0);
            m.on(Input::PointerAt(s.0, s.1));
            assert_eq!(m.on(Input::Click(BTN_LEFT)), [], "slice {k}");
        }
        assert_eq!(m.stage(), Stage::Grouping);
        // A lit one: the targets, with a fresh deadline.
        let s = slice(0, 150.0);
        assert_eq!(m.on(Input::PointerAt(s.0, s.1)), [Effect::Hover(Some(0))]);
        assert_eq!(m.on(Input::Click(0x111)), [], "only the left button");
        assert_eq!(
            m.on(Input::Click(BTN_LEFT)),
            [Effect::Arm(STAGE), Effect::Draw]
        );
        assert_eq!((m.stage(), m.hub()), (Stage::Choosing, "Zero"));
        assert_eq!(
            m.on(Input::Click(BTN_LEFT)),
            [Effect::Run(vec![PathBuf::from("/x")].into(), "p0".into())]
        );
        assert_eq!(m.on(Input::Click(BTN_LEFT)), []);
    }

    #[test]
    fn choose_runs_the_preset() {
        let mut m = Machine::with_menus(two_groups, two_targets);
        feed(&mut m, [Input::Configured(W, H), enter(HUB), list()]);
        let g = slice(3, 180.0);
        m.on(Input::DragMotion(g.0, g.1));
        assert_eq!(
            m.on(Input::Drop),
            [Effect::EndDrop, Effect::Arm(STAGE), Effect::Draw]
        );
        let s = slice(3, 180.0);
        assert_eq!(m.hover(), Some(3));
        assert_eq!(m.on(Input::PointerAt(s.0, s.1)), []);
        assert_eq!(
            m.on(Input::Click(BTN_LEFT)),
            [Effect::Run(
                vec![PathBuf::from("/a"), PathBuf::from("/b c")].into(),
                "p3".into()
            )]
        );
        assert_eq!(m.stage(), Stage::Done);
        // Nothing after the run: no second choice, no close.
        assert_eq!(m.on(Input::Click(BTN_LEFT)), []);
        assert_eq!(m.on(Input::Closed), []);
    }

    #[test]
    fn dim_target_is_not_clickable() {
        let mut m = Machine::with_menus(two_groups, two_targets);
        feed(&mut m, [Input::Configured(W, H), enter(HUB), Input::Drop]);
        m.on(Input::DropData(Ok(b"x".to_vec())));
        let g = slice(3, 150.0);
        m.on(Input::PointerAt(g.0, g.1));
        m.on(Input::Click(BTN_LEFT));
        assert_eq!(m.stage(), Stage::Choosing);
        let d = slice(1, 150.0);
        assert_eq!(m.on(Input::PointerAt(d.0, d.1)), [Effect::Hover(None)]);
        for b in [BTN_LEFT, 0x111, 0x112] {
            assert_eq!(m.on(Input::Click(b)), [], "button {b:#x}");
        }
        assert_eq!(m.stage(), Stage::Choosing);
        // Neither do empty slices.
        let e = slice(6, 150.0);
        m.on(Input::PointerAt(e.0, e.1));
        assert_eq!(m.on(Input::Click(BTN_LEFT)), []);
        assert_eq!(m.stage(), Stage::Choosing);
    }

    #[test]
    fn accept_only_on_lit_categories() {
        let mut m = dragging();
        // The list arrives with the pointer on the hub: not a target.
        assert_eq!(m.on(list()), [Effect::Draw, Effect::Accept(false)]);
        let lit = slice(0, 150.0);
        let dim = slice(4, 150.0);
        let empty = slice(2, 150.0);
        assert_eq!(
            m.on(Input::DragMotion(lit.0, lit.1)),
            [Effect::Hover(Some(0)), Effect::Accept(true)]
        );
        assert_eq!(
            m.on(Input::DragMotion(dim.0, dim.1)),
            [Effect::Hover(None), Effect::Accept(false)]
        );
        assert_eq!(m.on(Input::DragMotion(empty.0, empty.1)), []);
        m.on(Input::DragMotion(lit.0, lit.1));
        assert_eq!(
            m.on(Input::DragMotion(HUB.0, HUB.1)),
            [Effect::Hover(None), Effect::Accept(false)]
        );
        m.on(Input::DragMotion(lit.0, lit.1));
        assert_eq!(
            m.on(Input::DragMotion(OUT.0, OUT.1)),
            [Effect::Hover(None), Effect::Accept(false)]
        );
        // A drop off a lit category is cancelled by the compositor.
        for p in [dim, empty, HUB, OUT] {
            let mut m = listed();
            m.on(Input::DragMotion(p.0, p.1));
            assert_eq!(m.on(Input::Drop), [Effect::Close], "{p:?}");
        }
        // `Drop here` (no early list): the hub and every slice accept.
        let mut m = dragging();
        for p in [HUB, slice(1, 150.0), slice(5, 210.0), dim, empty] {
            assert_eq!(m.on(Input::DragMotion(p.0, p.1)), [], "{p:?}");
        }
        assert_eq!(
            m.on(Input::DragMotion(OUT.0, OUT.1)),
            [Effect::Accept(false)]
        );
        assert_eq!(m.on(Input::DragMotion(OUT.0, OUT.1 + 5.0)), []);
        assert_eq!(m.on(Input::DragMotion(f64::NAN, 0.0)), []);
        assert_eq!(
            m.on(Input::DragMotion(HUB.0, HUB.1)),
            [Effect::Accept(true)]
        );
        // So are they after a failed early read.
        let mut m = dragging();
        m.on(Input::DragList(1, Err("x".into())));
        assert_eq!(m.on(Input::DragMotion(dim.0, dim.1)), []);
        // Entered in a screen corner: the wheel is pushed onto the output,
        // so the pointer is outside it, and the drag rejected from the start.
        let mut m = fake_machine();
        m.on(Input::Configured(W, H));
        assert_eq!(
            m.on(enter((0.0, 0.0))),
            [Effect::ReadEarly(1), Effect::Draw, Effect::Accept(false)]
        );
        assert_eq!(m.origin(), Some((0, 0)));
        let c = f64::from(BOX / 2);
        assert_eq!(m.on(Input::DragMotion(c, c)), [Effect::Accept(true)]);
        // No URI list on offer: never accepted, a drop just closes.
        let mut m = fake_machine();
        m.on(Input::Configured(W, H));
        m.on(Input::DragEnter {
            x: HUB.0,
            y: HUB.1,
            mimes: vec!["text/plain".into()],
        });
        assert_eq!(m.on(Input::DragMotion(HUB.0 + 1.0, HUB.1)), []);
        assert_eq!(m.on(Input::Drop), [Effect::Close]);
        // A drag arriving later (grouping, choosing) is refused.
        for mut m in [grouping(), choosing(), reading()] {
            let stage = m.stage();
            assert_eq!(m.on(enter(HUB)), [Effect::Accept(false)]);
            assert_eq!(m.on(Input::DragLeave), []);
            assert_eq!(m.stage(), stage);
        }
    }

    #[test]
    fn reenter_discards_the_early_list() {
        let mut m = listed();
        assert_eq!(m.hub(), "x");
        // Another offer: back to `Drop here`, a new early read.
        assert_eq!(
            m.on(enter(HUB)),
            [Effect::ReadEarly(2), Effect::Draw, Effect::Accept(true)]
        );
        assert_eq!((m.stage(), m.hub()), (Stage::Waiting, DROP_HERE));
        // The old list is gone: a drop reads again, not the old categories.
        assert_eq!(m.on(Input::Drop), [Effect::Arm(STAGE), Effect::ReadDrop]);
        // An offer without a URI list also forgets the old one; a list
        // still in flight for the earlier offer is not used.
        let mut m = listed();
        let plain = Input::DragEnter {
            x: HUB.0,
            y: HUB.1,
            mimes: vec!["text/plain".into()],
        };
        assert_eq!(m.on(plain), [Effect::Draw, Effect::Accept(false)]);
        assert_eq!(m.on(list()), []);
        assert_eq!(m.hub(), DROP_HERE);
        // The new offer's own list is read and used.
        let mut m = listed();
        feed(&mut m, [enter(HUB)]);
        assert_eq!(m.on(list_n(2)), [Effect::Draw, Effect::Accept(false)]);
        assert_eq!(m.hub(), "x");
    }

    /// Regression: an `enter` and the old offer's list in one batch. The
    /// old list must not be shown, nor run on the new drag.
    #[test]
    fn a_stale_list_is_ignored() {
        let mut m = dragging();
        assert_eq!(
            m.on(enter(HUB)),
            [Effect::ReadEarly(2), Effect::Accept(true)]
        );
        assert_eq!(m.on(list_n(1)), [], "the first offer's list");
        assert_eq!((m.stage(), m.hub()), (Stage::Waiting, DROP_HERE));
        // A future or repeated tag is no better.
        assert_eq!(m.on(list_n(3)), []);
        assert_eq!(m.on(list_n(2)), [Effect::Draw, Effect::Accept(false)]);
        assert_eq!(m.hub(), "x");
        // Used once: the same tag again, now Ready, changes nothing.
        assert_eq!(m.on(list_n(2)), []);
        // An error for an old offer does not fail the new one.
        let mut m = dragging();
        m.on(enter(HUB));
        assert_eq!(m.on(Input::DragList(1, Err("x".into()))), []);
        assert_eq!(m.on(list_n(2)), [Effect::Draw, Effect::Accept(false)]);
    }

    /// Only PDFs, say, and the early read failed: nothing to choose, so a
    /// click closes (the early path closes on the drop).
    #[test]
    fn fallback_without_a_lit_category_closes_on_click() {
        for p in [HUB, slice(0, 150.0), slice(2, 150.0)] {
            let mut m = Machine::with_menus(dim_groups, fake_targets);
            feed(
                &mut m,
                [
                    Input::Configured(W, H),
                    enter(HUB),
                    Input::Drop,
                    Input::DropData(Ok(b"a".to_vec())),
                    Input::PointerAt(p.0, p.1),
                ],
            );
            assert_eq!(m.stage(), Stage::Grouping);
            assert_eq!(m.on(Input::Click(BTN_LEFT)), [Effect::Close], "{p:?}");
        }
    }

    /// A fast double click must not choose a category and, with the next
    /// click, the target in the same direction.
    #[test]
    fn a_click_right_after_a_swap_is_ignored() {
        let ms = Duration::from_millis;
        let s = slice(0, 150.0);
        let mut m = fake_machine();
        m.on_at(ms(0), Input::Configured(W, H));
        m.on_at(ms(0), enter(HUB));
        m.on_at(ms(10), Input::Drop);
        // The fallback's level 1 appears at 1000 ms.
        m.on_at(ms(1000), Input::DropData(Ok(b"a".to_vec())));
        m.on_at(ms(1010), Input::PointerAt(s.0, s.1));
        assert_eq!(m.on_at(ms(1100), Input::Click(BTN_LEFT)), [], "too soon");
        assert_eq!(m.stage(), Stage::Grouping);
        assert_eq!(
            m.on_at(ms(1200), Input::Click(BTN_LEFT)),
            [Effect::Arm(STAGE), Effect::Draw]
        );
        // The double click's second half, 50 ms later, is not a choice.
        assert_eq!(m.on_at(ms(1250), Input::Click(BTN_LEFT)), []);
        assert_eq!(m.stage(), Stage::Choosing);
        assert_eq!(
            m.on_at(ms(1500), Input::Click(BTN_LEFT)),
            [Effect::Run(vec![PathBuf::from("/x")].into(), "p0".into())]
        );
        // The guard does not delay closing outside the wheel.
        let mut m = fake_machine();
        m.on_at(ms(0), Input::Configured(W, H));
        m.on_at(ms(0), enter(HUB));
        m.on_at(ms(10), Input::Drop);
        m.on_at(ms(1000), Input::DropData(Ok(b"a".to_vec())));
        m.on_at(ms(1010), Input::PointerAt(OUT.0, OUT.1));
        assert_eq!(m.on_at(ms(1020), Input::Click(BTN_LEFT)), [Effect::Close]);
    }

    #[test]
    fn early_list_error_falls_back() {
        let mut m = dragging();
        assert_eq!(m.on(Input::DragList(1, Err("timed out".into()))), []);
        assert_eq!((m.stage(), m.hub()), (Stage::Waiting, DROP_HERE));
        // A second result does not change that.
        assert_eq!(m.on(list()), []);
        assert_eq!(m.hub(), DROP_HERE);
        assert_eq!(m.on(Input::Drop), [Effect::Arm(STAGE), Effect::ReadDrop]);
        assert_eq!(m.on(Input::DropData(Ok(b"a".to_vec()))), [Effect::Draw]);
        assert_eq!(m.stage(), Stage::Grouping);
        // The drop read failing closes.
        let mut m = reading();
        assert_eq!(
            m.on(Input::DropData(Err("timed out".into()))),
            [Effect::Close]
        );
        // A list outside a drag is nothing.
        assert_eq!(fake_machine().on(list()), []);
        assert_eq!(no_drag().on(list()), []);
    }

    #[test]
    fn escape_closes() {
        for (name, mut m) in every_stage() {
            assert_eq!(m.on(Input::Key(30)), [], "{name}: other keys do nothing");
            assert_eq!(m.on(Input::Key(KEY_ESC)), [Effect::Close], "{name}");
            assert_eq!(m.stage(), Stage::Done);
            assert_eq!(m.on(Input::Key(KEY_ESC)), [], "{name}: closes once");
        }
    }

    #[test]
    fn click_outside_closes() {
        let mut m = choosing();
        // A dim slice and the hub do nothing.
        let s = slice(4, 150.0);
        assert_eq!(m.on(Input::PointerAt(s.0, s.1)), [Effect::Hover(None)]);
        assert_eq!(m.on(Input::Click(BTN_LEFT)), []);
        assert_eq!(m.on(Input::PointerAt(HUB.0, HUB.1)), []);
        assert_eq!(m.on(Input::Click(BTN_LEFT)), []);
        // Another button on an applicable slice does not choose.
        let s = slice(0, 150.0);
        m.on(Input::PointerAt(s.0, s.1));
        assert_eq!(m.on(Input::Click(0x111)), []);
        assert_eq!(m.on(Input::PointerAt(OUT.0, OUT.1)), [Effect::Hover(None)]);
        assert_eq!(m.on(Input::Click(0x111)), [Effect::Close]);
        assert_eq!(m.stage(), Stage::Done);
        // Outside, the left button closes too, at either level.
        let mut m = choosing();
        m.on(Input::PointerAt(OUT.0, OUT.1));
        assert_eq!(m.on(Input::Click(BTN_LEFT)), [Effect::Close]);
        let mut m = grouping();
        m.on(Input::PointerAt(OUT.0, OUT.1));
        assert_eq!(m.on(Input::Click(BTN_LEFT)), [Effect::Close]);
    }

    #[test]
    fn drag_leave_without_drop_closes() {
        for (name, mut m) in [("dragging", dragging()), ("listed", listed())] {
            assert_eq!(m.on(Input::DragLeave), [Effect::Close], "{name}");
            assert_eq!(m.stage(), Stage::Done);
        }
        // Dropped off target: also closed, nothing read.
        let mut m = dragging();
        m.on(Input::DragMotion(OUT.0, OUT.1));
        assert_eq!(m.on(Input::Drop), [Effect::Close]);
    }

    #[test]
    fn deadline_closes_in_every_stage() {
        for (name, mut m) in every_stage() {
            assert_eq!(m.on(Input::Deadline), [Effect::Close], "{name}");
            assert_eq!(m.stage(), Stage::Done);
        }
        // A lost compositor closes too.
        for (name, mut m) in every_stage() {
            assert_eq!(m.on(Input::Closed), [Effect::Close], "{name}");
        }
    }

    /// Any input sequence followed by the deadline ends with exactly one
    /// `Close` or `Run`, as the last effect, and the machine `Done`.
    #[test]
    fn every_exit_path_tears_down() {
        let pool = |k: u64| -> Input {
            let pts = [HUB, OUT, slice(0, 150.0), slice(4, 150.0), slice(2, 100.0)];
            let p = pts[(k / 20) as usize % pts.len()];
            match k % 20 {
                0 => Input::Configured(W, H),
                1 => Input::PointerAt(p.0, p.1),
                2 => Input::Click(BTN_LEFT),
                3 => Input::Click(0x111),
                4 => Input::Key(KEY_ESC),
                5 => Input::Key(30),
                6 => enter(p),
                7 => Input::DragEnter {
                    x: p.0,
                    y: p.1,
                    mimes: vec!["text/plain".into()],
                },
                8 => Input::DragMotion(p.0, p.1),
                9 => Input::DragLeave,
                10 => Input::Drop,
                11 => Input::DropData(Ok(b"a".to_vec())),
                12 => Input::DropData(Ok(Vec::new())),
                13 => Input::DropData(Err("x".into())),
                14 => Input::Closed,
                15 => Input::DragList(1 + (k / 7 % 3) as u32, Ok(b"a".to_vec())),
                16 => Input::DragList(1, Ok(Vec::new())),
                17 => Input::DragList(1, Err("x".into())),
                18 => Input::Click(BTN_LEFT),
                _ => Input::PointerAt(f64::NAN, p.1),
            }
        };
        // xorshift: deterministic sequences.
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let (mut runs, mut closes) = (0, 0);
        // Random inputs after a random prefix of the way to a choice, so
        // every phase is reached: the early way, and the fallback's.
        let s0 = slice(0, 150.0);
        let paths = [
            vec![
                Input::Configured(W, H),
                enter(HUB),
                list(),
                Input::DragMotion(s0.0, s0.1),
                Input::Drop,
                Input::PointerAt(s0.0, s0.1),
            ],
            vec![
                Input::Configured(W, H),
                enter(HUB),
                Input::Drop,
                Input::DropData(Ok(b"a".to_vec())),
                Input::PointerAt(s0.0, s0.1),
                Input::Click(BTN_LEFT),
            ],
        ];
        for _ in 0..40_000 {
            let mut m = fake_machine();
            // A pseudo-random 0-299 ms between inputs, so the swap guard
            // is exercised both ways.
            let mut clock = Duration::ZERO;
            let mut step = |next: &mut dyn FnMut() -> u64| {
                clock += Duration::from_millis(next() % 300);
                clock
            };
            let happy = &paths[(next() % 2) as usize];
            let prefix = (next() % (happy.len() as u64 + 1)) as usize;
            let mut effects: Vec<Effect> = Vec::new();
            for i in &happy[..prefix] {
                effects.extend(m.on_at(step(&mut next), i.clone()));
            }
            let len = next() % 8;
            for _ in 0..len {
                let at = step(&mut next);
                effects.extend(m.on_at(at, pool(next())));
            }
            effects.extend(m.on_at(step(&mut next), Input::Deadline));
            let ends = effects
                .iter()
                .filter(|e| matches!(e, Effect::Close | Effect::Run(..)))
                .count();
            assert_eq!(ends, 1, "{effects:?}");
            assert!(
                matches!(effects.last(), Some(Effect::Close | Effect::Run(..))),
                "{effects:?}"
            );
            assert_eq!(m.stage(), Stage::Done);
            match effects.last() {
                Some(Effect::Run(..)) => runs += 1,
                _ => closes += 1,
            }
        }
        assert!(runs > 0 && closes > 0, "runs {runs}, closes {closes}");
    }

    #[test]
    fn hover_only_repaints_on_change() {
        let mut m = choosing();
        let (a, b) = (slice(0, 120.0), slice(0, 200.0));
        assert_eq!(m.on(Input::PointerAt(HUB.0, HUB.1)), [Effect::Hover(None)]);
        assert_eq!(m.on(Input::PointerAt(a.0, a.1)), [Effect::Hover(Some(0))]);
        assert_eq!(m.on(Input::PointerAt(b.0, b.1)), []);
        assert_eq!(m.hover(), Some(0));
        // A dim slice does not light up; leaving 0 for it unlights 0.
        let d = slice(4, 150.0);
        assert_eq!(m.on(Input::PointerAt(d.0, d.1)), [Effect::Hover(None)]);
        assert_eq!(m.on(Input::PointerAt(HUB.0, HUB.1)), []);
        assert_eq!(m.on(Input::PointerAt(OUT.0, OUT.1)), []);
        // An empty slice neither.
        let e = slice(2, 150.0);
        assert_eq!(m.on(Input::PointerAt(e.0, e.1)), []);
        // `Drop here` never lights a slice.
        let mut m = dragging();
        for k in 0..8 {
            let s = slice(k, 150.0);
            assert_eq!(m.on(Input::DragMotion(s.0, s.1)), []);
        }
        // The position at each swap is the hover of the new wheel.
        let mut m = dragging();
        let s = slice(0, 150.0);
        m.on(Input::DragMotion(s.0, s.1));
        m.on(Input::Drop);
        assert_eq!(m.on(Input::DropData(Ok(b"a".to_vec()))), [Effect::Draw]);
        assert_eq!(m.hover(), Some(0));
        m.on(Input::Click(BTN_LEFT));
        assert_eq!(m.stage(), Stage::Choosing);
        assert_eq!(m.hover(), Some(0));
    }

    /// A list past the limits (bytes, or more than 1024 URIs) says why and
    /// offers nothing, early and after the drop; it never offers a
    /// truncated selection.
    #[test]
    fn a_list_over_the_limits_is_refused_with_a_reason() {
        let many: Vec<u8> = (0..=crate::uri::MAX_URIS)
            .flat_map(|i| format!("file:///f{i}\n").into_bytes())
            .collect();
        for r in [Ok(many), Err(LIST_TOO_BIG.to_owned())] {
            let mut m = dragging();
            let fx = m.on(Input::DragList(1, r.clone()));
            assert_eq!(fx, [Effect::Draw, Effect::Accept(false)]);
            assert_eq!(m.hub(), TOO_MANY);
            assert_eq!(m.on(Input::Drop), [Effect::Close]);
            // After the drop: the reason stays until a click closes.
            let mut m = reading();
            assert_eq!(m.on(Input::DropData(r)), [Effect::Draw]);
            assert_eq!(m.hub(), TOO_MANY);
            assert_eq!(m.on(Input::PointerAt(HUB.0, HUB.1)), []);
            assert_eq!(m.on(Input::Click(BTN_LEFT)), [Effect::Close]);
        }
        // Another read error is not this: the fallback / close as before.
        let mut m = reading();
        assert_eq!(
            m.on(Input::DropData(Err("timed out".into()))),
            [Effect::Close]
        );
    }

    #[test]
    fn the_refusal_text_fits_the_hub_whole() {
        assert_eq!(ui::text::fit(TOO_MANY, ui::wheel::HUB_TEXT_W), TOO_MANY);
        assert!(ui::atlas::get().has_all(TOO_MANY));
    }

    #[test]
    fn no_drag_says_nothing_to_do() {
        let mut m = fake_machine();
        m.on(Input::Configured(W, H));
        assert_eq!(m.on(Input::PointerAt(HUB.0, HUB.1)), [Effect::Draw]);
        assert_eq!((m.stage(), m.hub()), (Stage::Waiting, NOTHING));
        let s = slice(0, 150.0);
        assert_eq!(m.on(Input::PointerAt(s.0, s.1)), []);
        // Any click closes, on the wheel too.
        assert_eq!(m.on(Input::Click(BTN_LEFT)), [Effect::Close]);
        // A drop whose list yields no file: nothing to do either, from the
        // early list and from the drop's own.
        let mut m = reading();
        assert_eq!(m.on(Input::DropData(Ok(Vec::new()))), [Effect::Draw]);
        assert_eq!(m.hub(), NOTHING);
        assert_eq!(m.on(Input::PointerAt(HUB.0, HUB.1)), []);
        assert_eq!(m.on(Input::Click(BTN_LEFT)), [Effect::Close]);
        let mut m = dragging();
        assert_eq!(
            m.on(Input::DragList(1, Ok(Vec::new()))),
            [Effect::Draw, Effect::Accept(false)]
        );
        assert_eq!(m.hub(), NOTHING);
        assert_eq!(m.on(Input::Drop), [Effect::Close]);
        // A drag that comes after all goes back to `Drop here`.
        let mut m = no_drag();
        assert_eq!(
            m.on(enter(HUB)),
            [Effect::ReadEarly(1), Effect::Draw, Effect::Accept(true)]
        );
        assert_eq!(m.hub(), DROP_HERE);
    }

    /// Regression: a drag that entered before the first configure was
    /// refused (nothing to hit yet) and stayed refused when the wheel was
    /// placed under it.
    #[test]
    fn configure_recomputes_the_accept() {
        let mut m = fake_machine();
        assert_eq!(
            m.on(enter(HUB)),
            [Effect::ReadEarly(1), Effect::Accept(false)]
        );
        assert_eq!(
            m.on(Input::Configured(W, H)),
            [Effect::Draw, Effect::Accept(true)]
        );
    }

    /// The position may arrive before the size: the wheel is placed once
    /// both are known, on the output.
    #[test]
    fn placed_once_on_the_output() {
        let mut m = fake_machine();
        assert_eq!(m.on(Input::PointerAt(5.0, 5.0)), []);
        assert_eq!(m.on(Input::Configured(W, H)), [Effect::Draw]);
        assert_eq!(m.origin(), Some((0, 0)));
        m.on(Input::PointerAt(900.0, 500.0));
        assert_eq!(m.origin(), Some((0, 0)), "placed once");
        // A click before any position is not a choice.
        let mut m = fake_machine();
        assert_eq!(m.on(Input::Click(BTN_LEFT)), [Effect::Close]);
    }
}
