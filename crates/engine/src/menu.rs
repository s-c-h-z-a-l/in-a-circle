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
}

/// A category on the first-level wheel. `direction`: as for [`Preset`].
pub struct Group {
    pub id: &'static str,
    pub label: &'static str,
    pub direction: u8,
}

pub const GROUPS: &[Group] = &[Group {
    id: "document",
    label: "Document",
    direction: 0,
}];

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "markdown",
        input: "text/plain",
        output: "text/markdown",
        label: "Markdown",
        direction: 0,
        group: "document",
    },
    Preset {
        id: "plain-text",
        input: "text/markdown",
        output: "text/plain",
        label: "Plain text",
        direction: 4,
        group: "document",
    },
];

/// Cheapest route from `input` to `output`: one direct action, else two
/// steps whose first is lossless. Actions with missing tools never take part.
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
        .filter(|a| a.input == input && a.output == output && usable(a))
        .min_by(|a, b| (a.cost, &a.id).cmp(&(b.cost, &b.id)));
    if let Some(a) = direct {
        return Some(vec![a]);
    }
    let mut best: Option<(u64, &Action, &Action)> = None;
    for a in cat
        .actions
        .iter()
        .filter(|a| a.input == input && !a.lossy && a.output != output && usable(a))
    {
        for b in cat
            .actions
            .iter()
            .filter(|b| b.input == a.output && b.output == output && usable(b))
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

/// Why a preset cannot run, `None` when it can.
fn disabled_reason(cat: &Catalog, p: &Preset) -> Option<String> {
    if route(cat, p.input, p.output).is_some() {
        return None;
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
        let lit = presets.iter().any(|p| {
            p.group == g.id && tally.of(p.input).0 > 0 && route(cat, p.input, p.output).is_some()
        });
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
        let applicable = tally.of(p.input).0;
        let disabled = if applicable == 0 {
            Some("no matching file".to_owned())
        } else {
            disabled_reason(cat, p)
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
    // Most common type that has presets; ties: first preset order.
    let mut best: Option<(&str, usize)> = None;
    for p in PRESETS {
        let n = tally.of(p.input).0;
        if n > 0 && best.is_none_or(|(_, m)| n > m) {
            best = Some((p.input, n));
        }
    }
    let Some((mime, applicable)) = best else {
        let hub = match &tally.first_label {
            Some(label) => format!("no action for {label}"),
            None => String::new(),
        };
        return Offer { slices, hub, total };
    };
    for p in PRESETS.iter().filter(|p| p.input == mime) {
        let disabled = disabled_reason(cat, p);
        slices[usize::from(p.direction)] = Some(Slice {
            preset: p.id,
            label: p.label.into(),
            applicable,
            disabled,
        });
    }
    let hub = if total == 1 {
        tally.of(mime).1.to_owned()
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
        assert_eq!(g.iter().flatten().count(), 1);
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
        }];
        let t = tally(&[("a.txt", "text/plain")]);
        let g = groups_in(GROUPS, &presets, &two_way(), &t);
        // Only the table's category appears, and the stray preset lights nothing.
        assert_eq!(g.iter().flatten().count(), 1);
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
    fn offer_empty() {
        let o = offer(&two_way(), &[]);
        assert!(o.slices.iter().all(Option::is_none));
        assert_eq!(o.total, 0);
    }
}
