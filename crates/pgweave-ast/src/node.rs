use serde::{Deserialize, Serialize};

/// Stable identifier for every AST node, assigned sequentially during parsing.
/// Used for cross-references in the symbol table and LSP position lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct NodeId(pub u32);

impl NodeId {
    pub const DUMMY: NodeId = NodeId(0);
}

/// Byte-offset span in the source SQL string.
/// start and end are byte positions (not char positions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub const DUMMY: Span = Span { start: 0, end: 0 };

    pub fn contains_offset(&self, offset: u32) -> bool {
        offset >= self.start && offset < self.end
    }
}
