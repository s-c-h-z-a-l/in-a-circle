//! Checks `wire::op` and `wire::event_fds` against the protocol XMLs in
//! `tests/data/`, with a small scanner for the tags that matter
//! (`interface`, `request`, `event`, `arg`) in document order.

use std::collections::BTreeMap;

use wire::Iface;
use wire::proto::Dir;

#[derive(Default)]
struct Interface {
    requests: Vec<String>,
    /// Name and number of fd arguments.
    events: Vec<(String, usize)>,
}

/// The value of attribute `name` in the tag body `tag`.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let at = tag.find(&format!(" {name}=\""))? + name.len() + 3;
    Some(&tag[at..at + tag[at..].find('"')?])
}

/// Index of the `>` closing the tag `s` starts with; quoted attribute
/// values may contain `>`.
fn tag_end(s: &str) -> Option<usize> {
    let mut quoted = false;
    s.char_indices().find_map(|(i, c)| match c {
        '"' => {
            quoted = !quoted;
            None
        }
        '>' if !quoted => Some(i),
        _ => None,
    })
}

fn scan(xml: &str, out: &mut BTreeMap<String, Interface>) {
    let mut rest = xml;
    let mut iface: Option<String> = None;
    let mut in_event = false;
    while let Some(i) = rest.find('<') {
        rest = &rest[i..];
        if let Some(r) = rest.strip_prefix("<!--") {
            rest = &r[r.find("-->").expect("unterminated comment") + 3..];
            continue;
        }
        let end = tag_end(rest).expect("unterminated tag");
        let tag = &rest[1..end];
        rest = &rest[end + 1..];
        let kind = tag.split_whitespace().next().unwrap_or("");
        match kind {
            "interface" => {
                let name = attr(tag, "name").expect("interface name").to_string();
                assert!(!out.contains_key(&name), "duplicate {name}");
                out.insert(name.clone(), Interface::default());
                iface = Some(name);
            }
            "request" | "event" => {
                let i = out.get_mut(iface.as_ref().expect("message outside interface"));
                let i = i.unwrap();
                let name = attr(tag, "name").expect("message name").to_string();
                in_event = kind == "event";
                if in_event {
                    i.events.push((name, 0));
                } else {
                    i.requests.push(name);
                }
            }
            "arg" if in_event && attr(tag, "type") == Some("fd") => {
                let i = out.get_mut(iface.as_ref().unwrap()).unwrap();
                i.events.last_mut().unwrap().1 += 1;
            }
            "/request" | "/event" => in_event = false,
            _ => {}
        }
    }
}

fn protocols() -> BTreeMap<String, Interface> {
    let mut out = BTreeMap::new();
    for f in [
        "wayland.xml",
        "viewporter.xml",
        "wlr-layer-shell-unstable-v1.xml",
    ] {
        let path = format!("{}/tests/data/{f}", env!("CARGO_MANIFEST_DIR"));
        scan(&std::fs::read_to_string(&path).unwrap(), &mut out);
    }
    out
}

#[test]
fn scanner_reads_documents_in_order() {
    let mut out = BTreeMap::new();
    scan(
        r#"<protocol><interface name="a" version="1">
            <!-- <request name="commented"/> -->
            <request name="r0"><arg name="x" type="fd"/></request>
            <event name="e0"><arg name="fd" type="fd" summary="a > b"/></event>
            <request name="r1" type="destructor"/>
            <event name="e1"><arg name="y" type="uint"/></event>
        </interface></protocol>"#,
        &mut out,
    );
    let a = &out["a"];
    assert_eq!(a.requests, ["r0", "r1"]);
    assert_eq!(a.events, [("e0".to_string(), 1), ("e1".to_string(), 0)]);
}

#[test]
fn opcodes_match_the_protocol_xmls() {
    let xml = protocols();
    let mut covered: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for &(iface, dir, name, op) in wire::op::ALL {
        let i = xml
            .get(iface)
            .unwrap_or_else(|| panic!("{iface} not in the XMLs"));
        let want = match dir {
            Dir::Request => i.requests.get(op as usize).map(String::as_str),
            Dir::Event => i.events.get(op as usize).map(|e| e.0.as_str()),
        };
        assert_eq!(
            want,
            Some(name.to_lowercase().as_str()),
            "{iface} {dir:?} {name} = {op}"
        );
        let c = covered.entry(iface).or_default();
        match dir {
            Dir::Request => c.0 += 1,
            Dir::Event => c.1 += 1,
        }
    }
    // Every interface with a vendored XML is listed in full, so a missing
    // constant is caught here rather than written by hand later.
    for iface in Iface::ALL.iter().copied() {
        let Some(i) = xml.get(iface.name()) else {
            continue;
        };
        assert_eq!(
            covered.get(iface.name()).copied(),
            Some((i.requests.len(), i.events.len())),
            "{} not listed in full",
            iface.name()
        );
    }
}

/// Every interface with opcodes is an `Iface` whose name the XMLs know, so a
/// typo in `Iface::name` cannot hide behind the "no vendored XML" skip.
#[test]
fn every_op_interface_resolves_under_its_iface_name() {
    let xml = protocols();
    for &(iface, ..) in wire::op::ALL {
        let named = Iface::ALL.iter().find(|i| i.name() == iface);
        let named = named.unwrap_or_else(|| panic!("{iface}: no Iface has this name"));
        assert!(xml.contains_key(named.name()), "{iface} not in the XMLs");
    }
}

#[test]
fn fd_carrying_events_match_the_protocol_xmls() {
    let xml = protocols();
    for iface in Iface::ALL.iter().copied() {
        let Some(i) = xml.get(iface.name()) else {
            continue;
        };
        for (op, (name, fds)) in i.events.iter().enumerate() {
            assert_eq!(
                wire::event_fds(iface, op as u16),
                *fds,
                "{}.{name}",
                iface.name()
            );
        }
    }
}
