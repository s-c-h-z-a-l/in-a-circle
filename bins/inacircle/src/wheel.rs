//! The wheel's state machine: pure, no syscalls, driven by `surface`.
//!
//! Stages: `Waiting` (stage 1: `Drop here`, every slice empty, the whole
//! wheel a drop target) -> `Choosing` (stage 2: the presets for the dropped
//! files) -> `Done`. Every way into `Done` emits exactly one `Close` or one
//! `Run`, and nothing after it, so the surface is torn down on every path.
//!
//! Coordinates are surface-local: the overlay covers the output, so they are
//! output-local too. The wheel is placed once, at the first position known.

use std::path::PathBuf;
use std::time::Duration;

use ui::wheel::{Hit, Slice, SliceState, Wheel, place};

/// Life of each stage: hotkey to drop, then drop to choice. The surface's
/// own deadline and the `alarm` backstop both use it.
pub const STAGE: Duration = Duration::from_secs(10);
pub const KEY_ESC: u32 = 1;
pub const BTN_LEFT: u32 = 0x110;
pub const URI_LIST: &str = "text/uri-list";

pub const DROP_HERE: &str = "Drop here";
pub const NOTHING: &str = "Nothing to do";

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
    /// Receive the drop's `text/uri-list` and feed back `DropData`.
    ReadDrop,
    /// Re-arm the deadline (surface and `alarm`) to this from now.
    Arm(Duration),
    /// Tear down and exit, no action.
    Close,
    /// Tear down, then run preset `.1` on the files.
    Run(Vec<PathBuf>, String /* preset id */),
}

/// One stage-2 slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub label: String,
    pub preset: String,
    /// False: drawn dim, a click does nothing.
    pub applicable: bool,
}

/// Stage 2, made from the dropped data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    pub paths: Vec<PathBuf>,
    /// By wheel slice index (0 = right, clockwise).
    pub slices: [Option<Choice>; 8],
    pub hub: String,
}

/// Builds stage 2 from the dropped bytes. The real one classifies files;
/// tests pass a pure one.
pub type OfferFn = fn(&[u8]) -> Menu;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the surface needs only the effects")
)]
pub enum Stage {
    Waiting,
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
    /// Dropped, waiting for the data.
    Reading,
    Choosing,
    Done,
}

pub struct Machine {
    phase: Phase,
    offer: OfferFn,
    output: Option<(u32, u32)>,
    /// Last known pointer position.
    cursor: Option<(f64, f64)>,
    /// Top-left of the wheel, once placed.
    origin: Option<(i32, i32)>,
    hover: Option<u8>,
    wheel: Wheel,
    hub: String,
    /// The stage-2 menu, once dropped.
    menu: Option<Menu>,
    /// Whether the drag offers `text/uri-list`.
    uris: bool,
    generation: u32,
}

impl Machine {
    pub fn new() -> Machine {
        Machine::with_offer(crate::drop_menu)
    }

    pub fn with_offer(offer: OfferFn) -> Machine {
        Machine {
            phase: Phase::Pending,
            offer,
            output: None,
            cursor: None,
            origin: None,
            hover: None,
            wheel: stage1(DROP_HERE),
            hub: DROP_HERE.into(),
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

    pub fn on(&mut self, i: Input) -> Vec<Effect> {
        let mut fx = Vec::new();
        if self.phase == Phase::Done {
            return fx;
        }
        match i {
            Input::Configured(w, h) => {
                self.output = Some((w, h));
                self.settle(&mut fx, false);
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
                    let redraw = self.phase == Phase::NoDrag;
                    if redraw {
                        self.set_wheel(stage1(DROP_HERE), DROP_HERE.into());
                    }
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
            Input::Drop => match self.phase {
                Phase::Dragging {
                    accepted: Some(true),
                } => {
                    self.phase = Phase::Reading;
                    fx.extend([Effect::ReadDrop, Effect::Arm(STAGE)]);
                }
                // Dropped off target: the compositor cancels the drag.
                Phase::Dragging { .. } => self.close(&mut fx),
                _ => {}
            },
            Input::DropData(r) => {
                if self.phase == Phase::Reading {
                    match r {
                        Ok(bytes) => {
                            let menu = (self.offer)(&bytes);
                            self.set_wheel(stage2(&menu), menu.hub.clone());
                            self.menu = Some(menu);
                            self.phase = Phase::Choosing;
                            self.settle(&mut fx, true);
                        }
                        Err(_) => self.close(&mut fx),
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
        self.generation = self.generation.wrapping_add(1);
    }

    /// What is under the pointer; `Outside` while unplaced.
    fn hit(&self) -> Hit {
        match (self.cursor, self.origin) {
            (Some((x, y)), Some((ox, oy))) => self.wheel.hit(x - f64::from(ox), y - f64::from(oy)),
            _ => Hit::Outside,
        }
    }

    /// The slice to light: an applicable one under the pointer, stage 2 only.
    fn lit(&self) -> Option<u8> {
        let (Hit::Slice(k), Some(menu)) = (self.hit(), &self.menu) else {
            return None;
        };
        let applicable = menu.slices[usize::from(k)]
            .as_ref()
            .is_some_and(|c| c.applicable);
        applicable.then_some(k)
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

    /// Over the wheel (hub or any slice) with a URI list on offer: accept.
    fn update_accept(&mut self, fx: &mut Vec<Effect>) {
        let Phase::Dragging { accepted } = self.phase else {
            return;
        };
        let want = self.uris && self.hit() != Hit::Outside;
        if accepted != Some(want) {
            self.phase = Phase::Dragging {
                accepted: Some(want),
            };
            fx.push(Effect::Accept(want));
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
            Hit::Slice(k) if button == BTN_LEFT => {
                if let Some(c) = &menu.slices[usize::from(k)]
                    && c.applicable
                {
                    let run = Effect::Run(menu.paths.clone(), c.preset.clone());
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

/// Stage 2: the menu's slices, applicable ones normal, others dim.
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
        let a = (f64::from(k) + 0.5) * std::f64::consts::FRAC_PI_4;
        (AT.0 + r * a.cos(), AT.1 + r * a.sin())
    }
    const HUB: (f64, f64) = AT;
    const OUT: (f64, f64) = (AT.0 + 300.0, AT.1);

    /// `a`: one file, slice 0 applies, slice 4 does not. Empty: nothing.
    fn fake(bytes: &[u8]) -> Menu {
        let mut slices: [Option<Choice>; 8] = Default::default();
        if bytes.is_empty() {
            return Menu {
                paths: Vec::new(),
                slices,
                hub: NOTHING.into(),
            };
        }
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
        Menu {
            paths: vec![PathBuf::from("/x")],
            slices,
            hub: "x".into(),
        }
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

    fn feed(m: &mut Machine, inputs: impl IntoIterator<Item = Input>) -> Vec<Effect> {
        inputs.into_iter().flat_map(|i| m.on(i)).collect()
    }

    fn dragging() -> Machine {
        let mut m = Machine::with_offer(fake);
        feed(&mut m, [Input::Configured(W, H), enter(HUB)]);
        m
    }

    fn reading() -> Machine {
        let mut m = dragging();
        feed(&mut m, [Input::Drop]);
        m
    }

    fn choosing() -> Machine {
        let mut m = reading();
        feed(&mut m, [Input::DropData(Ok(b"a".to_vec()))]);
        m
    }

    fn no_drag() -> Machine {
        let mut m = Machine::with_offer(fake);
        feed(
            &mut m,
            [Input::Configured(W, H), Input::PointerAt(HUB.0, HUB.1)],
        );
        m
    }

    /// One machine per non-final phase.
    fn every_stage() -> Vec<(&'static str, Machine)> {
        vec![
            ("pending", Machine::with_offer(fake)),
            ("no drag", no_drag()),
            ("dragging", dragging()),
            ("reading", reading()),
            ("choosing", choosing()),
        ]
    }

    #[test]
    fn drop_here_then_choose() {
        let mut m = Machine::with_offer(fake);
        assert_eq!((m.stage(), m.hub()), (Stage::Waiting, DROP_HERE));
        assert_eq!(m.on(Input::Configured(W, H)), []);
        assert_eq!(m.origin(), None);
        assert_eq!(m.on(enter(HUB)), [Effect::Draw, Effect::Accept(true)]);
        let half = (BOX / 2) as i32;
        assert_eq!(m.origin(), Some((960 - half, 540 - half)));
        // Stage-1 slices are targets too, and stay unlit.
        let s = slice(2, 150.0);
        assert_eq!(m.on(Input::DragMotion(s.0, s.1)), []);
        assert_eq!(m.hover(), None);
        assert_eq!(m.on(Input::Drop), [Effect::ReadDrop, Effect::Arm(STAGE)]);
        assert_eq!(m.stage(), Stage::Waiting);
        // The compositor's leave right after the drop does not close.
        assert_eq!(m.on(Input::DragLeave), []);
        assert_eq!(m.on(Input::DropData(Ok(b"a".to_vec()))), [Effect::Draw]);
        assert_eq!((m.stage(), m.hub()), (Stage::Choosing, "x"));
        let s = slice(0, 150.0);
        assert_eq!(m.on(Input::PointerAt(s.0, s.1)), [Effect::Hover(Some(0))]);
        assert_eq!(
            m.on(Input::Click(BTN_LEFT)),
            [Effect::Run(vec![PathBuf::from("/x")], "p0".into())]
        );
        assert_eq!(m.stage(), Stage::Done);
        assert_eq!(m.on(Input::Deadline), []);
        assert_eq!(m.on(Input::Click(BTN_LEFT)), []);
    }

    /// Two files; slice 3 runs `p3`, slice 1 is dim.
    fn two_files(_: &[u8]) -> Menu {
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
            paths: vec![PathBuf::from("/a"), PathBuf::from("/b c")],
            slices,
            hub: "2 / 2 files".into(),
        }
    }

    #[test]
    fn choose_runs_the_preset() {
        let mut m = Machine::with_offer(two_files);
        feed(&mut m, [Input::Configured(W, H), enter(HUB)]);
        // The drop re-arms the deadline: stage 2 gets its own.
        assert_eq!(m.on(Input::Drop), [Effect::ReadDrop, Effect::Arm(STAGE)]);
        assert_eq!(m.on(Input::DropData(Ok(b"x".to_vec()))), [Effect::Draw]);
        let s = slice(3, 180.0);
        assert_eq!(m.on(Input::PointerAt(s.0, s.1)), [Effect::Hover(Some(3))]);
        assert_eq!(
            m.on(Input::Click(BTN_LEFT)),
            [Effect::Run(
                vec![PathBuf::from("/a"), PathBuf::from("/b c")],
                "p3".into()
            )]
        );
        assert_eq!(m.stage(), Stage::Done);
        // Nothing after the run: no second choice, no close.
        assert_eq!(m.on(Input::Click(BTN_LEFT)), []);
        assert_eq!(m.on(Input::Closed), []);
    }

    #[test]
    fn dim_slice_is_not_clickable() {
        let mut m = Machine::with_offer(two_files);
        feed(
            &mut m,
            [
                Input::Configured(W, H),
                enter(HUB),
                Input::Drop,
                Input::DropData(Ok(b"x".to_vec())),
            ],
        );
        let d = slice(1, 150.0);
        assert_eq!(m.on(Input::PointerAt(d.0, d.1)), [], "not lit");
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
        assert_eq!(m.on(Input::PointerAt(s.0, s.1)), []);
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
        // Outside, every button closes; so does the left one.
        let mut m = choosing();
        m.on(Input::PointerAt(OUT.0, OUT.1));
        assert_eq!(m.on(Input::Click(BTN_LEFT)), [Effect::Close]);
    }

    #[test]
    fn drag_leave_without_drop_closes() {
        let mut m = dragging();
        assert_eq!(m.on(Input::DragLeave), [Effect::Close]);
        assert_eq!(m.stage(), Stage::Done);
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
        // A failed or stalled read, and a lost compositor, close too.
        let mut m = reading();
        assert_eq!(
            m.on(Input::DropData(Err("timed out".into()))),
            [Effect::Close]
        );
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
            let p = pts[(k / 16) as usize % pts.len()];
            match k % 16 {
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
        // every stage is reached.
        let s0 = slice(0, 150.0);
        let happy = [
            Input::Configured(W, H),
            enter(HUB),
            Input::Drop,
            Input::DropData(Ok(b"a".to_vec())),
            Input::PointerAt(s0.0, s0.1),
        ];
        for _ in 0..20_000 {
            let mut m = Machine::with_offer(fake);
            let prefix = (next() % (happy.len() as u64 + 1)) as usize;
            let mut effects: Vec<Effect> = feed(&mut m, happy[..prefix].iter().cloned());
            let len = next() % 8;
            for _ in 0..len {
                effects.extend(m.on(pool(next())));
            }
            effects.extend(m.on(Input::Deadline));
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
    fn accept_only_on_the_wheel() {
        let mut m = Machine::with_offer(fake);
        m.on(Input::Configured(W, H));
        assert_eq!(m.on(enter(HUB)), [Effect::Draw, Effect::Accept(true)]);
        // Hub and slices are all targets: no change while on the wheel.
        let s = slice(1, 150.0);
        assert_eq!(m.on(Input::DragMotion(s.0, s.1)), []);
        let s = slice(5, 210.0);
        assert_eq!(m.on(Input::DragMotion(s.0, s.1)), []);
        assert_eq!(
            m.on(Input::DragMotion(OUT.0, OUT.1)),
            [Effect::Accept(false)]
        );
        assert_eq!(m.on(Input::DragMotion(OUT.0, OUT.1 + 5.0)), []);
        let nan = Input::DragMotion(f64::NAN, 0.0);
        assert_eq!(m.on(nan), []);
        assert_eq!(
            m.on(Input::DragMotion(HUB.0, HUB.1)),
            [Effect::Accept(true)]
        );
        // Entered in a screen corner: the wheel is pushed onto the output,
        // so the pointer is outside it, and the drag rejected from the start.
        let mut m = Machine::with_offer(fake);
        m.on(Input::Configured(W, H));
        assert_eq!(
            m.on(enter((0.0, 0.0))),
            [Effect::Draw, Effect::Accept(false)]
        );
        assert_eq!(m.origin(), Some((0, 0)));
        let c = f64::from(BOX / 2);
        assert_eq!(m.on(Input::DragMotion(c, c)), [Effect::Accept(true)]);
        // No URI list on offer: never accepted, a drop just closes.
        let mut m = Machine::with_offer(fake);
        m.on(Input::Configured(W, H));
        let e = m.on(Input::DragEnter {
            x: HUB.0,
            y: HUB.1,
            mimes: vec!["text/plain".into()],
        });
        assert_eq!(e, [Effect::Draw, Effect::Accept(false)]);
        assert_eq!(m.on(Input::DragMotion(HUB.0 + 1.0, HUB.1)), []);
        assert_eq!(m.on(Input::Drop), [Effect::Close]);
        // A drag arriving while choosing is refused.
        let mut m = choosing();
        assert_eq!(m.on(enter(HUB)), [Effect::Accept(false)]);
        assert_eq!(m.on(Input::DragLeave), []);
        assert_eq!(m.stage(), Stage::Choosing);
    }

    #[test]
    fn hover_only_repaints_on_change() {
        let mut m = choosing();
        let (a, b) = (slice(0, 120.0), slice(0, 200.0));
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
        // Stage 1 never lights a slice.
        let mut m = dragging();
        for k in 0..8 {
            let s = slice(k, 150.0);
            assert_eq!(m.on(Input::DragMotion(s.0, s.1)), []);
        }
        // The position at the swap to stage 2 is the hover of the new wheel.
        let mut m = dragging();
        let s = slice(0, 150.0);
        m.on(Input::DragMotion(s.0, s.1));
        m.on(Input::Drop);
        assert_eq!(m.on(Input::DropData(Ok(b"a".to_vec()))), [Effect::Draw]);
        assert_eq!(m.hover(), Some(0));
    }

    #[test]
    fn no_drag_says_nothing_to_do() {
        let mut m = Machine::with_offer(fake);
        m.on(Input::Configured(W, H));
        assert_eq!(m.on(Input::PointerAt(HUB.0, HUB.1)), [Effect::Draw]);
        assert_eq!((m.stage(), m.hub()), (Stage::Waiting, NOTHING));
        let s = slice(0, 150.0);
        assert_eq!(m.on(Input::PointerAt(s.0, s.1)), []);
        // Any click closes, on the wheel too.
        assert_eq!(m.on(Input::Click(BTN_LEFT)), [Effect::Close]);
        // A drop whose list yields no file: nothing to do either.
        let mut m = reading();
        assert_eq!(m.on(Input::DropData(Ok(Vec::new()))), [Effect::Draw]);
        assert_eq!(m.hub(), NOTHING);
        assert_eq!(m.on(Input::PointerAt(HUB.0, HUB.1)), []);
        assert_eq!(m.on(Input::Click(BTN_LEFT)), [Effect::Close]);
        // A drag that comes after all goes back to stage 1.
        let mut m = no_drag();
        assert_eq!(m.on(enter(HUB)), [Effect::Draw, Effect::Accept(true)]);
        assert_eq!(m.hub(), DROP_HERE);
    }

    /// The position may arrive before the size: the wheel is placed once
    /// both are known, on the output.
    #[test]
    fn placed_once_on_the_output() {
        let mut m = Machine::with_offer(fake);
        assert_eq!(m.on(Input::PointerAt(5.0, 5.0)), []);
        assert_eq!(m.on(Input::Configured(W, H)), [Effect::Draw]);
        assert_eq!(m.origin(), Some((0, 0)));
        m.on(Input::PointerAt(900.0, 500.0));
        assert_eq!(m.origin(), Some((0, 0)), "placed once");
        // A click before any position is not a choice.
        let mut m = Machine::with_offer(fake);
        assert_eq!(m.on(Input::Click(BTN_LEFT)), [Effect::Close]);
    }
}
