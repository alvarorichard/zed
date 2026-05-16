use std::hash::{Hash, Hasher};
use std::ops::Range;

use collections::{FxHasher, HashMap};
use gpui::SharedString;
use html5ever::{Attribute, LocalName, ParseOpts, local_name, parse_document, tendril::TendrilSink};
use markup5ever_rcdom::{Node, NodeData, RcDom};

use crate::parser::MarkdownEvent;

/// Result of scanning a single HTML block that may open or close a `<details>` widget.
pub(crate) enum DisclosureScan {
    /// Block opens (and possibly closes inline) a `<details>` element.
    Open {
        open: bool,
        anchor: SharedString,
        summary_events: Vec<(Range<usize>, MarkdownEvent)>,
        /// True when the same block also contained the matching `</details>` —
        /// no further pairing required, body events come from any markdown nested
        /// inside the same HTML block (rare; we treat the block as self-contained).
        self_closing: bool,
    },
    /// Block consists solely of `</details>` (possibly with surrounding whitespace).
    Close,
}

/// Scan an HTML block's source for a `<details>` / `</details>` boundary.
/// Returns `None` if the block isn't a disclosure boundary — callers should
/// leave it untouched so it renders through the normal HTML pipeline.
pub(crate) fn scan_disclosure_tag(block: &str, range: Range<usize>) -> Option<DisclosureScan> {
    let trimmed = block.trim();
    if is_pure_close(trimmed) {
        return Some(DisclosureScan::Close);
    }
    if !starts_with_details_open(trimmed) {
        return None;
    }

    let dom = parse_document(RcDom::default(), ParseOpts::default())
        .from_utf8()
        .read_from(&mut std::io::Cursor::new(block.as_bytes()))
        .ok()?;

    let details = find_first_element(&dom.document, local_name!("details"))?;
    let (open, anchor) = read_details_attrs(&details, block);

    let mut summary_events = Vec::new();
    if let Some(summary) = find_first_element(&details, local_name!("summary")) {
        collect_summary_events(&summary, range, &mut summary_events);
    }

    let self_closing = block.contains("</details>");

    Some(DisclosureScan::Open {
        open,
        anchor,
        summary_events,
        self_closing,
    })
}

fn is_pure_close(trimmed: &str) -> bool {
    let lower = trimmed.to_ascii_lowercase();
    lower == "</details>" || lower.starts_with("</details>") && lower[10..].trim().is_empty()
}

fn starts_with_details_open(trimmed: &str) -> bool {
    let bytes = trimmed.as_bytes();
    if !bytes.starts_with(b"<") || bytes.len() < 9 {
        return false;
    }
    trimmed[..9].eq_ignore_ascii_case("<details>")
        || trimmed[..9].eq_ignore_ascii_case("<details ")
        || trimmed[..9].eq_ignore_ascii_case("<details\t")
        || trimmed[..9].eq_ignore_ascii_case("<details\n")
}

fn find_first_element(node: &Node, target: LocalName) -> Option<std::rc::Rc<Node>> {
    for child in node.children.borrow().iter() {
        if let NodeData::Element { name, .. } = &child.data {
            if name.local == target {
                return Some(child.clone());
            }
        }
        if let Some(found) = find_first_element(child, target.clone()) {
            return Some(found);
        }
    }
    None
}

fn read_details_attrs(node: &Node, source_for_anchor: &str) -> (bool, SharedString) {
    let NodeData::Element { attrs, .. } = &node.data else {
        return (false, default_anchor(source_for_anchor));
    };
    let attrs = attrs.borrow();
    let open = attr_present(&attrs, local_name!("open"));
    let anchor = attr_value(&attrs, local_name!("id"))
        .map(SharedString::from)
        .unwrap_or_else(|| default_anchor(source_for_anchor));
    (open, anchor)
}

fn attr_present(attrs: &[Attribute], name: LocalName) -> bool {
    attrs.iter().any(|a| a.name.local == name)
}

fn attr_value(attrs: &[Attribute], name: LocalName) -> Option<String> {
    attrs.iter().find_map(|a| {
        if a.name.local == name {
            Some(a.value.to_string())
        } else {
            None
        }
    })
}

/// Stable per-disclosure identifier derived from the trimmed open-tag text.
/// Editing the body of a disclosure does not change the anchor, so toggle
/// state survives interactive edits. Editing the open tag itself produces
/// a new anchor — semantically a different widget.
pub(crate) fn default_anchor(source: &str) -> SharedString {
    let mut hasher = FxHasher::default();
    let key = open_tag_only(source);
    key.trim().hash(&mut hasher);
    SharedString::from(format!("__details_{:x}", hasher.finish()))
}

fn open_tag_only(source: &str) -> &str {
    let trimmed = source.trim_start();
    if let Some(end) = trimmed.find('>') {
        &trimmed[..=end]
    } else {
        trimmed
    }
}

fn collect_summary_events(
    summary: &Node,
    range: Range<usize>,
    out: &mut Vec<(Range<usize>, MarkdownEvent)>,
) {
    use pulldown_cmark::TagEnd;

    use crate::parser::MarkdownTag;

    fn walk(
        node: &Node,
        range: Range<usize>,
        out: &mut Vec<(Range<usize>, MarkdownEvent)>,
    ) {
        match &node.data {
            NodeData::Text { contents } => {
                let text = contents.borrow().to_string();
                if !text.is_empty() {
                    out.push((
                        range,
                        MarkdownEvent::SubstitutedText(text),
                    ));
                }
            }
            NodeData::Element { name, .. } => {
                let local = &name.local;
                let (open_event, close_tag) = if *local == local_name!("b")
                    || *local == local_name!("strong")
                {
                    (
                        Some(MarkdownEvent::Start(MarkdownTag::Strong)),
                        Some(TagEnd::Strong),
                    )
                } else if *local == local_name!("i") || *local == local_name!("em") {
                    (
                        Some(MarkdownEvent::Start(MarkdownTag::Emphasis)),
                        Some(TagEnd::Emphasis),
                    )
                } else if *local == local_name!("del") || *local == local_name!("s") {
                    (
                        Some(MarkdownEvent::Start(MarkdownTag::Strikethrough)),
                        Some(TagEnd::Strikethrough),
                    )
                } else {
                    (None, None)
                };

                if let Some(event) = open_event {
                    out.push((range.clone(), event));
                }
                for child in node.children.borrow().iter() {
                    walk(child, range.clone(), out);
                }
                if let Some(tag_end) = close_tag {
                    out.push((range, MarkdownEvent::End(tag_end)));
                }
            }
            _ => {
                for child in node.children.borrow().iter() {
                    walk(child, range.clone(), out);
                }
            }
        }
    }

    for child in summary.children.borrow().iter() {
        walk(child, range.clone(), out);
    }
}

/// Per-`Markdown` runtime state tracking expand/collapse of each disclosure.
#[derive(Default, Clone)]
pub struct DisclosureState {
    expanded: HashMap<SharedString, bool>,
}

impl DisclosureState {
    pub fn is_expanded(&self, anchor: &SharedString, default_open: bool) -> bool {
        self.expanded
            .get(anchor)
            .copied()
            .unwrap_or(default_open)
    }

    pub fn toggle(&mut self, anchor: SharedString, default_open: bool) {
        let entry = self.expanded.entry(anchor).or_insert(default_open);
        *entry = !*entry;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_is_stable_across_body_edits() {
        let a1 = default_anchor("<details>\nbody one\n</details>");
        let a2 = default_anchor("<details>\nbody two with completely different content\n</details>");
        assert_eq!(a1, a2);
    }

    #[test]
    fn anchor_differs_when_open_tag_differs() {
        let closed = default_anchor("<details>");
        let opened = default_anchor("<details open>");
        assert_ne!(closed, opened);
    }

    #[test]
    fn scan_close_block() {
        let result = scan_disclosure_tag("</details>\n", 0..11);
        assert!(matches!(result, Some(DisclosureScan::Close)));
    }

    #[test]
    fn scan_open_with_summary() {
        let block = "<details open>\n<summary>Title</summary>\n";
        let scan = scan_disclosure_tag(block, 0..block.len()).expect("recognised");
        let DisclosureScan::Open {
            open,
            summary_events,
            ..
        } = scan
        else {
            panic!("expected open");
        };
        assert!(open);
        // Title text should appear in events
        assert!(summary_events.iter().any(|(_, ev)| matches!(
            ev,
            MarkdownEvent::SubstitutedText(t) if t.contains("Title")
        )));
    }

    #[test]
    fn scan_ignores_non_details() {
        assert!(scan_disclosure_tag("<div></div>", 0..11).is_none());
    }
}
