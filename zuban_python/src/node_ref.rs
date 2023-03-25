use std::fmt;

use parsa_python_ast::{
    Annotation, Assignment, Atom, AtomContent, Bytes, ClassDef, DoubleStarredExpression,
    Expression, Factor, FunctionDef, ImportFrom, Int, Name, NameDefinition, NamedExpression,
    NodeIndex, Primary, PythonString, StarredExpression, StringLiteral,
};

use crate::database::{
    ComplexPoint, Database, DbType, FileIndex, Locality, Point, PointLink, PointType,
};
use crate::diagnostics::{Issue, IssueType};
use crate::file::File;
use crate::file::PythonFile;
use crate::inference_state::InferenceState;
use crate::inferred::Inferred;
use crate::value::Module;

#[derive(Clone, Copy)]
pub struct NodeRef<'file> {
    pub file: &'file PythonFile,
    pub node_index: NodeIndex,
}

impl<'file> std::cmp::PartialEq for NodeRef<'file> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.file, other.file) && self.node_index == other.node_index
    }
}

impl<'file> NodeRef<'file> {
    #[inline]
    pub fn new(file: &'file PythonFile, node_index: NodeIndex) -> Self {
        Self { file, node_index }
    }

    pub fn from_link(db: &'file Database, point: PointLink) -> Self {
        let file = db.loaded_python_file(point.file);
        Self {
            file,
            node_index: point.node_index,
        }
    }

    pub fn in_module(&self, db: &'file Database) -> Module<'file> {
        Module::new(db, self.file)
    }

    pub fn to_db_lifetime(self, db: &Database) -> NodeRef {
        if std::cfg!(debug_assertions) {
            // Check that the file index is set, which means that it's in the database.
            self.file.file_index();
        }
        // This should be safe, because all files are added to the database.
        unsafe { std::mem::transmute(self) }
    }

    #[inline]
    pub fn add_to_node_index(&self, add: NodeIndex) -> Self {
        Self::new(self.file, self.node_index + add)
    }

    pub fn point(&self) -> Point {
        self.file.points.get(self.node_index)
    }

    pub fn set_point(&self, point: Point) {
        self.file.points.set(self.node_index, point)
    }

    pub fn set_point_redirect_in_same_file(&self, node_index: NodeIndex, locality: Locality) {
        self.file.points.set(
            self.node_index,
            Point::new_redirect(self.file.file_index(), node_index, locality),
        )
    }

    pub fn complex(&self) -> Option<&'file ComplexPoint> {
        let point = self.point();
        if let PointType::Complex = point.type_() {
            Some(self.file.complex_points.get(point.complex_index()))
        } else {
            None
        }
    }

    pub fn insert_complex(&self, complex: ComplexPoint, locality: Locality) {
        self.file
            .complex_points
            .insert(&self.file.points, self.node_index, complex, locality);
    }

    pub fn as_link(&self) -> PointLink {
        PointLink::new(self.file.file_index(), self.node_index)
    }

    pub fn as_expression(&self) -> Expression<'file> {
        Expression::by_index(&self.file.tree, self.node_index)
    }

    pub fn as_primary(&self) -> Primary<'file> {
        Primary::by_index(&self.file.tree, self.node_index)
    }

    pub fn as_name(&self) -> Name<'file> {
        Name::by_index(&self.file.tree, self.node_index)
    }

    pub fn as_name_def(&self) -> NameDefinition<'file> {
        NameDefinition::by_index(&self.file.tree, self.node_index)
    }

    pub fn as_annotation(&self) -> Annotation<'file> {
        Annotation::by_index(&self.file.tree, self.node_index)
    }

    pub fn as_bytes_literal(&self) -> Bytes<'file> {
        Bytes::by_index(&self.file.tree, self.node_index)
    }

    pub fn maybe_name(&self) -> Option<Name<'file>> {
        Name::maybe_by_index(&self.file.tree, self.node_index)
    }

    pub fn maybe_starred_expression(&self) -> Option<StarredExpression<'file>> {
        StarredExpression::maybe_by_index(&self.file.tree, self.node_index)
    }

    pub fn maybe_double_starred_expression(&self) -> Option<DoubleStarredExpression<'file>> {
        DoubleStarredExpression::maybe_by_index(&self.file.tree, self.node_index)
    }

    pub fn maybe_function(&self) -> Option<FunctionDef<'file>> {
        FunctionDef::maybe_by_index(&self.file.tree, self.node_index)
    }

    pub fn file_index(&self) -> FileIndex {
        self.file.file_index()
    }

    pub fn infer_int(&self) -> Option<i64> {
        Int::maybe_by_index(&self.file.tree, self.node_index).and_then(|i| i.as_str().parse().ok())
    }

    pub fn infer_str(&self) -> Option<PythonString<'file>> {
        Atom::maybe_by_index(&self.file.tree, self.node_index).and_then(|atom| {
            match atom.unpack() {
                AtomContent::Strings(s) => Some(s.as_python_string()),
                _ => None,
            }
        })
    }

    pub fn maybe_str(&self) -> Option<StringLiteral<'file>> {
        StringLiteral::maybe_by_index(&self.file.tree, self.node_index)
    }

    pub fn expect_int(&self) -> Int<'file> {
        Int::by_index(&self.file.tree, self.node_index)
    }

    pub fn maybe_class(&self) -> Option<ClassDef<'file>> {
        ClassDef::maybe_by_index(&self.file.tree, self.node_index)
    }

    pub fn maybe_factor(&self) -> Option<Factor<'file>> {
        Factor::maybe_by_index(&self.file.tree, self.node_index)
    }

    pub fn as_named_expression(&self) -> NamedExpression<'file> {
        NamedExpression::by_index(&self.file.tree, self.node_index)
    }

    pub fn expect_assignment(&self) -> Assignment<'file> {
        Assignment::by_index(&self.file.tree, self.node_index)
    }

    pub fn expect_import_from(&self) -> ImportFrom<'file> {
        ImportFrom::by_index(&self.file.tree, self.node_index)
    }

    pub fn debug_info(&self, db: &Database) -> String {
        format!(
            "{}: {}",
            self.file.file_path(db),
            self.file.tree.debug_info(self.node_index)
        )
    }

    pub fn compute_new_type_constraint(&self, i_s: &mut InferenceState) -> DbType {
        self.file
            .inference(i_s)
            .compute_new_type_constraint(self.as_expression())
    }

    pub fn as_code(&self) -> &'file str {
        self.file.tree.code_of_index(self.node_index)
    }

    pub(crate) fn add_typing_issue(&self, db: &Database, issue_type: IssueType) {
        let issue = Issue {
            type_: issue_type,
            node_index: self.node_index,
        };
        self.file.add_typing_issue(db, issue)
    }

    pub fn into_inferred(self) -> Inferred {
        Inferred::new_saved2(self.file, self.node_index)
    }
}

impl fmt::Debug for NodeRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut s = f.debug_struct("NodeRef");
        s.field("file_index", &self.file.file_index());
        s.field("node_index", &self.node_index);
        s.field(
            "node",
            &self.file.tree.short_debug_of_index(self.node_index),
        );
        let point = self.point();
        s.field("point", &point);
        if let Some(complex_index) = point.maybe_complex_index() {
            s.field(
                "complex",
                self.file.complex_points.get(point.complex_index()),
            );
        }
        s.finish()
    }
}
