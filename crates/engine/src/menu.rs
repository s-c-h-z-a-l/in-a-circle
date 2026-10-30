//! Built-in presets, the route planner and the wheel offer.

use crate::catalog::{Action, Catalog, missing_tools};
use crate::classify::Kind;

/// A fixed wheel slot. `direction`: 0 = north, clockwise in 45 degree steps.
pub struct Preset {
    pub id: &'static str,
    pub input: &'static str,
    pub output: &'static str,
    pub label: &'static str,
    /// Direction within its group's wheel.
    pub direction: u8,
    /// Id of a [`GROUPS`] entry.
    pub group: &'static str,
    /// MIME types a join action must accept for this preset to offer it
    /// (many inputs, one output); empty: the preset has no join form.
    pub join: &'static [&'static str],
}

/// A category on the first-level wheel. `direction`: as for [`Preset`].
pub struct Group {
    pub id: &'static str,
    pub label: &'static str,
    pub direction: u8,
}

pub const GROUPS: &[Group] = &[
    Group {
        id: "document",
        label: "Document",
        direction: 0,
    },
    Group {
        id: "book",
        label: "Book",
        direction: 2,
    },
];

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "markdown",
        input: "text/plain",
        output: "text/markdown",
        label: "Markdown",
        direction: 0,
        group: "document",
        join: &[],
    },
    Preset {
        id: "plain-text",
        input: "text/markdown",
        output: "text/plain",
        label: "Plain text",
        direction: 4,
        group: "document",
        join: &[],
    },
    Preset {
        id: "pdf",
        input: "application/vnd.comicbook+zip",
        output: "application/pdf",
        label: "PDF",
        direction: 0,
        group: "book",
        join: &["image/jpeg", "image/png"],
    },
];

/// Cheapest route from `input` to `output`: one direct action, else two
/// steps whose first is lossless. Actions with missing tools never take part,
/// and a join action never does either: it takes many inputs, so it cannot
/// be a step of a chain (see [`choose`]).
pub fn route<'a>(cat: &'a Catalog, input: &str, output: &str) -> Option<Vec<&'a Action>> {
    plan(cat, input, output, |a| missing_tools(a).is_empty())
}

fn plan<'a>(
    cat: &'a Catalog,
    input: &str,
    output: &str,
    usable: impl Fn(&Action) -> bool,
) -> Option<Vec<&'a Action>> {
    let direct = cat
        .actions
        .iter()
        .filter(|a| a.join.is_none() && a.input == input && a.output == output && usable(a))
        .min_by(|a, b| (a.cost, &a.id).cmp(&(b.cost, &b.id)));
    if let Some(a) = direct {
        return Some(vec![a]);
    }
    let mut best: Option<(u64, &Action, &Action)> = None;
    for a in cat.actions.iter().filter(|a| {
        a.join.is_none() && a.input == input && !a.lossy && a.output != output && usable(a)
    }) {
        for b in cat
            .actions
            .iter()
            .filter(|b| b.join.is_none() && b.input == a.output && b.output == output && usable(b))
        {
            let cost = u64::from(a.cost) + u64::from(b.cost);
            let better = best.is_none_or(|(c, x, y)| (cost, &a.id, &b.id) < (c, &x.id, &y.id));
            if better {
                best = Some((cost, a, b));
            }
        }
    }
    best.map(|(_, a, b)| vec![a, b])
}

pub struct Slice {
    pub preset: &'static str,
    pub label: String,
    pub applicable: usize,
    pub disabled: Option<String>,
}

pub struct Offer {
    pub slices: [Option<Slice>; 8],
    pub hub: String,
    pub total: usize,
}

/// What the offer needs to know about a selection, without keeping every
/// file: how many there are, how many of each type with the first one's
/// name, and the first file's type label.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tally {
    pub total: usize,
    /// MIME type, count, the first such file's name; in order of first
    /// appearance.
    pub types: Vec<(String, usize, String)>,
    pub first_label: Option<String>,
    /// The first file's name.
    pub first_name: Option<String>,
}

impl Tally {
    pub fn add(&mut self, name: &str, kind: &Kind) {
        self.total += 1;
        if self.first_name.is_none() {
            self.first_name = Some(name.to_owned());
        }
        if self.first_label.is_none() {
            self.first_label = Some(kind.label.clone());
        }
        let Some(mime) = &kind.mime else {
            return;
        };
        match self.types.iter_mut().find(|t| t.0 == *mime) {
            Some(t) => t.1 += 1,
            None => self.types.push((mime.clone(), 1, name.to_owned())),
        }
    }

    /// What the source is called on the hub: the file's name, or the count.
    pub fn source_text(&self) -> String {
        match (self.total, &self.first_name) {
            (0, _) => String::new(),
            (1, Some(name)) => name.clone(),
            (n, _) => format!("{n} files"),
        }
    }

    /// How many files have type `mime`, and the first one's name.
    pub fn of(&self, mime: &str) -> (usize, &str) {
        self.types
            .iter()
            .find(|t| t.0 == mime)
            .map_or((0, ""), |t| (t.1, t.2.as_str()))
    }
}

/// What a preset runs for a selection.
#[derive(Debug)]
pub enum Choice<'c> {
    /// The 1:1 route, one input at a time.
    Route(Vec<&'c Action>),
    /// A join action: all accepted inputs become one output.
    Join(&'c Action),
}

/// Files in `tally` of any type in `types`.
fn count_of<'t>(tally: &Tally, types: impl IntoIterator<Item = &'t str>) -> usize {
    types.into_iter().map(|m| tally.of(m).0).sum()
}

/// Files `a` would take from `tally`, as a join action.
fn join_count(a: &Action, tally: &Tally) -> usize {
    a.join
        .as_ref()
        .map_or(0, |j| count_of(tally, j.types.iter().map(String::as_str)))
}

/// The join actions of a preset: they accept every type of the preset's
/// join set and make its output. Type check only, no tool lookups.
fn join_candidates<'c>(cat: &'c Catalog, p: &Preset) -> impl Iterator<Item = &'c Action> {
    let (output, set) = (p.output, p.join);
    cat.joins().filter(move |a| {
        !set.is_empty()
            && a.output == output
            && a.join
                .as_ref()
                .is_some_and(|j| set.iter().all(|m| j.types.iter().any(|t| t == m)))
    })
}

/// The runnable join action of a preset, cheapest first (id breaks ties),
/// with the files it takes. The cheap checks (type set, a file of its types
/// in the selection) come before `missing_tools`, which stats `PATH`: this
/// runs on the drop to stage-2 path.
fn join_action<'c>(cat: &'c Catalog, p: &Preset, tally: &Tally) -> Option<(&'c Action, usize)> {
    if !join_candidates(cat, p).any(|a| join_count(a, tally) > 0) {
        return None;
    }
    let a = join_candidates(cat, p)
        .filter(|a| missing_tools(a).is_empty())
        .min_by(|a, b| (a.cost, &a.id).cmp(&(b.cost, &b.id)))?;
    let n = join_count(a, tally);
    let min = a.join.as_ref().map_or(0, |j| j.min);
    (n > 0 && n >= usize::try_from(min).unwrap_or(usize::MAX)).then_some((a, n))
}

/// What a preset runs for a selection and for how many of its files, `None`
/// when it cannot run. The 1:1 route and the join action compete on how many
/// files each takes; on a tie the 1:1 route wins (it keeps the files apart).
/// A join action needs at least its `min` files. The one source of truth for
/// "applicable": the wheel, the offer and the run all ask it.
pub fn choose<'c>(cat: &'c Catalog, preset: &Preset, tally: &Tally) -> Option<(Choice<'c>, usize)> {
    let one = tally.of(preset.input).0;
    let route = if one > 0 {
        route(cat, preset.input, preset.output)
    } else {
        None
    };
    let join = join_action(cat, preset, tally);
    match (route, join) {
        (Some(r), Some((_, n))) if one >= n => Some((Choice::Route(r), one)),
        (_, Some((a, n))) => Some((Choice::Join(a), n)),
        (Some(r), None) => Some((Choice::Route(r), one)),
        (None, None) => None,
    }
}

/// How many files a preset could take if something were installed: the 1:1
/// input or the join set, whichever has more, and the name of the first such
/// file with the type to group presets by.
fn raw_source<'t>(p: &Preset, tally: &'t Tally) -> (usize, &'t str, &'static str) {
    let (one, one_name) = tally.of(p.input);
    let join = count_of(tally, p.join.iter().copied());
    if join > one {
        let name = tally
            .types
            .iter()
            .find(|t| p.join.contains(&t.0.as_str()))
            .map_or("", |t| t.2.as_str());
        (join, name, p.join[0])
    } else {
        (one, one_name, p.input)
    }
}

/// Why a preset cannot run, `None` when it can.
fn disabled_reason(cat: &Catalog, p: &Preset, tally: &Tally) -> Option<String> {
    if choose(cat, p, tally).is_some() {
        return None;
    }
    // The side the files are on: the join set when it has more files.
    if count_of(tally, p.join.iter().copied()) > tally.of(p.input).0 {
        let mut joins = join_candidates(cat, p).peekable();
        if joins.peek().is_none() {
            return Some("no action installed".into());
        }
        return Some(
            match joins.find_map(|a| missing_tools(a).into_iter().next()) {
                Some(t) => format!("needs {t}"),
                None => "needs more files".into(),
            },
        );
    }
    Some(match plan(cat, p.input, p.output, |_| true) {
        Some(r) => match r.iter().find_map(|a| missing_tools(a).into_iter().next()) {
            Some(t) => format!("needs {t}"),
            None => "no action installed".into(),
        },
        None => "no action installed".into(),
    })
}

/// A first-level category slice. `lit`: some dragged file has a runnable
/// preset in it; dim: the category exists, nothing applies.
pub struct GroupSlice {
    pub group: &'static str,
    pub label: &'static str,
    pub lit: bool,
}

/// The first-level wheel, each category in its fixed direction.
pub fn groups(cat: &Catalog, tally: &Tally) -> [Option<GroupSlice>; 8] {
    groups_in(GROUPS, PRESETS, cat, tally)
}

fn groups_in(
    table: &'static [Group],
    presets: &[Preset],
    cat: &Catalog,
    tally: &Tally,
) -> [Option<GroupSlice>; 8] {
    warn_unknown_groups(table, presets);
    let mut out: [Option<GroupSlice>; 8] = Default::default();
    for g in table {
        let lit = presets
            .iter()
            .any(|p| p.group == g.id && choose(cat, p, tally).is_some());
        out[usize::from(g.direction)] = Some(GroupSlice {
            group: g.id,
            label: g.label,
            lit,
        });
    }
    out
}

/// One stderr line per run for presets whose group is not in the table.
fn warn_unknown_groups(table: &[Group], presets: &[Preset]) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    for p in presets {
        if !table.iter().any(|g| g.id == p.group)
            && !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            eprintln!("inacircle: preset {} has unknown group {}", p.id, p.group);
        }
    }
}

/// The second-level wheel: the presets of one category in their own
/// directions; a preset no dragged file fits is dim (`applicable == 0`).
/// The hub is the category's label.
pub fn offer_group(cat: &Catalog, tally: &Tally, group: &str) -> Offer {
    offer_group_in(GROUPS, PRESETS, cat, tally, group)
}

fn offer_group_in(
    table: &'static [Group],
    presets: &[Preset],
    cat: &Catalog,
    tally: &Tally,
    group: &str,
) -> Offer {
    let mut slices: [Option<Slice>; 8] = Default::default();
    let total = tally.total;
    let Some(g) = table.iter().find(|g| g.id == group) else {
        return Offer {
            slices,
            hub: String::new(),
            total,
        };
    };
    for p in presets.iter().filter(|p| p.group == g.id) {
        let applicable = match choose(cat, p, tally) {
            Some((_, n)) => n,
            None => raw_source(p, tally).0,
        };
        let disabled = if applicable == 0 {
            Some("no matching file".to_owned())
        } else {
            disabled_reason(cat, p, tally)
        };
        slices[usize::from(p.direction)] = Some(Slice {
            preset: p.id,
            label: p.label.into(),
            applicable,
            disabled,
        });
    }
    Offer {
        slices,
        hub: g.label.to_owned(),
        total,
    }
}

/// The wheel for a selection: presets of the most common file type, each in
/// its fixed direction.
pub fn offer(cat: &Catalog, files: &[(String, Kind)]) -> Offer {
    let mut tally = Tally::default();
    for (name, kind) in files {
        tally.add(name, kind);
    }
    offer_tally(cat, &tally)
}

/// [`offer`] from a [`Tally`].
pub fn offer_tally(cat: &Catalog, tally: &Tally) -> Offer {
    let total = tally.total;
    let mut slices: [Option<Slice>; 8] = Default::default();
    // Per preset: its files' count, the first one's name and the type that
    // groups presets sharing a source. The count is `choose`'s when the
    // preset can run, else what it would take.
    let source = |p: &Preset| match choose(cat, p, tally) {
        Some((Choice::Route(_), n)) => (n, tally.of(p.input).1, p.input),
        Some((Choice::Join(a), n)) => {
            let types = a.join.as_ref().map_or(&[][..], |j| &j.types[..]);
            let name = tally
                .types
                .iter()
                .find(|t| types.contains(&t.0))
                .map_or("", |t| t.2.as_str());
            (n, name, p.join[0])
        }
        None => raw_source(p, tally),
    };
    // Most common source that has presets; ties: first preset order.
    let mut best: Option<(usize, &str, &str)> = None;
    for p in PRESETS {
        let (n, name, key) = source(p);
        if n > 0 && best.is_none_or(|(m, ..)| n > m) {
            best = Some((n, name, key));
        }
    }
    let Some((applicable, name, key)) = best else {
        let hub = match &tally.first_label {
            Some(label) => format!("no action for {label}"),
            None => String::new(),
        };
        return Offer { slices, hub, total };
    };
    for p in PRESETS.iter().filter(|p| source(p).2 == key) {
        let disabled = disabled_reason(cat, p, tally);
        slices[usize::from(p.direction)] = Some(Slice {
            preset: p.id,
            label: p.label.into(),
            applicable,
            disabled,
        });
    }
    let hub = if total == 1 {
        name.to_owned()
    } else {
        format!("{applicable} / {total} files")
    };
    Offer { slices, hub, total }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(id: &str, input: &str, output: &str, cost: u32, lossy: bool) -> Action {
        Action {
            id: id.into(),
            family: "t".into(),
            bin: "/bin/true".into(),
            input: input.into(),
            output: output.into(),
            ext: "x".into(),
            label: id.into(),
            group: "g".into(),
            cost,
            lossy,
            batch: false,
            join: None,
            requires: vec![],
            builtin: true,
        }
    }

    fn ids(r: Option<Vec<&Action>>) -> Option<Vec<String>> {
        r.map(|v| v.iter().map(|a| a.id.clone()).collect())
    }

    fn kind(mime: Option<&str>, label: &str) -> Kind {
        Kind {
            mime: mime.map(String::from),
            label: label.into(),
        }
    }

    fn file(name: &str, mime: &str) -> (String, Kind) {
        (name.into(), kind(Some(mime), "X"))
    }

    fn two_way() -> Catalog {
        Catalog {
            actions: vec![
                act("to-md", "text/plain", "text/markdown", 1, false),
                act("to-txt", "text/markdown", "text/plain", 1, false),
            ],
        }
    }

    /// The tally keeps counts and one name per type, and the offer from it
    /// matches the offer from the full list.
    #[test]
    fn tally_keeps_counts_and_one_name_per_type() {
        let files = [
            file("a.txt", "text/plain"),
            file("b.md", "text/markdown"),
            file("c.txt", "text/plain"),
            ("d".to_owned(), kind(None, "PDF")),
        ];
        let mut t = Tally::default();
        for (n, k) in &files {
            t.add(n, k);
        }
        assert_eq!(t.total, 4);
        assert_eq!(t.of("text/plain"), (2, "a.txt"));
        assert_eq!(t.of("text/markdown"), (1, "b.md"));
        assert_eq!(t.of("image/png"), (0, ""));
        assert_eq!(t.types.len(), 2);
        assert_eq!(t.first_label.as_deref(), Some("X"));
        let (a, b) = (offer_tally(&two_way(), &t), offer(&two_way(), &files));
        assert_eq!((a.hub.as_str(), a.total), ("2 / 4 files", 4));
        assert_eq!((b.hub, b.total), (a.hub, a.total));
    }

    #[test]
    fn presets_have_unique_directions_per_input() {
        for (i, a) in PRESETS.iter().enumerate() {
            assert!(a.direction < 8, "{}", a.id);
            for b in &PRESETS[i + 1..] {
                assert!(
                    a.input != b.input || a.direction != b.direction,
                    "{} {}",
                    a.id,
                    b.id
                );
            }
        }
    }

    #[test]
    fn direct_edge_cheapest() {
        let cat = Catalog {
            actions: vec![
                act("b", "a/x", "b/y", 5, false),
                act("c", "a/x", "b/y", 2, false),
                act("d", "a/x", "b/y", 2, false),
                act("via1", "a/x", "m/z", 1, false),
                act("via2", "m/z", "b/y", 1, false),
            ],
        };
        assert_eq!(ids(route(&cat, "a/x", "b/y")), Some(vec!["c".into()]));
    }

    /// The shipped text actions: `text.txt-to-md` is lossy, so plain
    /// text reaches a Markdown-only target by no two-step route, while the
    /// direct lossy step itself is still planned.
    #[test]
    fn lossy_txt_to_md_is_not_a_lossless_intermediate() {
        let cat = Catalog {
            actions: vec![
                act("text.txt-to-md", "text/plain", "text/markdown", 10, true),
                act("text.md-to-txt", "text/markdown", "text/plain", 10, true),
                act("x.md-to-html", "text/markdown", "text/html", 5, false),
            ],
        };
        assert_eq!(ids(route(&cat, "text/plain", "text/html")), None);
        assert_eq!(
            ids(route(&cat, "text/plain", "text/markdown")),
            Some(vec!["text.txt-to-md".into()])
        );
    }

    #[test]
    fn two_steps_through_lossless_only() {
        let lossy = Catalog {
            actions: vec![
                act("s1", "a/x", "m/z", 1, true),
                act("s2", "m/z", "b/y", 1, false),
            ],
        };
        assert_eq!(ids(route(&lossy, "a/x", "b/y")), None);
        let ok = Catalog {
            actions: vec![
                act("s1", "a/x", "m/z", 3, false),
                act("s2", "m/z", "b/y", 1, true),
                act("t1", "a/x", "n/z", 1, false),
                act("t2", "n/z", "b/y", 1, true),
            ],
        };
        assert_eq!(
            ids(route(&ok, "a/x", "b/y")),
            Some(vec!["t1".into(), "t2".into()])
        );
    }

    #[test]
    fn no_route_none() {
        assert_eq!(ids(route(&two_way(), "text/plain", "image/png")), None);
        assert_eq!(ids(route(&Catalog { actions: vec![] }, "a/x", "b/y")), None);
    }

    #[test]
    fn missing_tool_disables_slice() {
        let mut a = act("to-md", "text/plain", "text/markdown", 1, false);
        a.requires = vec!["no-such-tool-inacircle".into()];
        let cat = Catalog { actions: vec![a] };
        assert_eq!(ids(route(&cat, "text/plain", "text/markdown")), None);
        let o = offer(&cat, &[file("a.txt", "text/plain")]);
        let s = o.slices[0].as_ref().unwrap();
        assert_eq!(s.disabled.as_deref(), Some("needs no-such-tool-inacircle"));
    }

    #[test]
    fn missing_tool_falls_back_to_runnable_route() {
        let mut a = act("bad", "text/plain", "text/markdown", 1, false);
        a.requires = vec!["no-such-tool-inacircle".into()];
        let cat = Catalog {
            actions: vec![a, act("good", "text/plain", "text/markdown", 9, false)],
        };
        assert_eq!(
            ids(route(&cat, "text/plain", "text/markdown")),
            Some(vec!["good".into()])
        );
    }

    #[test]
    fn no_action_installed_slice_still_shown() {
        let cat = Catalog { actions: vec![] };
        let o = offer(&cat, &[file("a.txt", "text/plain")]);
        let s = o.slices[0].as_ref().unwrap();
        assert_eq!(s.disabled.as_deref(), Some("no action installed"));
    }

    #[test]
    fn offer_single_txt() {
        let o = offer(&two_way(), &[file("notes.txt", "text/plain")]);
        let s = o.slices[0].as_ref().unwrap();
        assert_eq!((s.preset, s.label.as_str()), ("markdown", "Markdown"));
        assert_eq!((s.applicable, s.disabled.as_deref()), (1, None));
        assert_eq!(o.slices.iter().flatten().count(), 1);
        assert_eq!((o.hub.as_str(), o.total), ("notes.txt", 1));
    }

    #[test]
    fn offer_mixed_most_common_wins() {
        let mut f = vec![];
        for i in 0..3 {
            f.push(file(&format!("{i}.md"), "text/markdown"));
        }
        for i in 0..2 {
            f.push(file(&format!("{i}.txt"), "text/plain"));
        }
        let o = offer(&two_way(), &f);
        let s = o.slices[4].as_ref().unwrap();
        assert_eq!((s.preset, s.applicable), ("plain-text", 3));
        assert!(o.slices[0].is_none());
        assert_eq!((o.hub.as_str(), o.total), ("3 / 5 files", 5));
    }

    #[test]
    fn offer_tie_uses_preset_order() {
        let f = [file("a.md", "text/markdown"), file("b.txt", "text/plain")];
        let o = offer(&two_way(), &f);
        assert!(o.slices[0].is_some() && o.slices[4].is_none());
        assert_eq!(o.hub, "1 / 2 files");
    }

    #[test]
    fn offer_unknown_only() {
        let f = [
            ("a.pdf".to_string(), kind(Some("application/pdf"), "PDF")),
            ("b".to_string(), kind(None, "unknown")),
        ];
        let o = offer(&two_way(), &f);
        assert!(o.slices.iter().all(Option::is_none));
        assert_eq!((o.hub.as_str(), o.total), ("no action for PDF", 2));
    }

    fn tally(files: &[(&str, &str)]) -> Tally {
        let mut t = Tally::default();
        for (n, m) in files {
            t.add(n, &kind(Some(m), "X"));
        }
        t
    }

    #[test]
    fn groups_light_document_for_text() {
        let g = groups(&two_way(), &tally(&[("a.txt", "text/plain")]));
        let s = g[0].as_ref().unwrap();
        assert_eq!((s.group, s.label, s.lit), ("document", "Document", true));
        // Book is on the wheel too, dim: nothing here is a comic book.
        assert_eq!(g.iter().flatten().count(), 2);
        assert!(!g[2].as_ref().unwrap().lit);
    }

    #[test]
    fn groups_dim_document_without_a_route() {
        let t = tally(&[("a.txt", "text/plain")]);
        let g = groups(&Catalog { actions: vec![] }, &t);
        assert!(!g[0].as_ref().unwrap().lit);
        // A file nothing applies to: the category exists, so it is dim.
        let g = groups(&two_way(), &tally(&[("a.pdf", "application/pdf")]));
        assert!(!g[0].as_ref().unwrap().lit);
        // No files at all: dim as well, never absent.
        let g = groups(&two_way(), &Tally::default());
        assert!(!g[0].as_ref().unwrap().lit);
    }

    #[test]
    fn unknown_group_is_not_shown() {
        let presets = [Preset {
            id: "x",
            input: "text/plain",
            output: "text/markdown",
            label: "X",
            direction: 0,
            group: "nonexistent",
            join: &[],
        }];
        let t = tally(&[("a.txt", "text/plain")]);
        let g = groups_in(GROUPS, &presets, &two_way(), &t);
        // Only the table's categories appear, and the stray preset lights nothing.
        assert_eq!(g.iter().flatten().count(), GROUPS.len());
        assert!(!g[0].as_ref().unwrap().lit);
        let o = offer_group_in(GROUPS, &presets, &two_way(), &t, "nonexistent");
        assert!(o.slices.iter().all(Option::is_none));
    }

    #[test]
    fn offer_group_lists_only_its_presets() {
        let t = tally(&[("a.txt", "text/plain")]);
        let o = offer_group(&two_way(), &t, "document");
        assert_eq!(o.hub, "Document");
        assert_eq!(o.total, 1);
        let md = o.slices[0].as_ref().unwrap();
        assert_eq!(
            (md.preset, md.applicable, md.disabled.as_deref()),
            ("markdown", 1, None)
        );
        // plain-text is in the group, but no file fits it: dim.
        let pt = o.slices[4].as_ref().unwrap();
        assert_eq!(pt.applicable, 0);
        assert!(pt.disabled.is_some());
        assert_eq!(o.slices.iter().flatten().count(), 2);
        let o = offer_group(&two_way(), &t, "nope");
        assert!(o.slices.iter().all(Option::is_none));
        assert_eq!(o.hub, "");
    }

    #[test]
    fn mixed_types_light_the_group_and_both_targets() {
        let t = tally(&[
            ("a.txt", "text/plain"),
            ("b.md", "text/markdown"),
            ("c.md", "text/markdown"),
        ]);
        let g = groups(&two_way(), &t);
        assert!(g[0].as_ref().unwrap().lit);
        let o = offer_group(&two_way(), &t, "document");
        let md = o.slices[0].as_ref().unwrap();
        let pt = o.slices[4].as_ref().unwrap();
        assert_eq!((md.applicable, md.disabled.as_deref()), (1, None));
        assert_eq!((pt.applicable, pt.disabled.as_deref()), (2, None));
        assert_eq!(o.total, 3);
        assert_eq!(t.source_text(), "3 files");
        assert_eq!(tally(&[("a.txt", "text/plain")]).source_text(), "a.txt");
        assert_eq!(Tally::default().source_text(), "");
    }

    /// Directions are part of the muscle memory: pinned, and unique where
    /// they share a table.
    #[test]
    fn group_directions_are_fixed() {
        let doc = GROUPS.iter().find(|g| g.id == "document").unwrap();
        assert_eq!((doc.label, doc.direction), ("Document", 0));
        for (i, a) in GROUPS.iter().enumerate() {
            assert!(a.direction < 8, "{}", a.id);
            assert!(GROUPS[i + 1..].iter().all(|b| b.direction != a.direction));
        }
        let dir = |id: &str| PRESETS.iter().find(|p| p.id == id).unwrap().direction;
        assert_eq!((dir("markdown"), dir("plain-text")), (0, 4));
        for (i, a) in PRESETS.iter().enumerate() {
            assert!(GROUPS.iter().any(|g| g.id == a.group), "{}", a.id);
            assert!(
                PRESETS[i + 1..]
                    .iter()
                    .all(|b| a.group != b.group || a.direction != b.direction),
                "{} shares a direction in its group",
                a.id
            );
        }
    }

    #[test]
    fn book_group_sits_east() {
        let book = GROUPS.iter().find(|g| g.id == "book").unwrap();
        assert_eq!((book.label, book.direction), ("Book", 2));
        let pdf = PRESETS.iter().find(|p| p.id == "pdf").unwrap();
        assert_eq!(
            (pdf.input, pdf.output, pdf.label, pdf.direction, pdf.group),
            (
                "application/vnd.comicbook+zip",
                "application/pdf",
                "PDF",
                0,
                "book"
            )
        );
    }

    #[test]
    fn comic_book_offers_pdf_north_in_book() {
        let mut a = act(
            "pages.cbz-to-pdf",
            "application/vnd.comicbook+zip",
            "application/pdf",
            10,
            false,
        );
        a.group = "book".into();
        let cat = Catalog { actions: vec![a] };
        let t = tally(&[("x.cbz", "application/vnd.comicbook+zip")]);
        let g = groups(&cat, &t);
        let book = g[2].as_ref().unwrap();
        assert_eq!((book.group, book.label, book.lit), ("book", "Book", true));
        assert!(!g[0].as_ref().unwrap().lit, "Document stays dim");
        let o = offer_group(&cat, &t, "book");
        assert_eq!(o.hub, "Book");
        let pdf = o.slices[0].as_ref().unwrap();
        assert_eq!(
            (
                pdf.preset,
                pdf.label.as_str(),
                pdf.applicable,
                pdf.disabled.as_deref()
            ),
            ("pdf", "PDF", 1, None)
        );
        assert_eq!(o.slices.iter().flatten().count(), 1);
        // The flat offer for the same file offers it too.
        let o = offer(
            &cat,
            &[(
                "x.cbz".into(),
                kind(Some("application/vnd.comicbook+zip"), "Comic book"),
            )],
        );
        assert_eq!(o.slices[0].as_ref().unwrap().preset, "pdf");
    }

    const CBZ: &str = "application/vnd.comicbook+zip";

    fn pdf_catalog(with_join: bool) -> Catalog {
        let mut cbz = act("pages.cbz-to-pdf", CBZ, "application/pdf", 10, false);
        cbz.group = "book".into();
        let mut actions = vec![cbz];
        if with_join {
            let mut j = act(
                "pages.images-to-pdf",
                "image/jpeg",
                "application/pdf",
                10,
                true,
            );
            j.group = "book".into();
            j.batch = true;
            j.join = Some(crate::catalog::Join {
                types: vec!["image/jpeg".into(), "image/png".into()],
                min: 1,
            });
            actions.push(j);
        }
        Catalog { actions }
    }

    fn many(mime: &str, n: usize, ext: &str) -> Vec<(String, String)> {
        (0..n)
            .map(|i| (format!("{i}.{ext}"), mime.to_owned()))
            .collect()
    }

    fn tally_of(parts: &[Vec<(String, String)>]) -> Tally {
        let mut t = Tally::default();
        for (n, m) in parts.iter().flatten() {
            t.add(n, &kind(Some(m), "X"));
        }
        t
    }

    fn pdf_preset() -> &'static Preset {
        PRESETS.iter().find(|p| p.id == "pdf").unwrap()
    }

    fn pdf_slice(o: &Offer) -> &Slice {
        o.slices[0].as_ref().unwrap()
    }

    #[test]
    fn images_offer_pdf_in_book() {
        let cat = pdf_catalog(true);
        let t = tally_of(&[many("image/jpeg", 10, "jpg"), many("image/png", 12, "png")]);
        let g = groups(&cat, &t);
        assert!(g[2].as_ref().unwrap().lit, "Book lights");
        let o = offer_group(&cat, &t, "book");
        let s = pdf_slice(&o);
        assert_eq!(
            (s.preset, s.applicable, s.disabled.as_deref()),
            ("pdf", 22, None)
        );
        let o = offer_tally(&cat, &t);
        assert_eq!(pdf_slice(&o).applicable, 22);
        assert_eq!(o.hub, "22 / 22 files");
    }

    #[test]
    fn single_image_offers_pdf() {
        let cat = pdf_catalog(true);
        let t = tally(&[("kep.png", "image/png")]);
        let o = offer_tally(&cat, &t);
        let s = pdf_slice(&o);
        assert_eq!(
            (s.preset, s.applicable, s.disabled.as_deref()),
            ("pdf", 1, None)
        );
        assert_eq!(o.hub, "kep.png");
        let o = offer_group(&cat, &t, "book");
        assert_eq!(pdf_slice(&o).applicable, 1);
    }

    #[test]
    fn mixed_selection_counts_the_set() {
        let cat = pdf_catalog(true);
        let check = |books: usize, images: usize, join: bool, n: usize| {
            let t = tally_of(&[many(CBZ, books, "cbz"), many("image/png", images, "png")]);
            let (c, got) = choose(&cat, pdf_preset(), &t).unwrap();
            assert_eq!(
                (matches!(c, Choice::Join(_)), got),
                (join, n),
                "{books}+{images}"
            );
            let o = offer_tally(&cat, &t);
            assert_eq!(pdf_slice(&o).applicable, n);
            assert_eq!(o.hub, format!("{n} / 8 files"));
            let o = offer_group(&cat, &t, "book");
            assert_eq!(pdf_slice(&o).applicable, n);
        };
        check(3, 5, true, 5);
        check(5, 3, false, 5);
        // Tie: the 1:1 input wins.
        check(4, 4, false, 4);
    }

    #[test]
    fn images_and_text_offer_pdf() {
        let cat = Catalog {
            actions: [pdf_catalog(true).actions, two_way().actions].concat(),
        };
        let t = tally_of(&[many("image/png", 20, "png"), many("text/plain", 2, "txt")]);
        let g = groups(&cat, &t);
        assert!(g[0].as_ref().unwrap().lit && g[2].as_ref().unwrap().lit);
        let o = offer_group(&cat, &t, "book");
        assert_eq!(pdf_slice(&o).applicable, 20);
        let o = offer_tally(&cat, &t);
        assert_eq!(pdf_slice(&o).applicable, 20);
        assert_eq!(o.hub, "20 / 22 files");
    }

    #[test]
    fn join_is_never_chained() {
        let cat = pdf_catalog(true);
        assert!(plan(&cat, "image/jpeg", "application/pdf", |_| true).is_none());
        assert!(route(&cat, "image/jpeg", "application/pdf").is_none());
        // Nor as the second step of a chain.
        let mut cat = cat;
        cat.actions
            .push(act("t.to-jpeg", "image/gif", "image/jpeg", 1, false));
        assert!(route(&cat, "image/gif", "application/pdf").is_none());
    }

    #[test]
    fn no_join_action_installed_dims_pdf_for_images() {
        let cat = pdf_catalog(false);
        let t = tally(&[("a.png", "image/png"), ("b.jpg", "image/jpeg")]);
        let g = groups(&cat, &t);
        assert!(!g[2].as_ref().unwrap().lit);
        let o = offer_group(&cat, &t, "book");
        let s = pdf_slice(&o);
        assert_eq!(
            (s.applicable, s.disabled.as_deref()),
            (2, Some("no action installed"))
        );
        let o = offer_tally(&cat, &t);
        assert_eq!(
            pdf_slice(&o).disabled.as_deref(),
            Some("no action installed")
        );
        assert!(choose(&cat, pdf_preset(), &t).is_none());
    }

    #[test]
    fn lone_png_without_join_action_dims_pdf_and_names_the_file() {
        let cat = pdf_catalog(false);
        let t = tally(&[("a.png", "image/png")]);
        let o = offer_tally(&cat, &t);
        assert_eq!(o.hub, "a.png");
        let s = pdf_slice(&o);
        assert_eq!(
            (s.applicable, s.disabled.as_deref()),
            (1, Some("no action installed"))
        );
    }

    #[test]
    fn join_needs_its_minimum() {
        let mut cat = pdf_catalog(true);
        cat.actions[1].join.as_mut().unwrap().min = 3;
        let two = tally_of(&[many("image/png", 2, "png")]);
        assert!(choose(&cat, pdf_preset(), &two).is_none());
        let o = offer_group(&cat, &two, "book");
        assert_eq!(pdf_slice(&o).disabled.as_deref(), Some("needs more files"));
        let three = tally_of(&[many("image/png", 3, "png")]);
        assert!(matches!(
            choose(&cat, pdf_preset(), &three),
            Some((Choice::Join(_), 3))
        ));
    }

    #[test]
    fn join_candidates_need_a_join_set_and_all_its_types() {
        let cat = pdf_catalog(true);
        let none = Preset {
            join: &[],
            ..*pdf_preset()
        };
        assert_eq!(join_candidates(&cat, &none).count(), 0);
        assert_eq!(join_candidates(&cat, pdf_preset()).count(), 1);
        let mut gif = pdf_catalog(true);
        gif.actions[1].join.as_mut().unwrap().types = vec!["image/gif".into()];
        assert_eq!(join_candidates(&gif, pdf_preset()).count(), 0);
    }

    #[test]
    fn tie_between_sides_reports_the_one_to_one_reason() {
        let mut cat = pdf_catalog(true);
        cat.actions[1].join.as_mut().unwrap().min = 3;
        cat.actions.remove(0);
        let t = tally_of(&[many(CBZ, 1, "cbz"), many("image/png", 1, "png")]);
        assert!(choose(&cat, pdf_preset(), &t).is_none());
        assert_eq!(
            disabled_reason(&cat, pdf_preset(), &t).as_deref(),
            Some("no action installed")
        );
    }

    #[test]
    fn offer_empty() {
        let o = offer(&two_way(), &[]);
        assert!(o.slices.iter().all(Option::is_none));
        assert_eq!(o.total, 0);
    }
}
