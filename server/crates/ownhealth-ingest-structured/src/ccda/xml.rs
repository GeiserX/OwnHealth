//! Reading a C-CDA file safely, and the small tree helpers the converter uses.
//!
//! The file comes from outside (a portal download, an email), so the parser takes no chances:
//! - a file over [`MAX_BYTES`] is refused before it is read (the import checks the stored size first);
//! - a DOCTYPE is refused (roxmltree's default), so no entity is ever declared: billion laughs and
//!   external entities cannot start;
//! - the tree holds at most [`MAX_NODES`] nodes and [`MAX_DEPTH`] levels;
//! - the parser and the depth check walk the tree in a loop, they never recurse.

use std::collections::HashMap;

use ownhealth_core::{Error, Result};
use roxmltree::{Document, Node, ParsingOptions};

/// The CDA namespace every C-CDA element is in.
pub const NS: &str = "urn:hl7-org:v3";
const XSI: &str = "http://www.w3.org/2001/XMLSchema-instance";

/// The largest C-CDA file read. Portal downloads with long narrative run to a few MB.
pub const MAX_BYTES: usize = 32 * 1024 * 1024;
/// The most nodes (elements, text, comments) one file may hold; about 100 MB of tree at most.
pub const MAX_NODES: u32 = 2_000_000;
/// The deepest element nesting accepted, the root element being level 1. Real C-CDA stays under 40.
pub const MAX_DEPTH: usize = 256;
/// The longest text read out of one narrative node, in characters.
const MAX_TEXT: usize = 2_000;
/// The most nodes visited to read one narrative text.
const MAX_TEXT_NODES: usize = 20_000;

fn refused(why: impl Into<String>) -> Error {
    Error::Invalid(why.into())
}

/// The refusal of a file of `size` bytes, over [`MAX_BYTES`].
pub fn too_large(size: u64) -> Error {
    refused(format!(
        "the file is {} MB, over the {} MB a C-CDA document may have",
        size / (1024 * 1024),
        MAX_BYTES / (1024 * 1024)
    ))
}

/// The file as text: UTF-8 (a byte order mark dropped), or Latin-1 when it is not UTF-8.
pub fn text_of_file(bytes: &[u8]) -> Result<std::borrow::Cow<'_, str>> {
    if bytes.len() > MAX_BYTES {
        return Err(too_large(bytes.len() as u64));
    }
    Ok(ownhealth_core::documents::decode_text(bytes))
}

/// Parse a C-CDA document: a `ClinicalDocument` root in the CDA namespace, within the limits above.
pub fn parse(text: &str) -> Result<Document<'_>> {
    let opt = ParsingOptions {
        allow_dtd: false,
        nodes_limit: MAX_NODES,
        ..ParsingOptions::default()
    };
    let doc = Document::parse_with_options(text, opt).map_err(|e| match e {
        roxmltree::Error::DtdDetected => refused("the file declares a DOCTYPE, which a C-CDA document never needs"),
        roxmltree::Error::NodesLimitReached => refused(format!("the file holds more than {MAX_NODES} XML nodes")),
        other => refused(format!("the XML is not well formed: {other}")),
    })?;
    let root = doc.root_element();
    if root.tag_name().name() != "ClinicalDocument" || root.tag_name().namespace() != Some(NS) {
        return Err(refused("the root element is not a CDA ClinicalDocument"));
    }
    // descendants come in document order, so the elements still open before an element are its
    // ancestors
    let mut open: Vec<Node> = Vec::new();
    for n in root.descendants().filter(Node::is_element) {
        while open.last().is_some_and(|top| n.parent() != Some(*top)) {
            open.pop();
        }
        open.push(n);
        if open.len() > MAX_DEPTH {
            return Err(refused(format!("the XML nests deeper than {MAX_DEPTH} levels")));
        }
    }
    Ok(doc)
}

/// Element children of `n` in the CDA namespace named `name`.
pub fn children<'a, 'i>(n: Node<'a, 'i>, name: &'static str) -> impl Iterator<Item = Node<'a, 'i>> {
    n.children()
        .filter(move |c| c.is_element() && c.tag_name().name() == name && c.tag_name().namespace() == Some(NS))
}

/// The first element child of `n` named `name`.
pub fn child<'a, 'i>(n: Node<'a, 'i>, name: &'static str) -> Option<Node<'a, 'i>> {
    children(n, name).next()
}

/// `child(child(n, a), b)` along a path.
pub fn path<'a, 'i>(n: Node<'a, 'i>, names: &[&'static str]) -> Option<Node<'a, 'i>> {
    names.iter().try_fold(n, |at, name| child(at, name))
}

/// Element children of `n` in the CDA namespace, whatever their name.
pub fn elements<'a, 'i>(n: Node<'a, 'i>) -> impl Iterator<Item = Node<'a, 'i>> {
    n.children()
        .filter(|c| c.is_element() && c.tag_name().namespace() == Some(NS))
}

/// A non-blank attribute without a namespace.
pub fn attr<'a>(n: Node<'a, '_>, name: &str) -> Option<&'a str> {
    n.attribute(name).map(str::trim).filter(|v| !v.is_empty())
}

/// `xsi:type` without any prefix (`CD`, `PQ`, `IVL_TS`).
pub fn xsi_type<'a>(n: Node<'a, '_>) -> Option<&'a str> {
    n.attribute((XSI, "type"))
        .map(|t| t.rsplit(':').next().unwrap_or(t).trim())
        .filter(|t| !t.is_empty())
}

/// The roots of an element's `templateId` children.
pub fn templates<'a>(n: Node<'a, '_>) -> Vec<&'a str> {
    children(n, "templateId").filter_map(|t| attr(t, "root")).collect()
}

pub fn has_template(n: Node<'_, '_>, roots: &[&str]) -> bool {
    templates(n).iter().any(|t| roots.contains(t))
}

/// Whether the attribute is the string `true`.
pub fn is_true(n: Node<'_, '_>, name: &str) -> bool {
    attr(n, name).is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// The text inside `n`, whitespace collapsed, at most [`MAX_TEXT`] characters. It stops reading at
/// that length, also inside one long text node.
pub fn text(n: Node<'_, '_>) -> Option<String> {
    let mut out = String::new();
    let mut last_space = true;
    let mut chars = 0usize;
    'nodes: for (seen, d) in n.descendants().enumerate() {
        if seen >= MAX_TEXT_NODES || chars >= MAX_TEXT {
            break;
        }
        // a line break or table cell in the narrative separates words
        if d.is_element()
            && matches!(d.tag_name().name(), "br" | "td" | "th" | "tr" | "paragraph" | "item")
            && !last_space
        {
            out.push(' ');
            last_space = true;
            chars += 1;
        }
        let Some(t) = d.is_text().then(|| d.text()).flatten() else {
            continue;
        };
        for c in t.chars() {
            if chars >= MAX_TEXT {
                break 'nodes;
            }
            if c.is_whitespace() {
                if !last_space {
                    out.push(' ');
                    last_space = true;
                    chars += 1;
                }
            } else {
                out.push(c);
                last_space = false;
                chars += 1;
            }
        }
    }
    let out: String = out.trim().chars().take(MAX_TEXT).collect();
    (!out.is_empty()).then_some(out)
}

/// The narrative of the document by `ID` attribute, for `<reference value="#id"/>`.
pub fn narrative_ids<'a, 'i>(doc: &'a Document<'i>) -> HashMap<&'a str, Node<'a, 'i>> {
    let mut ids = HashMap::new();
    for n in doc.descendants().filter(Node::is_element) {
        if let Some(id) = attr(n, "ID") {
            ids.entry(id).or_insert(n);
        }
    }
    ids
}
