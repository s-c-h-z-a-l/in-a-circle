//! Built-in presets, the route planner and the wheel offer.

use crate::catalog::{Action, Catalog, missing_tools};
use crate::classify::Kind;

/// A fixed wheel slot. `direction`: 0 = north, clockwise in 45 degree steps.
pub struct Preset {
    pub id: &'static str,
    /// The 1:1 form's input type; `None`: the preset has no 1:1 form (it
    /// only joins many files into one).
    pub input: Option<&'static str>,
    pub output: &'static str,
    pub label: &'static str,
    /// Direction within its group's wheel.
    pub direction: u8,
    /// Id of a [`GROUPS`] entry.
    pub group: &'static str,
    /// MIME types a join action must accept for this preset to offer it
    /// (many inputs, one output); empty: the preset has no join form.
    pub join: &'static [&'static str],
    /// The id of the one action the preset's 1:1 form runs, when two
    /// actions share its input and output types and only the id tells
    /// them apart (two code pages, text to UTF-8). It runs exactly that
    /// action, never a route the planner finds instead; one that is not
    /// installed (or does not have the preset's types, or is a join)
    /// dims the slice. `None`: the planner finds the route.
    pub action: Option<&'static str>,
}

/// A category on the first-level wheel. `direction`: as for [`Preset`].
pub struct Group {
    pub id: &'static str,
    pub label: &'static str,
    pub direction: u8,
}

/// The id of the Encoding group. The final notice names the code page
/// of an action in this group, so the engine's caller shares the id.
pub const ENCODING_GROUP: &str = "encoding";

/// The id of the Subtitles group (SRT, VTT and ASS conversions).
pub const SUBTITLES_GROUP: &str = "subtitles";

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
    // West and south are the free directions of the muscle-memory layout
    // (north Document, east Book). An 8-bit text file lights only Encoding,
    // a subtitle file only Subtitles.
    Group {
        id: ENCODING_GROUP,
        label: "Encoding",
        direction: 6,
    },
    Group {
        id: SUBTITLES_GROUP,
        label: "Subtitles",
        direction: 4,
    },
];

/// The order is the tie-break of both wheel levels: when two presets of
/// one group share a direction (HTML from Markdown and from plain text)
/// and the dropped files fit both equally, the earlier preset wins; the
/// flat offer's most common type picks the first preset's input on a tie
/// the same way. Plain text therefore precedes Markdown for HTML and EPUB,
/// as `markdown` (which takes plain text) does at the head of the table.
/// `plain-text-epub` shares Document's south slot with `plain-text`: on a
/// tie (one EPUB and one Markdown file) the earlier preset, `plain-text`,
/// holds the slot and wins the flat offer; more EPUBs than Markdown files
/// give it to `plain-text-epub`. `markdown-epub` shares the north slot with
/// `markdown` the same way: a tie (one EPUB and one plain-text file) goes to
/// `markdown`, more EPUBs than plain-text files give it to `markdown-epub`.
pub const PRESETS: &[Preset] = &[
    Preset {
        id: "markdown",
        input: Some("text/plain"),
        output: "text/markdown",
        label: "Markdown",
        direction: 0,
        group: "document",
        join: &[],
        action: None,
    },
    Preset {
        id: "plain-text",
        input: Some("text/markdown"),
        output: "text/plain",
        label: "Plain text",
        direction: 4,
        group: "document",
        join: &[],
        action: None,
    },
    Preset {
        id: "html-txt",
        input: Some("text/plain"),
        output: "text/html",
        label: "HTML",
        direction: 2,
        group: "document",
        join: &[],
        action: None,
    },
    Preset {
        id: "html",
        input: Some("text/markdown"),
        output: "text/html",
        label: "HTML",
        direction: 2,
        group: "document",
        join: &[],
        action: None,
    },
    Preset {
        id: "epub-txt",
        input: Some("text/plain"),
        output: "application/epub+zip",
        label: "EPUB",
        direction: 2,
        group: "book",
        join: &[],
        action: None,
    },
    // Many Markdown files (with the images they use) join into one book.
    Preset {
        id: "epub",
        input: Some("text/markdown"),
        output: "application/epub+zip",
        label: "EPUB",
        direction: 2,
        group: "book",
        join: &["text/markdown"],
        action: None,
    },
    Preset {
        id: "pdf",
        input: Some("application/vnd.comicbook+zip"),
        output: "application/pdf",
        label: "PDF",
        direction: 0,
        group: "book",
        join: &["image/jpeg", "image/png"],
        action: None,
    },
    Preset {
        id: "cbz",
        input: None,
        output: "application/vnd.comicbook+zip",
        label: "CBZ",
        direction: 4,
        group: "book",
        join: &["image/jpeg", "image/png"],
        action: None,
    },
    Preset {
        id: "plain-text-epub",
        input: Some("application/epub+zip"),
        output: "text/plain",
        label: "Plain text",
        direction: 4,
        group: "document",
        join: &[],
        action: None,
    },
    Preset {
        id: "markdown-epub",
        input: Some("application/epub+zip"),
        output: "text/markdown",
        label: "Markdown",
        direction: 0,
        group: "document",
        join: &[],
        action: None,
    },
    // Two code pages share the input and output types, so each names its
    // action; west and east are the two slices of the group's wheel.
    Preset {
        id: "cp1250",
        input: Some("text/x-8bit"),
        output: "text/plain",
        label: "Windows-1250",
        direction: 6,
        group: ENCODING_GROUP,
        join: &[],
        action: Some("text.cp1250-to-utf8"),
    },
    Preset {
        id: "iso88592",
        input: Some("text/x-8bit"),
        output: "text/plain",
        label: "ISO-8859-2",
        direction: 2,
        group: ENCODING_GROUP,
        join: &[],
        action: Some("text.iso88592-to-utf8"),
    },
    // Subtitles: SRT (south) and VTT (north) from any other format; an ASS
    // or a TTML file lights both slices. The planner finds each converter.
    Preset {
        id: "srt-vtt",
        input: Some("text/vtt"),
        output: "application/x-subrip",
        label: "SRT",
        direction: 4,
        group: SUBTITLES_GROUP,
        join: &[],
        action: None,
    },
    Preset {
        id: "srt-ass",
        input: Some("text/x-ssa"),
        output: "application/x-subrip",
        label: "SRT",
        direction: 4,
        group: SUBTITLES_GROUP,
        join: &[],
        action: None,
    },
    Preset {
        id: "vtt-srt",
        input: Some("application/x-subrip"),
        output: "text/vtt",
        label: "VTT",
        direction: 0,
        group: SUBTITLES_GROUP,
        join: &[],
        action: None,
    },
    Preset {
        id: "vtt-ass",
        input: Some("text/x-ssa"),
        output: "text/vtt",
        label: "VTT",
        direction: 0,
        group: SUBTITLES_GROUP,
        join: &[],
        action: None,
    },
    Preset {
        id: "srt-ttml",
        input: Some("application/ttml+xml"),
        output: "application/x-subrip",
        label: "SRT",
        direction: 4,
        group: SUBTITLES_GROUP,
        join: &[],
        action: None,
    },
    Preset {
        id: "vtt-ttml",
        input: Some("application/ttml+xml"),
        output: "text/vtt",
        label: "VTT",
        direction: 0,
        group: SUBTITLES_GROUP,
        join: &[],
        action: None,
    },
];

/// Cheapest route from `input` to `output`: one direct action, else two
/// steps whose first is lossless. Actions with missing tools never take part,
/// and a join action never does either: it takes many inputs, so it cannot
/// be a step of a chain (see [`choose`]).
pub fn route<'a>(cat: &'a Catalog, input: &str, output: &str) -> Option<Vec<&'a Action>> {
    plan(cat, input, output, |a| missing_tools(a).is_empty())
}

/// The 1:1 route `preset` runs for `input`: its named action alone when
/// it names one ([`Preset::action`]; `None` when that one is not
/// installed, not runnable or not of these types), else the planner's
/// [`route`] to the preset's output.
pub fn route_for<'a>(cat: &'a Catalog, preset: &Preset, input: &str) -> Option<Vec<&'a Action>> {
    route_with(cat, preset, input, |a| missing_tools(a).is_empty())
}

fn route_with<'a>(
    cat: &'a Catalog,
    preset: &Preset,
    input: &str,
    usable: impl Fn(&Action) -> bool,
) -> Option<Vec<&'a Action>> {
    let Some(id) = preset.action else {
        return plan(cat, input, preset.output, usable);
    };
    // The name alone is not enough: an action of other types (a plugin's
    // id clash, a typo) must not run in its place.
    cat.get(id)
        .filter(|a| a.join.is_none() && a.input == input && a.output == preset.output)
        .filter(|a| usable(a))
        .map(|a| vec![a])
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

/// Files `a` would take from `tally`, as a join action: `(documents,
/// assets)`. Assets count only beside a document: alone they never make a
/// run.
fn join_parts(a: &Action, tally: &Tally) -> (usize, usize) {
    let Some(j) = &a.join else {
        return (0, 0);
    };
    let docs = count_of(tally, j.types.iter().map(String::as_str));
    let assets = if docs > 0 {
        count_of(tally, j.assets.iter().map(String::as_str))
    } else {
        0
    };
    (docs, assets)
}

/// Files `a` would take from `tally`, as a join action: its documents and
/// the assets beside them.
fn join_count(a: &Action, tally: &Tally) -> usize {
    let (docs, assets) = join_parts(a, tally);
    docs + assets
}

/// The most files any join action of `p` could take, if it were runnable:
/// the preset's join set, or a candidate's documents and assets. A
/// candidate counts its whole type set, beyond the preset's set too, so a
/// disabled join shows what [`choose`] would give it once runnable. Type
/// checks only.
fn raw_join(cat: &Catalog, p: &Preset, tally: &Tally) -> usize {
    join_candidates(cat, p)
        .map(|a| join_count(a, tally))
        .fold(count_of(tally, p.join.iter().copied()), usize::max)
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
/// with the documents and assets it takes. The cheap checks (type set, a
/// document of its types in the selection) come before `missing_tools`,
/// which stats `PATH`: this runs on the drop to stage-2 path.
fn join_action<'c>(
    cat: &'c Catalog,
    p: &Preset,
    tally: &Tally,
) -> Option<(&'c Action, usize, usize)> {
    if !join_candidates(cat, p).any(|a| join_parts(a, tally).0 > 0) {
        return None;
    }
    let a = join_candidates(cat, p)
        .filter(|a| missing_tools(a).is_empty())
        .min_by(|a, b| (a.cost, &a.id).cmp(&(b.cost, &b.id)))?;
    let (docs, assets) = join_parts(a, tally);
    let min = a.join.as_ref().map_or(0, |j| j.min);
    (docs > 0 && docs >= usize::try_from(min).unwrap_or(usize::MAX)).then_some((a, docs, assets))
}

/// Whether the join action `a`, taking `docs` documents and `assets`
/// assets, wins over a 1:1 route that takes `one` files. A join without
/// assets wins only on more files: on a tie the 1:1 route keeps the files
/// apart. A join with assets (a book of documents and their images) wins
/// whenever it makes something one file cannot: two or more documents, or
/// a document with an asset, unless the 1:1 route takes more files; one
/// document alone keeps its 1:1 form.
fn join_wins(a: &Action, one: usize, docs: usize, assets: usize) -> bool {
    let n = docs + assets;
    if a.join.as_ref().is_some_and(|j| !j.assets.is_empty()) {
        (docs >= 2 || (docs >= 1 && assets >= 1)) && n >= one
    } else {
        n > one
    }
}

/// What a preset runs for a selection and for how many of its files, `None`
/// when it cannot run. The 1:1 route and the join action compete as
/// [`join_wins`] says. A join action needs at least its `min` documents;
/// its count is its documents and assets. The one source of truth for
/// "applicable": the wheel, the offer and the run all ask it.
pub fn choose<'c>(cat: &'c Catalog, preset: &Preset, tally: &Tally) -> Option<(Choice<'c>, usize)> {
    let (one, route) = match preset.input {
        Some(input) if tally.of(input).0 > 0 => (tally.of(input).0, route_for(cat, preset, input)),
        _ => (0, None),
    };
    let join = join_action(cat, preset, tally);
    match (route, join) {
        (Some(r), Some((a, docs, assets))) if !join_wins(a, one, docs, assets) => {
            Some((Choice::Route(r), one))
        }
        (_, Some((a, docs, assets))) => Some((Choice::Join(a), docs + assets)),
        (Some(r), None) => Some((Choice::Route(r), one)),
        (None, None) => None,
    }
}

/// How many files a preset could take if something were installed: the 1:1
/// input or the join side ([`raw_join`]), whichever has more, and the name
/// of the first such file with the type to group presets by.
fn raw_source<'t>(cat: &Catalog, p: &Preset, tally: &'t Tally) -> (usize, &'t str, &'static str) {
    let (one, one_name) = p.input.map_or((0, ""), |i| tally.of(i));
    let join = raw_join(cat, p, tally);
    if join > one {
        let name = tally
            .types
            .iter()
            .find(|t| p.join.contains(&t.0.as_str()))
            .map_or("", |t| t.2.as_str());
        (join, name, p.join[0])
    } else {
        (one, one_name, p.input.unwrap_or(p.id))
    }
}

/// Why a preset cannot run, `None` when it can.
fn disabled_reason(cat: &Catalog, p: &Preset, tally: &Tally) -> Option<String> {
    if choose(cat, p, tally).is_some() {
        return None;
    }
    // The side the files are on: the join set when it has more files.
    if raw_join(cat, p, tally) > p.input.map_or(0, |i| tally.of(i).0) {
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
    let Some(input) = p.input else {
        return Some("no action installed".into());
    };
    Some(match route_with(cat, p, input, |_| true) {
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
/// The hub is the category's label; when the category's lit targets take
/// fewer than all dropped files it is `<n> / <total> files`, `n` the most
/// files any one lit target takes (the count `choose` gives, the same the
/// flat offer shows).
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
            None => raw_source(cat, p, tally).0,
        };
        let disabled = if applicable == 0 {
            Some("no matching file".to_owned())
        } else {
            disabled_reason(cat, p, tally)
        };
        // Presets of different inputs may share a direction (HTML from
        // Markdown and from plain text). A preset that can run beats a dim
        // one; then the one that takes more of the dropped files holds the
        // slot; a tie keeps the earlier preset in [`PRESETS`] order (the
        // flat offer breaks its ties by the same order).
        let score = (disabled.is_none(), applicable);
        let held = slices[usize::from(p.direction)]
            .as_ref()
            .map(|h| (h.disabled.is_none(), h.applicable));
        if held.is_none_or(|h| score > h) {
            slices[usize::from(p.direction)] = Some(Slice {
                preset: p.id,
                label: p.label.into(),
                applicable,
                disabled,
            });
        }
    }
    // A lit target is one that can run; its `applicable` is `choose`'s count.
    let taken = slices
        .iter()
        .flatten()
        .filter(|s| s.disabled.is_none())
        .map(|s| s.applicable)
        .max()
        .unwrap_or(0);
    let hub = if total > 1 && taken > 0 && taken < total {
        format!("{taken} / {total} files")
    } else {
        g.label.to_owned()
    };
    Offer { slices, hub, total }
}

/// The flat offer for a selection: the presets of the most common file
/// type, each in its fixed direction inside its own group. Two groups may
/// use the same direction for the same source (Document and Book both have
/// an east slot for Markdown), so the slices are kept per group, never in
/// one array.
pub struct FlatOffer {
    /// Groups that have a preset for the best source, in [`GROUPS`] order.
    pub groups: Vec<(&'static str, Offer)>,
    pub hub: String,
    pub total: usize,
}

impl FlatOffer {
    /// The first group's wheel; an empty wheel (with the hub) when no group
    /// has a preset for the selection.
    #[cfg(test)]
    pub fn into_first(self) -> Offer {
        let (hub, total) = (self.hub, self.total);
        match self.groups.into_iter().next() {
            Some((_, o)) => o,
            None => Offer {
                slices: Default::default(),
                hub,
                total,
            },
        }
    }
}

/// The flat offer for a selection (see [`FlatOffer`]).
pub fn offer(cat: &Catalog, files: &[(String, Kind)]) -> FlatOffer {
    let mut tally = Tally::default();
    for (name, kind) in files {
        tally.add(name, kind);
    }
    offer_tally(cat, &tally)
}

/// [`offer`] from a [`Tally`].
pub fn offer_tally(cat: &Catalog, tally: &Tally) -> FlatOffer {
    let total = tally.total;
    // Per preset: its files' count, the first one's name and the type that
    // groups presets sharing a source. The count is `choose`'s when the
    // preset can run, else what it would take.
    let source = |p: &Preset| match choose(cat, p, tally) {
        Some((Choice::Route(_), n)) => {
            let input = p.input.unwrap_or_default();
            (n, tally.of(input).1, input)
        }
        Some((Choice::Join(a), n)) => {
            let types = a.join.as_ref().map_or(&[][..], |j| &j.types[..]);
            let name = tally
                .types
                .iter()
                .find(|t| types.contains(&t.0))
                .map_or("", |t| t.2.as_str());
            (n, name, p.join[0])
        }
        None => raw_source(cat, p, tally),
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
        return FlatOffer {
            groups: Vec::new(),
            hub,
            total,
        };
    };
    let hub = if total == 1 {
        name.to_owned()
    } else {
        format!("{applicable} / {total} files")
    };
    let mut groups = Vec::new();
    for g in GROUPS {
        let mut slices: [Option<Slice>; 8] = Default::default();
        for p in PRESETS
            .iter()
            .filter(|p| p.group == g.id && source(p).2 == key)
        {
            let disabled = disabled_reason(cat, p, tally);
            slices[usize::from(p.direction)] = Some(Slice {
                preset: p.id,
                label: p.label.into(),
                applicable,
                disabled,
            });
        }
        if slices.iter().any(Option::is_some) {
            groups.push((
                g.id,
                Offer {
                    slices,
                    hub: hub.clone(),
                    total,
                },
            ));
        }
    }
    FlatOffer { groups, hub, total }
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
    fn presets_unique_per_group_and_input() {
        for (i, a) in PRESETS.iter().enumerate() {
            assert!(a.direction < 8, "{}", a.id);
            for b in &PRESETS[i + 1..] {
                assert!(
                    a.group != b.group || a.input != b.input || a.direction != b.direction,
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
        let o = offer(&cat, &[file("a.txt", "text/plain")]).into_first();
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
        let o = offer(&cat, &[file("a.txt", "text/plain")]).into_first();
        let s = o.slices[0].as_ref().unwrap();
        assert_eq!(s.disabled.as_deref(), Some("no action installed"));
    }

    #[test]
    fn offer_single_txt() {
        let o = offer(&two_way(), &[file("notes.txt", "text/plain")]).into_first();
        let s = o.slices[0].as_ref().unwrap();
        assert_eq!((s.preset, s.label.as_str()), ("markdown", "Markdown"));
        assert_eq!((s.applicable, s.disabled.as_deref()), (1, None));
        assert_eq!(o.slices.iter().flatten().count(), 2);
        assert_eq!((o.hub.as_str(), o.total), ("notes.txt", 1));
    }

    /// One markdown file: Document's east slot is HTML and Book's east slot
    /// is EPUB; neither overwrites the other.
    #[test]
    fn offer_keeps_epub_and_html_apart() {
        let f = offer(&two_way(), &[file("a.md", "text/markdown")]);
        let slot = |g: &str| {
            let o = &f.groups.iter().find(|(id, _)| *id == g).unwrap().1;
            let s = o.slices[2].as_ref().unwrap();
            (s.preset, s.label.clone())
        };
        assert_eq!(slot("document"), ("html", "HTML".to_owned()));
        assert_eq!(slot("book"), ("epub", "EPUB".to_owned()));
        // Document keeps its other slice; Book has only EPUB.
        let doc = &f.groups[0].1;
        assert_eq!(doc.slices[4].as_ref().unwrap().preset, "plain-text");
        assert_eq!(f.groups[1].1.slices.iter().flatten().count(), 1);
    }

    /// The two-level wheel holds one slice per direction: the HTML preset
    /// whose input the dropped files have.
    #[test]
    fn group_wheel_picks_the_html_of_the_dropped_input() {
        let slot = |name: &str, mime: &str| {
            let o = offer_group(&two_way(), &tally(&[(name, mime)]), "document");
            o.slices[2].as_ref().unwrap().preset
        };
        assert_eq!(slot("a.md", "text/markdown"), "html");
        assert_eq!(slot("a.txt", "text/plain"), "html-txt");
    }

    /// One Markdown and one plain-text file: the flat offer and both
    /// two-level wheels pick the same variant (the earlier preset wins).
    #[test]
    fn tie_of_md_and_txt_is_the_same_at_both_levels() {
        let files = [file("a.md", "text/markdown"), file("b.txt", "text/plain")];
        let f = offer(&two_way(), &files);
        let mut t = Tally::default();
        for (n, k) in &files {
            t.add(n, k);
        }
        for g in ["document", "book"] {
            let flat = f.groups.iter().find(|(id, _)| *id == g).unwrap().1.slices[2]
                .as_ref()
                .unwrap()
                .preset;
            let wheel = offer_group(&two_way(), &t, g).slices[2]
                .as_ref()
                .unwrap()
                .preset;
            assert_eq!(flat, wheel, "{g}");
            assert!(flat.ends_with("-txt"), "{g}: {flat}");
        }
    }

    /// EPUB and Markdown share Document's south slot: a tie goes to the
    /// earlier preset (Markdown), more EPUBs take it, at both levels.
    #[test]
    fn epub_and_md_share_the_south_slot_by_count_then_order() {
        let cat = Catalog {
            actions: vec![
                act("md-txt", "text/markdown", "text/plain", 1, false),
                act("epub-txt", "application/epub+zip", "text/plain", 1, false),
            ],
        };
        let md = file("a.md", "text/markdown");
        let ep = |n: &str| file(n, "application/epub+zip");
        for (files, want) in [
            (vec![ep("a.epub"), md.clone()], "md-txt"),
            (vec![ep("a.epub"), ep("b.epub"), md.clone()], "epub-txt"),
        ] {
            let mut t = Tally::default();
            for (n, k) in &files {
                t.add(n, k);
            }
            let preset = offer_group(&cat, &t, "document").slices[4]
                .as_ref()
                .unwrap()
                .preset;
            let flat = offer(&cat, &files);
            let flat_south = flat
                .groups
                .iter()
                .find(|(g, _)| *g == "document")
                .unwrap()
                .1
                .slices[4]
                .as_ref()
                .unwrap()
                .preset;
            let id = if want == "md-txt" {
                "plain-text"
            } else {
                "plain-text-epub"
            };
            assert_eq!(preset, id, "{want}");
            assert_eq!(flat_south, id);
        }
    }

    #[test]
    fn txt_offers_txt_variants() {
        let f = offer(&two_way(), &[file("a.txt", "text/plain")]);
        let ids: Vec<(&str, usize, &str)> = f
            .groups
            .iter()
            .flat_map(|(g, o)| {
                o.slices
                    .iter()
                    .enumerate()
                    .filter_map(move |(d, s)| s.as_ref().map(|s| (*g, d, s.preset)))
            })
            .collect();
        assert_eq!(
            ids,
            [
                ("document", 0, "markdown"),
                ("document", 2, "html-txt"),
                ("book", 2, "epub-txt"),
            ]
        );
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
        let o = offer(&two_way(), &f).into_first();
        let s = o.slices[4].as_ref().unwrap();
        assert_eq!((s.preset, s.applicable), ("plain-text", 3));
        assert!(o.slices[0].is_none());
        assert_eq!((o.hub.as_str(), o.total), ("3 / 5 files", 5));
    }

    #[test]
    fn offer_tie_uses_preset_order() {
        let f = [file("a.md", "text/markdown"), file("b.txt", "text/plain")];
        let o = offer(&two_way(), &f).into_first();
        assert!(o.slices[0].is_some() && o.slices[4].is_none());
        assert_eq!(o.hub, "1 / 2 files");
    }

    #[test]
    fn offer_unknown_only() {
        let f = [
            ("a.pdf".to_string(), kind(Some("application/pdf"), "PDF")),
            ("b".to_string(), kind(None, "unknown")),
        ];
        let o = offer(&two_way(), &f).into_first();
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
        // Book and Subtitles are on the wheel too, dim: nothing here is a
        // comic book or a subtitle.
        assert_eq!(g.iter().flatten().count(), 4);
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

    /// Two actions with the same input and output types (two code pages),
    /// each named by its own preset.
    fn code_pages() -> Catalog {
        Catalog {
            actions: vec![
                act("cp.a", "text/x-8bit", "text/plain", 10, false),
                act("cp.b", "text/x-8bit", "text/plain", 10, false),
            ],
        }
    }

    fn encoding_catalog() -> Catalog {
        let mut cat = Catalog { actions: vec![] };
        for id in ["text.cp1250-to-utf8", "text.iso88592-to-utf8"] {
            let mut a = act(id, "text/x-8bit", "text/plain", 10, false);
            a.group = "encoding".into();
            cat.actions.push(a);
        }
        cat
    }

    /// An 8-bit text file lights Encoding on the west of the first level;
    /// its wheel has Windows-1250 west and ISO-8859-2 east, both lit.
    #[test]
    fn encoding_group_west_with_two_slices() {
        let cat = encoding_catalog();
        let t = tally(&[("Film.srt", "text/x-8bit")]);
        let g = groups(&cat, &t);
        let enc = g[6].as_ref().unwrap();
        assert_eq!(
            (enc.group, enc.label, enc.lit),
            ("encoding", "Encoding", true)
        );
        let o = offer_group(&cat, &t, "encoding");
        assert_eq!(o.hub, "Encoding");
        let slot = |k: usize| {
            let s = o.slices[k].as_ref().unwrap();
            (s.preset, s.label.as_str(), s.applicable, s.disabled.clone())
        };
        assert_eq!(slot(6), ("cp1250", "Windows-1250", 1, None));
        assert_eq!(slot(2), ("iso88592", "ISO-8859-2", 1, None));
        assert_eq!(o.slices.iter().flatten().count(), 2);
    }

    /// Valid UTF-8 text has nothing to convert: Encoding stays on the wheel
    /// but dim, and neither slice is lit.
    #[test]
    fn utf8_files_offer_no_encoding() {
        let cat = encoding_catalog();
        for (n, m) in [("a.srt", "text/plain"), ("a.txt", "text/markdown")] {
            let t = tally(&[(n, m)]);
            assert!(!groups(&cat, &t)[6].as_ref().unwrap().lit, "{n}");
            let o = offer_group(&cat, &t, "encoding");
            assert!(
                o.slices
                    .iter()
                    .flatten()
                    .all(|s| s.applicable == 0 && s.disabled.is_some()),
                "{n}"
            );
        }
        // And an 8-bit file lights no other group.
        let g = groups(&encoding_catalog(), &tally(&[("a.srt", "text/x-8bit")]));
        assert!(!g[0].as_ref().unwrap().lit && !g[2].as_ref().unwrap().lit);
    }

    /// The six subtitle actions, each in the group its manifest names.
    fn subtitle_catalog() -> Catalog {
        let mut cat = Catalog { actions: vec![] };
        for (id, input, output) in [
            ("subtitles.srt-to-vtt", "application/x-subrip", "text/vtt"),
            ("subtitles.vtt-to-srt", "text/vtt", "application/x-subrip"),
            ("subtitles.ass-to-srt", "text/x-ssa", "application/x-subrip"),
            ("subtitles.ass-to-vtt", "text/x-ssa", "text/vtt"),
            (
                "subtitles.ttml-to-srt",
                "application/ttml+xml",
                "application/x-subrip",
            ),
            ("subtitles.ttml-to-vtt", "application/ttml+xml", "text/vtt"),
        ] {
            let mut a = act(id, input, output, 10, true);
            a.group = "subtitles".into();
            cat.actions.push(a);
        }
        cat
    }

    /// The lit targets of a level-2 wheel: (direction, preset). A dim slice
    /// is drawn but takes no click, so it is not in this list.
    fn lit_targets(o: &Offer) -> Vec<(usize, &'static str)> {
        o.slices
            .iter()
            .enumerate()
            .filter_map(|(k, s)| {
                s.as_ref()
                    .filter(|s| s.disabled.is_none())
                    .map(|s| (k, s.preset))
            })
            .collect()
    }

    /// Subtitles is the south category: lit for every subtitle type.
    #[test]
    fn subtitles_group_south() {
        let g = GROUPS.iter().find(|g| g.id == "subtitles").unwrap();
        assert_eq!((g.label, g.direction), ("Subtitles", 4));
        let cat = subtitle_catalog();
        for (name, mime) in [
            ("Film.srt", "application/x-subrip"),
            ("Film.vtt", "text/vtt"),
            ("Film.ass", "text/x-ssa"),
            ("Film.ttml", "application/ttml+xml"),
        ] {
            let g = groups(&cat, &tally(&[(name, mime)]));
            let s = g[4].as_ref().unwrap();
            assert_eq!((s.group, s.label, s.lit), ("subtitles", "Subtitles", true));
        }
    }

    /// An SRT lights only VTT (north), a VTT only SRT (south), and an ASS
    /// both: SRT from ASS in south, VTT from ASS in north.
    #[test]
    fn srt_offers_only_vtt_vtt_offers_only_srt_ass_offers_both() {
        let cat = subtitle_catalog();
        let srt = offer_group(
            &cat,
            &tally(&[("Film.srt", "application/x-subrip")]),
            "subtitles",
        );
        assert_eq!(lit_targets(&srt), [(0, "vtt-srt")]);
        let s = srt.slices[0].as_ref().unwrap();
        assert_eq!((s.label.as_str(), s.applicable), ("VTT", 1));
        let vtt = offer_group(&cat, &tally(&[("Film.vtt", "text/vtt")]), "subtitles");
        assert_eq!(lit_targets(&vtt), [(4, "srt-vtt")]);
        let s = vtt.slices[4].as_ref().unwrap();
        assert_eq!((s.label.as_str(), s.applicable), ("SRT", 1));
        let ass = offer_group(&cat, &tally(&[("Film.ass", "text/x-ssa")]), "subtitles");
        assert_eq!(lit_targets(&ass), [(0, "vtt-ass"), (4, "srt-ass")]);
        let s = ass.slices[4].as_ref().unwrap();
        assert_eq!((s.label.as_str(), s.applicable), ("SRT", 1));
    }

    /// A TTML file (the classifier's `application/ttml+xml`, whatever its
    /// extension) lights VTT north and SRT south, both with one file.
    #[test]
    fn ttml_offers_srt_south_and_vtt_north() {
        let cat = subtitle_catalog();
        let o = offer_group(
            &cat,
            &tally(&[("Film.xml", "application/ttml+xml")]),
            "subtitles",
        );
        assert_eq!(lit_targets(&o), [(0, "vtt-ttml"), (4, "srt-ttml")]);
        let s = o.slices[0].as_ref().unwrap();
        assert_eq!((s.label.as_str(), s.applicable), ("VTT", 1));
        let s = o.slices[4].as_ref().unwrap();
        assert_eq!((s.label.as_str(), s.applicable), ("SRT", 1));
    }

    /// Every slice of the SRT, VTT and ASS offers, dim ones included, is
    /// what the four older presets give. The TTML presets are appended to
    /// the table, so a dim slice of theirs ties to the earlier preset and
    /// loses the slot: the expected values are the ones of the table before
    /// they were added (a dim slice is "no matching file").
    #[test]
    fn existing_subtitle_offers_are_unchanged() {
        /// (direction, preset, label, applicable, reason) of every slice.
        fn slots(o: &Offer) -> Vec<(usize, &str, &str, usize, Option<&str>)> {
            o.slices
                .iter()
                .enumerate()
                .filter_map(|(k, s)| {
                    s.as_ref().map(|s| {
                        (
                            k,
                            s.preset,
                            s.label.as_str(),
                            s.applicable,
                            s.disabled.as_deref(),
                        )
                    })
                })
                .collect()
        }
        let cat = subtitle_catalog();
        let srt = offer_group(
            &cat,
            &tally(&[("Film.srt", "application/x-subrip")]),
            "subtitles",
        );
        assert_eq!(srt.hub, "Subtitles");
        assert_eq!(
            slots(&srt),
            [
                (0, "vtt-srt", "VTT", 1, None),
                (4, "srt-vtt", "SRT", 0, Some("no matching file")),
            ]
        );
        let vtt = offer_group(&cat, &tally(&[("Film.vtt", "text/vtt")]), "subtitles");
        assert_eq!(
            slots(&vtt),
            [
                (0, "vtt-srt", "VTT", 0, Some("no matching file")),
                (4, "srt-vtt", "SRT", 1, None),
            ]
        );
        let ass = offer_group(&cat, &tally(&[("Film.ass", "text/x-ssa")]), "subtitles");
        assert_eq!(
            slots(&ass),
            [
                (0, "vtt-ass", "VTT", 1, None),
                (4, "srt-ass", "SRT", 1, None),
            ]
        );
    }

    /// UTF-8 text is not a subtitle: Subtitles stays dim and no target
    /// lights, for a `.srt` or `.ttml` the classifier left as plain text.
    #[test]
    fn utf8_text_files_do_not_offer_subtitles() {
        let cat = subtitle_catalog();
        for (n, m) in [
            ("a.srt", "text/plain"),
            ("a.vtt", "text/plain"),
            ("a.ttml", "text/plain"),
            ("a.txt", "text/markdown"),
            ("a.srt", "text/x-8bit"),
        ] {
            let t = tally(&[(n, m)]);
            assert!(!groups(&cat, &t)[4].as_ref().unwrap().lit, "{n}");
            let o = offer_group(&cat, &t, "subtitles");
            assert!(lit_targets(&o).is_empty(), "{n}");
        }
    }

    const fn named(id: &'static str, action: &'static str, direction: u8) -> Preset {
        Preset {
            id,
            input: Some("text/x-8bit"),
            output: "text/plain",
            label: id,
            direction,
            group: "document",
            join: &[],
            action: Some(action),
        }
    }

    /// A preset that names its action runs exactly that one, even where
    /// the planner would pick the other (same types and cost, the id
    /// breaks the tie); a preset without a name still gets the planner's.
    #[test]
    fn preset_runs_its_named_action() {
        let cat = code_pages();
        let t = tally(&[("a.srt", "text/x-8bit"), ("b.srt", "text/x-8bit")]);
        let ran = |p: &Preset| match choose(&cat, p, &t) {
            Some((Choice::Route(r), n)) => (ids(Some(r)), n),
            other => panic!("{}: {other:?}", p.id),
        };
        let some = |v: &[&str]| Some(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(ran(&named("a", "cp.a", 6)), (some(&["cp.a"]), 2));
        assert_eq!(ran(&named("b", "cp.b", 2)), (some(&["cp.b"]), 2));
        let plain = Preset {
            action: None,
            ..named("p", "", 0)
        };
        assert_eq!(ran(&plain), (some(&["cp.a"]), 2));
        assert_eq!(
            ids(route_for(&cat, &named("b", "cp.b", 2), "text/x-8bit")),
            some(&["cp.b"])
        );
        // Both slices are lit, each in its own direction.
        let presets = [named("a", "cp.a", 6), named("b", "cp.b", 2)];
        let o = offer_group_in(GROUPS, &presets, &cat, &t, "document");
        for (d, id) in [(6, "a"), (2, "b")] {
            let s = o.slices[d].as_ref().unwrap();
            assert_eq!(
                (s.preset, s.applicable, s.disabled.as_deref()),
                (id, 2, None)
            );
        }
    }

    /// A named action that is not installed dims its slice with the usual
    /// reason, even when another action of the same types is: the preset
    /// never falls back to a route. Neither does one of other types or a
    /// join that happens to have the name; one whose tool is missing says
    /// what to install.
    #[test]
    fn named_action_missing_dims_the_slice() {
        let t = tally(&[("a.srt", "text/x-8bit")]);
        let dim = |cat: &Catalog, p: &Preset| {
            assert!(choose(cat, p, &t).is_none(), "{}", p.id);
            assert!(route_for(cat, p, "text/x-8bit").is_none(), "{}", p.id);
            let presets = [Preset { ..*p }];
            let o = offer_group_in(GROUPS, &presets, cat, &t, "document");
            let s = o.slices[usize::from(p.direction)].as_ref().unwrap();
            assert_eq!(s.applicable, 1);
            s.disabled.clone()
        };
        let cat = code_pages();
        let gone = named("c", "cp.c", 6);
        assert_eq!(dim(&cat, &gone).as_deref(), Some("no action installed"));
        // Control: with it installed the slice is lit.
        let mut with = code_pages();
        with.actions
            .push(act("cp.c", "text/x-8bit", "text/plain", 10, false));
        assert!(choose(&with, &gone, &t).is_some());
        // The name on an action of other types, or on a join.
        let mut other = code_pages();
        other
            .actions
            .push(act("cp.c", "text/plain", "text/plain", 10, false));
        assert_eq!(dim(&other, &gone).as_deref(), Some("no action installed"));
        let mut join = code_pages();
        let mut j = act("cp.c", "text/x-8bit", "text/plain", 10, false);
        j.join = Some(crate::catalog::Join {
            types: vec!["text/x-8bit".into()],
            min: 1,
            assets: Vec::new(),
        });
        join.actions.push(j);
        assert_eq!(dim(&join, &gone).as_deref(), Some("no action installed"));
        // Installed, but its tool is not.
        let mut tool = code_pages();
        let mut c = act("cp.c", "text/x-8bit", "text/plain", 10, false);
        c.requires = vec!["no-such-tool-inacircle".into()];
        tool.actions.push(c);
        assert_eq!(
            dim(&tool, &gone).as_deref(),
            Some("needs no-such-tool-inacircle")
        );
    }

    #[test]
    fn unknown_group_is_not_shown() {
        let presets = [Preset {
            id: "x",
            input: Some("text/plain"),
            output: "text/markdown",
            label: "X",
            direction: 0,
            group: "nonexistent",
            join: &[],
            action: None,
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
        // html (from Markdown) is dim too; html-txt would take the file but
        // has no action yet, so its slot stays the dim, runnable-less one.
        assert_eq!(o.slices[2].as_ref().unwrap().preset, "html-txt");
        assert_eq!(o.slices.iter().flatten().count(), 3);
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
                PRESETS[i + 1..].iter().all(|b| a.group != b.group
                    || a.input != b.input
                    || a.direction != b.direction),
                "{} shares a direction in its group and input",
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
            (
                pdf.input.unwrap(),
                pdf.output,
                pdf.label,
                pdf.direction,
                pdf.group
            ),
            (
                "application/vnd.comicbook+zip",
                "application/pdf",
                "PDF",
                0,
                "book"
            )
        );
    }

    fn epub_to_txt() -> Catalog {
        let mut a = act(
            "text.epub-to-txt",
            "application/epub+zip",
            "text/plain",
            10,
            true,
        );
        a.group = "document".into();
        Catalog { actions: vec![a] }
    }

    /// An EPUB lights Document, whose south slice is plain text; the
    /// north (Markdown) and east (HTML) slices stay dim.
    #[test]
    fn epub_offers_plain_text_south() {
        let cat = epub_to_txt();
        let t = tally(&[("b.epub", "application/epub+zip")]);
        let g = groups(&cat, &t);
        let doc = g[0].as_ref().unwrap();
        assert_eq!((doc.group, doc.lit), ("document", true));
        let o = offer_group(&cat, &t, "document");
        assert_eq!(o.hub, "Document");
        let pt = o.slices[4].as_ref().unwrap();
        assert_eq!(
            (
                pt.preset,
                pt.label.as_str(),
                pt.applicable,
                pt.disabled.as_deref()
            ),
            ("plain-text-epub", "Plain text", 1, None)
        );
        // North (Markdown) fits an EPUB but no action is installed here, so
        // it is dim too; east (HTML) fits nothing.
        let n = o.slices[0].as_ref().unwrap();
        assert_eq!(
            (n.preset, n.applicable, n.disabled.as_deref()),
            ("markdown-epub", 1, Some("no action installed"))
        );
        let e = o.slices[2].as_ref().unwrap();
        assert!(e.disabled.is_some(), "east is dim");
        assert_eq!(e.applicable, 0);
        assert_eq!(o.slices.iter().flatten().count(), 3);
    }

    fn epub_to_md_and_txt() -> Catalog {
        let mut md = act(
            "text.epub-to-md",
            "application/epub+zip",
            "text/markdown",
            10,
            false,
        );
        md.group = "document".into();
        let mut cat = epub_to_txt();
        cat.actions.push(md);
        cat
    }

    /// An EPUB lights Document with both text slices: Markdown north and
    /// Plain text south, both lit; the east (HTML) slice stays dim.
    #[test]
    fn epub_offers_markdown_north() {
        let cat = epub_to_md_and_txt();
        let t = tally(&[("b.epub", "application/epub+zip")]);
        assert!(groups(&cat, &t)[0].as_ref().unwrap().lit);
        assert!(!groups(&cat, &t)[2].as_ref().unwrap().lit, "Book stays dim");
        let o = offer_group(&cat, &t, "document");
        let slot = |k: usize| {
            let s = o.slices[k].as_ref().unwrap();
            (s.preset, s.label.as_str(), s.applicable, s.disabled.clone())
        };
        assert_eq!(slot(0), ("markdown-epub", "Markdown", 1, None));
        assert_eq!(slot(4), ("plain-text-epub", "Plain text", 1, None));
        let east = o.slices[2].as_ref().unwrap();
        assert!(east.applicable == 0 && east.disabled.is_some(), "east dim");
        // The flat offer names both slices for the same file.
        let flat = offer(&cat, &[file("b.epub", "application/epub+zip")]).into_first();
        assert_eq!(flat.slices[0].as_ref().unwrap().preset, "markdown-epub");
        assert_eq!(flat.slices[4].as_ref().unwrap().preset, "plain-text-epub");
    }

    /// EPUB and plain text share Document's north slot: a tie goes to the
    /// earlier preset (plain text to Markdown), more EPUBs take it, at both
    /// levels.
    #[test]
    fn epub_and_txt_share_the_north_slot_by_count_then_order() {
        let cat = Catalog {
            actions: vec![
                act("txt-md", "text/plain", "text/markdown", 1, false),
                act("epub-md", "application/epub+zip", "text/markdown", 1, false),
            ],
        };
        let txt = file("a.txt", "text/plain");
        let ep = |n: &str| file(n, "application/epub+zip");
        for (files, want) in [
            (vec![ep("a.epub"), txt.clone()], "markdown"),
            (
                vec![ep("a.epub"), ep("b.epub"), txt.clone()],
                "markdown-epub",
            ),
        ] {
            let mut t = Tally::default();
            for (n, k) in &files {
                t.add(n, k);
            }
            let wheel = offer_group(&cat, &t, "document").slices[0]
                .as_ref()
                .unwrap()
                .preset;
            let flat = offer(&cat, &files);
            let flat_north = flat
                .groups
                .iter()
                .find(|(g, _)| *g == "document")
                .unwrap()
                .1
                .slices[0]
                .as_ref()
                .unwrap()
                .preset;
            assert_eq!(wheel, want);
            assert_eq!(flat_north, want);
        }
    }

    /// Nothing in Book applies to an EPUB: the group stays dim.
    #[test]
    fn book_group_has_nothing_for_epub() {
        let cat = epub_to_txt();
        let t = tally(&[("b.epub", "application/epub+zip")]);
        let g = groups(&cat, &t);
        assert!(!g[2].as_ref().unwrap().lit, "Book stays dim");
        let o = offer_group(&cat, &t, "book");
        assert!(o.slices.iter().flatten().all(|s| s.applicable == 0));
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
        // The CBZ slice is on the wheel too, dim: a comic book is not an image.
        assert_eq!(o.slices.iter().flatten().count(), 3);
        assert_eq!(o.slices[4].as_ref().unwrap().applicable, 0);
        // The flat offer for the same file offers it too.
        let o = offer(
            &cat,
            &[(
                "x.cbz".into(),
                kind(Some("application/vnd.comicbook+zip"), "Comic book"),
            )],
        )
        .into_first();
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
                assets: Vec::new(),
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

    /// The catalog of the real pages family: both joins and the CBZ reader.
    fn book_catalog() -> Catalog {
        let mut cat = pdf_catalog(true);
        let mut j = act("pages.images-to-cbz", "image/jpeg", CBZ, 10, false);
        j.group = "book".into();
        j.batch = true;
        j.join = Some(crate::catalog::Join {
            types: vec!["image/jpeg".into(), "image/png".into()],
            min: 1,
            assets: Vec::new(),
        });
        cat.actions.push(j);
        cat
    }

    #[test]
    fn cbz_preset_offers_join_for_images() {
        let cat = book_catalog();
        let t = tally_of(&[
            many("image/jpeg", 2, "jpg"),
            many("image/png", 1, "png"),
            many("text/plain", 1, "txt"),
        ]);
        let o = offer_group(&cat, &t, "book");
        let cbz = o.slices[4].as_ref().expect("cbz points south");
        let pdf = o.slices[0].as_ref().expect("pdf points north");
        assert_eq!(
            (
                cbz.preset,
                cbz.label.as_str(),
                cbz.applicable,
                cbz.disabled.as_deref()
            ),
            ("cbz", "CBZ", 3, None)
        );
        assert_eq!(
            (pdf.preset, pdf.applicable, pdf.disabled.as_deref()),
            ("pdf", 3, None)
        );
        assert_eq!(o.hub, "3 / 4 files");
        // Only a comic book: nothing to join, the CBZ slice is dim.
        let t = tally(&[("x.cbz", CBZ)]);
        let o = offer_group(&cat, &t, "book");
        let cbz = o.slices[4].as_ref().unwrap();
        assert_eq!(cbz.applicable, 0);
        assert!(cbz.disabled.is_some());
        let pdf = o.slices[0].as_ref().unwrap();
        assert_eq!((pdf.applicable, pdf.disabled.as_deref()), (1, None));
    }

    #[test]
    fn presets_without_one_to_one_never_route() {
        let cat = book_catalog();
        let cbz = PRESETS.iter().find(|p| p.id == "cbz").unwrap();
        assert!(cbz.input.is_none());
        for t in [
            tally_of(&[many("image/png", 3, "png")]),
            tally_of(&[many(CBZ, 2, "cbz"), many("image/jpeg", 1, "jpg")]),
            tally_of(&[many("text/plain", 2, "txt")]),
            Tally::default(),
        ] {
            assert!(
                !matches!(choose(&cat, cbz, &t), Some((Choice::Route(_), _))),
                "{t:?}"
            );
        }
        // Without a join action it cannot run at all.
        let t = tally_of(&[many("image/png", 3, "png")]);
        assert!(choose(&pdf_catalog(false), cbz, &t).is_none());
    }

    #[test]
    fn images_offer_pdf_in_book() {
        let cat = pdf_catalog(true);
        let t = tally_of(&[many("image/jpeg", 10, "jpg"), many("image/png", 12, "png")]);
        let g = groups(&cat, &t);
        assert!(g[2].as_ref().unwrap().lit, "Book lights");
        let o = offer_group(&cat, &t, "book");
        assert_eq!(o.hub, "Book", "all files apply");
        let s = pdf_slice(&o);
        assert_eq!(
            (s.preset, s.applicable, s.disabled.as_deref()),
            ("pdf", 22, None)
        );
        let o = offer_tally(&cat, &t).into_first();
        assert_eq!(pdf_slice(&o).applicable, 22);
        assert_eq!(o.hub, "22 / 22 files");
    }

    #[test]
    fn single_image_offers_pdf() {
        let cat = pdf_catalog(true);
        let t = tally(&[("kep.png", "image/png")]);
        let o = offer_tally(&cat, &t).into_first();
        let s = pdf_slice(&o);
        assert_eq!(
            (s.preset, s.applicable, s.disabled.as_deref()),
            ("pdf", 1, None)
        );
        assert_eq!(o.hub, "kep.png");
        let o = offer_group(&cat, &t, "book");
        assert_eq!(pdf_slice(&o).applicable, 1);
        assert_eq!(o.hub, "Book", "one file keeps the label");
    }

    #[test]
    fn target_hub_counts_files_when_not_all_apply() {
        let cat = Catalog {
            actions: [pdf_catalog(true).actions, two_way().actions].concat(),
        };
        let t = tally_of(&[many("image/png", 5, "png"), many("text/plain", 1, "txt")]);
        assert_eq!(offer_group(&cat, &t, "book").hub, "5 / 6 files");
        let t = tally_of(&[many("image/png", 3, "png")]);
        assert_eq!(offer_group(&cat, &t, "book").hub, "Book");
        // Nothing in the category can run: the label stays.
        let t = tally_of(&[
            many("image/png", 2, "png"),
            many("application/zip", 1, "zip"),
        ]);
        let none = pdf_catalog(false);
        assert_eq!(offer_group(&none, &t, "book").hub, "Book");
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
            let o = offer_tally(&cat, &t).into_first();
            assert_eq!(pdf_slice(&o).applicable, n);
            assert_eq!(o.hub, format!("{n} / 8 files"));
            let o = offer_group(&cat, &t, "book");
            assert_eq!(pdf_slice(&o).applicable, n);
            assert_eq!(o.hub, format!("{n} / 8 files"));
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
        assert_eq!(o.hub, "20 / 22 files");
        let o = offer_tally(&cat, &t).into_first();
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
        let o = offer_tally(&cat, &t).into_first();
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
        let o = offer_tally(&cat, &t).into_first();
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

    const EPUB: &str = "application/epub+zip";
    const MD: &str = "text/markdown";

    /// The book catalog with the text family's EPUB actions: the 1:1 forms
    /// from Markdown and plain text, and the Markdown join with images.
    fn epub_catalog() -> Catalog {
        let mut cat = book_catalog();
        for (id, input) in [("text.md-to-epub", MD), ("text.txt-to-epub", "text/plain")] {
            let mut a = act(id, input, EPUB, 10, true);
            a.group = "book".into();
            cat.actions.push(a);
        }
        let mut j = act("text.md-join-epub", MD, EPUB, 10, true);
        j.group = "book".into();
        j.batch = true;
        j.join = Some(crate::catalog::Join {
            types: vec![MD.into()],
            min: 1,
            assets: vec!["image/jpeg".into(), "image/png".into()],
        });
        cat.actions.push(j);
        cat
    }

    fn epub_preset() -> &'static Preset {
        PRESETS.iter().find(|p| p.id == "epub").unwrap()
    }

    /// The id of what the `epub` preset runs and for how many files.
    fn epub_choice(cat: &Catalog, t: &Tally) -> Option<(String, usize)> {
        choose(cat, epub_preset(), t).map(|(c, n)| match c {
            Choice::Route(r) => (r[0].id.clone(), n),
            Choice::Join(a) => (a.id.clone(), n),
        })
    }

    /// The Book wheel's east slot (EPUB).
    fn epub_slice(o: &Offer) -> &Slice {
        o.slices[2].as_ref().unwrap()
    }

    #[test]
    fn join_with_assets_wins_with_two_docs() {
        let cat = epub_catalog();
        // A tie on the files taken: the explicit rule, not the tie, decides.
        let t = tally_of(&[many(MD, 2, "md")]);
        assert_eq!(
            epub_choice(&cat, &t),
            Some(("text.md-join-epub".to_owned(), 2))
        );
        let o = offer_group(&cat, &t, "book");
        let s = epub_slice(&o);
        assert_eq!(
            (s.preset, s.applicable, s.disabled.as_deref()),
            ("epub", 2, None)
        );
        assert_eq!(o.hub, "Book", "all files apply");
    }

    #[test]
    fn join_with_assets_wins_with_doc_and_image() {
        let cat = epub_catalog();
        let t = tally_of(&[many(MD, 1, "md"), many("image/png", 1, "png")]);
        assert_eq!(
            epub_choice(&cat, &t),
            Some(("text.md-join-epub".to_owned(), 2))
        );
        let t = tally_of(&[many(MD, 1, "md"), many("image/jpeg", 4, "jpg")]);
        assert_eq!(
            epub_choice(&cat, &t),
            Some(("text.md-join-epub".to_owned(), 5))
        );
        let o = offer_group(&cat, &t, "book");
        assert_eq!(epub_slice(&o).applicable, 5);
        // The plan-10 joins are untouched: the four images still join.
        let s = pdf_slice(&o);
        assert_eq!((s.applicable, s.disabled.as_deref()), (4, None));
    }

    #[test]
    fn one_doc_alone_takes_the_one_to_one() {
        let cat = epub_catalog();
        let t = tally(&[("notes.md", MD)]);
        assert_eq!(
            epub_choice(&cat, &t),
            Some(("text.md-to-epub".to_owned(), 1))
        );
        // A file of no asset type beside it changes nothing.
        let t = tally_of(&[many(MD, 1, "md"), many("text/plain", 1, "txt")]);
        assert_eq!(
            epub_choice(&cat, &t),
            Some(("text.md-to-epub".to_owned(), 1))
        );
        // Without the 1:1 action, the join runs the one document.
        let mut cat = cat;
        cat.actions.retain(|a| a.id != "text.md-to-epub");
        let t = tally(&[("notes.md", MD)]);
        assert_eq!(
            epub_choice(&cat, &t),
            Some(("text.md-join-epub".to_owned(), 1))
        );
    }

    #[test]
    fn images_alone_offer_no_epub() {
        let cat = epub_catalog();
        let t = tally_of(&[many("image/png", 3, "png"), many("image/jpeg", 2, "jpg")]);
        assert!(choose(&cat, epub_preset(), &t).is_none());
        let o = offer_group(&cat, &t, "book");
        let s = epub_slice(&o);
        assert_eq!(
            (s.applicable, s.disabled.as_deref()),
            (0, Some("no matching file"))
        );
        // PDF and CBZ stay.
        assert_eq!(pdf_slice(&o).applicable, 5);
        assert_eq!(o.slices[4].as_ref().unwrap().applicable, 5);
        assert_eq!(o.hub, "Book");
        // The flat offer has no EPUB slice at all.
        let f = offer_tally(&cat, &t);
        assert!(
            f.groups
                .iter()
                .flat_map(|(_, o)| o.slices.iter().flatten())
                .all(|s| s.preset != "epub" && s.preset != "epub-txt")
        );
        assert_eq!(f.hub, "5 / 5 files");
    }

    #[test]
    fn hub_counts_docs_and_assets() {
        let cat = Catalog {
            actions: [epub_catalog().actions, two_way().actions].concat(),
        };
        let t = tally_of(&[
            many(MD, 2, "md"),
            many("image/png", 3, "png"),
            many("text/plain", 1, "txt"),
            many(CBZ, 1, "cbz"),
        ]);
        let o = offer_group(&cat, &t, "book");
        let s = epub_slice(&o);
        assert_eq!(
            (s.preset, s.applicable, s.disabled.as_deref()),
            ("epub", 5, None)
        );
        assert_eq!(o.hub, "5 / 7 files");
        let f = offer_tally(&cat, &t);
        assert_eq!(f.hub, "5 / 7 files");
        let book = &f.groups.iter().find(|(g, _)| *g == "book").unwrap().1;
        assert_eq!(epub_slice(book).preset, "epub");
        assert_eq!(epub_slice(book).applicable, 5);
    }

    /// `min=` counts documents: images never make up the minimum.
    #[test]
    fn min_counts_documents() {
        let mut cat = epub_catalog();
        let j = cat.actions.iter_mut().find(|a| a.id == "text.md-join-epub");
        j.unwrap().join.as_mut().unwrap().min = 2;
        let t = tally_of(&[many(MD, 1, "md"), many("image/png", 5, "png")]);
        // The join cannot run; the one document still has its 1:1 form.
        assert_eq!(
            epub_choice(&cat, &t),
            Some(("text.md-to-epub".to_owned(), 1))
        );
        cat.actions.retain(|a| a.id != "text.md-to-epub");
        assert!(choose(&cat, epub_preset(), &t).is_none());
        let o = offer_group(&cat, &t, "book");
        let s = epub_slice(&o);
        assert_eq!(
            (s.applicable, s.disabled.as_deref()),
            (6, Some("needs more files"))
        );
        let t = tally_of(&[many(MD, 2, "md"), many("image/png", 5, "png")]);
        assert_eq!(
            epub_choice(&cat, &t),
            Some(("text.md-join-epub".to_owned(), 7))
        );
    }

    #[test]
    fn offer_empty() {
        let o = offer(&two_way(), &[]).into_first();
        assert!(o.slices.iter().all(Option::is_none));
        assert_eq!(o.total, 0);
    }
}
