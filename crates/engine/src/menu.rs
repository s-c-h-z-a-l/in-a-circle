//! Built-in presets, the route planner and the wheel offer.

use crate::catalog::{Action, Catalog, missing_tools};
use crate::classify::Kind;

/// A fixed wheel slot. `direction`: 0 = north, clockwise in 45 degree steps.
pub struct Preset {
    pub id: &'static str,
    pub input: &'static str,
    pub output: &'static str,
    pub label: &'static str,
    pub direction: u8,
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "markdown",
        input: "text/plain",
        output: "text/markdown",
        label: "Markdown",
        direction: 0,
    },
    Preset {
        id: "plain-text",
        input: "text/markdown",
        output: "text/plain",
        label: "Plain text",
        direction: 4,
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

/// The wheel for a selection: presets of the most common file type, each in
/// its fixed direction.
pub fn offer(cat: &Catalog, files: &[(String, Kind)]) -> Offer {
    let total = files.len();
    let mut slices: [Option<Slice>; 8] = Default::default();
    let count = |mime: &str| {
        files
            .iter()
            .filter(|(_, k)| k.mime.as_deref() == Some(mime))
            .count()
    };
    // Most common type that has presets; ties: first preset order.
    let mut best: Option<(&str, usize)> = None;
    for p in PRESETS {
        let n = count(p.input);
        if n > 0 && best.is_none_or(|(_, m)| n > m) {
            best = Some((p.input, n));
        }
    }
    let Some((mime, applicable)) = best else {
        let hub = match files.first() {
            Some((_, k)) => format!("no action for {}", k.label),
            None => String::new(),
        };
        return Offer { slices, hub, total };
    };
    for p in PRESETS.iter().filter(|p| p.input == mime) {
        let disabled = if route(cat, p.input, p.output).is_some() {
            None
        } else if let Some(r) = plan(cat, p.input, p.output, |_| true) {
            let tool = r.iter().find_map(|a| missing_tools(a).into_iter().next());
            Some(match tool {
                Some(t) => format!("needs {t}"),
                None => "no action installed".into(),
            })
        } else {
            Some("no action installed".into())
        };
        slices[usize::from(p.direction)] = Some(Slice {
            preset: p.id,
            label: p.label.into(),
            applicable,
            disabled,
        });
    }
    let hub = if total == 1 {
        files[0].0.clone()
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

    #[test]
    fn offer_empty() {
        let o = offer(&two_way(), &[]);
        assert!(o.slices.iter().all(Option::is_none));
        assert_eq!(o.total, 0);
    }
}
