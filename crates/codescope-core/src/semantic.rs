//! Semantic domain: language-neutral symbols, symbol trees, references, locations.
//!
//! Symbol trees come from hierarchical `DocumentSymbol` responses (research 03); the LSP
//! client converts via [`SymbolTree::from_document_symbols`]. The mapping layer
//! (`codescope-analysis`) consumes trees through the containment/nearest helpers here, so a
//! future tree-sitter producer can slot in without touching consumers.

use crate::file::FileId;
use crate::position::{LineRange, Position};
use std::fmt;

/// Language-neutral symbol kind (mirrors the LSP `SymbolKind` value space).
///
/// Serializes as `snake_case` (e.g. `"function"`, `"type_parameter"`). `Unknown` covers
/// kinds outside the LSP 1–26 range and non-LSP producers (forward compatibility).
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    /// File-level symbol.
    File,
    /// Module.
    Module,
    /// Namespace.
    Namespace,
    /// Package.
    Package,
    /// Class.
    Class,
    /// Method (Go method names include the receiver: `(Greeter).Hello`).
    Method,
    /// Property.
    Property,
    /// Field (Go struct fields appear as children of the struct symbol).
    Field,
    /// Constructor.
    Constructor,
    /// Enum.
    Enum,
    /// Interface.
    Interface,
    /// Function.
    Function,
    /// Variable.
    Variable,
    /// Constant.
    Constant,
    /// String literal symbol.
    String,
    /// Number literal symbol.
    Number,
    /// Boolean literal symbol.
    Boolean,
    /// Array literal symbol.
    Array,
    /// Object literal symbol.
    Object,
    /// Object key.
    Key,
    /// Null literal symbol.
    Null,
    /// Enum member.
    EnumMember,
    /// Struct.
    Struct,
    /// Event.
    Event,
    /// Operator.
    Operator,
    /// Type parameter.
    TypeParameter,
    /// Kind outside the known set (forward compatibility).
    #[default]
    Unknown,
}

impl From<lsp_types::SymbolKind> for SymbolKind {
    /// Map an LSP symbol kind. Kinds outside the LSP 3.17 range map to
    /// [`SymbolKind::Unknown`].
    fn from(kind: lsp_types::SymbolKind) -> Self {
        use lsp_types::SymbolKind as L;
        match kind {
            L::FILE => SymbolKind::File,
            L::MODULE => SymbolKind::Module,
            L::NAMESPACE => SymbolKind::Namespace,
            L::PACKAGE => SymbolKind::Package,
            L::CLASS => SymbolKind::Class,
            L::METHOD => SymbolKind::Method,
            L::PROPERTY => SymbolKind::Property,
            L::FIELD => SymbolKind::Field,
            L::CONSTRUCTOR => SymbolKind::Constructor,
            L::ENUM => SymbolKind::Enum,
            L::INTERFACE => SymbolKind::Interface,
            L::FUNCTION => SymbolKind::Function,
            L::VARIABLE => SymbolKind::Variable,
            L::CONSTANT => SymbolKind::Constant,
            L::STRING => SymbolKind::String,
            L::NUMBER => SymbolKind::Number,
            L::BOOLEAN => SymbolKind::Boolean,
            L::ARRAY => SymbolKind::Array,
            L::OBJECT => SymbolKind::Object,
            L::KEY => SymbolKind::Key,
            L::NULL => SymbolKind::Null,
            L::ENUM_MEMBER => SymbolKind::EnumMember,
            L::STRUCT => SymbolKind::Struct,
            L::EVENT => SymbolKind::Event,
            L::OPERATOR => SymbolKind::Operator,
            L::TYPE_PARAMETER => SymbolKind::TypeParameter,
            _ => SymbolKind::Unknown,
        }
    }
}

impl SymbolKind {
    /// `true` for kinds that can contain other symbols as children in a hierarchical tree.
    #[must_use]
    pub fn can_have_children(self) -> bool {
        matches!(
            self,
            SymbolKind::File
                | SymbolKind::Module
                | SymbolKind::Namespace
                | SymbolKind::Package
                | SymbolKind::Class
                | SymbolKind::Struct
                | SymbolKind::Interface
                | SymbolKind::Enum
                | SymbolKind::Object
        )
    }
}

/// Tree-local unique symbol identifier, assigned by the producer.
///
/// [`SymbolTree::from_document_symbols`] assigns hierarchical path ids (`"0"`, `"0/2/1"`);
/// other producers may choose any scheme, but ids must be unique within one tree. Cross-file
/// and cross-revision identity is expressed with [`SymbolRef`] instead.
#[derive(
    Debug,
    Default,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct SymbolId(pub String);

impl SymbolId {
    /// Wrap an id string.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        SymbolId(id.into())
    }

    /// The id string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SymbolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One node in a hierarchical symbol tree.
///
/// `range` is the full extent (declaration through closing brace; doc comments excluded per
/// gopls); `selection` is the identifier range only. Children are kept sorted by range
/// ([`SymbolTree::sort_recursive`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SymbolNode {
    /// Tree-local unique id.
    pub id: SymbolId,
    /// Symbol name (Go methods include the receiver: `(Greeter).Hello`).
    pub name: String,
    /// Extra detail, e.g. the signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Language-neutral kind.
    pub kind: SymbolKind,
    /// Full extent (declaration through closing brace).
    pub range: LineRange,
    /// Identifier-only range, contained in `range`.
    pub selection: LineRange,
    /// Child symbols (struct fields, enum members, …), sorted by range.
    #[serde(default)]
    pub children: Vec<SymbolNode>,
}

impl SymbolNode {
    /// Depth-first pre-order iterator over this node and all descendants.
    pub fn iter(&self) -> impl Iterator<Item = &SymbolNode> {
        let mut stack = vec![self];
        std::iter::from_fn(move || {
            let node = stack.pop()?;
            stack.extend(node.children.iter().rev());
            Some(node)
        })
    }

    /// The deepest descendant (or this node) whose range's **line span** fully contains
    /// `target`'s line span, if any.
    ///
    /// Containment is line-granular ([`LineRange::contains_lines`]) because its primary
    /// consumer is hunk→symbol mapping and hunks carry no columns; an indented symbol still
    /// contains a hunk covering its whole first line. For cursor-style lookup use
    /// [`SymbolNode::find_at_position`].
    ///
    /// Assumes children do not overlap (a tree property producers must uphold); the first
    /// containing child is recursed into, so the result is the *smallest* containing symbol.
    #[must_use]
    pub fn find_smallest_containing(&self, target: &LineRange) -> Option<&SymbolNode> {
        if !self.range.contains_lines(target) {
            return None;
        }
        for child in &self.children {
            if let Some(found) = child.find_smallest_containing(target) {
                return Some(found);
            }
        }
        Some(self)
    }

    /// The deepest descendant (or this node) whose [`SymbolNode::range`] contains `pos`
    /// (column-exact, inclusive bounds).
    #[must_use]
    pub fn find_at_position(&self, pos: Position) -> Option<&SymbolNode> {
        if !self.range.contains_pos(pos) {
            return None;
        }
        for child in &self.children {
            if let Some(found) = child.find_at_position(pos) {
                return Some(found);
            }
        }
        Some(self)
    }

    /// Count of this node plus all descendants.
    #[must_use]
    pub fn descendant_count(&self) -> usize {
        self.iter().count()
    }
}

/// Which revision of a file a symbol tree describes (research 03).
///
/// New-side hunks map against the `Worktree` tree; pure deletions map against the `Base`
/// tree (obtained via an in-memory `git show` overlay).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Revision {
    /// Merge-base revision content (`git show <base>:<path>` overlay).
    Base,
    /// Index content.
    Staged,
    /// On-disk worktree content (what the language server reads).
    Worktree,
}

/// Hierarchical symbol tree for one file at one revision.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SymbolTree {
    /// The file this tree describes.
    pub file: FileId,
    /// Which revision of the file.
    pub revision: Revision,
    /// Top-level symbols, sorted by range.
    #[serde(default)]
    pub roots: Vec<SymbolNode>,
}

impl SymbolTree {
    /// Create a tree. Does not sort; call [`SymbolTree::sort_recursive`] if the producer
    /// did not emit sorted symbols.
    #[must_use]
    pub fn new(file: FileId, revision: Revision, roots: Vec<SymbolNode>) -> Self {
        SymbolTree {
            file,
            revision,
            roots,
        }
    }

    /// Sort roots and all children by range (start, then end).
    pub fn sort_recursive(&mut self) {
        fn sort_nodes(nodes: &mut [SymbolNode]) {
            nodes.sort_by_key(|n| n.range);
            for n in nodes {
                sort_nodes(&mut n.children);
            }
        }
        sort_nodes(&mut self.roots);
    }

    /// Depth-first pre-order iterator over every symbol in the tree.
    pub fn iter(&self) -> impl Iterator<Item = &SymbolNode> {
        self.roots.iter().flat_map(|r| r.iter())
    }

    /// The smallest symbol whose range's line span contains `target`'s line span, searching
    /// all roots (line-granular; see [`SymbolNode::find_smallest_containing`]).
    ///
    /// Returns `None` for changes in gaps between top-level symbols (doc comments, import
    /// blocks) — the mapping layer falls back to [`SymbolTree::nearest_above`] /
    /// [`SymbolTree::nearest_below`] in that case (research 03).
    #[must_use]
    pub fn find_smallest_containing(&self, target: &LineRange) -> Option<&SymbolNode> {
        self.roots
            .iter()
            .find_map(|r| r.find_smallest_containing(target))
    }

    /// The smallest symbol containing `pos` (column-exact), searching all roots.
    #[must_use]
    pub fn find_at_position(&self, pos: Position) -> Option<&SymbolNode> {
        self.roots.iter().find_map(|r| r.find_at_position(pos))
    }

    /// Top-level symbol that ends strictly before zero-based `line`, nearest first.
    ///
    /// Used by the gap-fallback mapping: a change between symbols maps approximately to the
    /// nearest symbol within a few lines.
    #[must_use]
    pub fn nearest_above(&self, line: u32) -> Option<&SymbolNode> {
        self.roots
            .iter()
            .filter(|n| n.range.end_line < line)
            .max_by_key(|n| (n.range.end_line, n.range.end_col))
    }

    /// Top-level symbol that starts strictly after zero-based `line`, nearest first.
    #[must_use]
    pub fn nearest_below(&self, line: u32) -> Option<&SymbolNode> {
        self.roots
            .iter()
            .filter(|n| n.range.start_line > line)
            .min_by_key(|n| (n.range.start_line, n.range.start_col))
    }

    /// Total symbol count including nested symbols.
    #[must_use]
    pub fn symbol_count(&self) -> usize {
        self.iter().count()
    }

    /// Build a tree from hierarchical LSP `DocumentSymbol`s.
    ///
    /// Sorts by range, then assigns hierarchical path ids (`"0"`, `"0/2"`, …) in document
    /// order, and maps kinds via [`SymbolKind`]'s `From` impl. The LSP client must convert
    /// ranges to UTF-8 columns first ([`LineRange::from_lsp`] is a field rename, not an
    /// encoding conversion).
    #[must_use]
    pub fn from_document_symbols(
        file: FileId,
        revision: Revision,
        symbols: Vec<lsp_types::DocumentSymbol>,
    ) -> Self {
        fn convert(sym: lsp_types::DocumentSymbol) -> SymbolNode {
            let children = sym.children.unwrap_or_default();
            SymbolNode {
                id: SymbolId::new(""), // assigned after sorting, in document order
                name: sym.name,
                detail: sym.detail,
                kind: SymbolKind::from(sym.kind),
                range: LineRange::from_lsp(sym.range),
                selection: LineRange::from_lsp(sym.selection_range),
                children: children.into_iter().map(convert).collect(),
            }
        }
        fn assign_ids(nodes: &mut [SymbolNode], parent: Option<&str>) {
            for (i, n) in nodes.iter_mut().enumerate() {
                let id = match parent {
                    Some(p) => format!("{p}/{i}"),
                    None => i.to_string(),
                };
                n.id = SymbolId::new(id.clone());
                assign_ids(&mut n.children, Some(&id));
            }
        }
        let roots = symbols.into_iter().map(convert).collect();
        let mut tree = SymbolTree {
            file,
            revision,
            roots,
        };
        tree.sort_recursive();
        assign_ids(&mut tree.roots, None);
        tree
    }
}

/// Cross-file, cross-revision symbol identity: repo-relative file, fully-qualified
/// language-neutral name, and kind.
///
/// The fully-qualified name uses the producer's natural qualification (Go:
/// `pkg.Type.Method` or `(Type).Method`); it only needs to be unique within `file`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SymbolRef {
    /// Repo-relative file path.
    pub file: FileId,
    /// Fully-qualified symbol name.
    pub name: String,
    /// Language-neutral kind.
    pub kind: SymbolKind,
    /// Full source extent when the language-server response carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<LineRange>,
    /// Identifier-only source range, suitable for a follow-up semantic query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<LineRange>,
}

impl fmt::Display for SymbolRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.file, self.name)
    }
}

/// A position range inside a file (definition/reference/implementation sites).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Location {
    /// Repo-relative file path.
    pub file: FileId,
    /// Location range (UTF-8, zero-based).
    pub range: LineRange,
}

/// Reference to a fact-store entity: a file, optionally narrowed to a symbol and/or range.
///
/// Shared by the impact graph and AI visualization plans. `symbol: None` denotes a
/// file-level entity; `range: None` means "the whole symbol/file" (research 05 §2: range
/// must be optional for tree roots).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct EntityRef {
    /// Repo-relative file path.
    pub file: FileId,
    /// Fully-qualified symbol name within `file`; `None` for file-level entities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// Optional range within the symbol's extent (or equal to it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<LineRange>,
}

impl EntityRef {
    /// A file-level entity reference.
    #[must_use]
    pub fn for_file(file: FileId) -> Self {
        EntityRef {
            file,
            symbol: None,
            range: None,
        }
    }

    /// A symbol-level entity reference.
    #[must_use]
    pub fn for_symbol(file: FileId, symbol: impl Into<String>, range: Option<LineRange>) -> Self {
        EntityRef {
            file,
            symbol: Some(symbol.into()),
            range,
        }
    }

    /// `true` when this references a whole file (no symbol).
    #[must_use]
    pub fn is_file_level(&self) -> bool {
        self.symbol.is_none()
    }
}

impl fmt::Display for EntityRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.symbol {
            Some(sym) => write!(f, "{}:{sym}", self.file),
            None => write!(f, "{}", self.file),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, name: &str, range: LineRange, children: Vec<SymbolNode>) -> SymbolNode {
        SymbolNode {
            id: SymbolId::new(id),
            name: name.to_string(),
            detail: None,
            kind: SymbolKind::Function,
            range,
            selection: LineRange::new(range.start_line, 5, range.start_line, 5 + name.len() as u32),
            children,
        }
    }

    fn sample_tree() -> SymbolTree {
        // main: lines 0-10; type Greeter: lines 12-30 with field Name at 14-15;
        // method Hello: lines 32-45.
        let greeter = SymbolNode {
            kind: SymbolKind::Struct,
            children: vec![node("1/0", "Name", LineRange::new(14, 1, 15, 15), vec![])],
            ..node("1", "Greeter", LineRange::new(12, 0, 30, 1), vec![])
        };
        SymbolTree::new(
            FileId::new("main.go").unwrap(),
            Revision::Worktree,
            vec![
                node("0", "main", LineRange::new(0, 0, 10, 1), vec![]),
                greeter,
                node("2", "(Greeter).Hello", LineRange::new(32, 0, 45, 1), vec![]),
            ],
        )
    }

    #[test]
    fn find_smallest_containing_walks_to_deepest() {
        let tree = sample_tree();
        // Inside the Name field (nested child of Greeter).
        let hit = tree
            .find_smallest_containing(&LineRange::from_line_span(14, 15))
            .unwrap();
        assert_eq!(hit.name, "Name");
        // Inside Greeter but outside the field.
        let hit = tree
            .find_smallest_containing(&LineRange::from_line_span(20, 25))
            .unwrap();
        assert_eq!(hit.name, "Greeter");
        // Exactly the method extent.
        let hit = tree
            .find_smallest_containing(&LineRange::from_line_span(32, 45))
            .unwrap();
        assert_eq!(hit.name, "(Greeter).Hello");
    }

    #[test]
    fn find_smallest_containing_gap_returns_none() {
        let tree = sample_tree();
        // Line 11 is between main (ends 10) and Greeter (starts 12).
        assert!(tree
            .find_smallest_containing(&LineRange::from_line_span(11, 11))
            .is_none());
        // Spanning two top-level symbols → no single container.
        assert!(tree
            .find_smallest_containing(&LineRange::from_line_span(5, 35))
            .is_none());
    }

    #[test]
    fn find_at_position_is_column_exact() {
        let tree = sample_tree();
        // Inside the Name field's single-line extent (cols 1..15 on line 14).
        let hit = tree.find_at_position(Position::new(14, 5)).unwrap();
        assert_eq!(hit.name, "Name");
        // Line 14 col 0 precedes the field's start col → column-exact falls back to Greeter.
        let hit = tree.find_at_position(Position::new(14, 0)).unwrap();
        assert_eq!(hit.name, "Greeter");
        // Gap between symbols → None.
        assert!(tree.find_at_position(Position::new(11, 0)).is_none());
    }

    #[test]
    fn nearest_above_and_below() {
        let tree = sample_tree();
        assert_eq!(tree.nearest_above(11).unwrap().name, "main");
        assert_eq!(tree.nearest_below(11).unwrap().name, "Greeter");
        assert!(tree.nearest_above(0).is_none());
        assert_eq!(tree.nearest_below(45).map(|n| n.name.as_str()), None);
    }

    #[test]
    fn iter_is_preorder_and_covers_all() {
        let tree = sample_tree();
        let names: Vec<&str> = tree.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["main", "Greeter", "Name", "(Greeter).Hello"]);
        assert_eq!(tree.symbol_count(), 4);
    }

    #[test]
    fn sort_recursive_orders_children() {
        let mut tree = sample_tree();
        tree.roots.reverse();
        tree.sort_recursive();
        assert_eq!(tree.roots[0].name, "main");
        assert_eq!(tree.roots[2].name, "(Greeter).Hello");
    }

    #[test]
    fn symbol_kind_maps_all_lsp_kinds() {
        use lsp_types::SymbolKind as L;
        let cases = [
            (L::FILE, SymbolKind::File),
            (L::MODULE, SymbolKind::Module),
            (L::NAMESPACE, SymbolKind::Namespace),
            (L::PACKAGE, SymbolKind::Package),
            (L::CLASS, SymbolKind::Class),
            (L::METHOD, SymbolKind::Method),
            (L::PROPERTY, SymbolKind::Property),
            (L::FIELD, SymbolKind::Field),
            (L::CONSTRUCTOR, SymbolKind::Constructor),
            (L::ENUM, SymbolKind::Enum),
            (L::INTERFACE, SymbolKind::Interface),
            (L::FUNCTION, SymbolKind::Function),
            (L::VARIABLE, SymbolKind::Variable),
            (L::CONSTANT, SymbolKind::Constant),
            (L::STRING, SymbolKind::String),
            (L::NUMBER, SymbolKind::Number),
            (L::BOOLEAN, SymbolKind::Boolean),
            (L::ARRAY, SymbolKind::Array),
            (L::OBJECT, SymbolKind::Object),
            (L::KEY, SymbolKind::Key),
            (L::NULL, SymbolKind::Null),
            (L::ENUM_MEMBER, SymbolKind::EnumMember),
            (L::STRUCT, SymbolKind::Struct),
            (L::EVENT, SymbolKind::Event),
            (L::OPERATOR, SymbolKind::Operator),
            (L::TYPE_PARAMETER, SymbolKind::TypeParameter),
        ];
        assert_eq!(cases.len(), 26);
        for (lsp, ours) in cases {
            assert_eq!(SymbolKind::from(lsp), ours);
        }
    }

    #[test]
    fn entity_ref_kinds() {
        let file = FileId::new("pkg/a.go").unwrap();
        assert!(EntityRef::for_file(file.clone()).is_file_level());
        let sym = EntityRef::for_symbol(file, "pkg.Foo", Some(LineRange::new(1, 0, 5, 1)));
        assert!(!sym.is_file_level());
        assert_eq!(sym.to_string(), "pkg/a.go:pkg.Foo");
    }
}
