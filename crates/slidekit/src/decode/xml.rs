//! Minimal XML element tree shared by vendor drivers with embedded XML
//! metadata (Philips `DPUfsImport`, Ventana `iScan`/`EncodeInfo`).
//!
//! ponytail: a tiny DOM, not a generic XML library — just enough to walk
//! elements by tag name and read attributes. `quick-xml` does the lexing; this
//! builds the tree. Lift more helpers here as more vendors need them.

use crate::error::{OpenSlideError, Result};
use quick_xml::Reader;
use quick_xml::events::Event;

/// A parsed XML element: tag name, attributes, child elements, and the
/// concatenated text directly inside it.
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
    pub text: String,
}

impl Node {
    pub fn empty() -> Self {
        Node {
            name: String::new(),
            attrs: Vec::new(),
            children: Vec::new(),
            text: String::new(),
        }
    }

    /// Value of an attribute by name.
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Direct children with the given element name.
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// First direct child with the given element name.
    pub fn find_child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    /// Descend a chain of single-named children (xpath-ish `/a/b/c`), returning
    /// the deepest node or `None` if any step is missing.
    pub fn descend(&self, path: &[&str]) -> Option<&Node> {
        let mut node = self;
        for name in path {
            node = node.find_child(name)?;
        }
        Some(node)
    }
}

fn start_node(e: &quick_xml::events::BytesStart) -> Node {
    let mut node = Node::empty();
    node.name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    for a in e.attributes().flatten() {
        let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
        let value = String::from_utf8_lossy(a.value.as_ref()).into_owned();
        node.attrs.push((key, value));
    }
    node
}

/// Parse an XML document into a tree, returning the root element.
pub fn parse_tree(xml: &str) -> Result<Node> {
    let mut reader = Reader::from_str(xml);
    // Sentinel root collects the real document root as its only child.
    let mut stack: Vec<Node> = vec![Node::empty()];
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => stack.push(start_node(&e)),
            Ok(Event::Empty(e)) => {
                let node = start_node(&e);
                stack.last_mut().unwrap().children.push(node);
            }
            Ok(Event::End(_)) => {
                if stack.len() > 1 {
                    let node = stack.pop().unwrap();
                    stack.last_mut().unwrap().children.push(node);
                }
            }
            Ok(Event::Text(t)) => {
                let s = String::from_utf8_lossy(t.as_ref());
                if !s.trim().is_empty() {
                    stack.last_mut().unwrap().text.push_str(&s);
                }
            }
            Ok(Event::CData(c)) => {
                let s = String::from_utf8_lossy(c.as_ref());
                stack.last_mut().unwrap().text.push_str(&s);
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(OpenSlideError::Decode(format!("XML parse error: {e}"))),
            _ => {}
        }
    }
    stack
        .pop()
        .and_then(|sentinel| sentinel.children.into_iter().next())
        .ok_or_else(|| OpenSlideError::Decode("Empty XML document".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_elements_and_attrs() {
        let xml = r#"<?xml version="1.0"?>
<root a="1">
  <child name="x">hi</child>
  <child name="y"/>
  <group><leaf k="v"/></group>
</root>"#;
        let root = parse_tree(xml).unwrap();
        assert_eq!(root.name, "root");
        assert_eq!(root.attr("a"), Some("1"));

        let children: Vec<&Node> = root.children_named("child").collect();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].attr("name"), Some("x"));
        assert_eq!(children[0].text, "hi");

        let leaf = root.descend(&["group", "leaf"]).unwrap();
        assert_eq!(leaf.attr("k"), Some("v"));
        assert!(root.descend(&["group", "missing"]).is_none());
    }
}
