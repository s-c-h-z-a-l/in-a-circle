//! Styles and regions (TTML2 10.2, 10.3, 10.4, 11.3.1). A `style` element and a
//! `region` element are both entries of a [`Sheet`]: the declarations they
//! carry, and the entries they reference, in document order. Referenced entries
//! merge first and the entry's own declarations last (10.4.4.2: referential
//! styling, then nested styling, then inline styling). The sheet is resolved once,
//! when the body begins ([`Sheet::freeze`]); after that an element's `style`
//! attribute is a lookup and a merge.
//!
//! A declaration the reader does not carry sets a flag ([`Css::styling`] or
//! [`Css::position`]); the paragraphs it reaches count it once per cue (see the
//! accounting table in `mod.rs`).

use std::collections::BTreeMap;

use xml::Element;

use crate::{Budget, ReadError};

/// The styling namespace (TTML2 Table 5-1) and the DFXP one of TTML 1.
const TTS_NS: &str = "http://www.w3.org/ns/ttml#styling";
const DFXP_TTS_NS: &str = "http://www.w3.org/2006/10/ttaf1#styling";

/// Longest chain of entries one reference path may hold: an entry at height
/// 32 is read, a reference that would make a chain of 33 is refused and
/// counted as `Styling`. Real files chain two or three styles. The cap makes a
/// deep chain an explicit refusal rather than a silent truncation, and bounds
/// the height the resolution keeps for each entry.
pub(super) const MAX_CHAIN: u8 = 32;

/// Digits kept in one length: the numerator and the denominator must fit in a
/// `u32` with room to multiply, and nine digits (a billionth of the root
/// container) is far finer than any layout. A longer number is `Unknown`.
const MAX_DIGITS: usize = 9;

/// An anchor at or above this percentage of the root container's height is not
/// top: an anchor at or below it is top. 33 %, a rounded third (the exact third
/// is 33.3 %), so an anchor at exactly 33 % is top and 34 % is not.
const TOP_LIMIT_PERCENT: u128 = 33;

/// The unit of a length (10.3.22). `Unknown` covers `em`, `c`, `rw`, `rh`, a
/// negative value and anything malformed: a placement that cannot be
/// computed is not top.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Unit {
    Percent,
    Px,
    Unknown,
}

/// A length as `num / den` in its unit, `den >= 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Len {
    pub(super) unit: Unit,
    pub(super) num: u32,
    pub(super) den: u32,
}

/// A point or an extent: its two lengths (10.3.29, 10.3.14).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Pos {
    pub(super) x: Len,
    pub(super) y: Len,
}

/// The `displayAlign` value (10.2.15): where the block sits in its region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Align {
    Before,
    Center,
    After,
}

/// The declarations an element specifies, and what the reader keeps of them.
/// Inheritable values are `None` when not specified. A carried value overrides
/// the one before it in the merge order; `styling` and `position` are flags:
/// a declaration that is not carried, once set, stays set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Css {
    pub(super) italic: Option<bool>,
    pub(super) bold: Option<bool>,
    pub(super) underline: Option<bool>,
    pub(super) line_through: Option<bool>,
    pub(super) overline: Option<bool>,
    pub(super) origin: Option<Pos>,
    pub(super) extent: Option<Pos>,
    pub(super) align: Option<Align>,
    /// `tts:display="none"` (10.2.14). Not inherited: the element and its
    /// content are not shown, which [`Frame::concealed`](super) carries down.
    pub(super) display_none: Option<bool>,
    /// `tts:visibility` (10.2.49), inherited: `Some(true)` is `hidden`.
    pub(super) hidden: Option<bool>,
    /// `tts:opacity` (10.2.30), not inherited. Only the two values that change
    /// nothing or hide everything are carried; any other value is `Styling`.
    pub(super) opacity: Option<Opacity>,
    pub(super) styling: bool,
    pub(super) position: bool,
}

/// The opacities the reader acts on: 0 makes the content transparent (it is
/// not shown), 1 is the initial value (10.2.30) and changes nothing. A value
/// between them cannot be drawn in a text file and is `Styling`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Opacity {
    Zero,
    One,
}

/// `<alpha>` (10.3.1), a decimal number from 0 to 1. Read exactly, with no
/// floating point: a whole part of zeros or of one, and a fraction of zeros
/// (`0`, `0.00`, `1`, `1.0`). Any other spelling is `None`, and the caller
/// counts it.
fn opacity(value: &str) -> Option<Opacity> {
    let (whole, frac) = value.split_once('.').unwrap_or((value, ""));
    if !frac.bytes().all(|b| b == b'0') {
        return None;
    }
    if !whole.is_empty() && whole.bytes().all(|b| b == b'0') {
        Some(Opacity::Zero)
    } else if whole == "1" {
        Some(Opacity::One)
    } else {
        None
    }
}

/// Whether an element's declarations hide or show content by `display`,
/// `visibility` or `opacity`. Those apply to `body`, `div`, `p`, `span` and
/// `region` only (10.2.14, 10.2.30, 10.2.49); a `br`, or the root `tt`, carrying
/// one is a declaration the reader does not apply, so it is `Styling`.
pub(super) fn hiding_declared(decl: &Css) -> bool {
    decl.display_none.is_some() || decl.hidden.is_some() || decl.opacity.is_some()
}

/// What a text piece or a `br` is affected by: a `Styling` or a `Position`
/// declaration on it or on an element it is in, or an ignored element
/// (`Elements`) it contains. Each counts once per cue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Flags {
    pub(super) styling: bool,
    pub(super) position: bool,
    pub(super) elements: bool,
}

impl Flags {
    pub(super) fn join(self, other: Flags) -> Flags {
        Flags {
            styling: self.styling || other.styling,
            position: self.position || other.position,
            elements: self.elements || other.elements,
        }
    }
}

/// `origin="auto"` is the root container's origin, `extent="auto"` the whole
/// container (10.2.31, 10.2.16): both are the percentages below.
const AUTO_ORIGIN: Pos = Pos {
    x: percent(0),
    y: percent(0),
};
const AUTO_EXTENT: Pos = Pos {
    x: percent(100),
    y: percent(100),
};
/// A value that cannot be placed: its unit is unknown, so it is never top.
const UNKNOWN_POS: Pos = Pos {
    x: Len {
        unit: Unit::Unknown,
        num: 0,
        den: 1,
    },
    y: Len {
        unit: Unit::Unknown,
        num: 0,
        den: 1,
    },
};

const fn percent(n: u32) -> Len {
    Len {
        unit: Unit::Percent,
        num: n,
        den: 1,
    }
}

/// Every `tts:` attribute as (local name, value).
fn tts_attrs<'e>(el: &'e Element<'_>) -> impl Iterator<Item = (&'e str, &'e str)> {
    el.attrs().filter_map(|(name, value)| {
        matches!(name.ns, Some(TTS_NS | DFXP_TTS_NS)).then_some((name.local, value.as_ref()))
    })
}

/// The value of one `tts:` attribute, by local name.
pub(super) fn tts_value<'e>(el: &'e Element<'_>, local: &str) -> Option<&'e str> {
    tts_attrs(el)
        .find(|(name, _)| *name == local)
        .map(|(_, value)| value)
}

/// Whether an animation element (`set`, `animate`) targets `display`,
/// `visibility` or `opacity`: its `tts:` attribute names the target (13.1.3, 13.1.1).
pub(super) fn animates_visibility(el: &Element) -> bool {
    tts_attrs(el).any(|(local, _)| matches!(local, "display" | "visibility" | "opacity"))
}

/// The flags an element's own declarations give its content: a declaration the
/// reader does not carry (`Styling`), or a placement it does not use (`Position`).
pub(super) fn own_flags(decl: &Css) -> Flags {
    Flags {
        styling: decl.styling,
        position: decl.position
            || decl.origin.is_some()
            || decl.extent.is_some()
            || decl.align.is_some(),
        elements: false,
    }
}

/// The flags of a duplicate definition that is ignored whole: every declaration
/// it makes is dropped, carried ones included, so any declaration counts. A
/// carried inheritable value counts as `Styling`; the rest is [`own_flags`].
pub(super) fn dropped_flags(decl: &Css) -> Flags {
    let carried = decl.italic.is_some()
        || decl.bold.is_some()
        || decl.underline.is_some()
        || decl.line_through.is_some()
        || decl.overline.is_some()
        || hiding_declared(decl);
    let flags = own_flags(decl);
    Flags {
        styling: flags.styling || carried,
        ..flags
    }
}

/// The `tts:` declarations of one element, as the reader keeps them. A value
/// the reader does not use sets the flag of its kind: the declarations this
/// reader does not carry count as `Styling`, the placement ones as `Position`
/// (the groups of the accounting table in `mod.rs`).
pub(super) fn attrs_css(el: &Element) -> Css {
    let mut css = Css::default();
    for (local, value) in tts_attrs(el) {
        let value = value.trim();
        match local {
            "fontStyle" => match value {
                "normal" => css.italic = Some(false),
                "italic" | "oblique" => css.italic = Some(true),
                _ => css.styling = true,
            },
            "fontWeight" => match value {
                // Only these two values exist (10.2.24); a number is not a weight of the spec.
                "normal" => css.bold = Some(false),
                "bold" => css.bold = Some(true),
                _ => css.styling = true,
            },
            "textDecoration" => {
                if !decoration(value, &mut css) {
                    css.styling = true;
                }
            }
            // 10.2.14: `inlineBlock` is a layout candidate the reader does not
            // draw, so it is `Styling` like any value outside the grammar.
            "display" => match value {
                "auto" => css.display_none = Some(false),
                "none" => css.display_none = Some(true),
                _ => css.styling = true,
            },
            "visibility" => match value {
                "visible" => css.hidden = Some(false),
                "hidden" => css.hidden = Some(true),
                _ => css.styling = true,
            },
            "opacity" => match opacity(value) {
                Some(o) => css.opacity = Some(o),
                None => css.styling = true,
            },
            "origin" => css.origin = Some(origin(value)),
            "extent" => css.extent = Some(extent(value)),
            "displayAlign" => match value {
                "before" => css.align = Some(Align::Before),
                "center" => css.align = Some(Align::Center),
                "after" => css.align = Some(Align::After),
                _ => css.position = true,
            },
            "padding" | "position" | "textAlign" | "bpd" | "ipd" | "overflow" | "zIndex"
            | "disparity" => css.position = true,
            _ => css.styling = true,
        }
    }
    css
}

/// `textDecoration` (10.3.37): `none`, or each group once in any order, with
/// `underline`/`noUnderline`, `lineThrough`/`noLineThrough`,
/// `overline`/`noOverline`. Each component is set on its own (10.2.43.1), so
/// the three are inheritable apart. Returns false for a value outside the grammar.
fn decoration(value: &str, css: &mut Css) -> bool {
    if value == "none" {
        css.underline = Some(false);
        css.line_through = Some(false);
        css.overline = Some(false);
        return true;
    }
    let mut seen = [false; 3];
    let mut valid = !value.is_empty();
    for token in value.split_ascii_whitespace() {
        let (group, on, off) = match token {
            "underline" | "noUnderline" => (0, &mut css.underline, token == "underline"),
            "lineThrough" | "noLineThrough" => (1, &mut css.line_through, token == "lineThrough"),
            "overline" | "noOverline" => (2, &mut css.overline, token == "overline"),
            _ => {
                valid = false;
                continue;
            }
        };
        if std::mem::replace(&mut seen[group], true) {
            valid = false;
        }
        *on = Some(off);
    }
    valid
}

/// `origin` (10.3.29): `auto` or two lengths. Anything else is unknown.
fn origin(value: &str) -> Pos {
    if value == "auto" {
        return AUTO_ORIGIN;
    }
    pair(value).unwrap_or(UNKNOWN_POS)
}

/// `extent` (10.3.14): `auto`, or two lengths. `contain` and `cover` are not
/// placed by this reader, so they are unknown.
fn extent(value: &str) -> Pos {
    if value == "auto" {
        return AUTO_EXTENT;
    }
    pair(value).unwrap_or(UNKNOWN_POS)
}

/// Two lengths separated by white space, `None` when there are not two.
fn pair(value: &str) -> Option<Pos> {
    let mut tokens = value.split_ascii_whitespace();
    let x = length(tokens.next()?);
    let y = length(tokens.next()?);
    tokens.next().is_none().then_some(Pos { x, y })
}

/// One `<length>` (10.3.22): a number with `%` or `px`. Other units, a sign and
/// a number with too many digits are `Unknown`.
fn length(value: &str) -> Len {
    let (number, unit) = if let Some(n) = value.strip_suffix('%') {
        (n, Unit::Percent)
    } else if let Some(n) = value.strip_suffix("px") {
        (n, Unit::Px)
    } else {
        return Len {
            unit: Unit::Unknown,
            num: 0,
            den: 1,
        };
    };
    match number_parts(number) {
        Some((num, den)) => Len { unit, num, den },
        None => Len {
            unit: Unit::Unknown,
            num: 0,
            den: 1,
        },
    }
}

/// A decimal number of digits only (`12`, `16.7`, `.5`) as numerator and
/// denominator. A sign, an exponent or a second point is refused.
fn number_parts(s: &str) -> Option<(u32, u32)> {
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    if !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    if int.len() + frac.len() > MAX_DIGITS {
        return None;
    }
    let num = int
        .bytes()
        .chain(frac.bytes())
        .fold(0u64, |n, b| n * 10 + u64::from(b - b'0'));
    let den = 10u64.pow(u32::try_from(frac.len()).ok()?);
    Some((u32::try_from(num).ok()?, u32::try_from(den).ok()?))
}

/// The root container's height in px, from `tts:extent` on `tt` (10.2.16):
/// two px lengths, the height being the second. `None` for anything else.
pub(super) fn root_extent(value: &str) -> Option<Len> {
    let pos = pair(value)?;
    let (x, y) = (pos.x, pos.y);
    (x.unit == Unit::Px && y.unit == Unit::Px && y.num > 0).then_some(y)
}

/// Whether an element's `tts:extent` on `tt` is one the reader can use: `auto`,
/// `contain`, or [`root_extent`]'s two px lengths. Anything else is not read.
pub(super) fn root_extent_is_usable(value: &str) -> bool {
    matches!(value.trim(), "auto" | "contain") || root_extent(value.trim()).is_some()
}

/// One `Frac` is `num / den` of the root container's height.
type Frac = (u128, u128);

/// A length as a fraction of the root height. Percent is `num / (den * 100)`.
/// Px needs the root height in px; without it, or with a zero height, a px
/// value has no fraction.
fn fraction(len: Len, root: Option<Len>) -> Option<Frac> {
    let (n, d) = (u128::from(len.num), u128::from(len.den));
    match len.unit {
        Unit::Percent => Some((n, d.checked_mul(100)?)),
        Unit::Px => {
            let r = root?;
            if r.num == 0 {
                return None;
            }
            // (n/d) px over (rn/rd) px.
            Some((
                n.checked_mul(u128::from(r.den))?,
                d.checked_mul(u128::from(r.num))?,
            ))
        }
        Unit::Unknown => None,
    }
}

/// `a + b` as fractions, with every product checked.
fn add((a, b): Frac, (c, d): Frac) -> Option<Frac> {
    Some((
        a.checked_mul(d)?.checked_add(c.checked_mul(b)?)?,
        b.checked_mul(d)?,
    ))
}

/// Whether the region's anchor, the line the block is aligned to, lies in the
/// top third of the root container. The anchor is `origin.y`
/// before, `origin.y + extent.y / 2` centred, `origin.y + extent.y` after
/// (10.2.15, 10.2.31 and 10.2.16). `None` when a length has no fraction.
pub(super) fn top_third(css: &Css, root: Option<Len>) -> Option<bool> {
    let origin = css.origin.unwrap_or(AUTO_ORIGIN);
    let extent = css.extent.unwrap_or(AUTO_EXTENT);
    let top = fraction(origin.y, root)?;
    let height = fraction(extent.y, root)?;
    let (k_num, k_den) = match css.align.unwrap_or(Align::Before) {
        Align::Before => (0, 1),
        Align::Center => (1, 2),
        Align::After => (1, 1),
    };
    let offset = (height.0.checked_mul(k_num)?, height.1.checked_mul(k_den)?);
    let (n, d) = add(top, offset)?;
    // n / d <= 33 / 100, with the products checked.
    Some(n.checked_mul(100)? <= TOP_LIMIT_PERCENT.checked_mul(d)?)
}

impl Css {
    /// The inheritable values this element specifies, else those of `below`:
    /// the content ancestor (10.4.2.1), then the region (10.4.2.2), which a
    /// paragraph passes as `below`. The placement values are not inherited.
    pub(super) fn over(self, below: &Css) -> Css {
        Css {
            italic: self.italic.or(below.italic),
            bold: self.bold.or(below.bold),
            underline: self.underline.or(below.underline),
            line_through: self.line_through.or(below.line_through),
            overline: self.overline.or(below.overline),
            hidden: self.hidden.or(below.hidden),
            ..Css::default()
        }
    }

    /// Merges `from` over `self` (10.4.4.2 merging): each value `from` gives
    /// replaces this one; the flags stay once set.
    pub(super) fn merge(&mut self, from: &Css) {
        self.italic = from.italic.or(self.italic);
        self.bold = from.bold.or(self.bold);
        self.underline = from.underline.or(self.underline);
        self.line_through = from.line_through.or(self.line_through);
        self.overline = from.overline.or(self.overline);
        self.origin = from.origin.or(self.origin);
        self.extent = from.extent.or(self.extent);
        self.align = from.align.or(self.align);
        self.display_none = from.display_none.or(self.display_none);
        self.hidden = from.hidden.or(self.hidden);
        self.opacity = from.opacity.or(self.opacity);
        self.styling |= from.styling;
        self.position |= from.position;
    }
}

/// One style or region, resolved when the body begins.
struct Entry {
    /// Own declarations until the sheet is frozen, the resolved set after.
    css: Css,
    /// Referenced entries, in document order. Emptied by the freeze.
    refs: Vec<usize>,
    /// False for a name that is referenced but never defined.
    defined: bool,
    /// The longest chain of accepted references from this entry, counting
    /// itself (a leaf is 1). Set by the freeze.
    height: u8,
    /// The flags of the ignored duplicate definitions of this entry's name
    /// (see [`Sheet::define_style`]): every reference to the name carries them.
    dup: Flags,
    /// A `set` or `animate` in this region changes its visibility over time:
    /// every paragraph placed in it is refused (see `mod.rs`).
    animated: bool,
}

/// The styles and regions of the head. Names are the `xml:id`s: styles and
/// regions have separate namespaces. A reference to a name not defined yet is
/// a placeholder entry, so a style may refer to one that comes later in the
/// head.
#[derive(Default)]
pub(super) struct Sheet {
    entries: Vec<Entry>,
    styles: BTreeMap<String, usize>,
    regions: BTreeMap<String, usize>,
}

/// Where an entry's refs are resolved from (see [`Sheet::freeze`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Fresh,
    Active,
    Done,
}

impl Sheet {
    /// Adds an entry. Charged before it is stored: one item.
    fn push(&mut self, budget: &mut Budget, css: Css, defined: bool) -> Result<usize, ReadError> {
        budget.charge()?;
        self.entries.push(Entry {
            css,
            refs: Vec::new(),
            defined,
            height: 0,
            dup: Flags::default(),
            animated: false,
        });
        Ok(self.entries.len() - 1)
    }

    /// A `style` element. Named ones are in the style namespace; an unnamed one
    /// cannot be referenced and is kept only as the entry of its own content.
    pub(super) fn define_style(
        &mut self,
        budget: &mut Budget,
        id: Option<&str>,
        own: Css,
    ) -> Result<usize, ReadError> {
        let Some(id) = id else {
            return self.push(budget, own, true);
        };
        match self.styles.get(id).copied() {
            // A duplicate `xml:id` is an xml:id error, which is not fatal (XML ID
            // section 6). The first definition stands; this one is kept as an
            // entry that nothing names, and every reference to the name is
            // flagged with its declarations: the references are where it matters.
            Some(i) if self.entries[i].defined => {
                self.entries[i].dup = self.entries[i].dup.join(dropped_flags(&own));
                self.push(budget, own, true)
            }
            Some(i) => {
                self.entries[i] = Entry {
                    css: own,
                    refs: Vec::new(),
                    defined: true,
                    height: 0,
                    dup: Flags::default(),
                    animated: false,
                };
                Ok(i)
            }
            None => {
                let i = self.push(budget, own, true)?;
                self.styles.insert(id.to_owned(), i);
                Ok(i)
            }
        }
    }

    /// A `region` element, named by its `xml:id` when it has one.
    pub(super) fn define_region(
        &mut self,
        budget: &mut Budget,
        id: Option<&str>,
        own: Css,
    ) -> Result<usize, ReadError> {
        let Some(id) = id else {
            return self.push(budget, own, true);
        };
        // A duplicate region is handled as a duplicate style (see `define_style`).
        if let Some(&i) = self.regions.get(id) {
            self.entries[i].dup = self.entries[i].dup.join(dropped_flags(&own));
            return self.push(budget, own, true);
        }
        let i = self.push(budget, own, true)?;
        self.regions.insert(id.to_owned(), i);
        Ok(i)
    }

    /// The entry a `style` attribute token names. A name not defined yet gets
    /// a placeholder, which the definition later fills.
    pub(super) fn style_ref(&mut self, budget: &mut Budget, id: &str) -> Result<usize, ReadError> {
        if let Some(&i) = self.styles.get(id) {
            return Ok(i);
        }
        let i = self.push(budget, Css::default(), false)?;
        self.styles.insert(id.to_owned(), i);
        Ok(i)
    }

    /// Records that `from` references `to`, charged as one item.
    pub(super) fn push_ref(
        &mut self,
        budget: &mut Budget,
        from: usize,
        to: usize,
    ) -> Result<(), ReadError> {
        budget.charge()?;
        self.entries[from].refs.push(to);
        Ok(())
    }

    /// Marks a region entry as one whose visibility changes over time.
    pub(super) fn mark_animated(&mut self, entry: usize) {
        self.entries[entry].animated = true;
    }

    /// Whether a region entry's visibility changes over time (see [`Entry`]).
    pub(super) fn animated(&self, entry: usize) -> bool {
        self.entries[entry].animated
    }

    /// The region an `xml:id` names, if one is defined.
    pub(super) fn region_named(&self, id: &str) -> Option<usize> {
        self.regions.get(id).copied()
    }

    /// The resolved set of a region or style entry.
    pub(super) fn css(&self, entry: usize) -> &Css {
        &self.entries[entry].css
    }

    /// The style a `style` attribute names, `None` when it is not defined.
    /// Only after the freeze.
    fn style_entry(&self, id: &str) -> Option<&Entry> {
        self.styles
            .get(id)
            .map(|&i| &self.entries[i])
            .filter(|e| e.defined)
    }

    /// The flags a region or style entry gives its content: its declarations
    /// and those of the duplicate definitions of its name.
    pub(super) fn flags(&self, entry: usize) -> Flags {
        let e = &self.entries[entry];
        Flags {
            styling: e.css.styling || e.dup.styling,
            position: e.css.position || e.dup.position,
            elements: false,
        }
    }

    /// The declarations of an element with a `style` attribute, its
    /// referenced styles merged in order and its own inline ones last
    /// (10.4.1.1 to 10.4.1.3). A name that is not defined, a reference that
    /// closes a loop, or one past [`MAX_CHAIN`] is a `Styling` flag on the
    /// element. Only after the freeze.
    pub(super) fn declared(&self, el: &Element) -> Css {
        let mut out = Css::default();
        if let Some(refs) = el.attr(None, "style") {
            for id in refs.split_ascii_whitespace() {
                match self.style_entry(id) {
                    Some(e) => {
                        out.merge(&e.css);
                        out.styling |= e.dup.styling;
                        out.position |= e.dup.position;
                    }
                    None => out.styling = true,
                }
            }
        }
        out.merge(&attrs_css(el));
        out
    }

    /// Resolves every entry once, in dependency order, without recursion: a
    /// depth-first walk that visits each entry after the ones it references.
    /// A reference is accepted only when the target is defined, finished and
    /// its height is below [`MAX_CHAIN`]. A target still on the walk's path is
    /// a loop: that reference is refused and sets `styling` on the referencing
    /// entry. The walk starts from the entries in definition order, so a loop
    /// is refused at the same reference for the same file.
    pub(super) fn freeze(&mut self) {
        let mut phase = vec![Phase::Fresh; self.entries.len()];
        for root in 0..self.entries.len() {
            if phase[root] != Phase::Fresh {
                continue;
            }
            phase[root] = Phase::Active;
            let mut path = vec![(root, 0usize)];
            while let Some(&(v, next)) = path.last() {
                if let Some(&r) = self.entries[v].refs.get(next) {
                    if let Some(top) = path.last_mut() {
                        top.1 += 1;
                    }
                    if phase[r] == Phase::Fresh {
                        phase[r] = Phase::Active;
                        path.push((r, 0));
                    }
                } else {
                    path.pop();
                    self.resolve(v, &phase);
                    phase[v] = Phase::Done;
                }
            }
        }
    }

    /// Resolves one entry whose references are all finished or on the path.
    fn resolve(&mut self, v: usize, phase: &[Phase]) {
        let own = self.entries[v].css;
        let refs = std::mem::take(&mut self.entries[v].refs);
        let mut out = Css::default();
        let mut height = 0u8;
        for &r in &refs {
            let target = &self.entries[r];
            if target.defined && phase[r] == Phase::Done && target.height < MAX_CHAIN {
                out.merge(&target.css);
                out.styling |= target.dup.styling;
                out.position |= target.dup.position;
                height = height.max(target.height);
            } else {
                out.styling = true;
            }
        }
        out.merge(&own);
        self.entries[v].css = out;
        self.entries[v].height = height + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(n: u32) -> Len {
        Len {
            unit: Unit::Px,
            num: n,
            den: 1,
        }
    }

    /// Lengths: percent and decimal percent read exactly; `em`, a sign and
    /// too many digits are unknown; a bare number has no unit.
    #[test]
    fn lengths_read_their_unit_and_refuse_the_rest() {
        assert_eq!(
            length("16.7%"),
            Len {
                unit: Unit::Percent,
                num: 167,
                den: 10
            }
        );
        assert_eq!(length("10px"), px(10));
        assert_eq!(length("1em").unit, Unit::Unknown);
        assert_eq!(length("-10%").unit, Unit::Unknown);
        assert_eq!(length("12").unit, Unit::Unknown);
        assert_eq!(length("1234567890%").unit, Unit::Unknown);
    }

    /// The anchor of 10.2.15: before is the origin, centred the middle, after
    /// the far edge. A third of the root is the limit, 33 %.
    #[test]
    fn anchors_pick_the_edge_the_alignment_names() {
        let at = |y: u32, h: u32, align: Align| Css {
            origin: Some(Pos {
                x: percent(0),
                y: percent(y),
            }),
            extent: Some(Pos {
                x: percent(100),
                y: percent(h),
            }),
            align: Some(align),
            ..Css::default()
        };
        assert_eq!(top_third(&at(10, 20, Align::Before), None), Some(true));
        assert_eq!(top_third(&at(10, 40, Align::Center), None), Some(true));
        assert_eq!(top_third(&at(10, 40, Align::After), None), Some(false));
        assert_eq!(top_third(&at(33, 0, Align::Before), None), Some(true));
        assert_eq!(top_third(&at(34, 0, Align::Before), None), Some(false));
    }

    /// Chains are bounded: a chain of [`MAX_CHAIN`] entries is read whole; the
    /// reference that would make it one longer is refused and flags the entry
    /// that holds it. Entry `k` references entry `k - 1`, so the top entry of a
    /// chain of `MAX_CHAIN + 1` has only a refused reference.
    #[test]
    fn chain_height_is_capped_at_the_limit() {
        let mut budget = Budget::new(1_000);
        let mut sheet = Sheet::default();
        let mut chain = Vec::new();
        for k in 0..=usize::from(MAX_CHAIN) {
            let i = sheet
                .push(&mut budget, Css::default(), true)
                .expect("charged");
            if k > 0 {
                sheet
                    .push_ref(&mut budget, i, chain[k - 1])
                    .expect("charged");
            }
            chain.push(i);
        }
        sheet.freeze();
        let longest = chain[usize::from(MAX_CHAIN) - 1];
        assert_eq!(sheet.entries[longest].height, MAX_CHAIN);
        assert!(
            !sheet.entries[longest].css.styling,
            "a chain of the cap is read"
        );
        let top = chain[usize::from(MAX_CHAIN)];
        assert!(
            sheet.entries[top].css.styling,
            "the reference past the cap is refused"
        );
        assert_eq!(sheet.entries[top].height, 1);
    }
}
