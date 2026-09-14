//! The document tree shared by the YAML and JSON parsers.

/// A parsed node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// What the node holds.
    pub value: Value,
    /// Byte offset of the node in the file, for error messages.
    pub offset: usize,
}

/// A node's contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// A missing value (`key:` with nothing after it, or JSON `null`).
    Null,
    /// A scalar: a string, or the text of a number or boolean.
    Scalar(String),
    /// A sequence.
    Seq(Vec<Node>),
    /// A mapping, in document order.
    Map(Vec<(String, Node)>),
}

impl Node {
    /// The scalar text, if this is a scalar.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match &self.value {
            Value::Scalar(s) => Some(s),
            _ => None,
        }
    }

    /// The items, if this is a sequence. `Null` counts as empty.
    #[must_use]
    pub fn as_seq(&self) -> Option<&[Node]> {
        match &self.value {
            Value::Seq(items) => Some(items),
            Value::Null => Some(&[]),
            _ => None,
        }
    }

    /// The entries, if this is a mapping.
    #[must_use]
    pub fn as_map(&self) -> Option<&[(String, Node)]> {
        match &self.value {
            Value::Map(entries) => Some(entries),
            _ => None,
        }
    }

    /// Looks up `key` in a mapping.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Node> {
        self.as_map()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, node)| node)
    }
}
