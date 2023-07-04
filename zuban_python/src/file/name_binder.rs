use std::cell::RefCell;

use crate::database::{
    ClassStorage, ComplexPoint, FileIndex, Locality, Overload, ParentScope, Point, PointLink,
    PointType, Points, Specific,
};
use crate::debug;
use crate::diagnostics::{Diagnostics, Issue, IssueType};
use crate::file::python_file::StarImport;
use crate::file::ComplexValues;
use crate::utils::SymbolTable;
use parsa_python_ast::{
    AssignmentContentWithSimpleTargets, AssignmentRightSide, AsyncStmtContent, AtomContent, Block,
    BlockContent, ClassDef, CommonComprehensionExpression, Comprehension, Decoratee, Decorators,
    DictComprehension, Expression, ExpressionContent, ExpressionPart, File, ForIfClause,
    ForIfClauseIterator, ForStmt, FunctionDef, IfBlockType, IfStmt, ImportFromTargets,
    InterestingNode, InterestingNodeSearcher, Lambda, MatchStmt, Name, NameDefinition, NameParent,
    NodeIndex, SimpleStmts, StmtContent, StmtIterator, Tree, TryBlockType, TryStmt, WhileStmt,
    WithStmt,
};

#[derive(PartialEq, Debug)]
enum NameBinderType {
    Global,
    Function,
    Class,
    Lambda,
    Comprehension,
}

enum Unresolved<'db> {
    FunctionDef {
        func: FunctionDef<'db>,
        is_method: bool,
    },
    Lambda(Lambda<'db>),
    Comprehension(Comprehension<'db>),
    #[allow(dead_code)] // TODO remove this
    DictComprehension(DictComprehension<'db>),
    Name(Name<'db>),
}

pub(crate) struct NameBinder<'db, 'a> {
    mypy_compatible: bool,
    tree: &'db Tree,
    type_: NameBinderType,
    scope_node: NodeIndex,
    symbol_table: &'a SymbolTable,
    points: &'db Points,
    complex_points: &'db ComplexValues,
    issues: &'db Diagnostics,
    star_imports: &'db RefCell<Vec<StarImport>>,
    unordered_references: Vec<Name<'db>>,
    unresolved_nodes: Vec<Unresolved<'db>>,
    names_to_be_resolved_in_parent: Vec<Name<'db>>,
    unresolved_self_vars: Vec<ClassDef<'db>>,
    annotation_names: Vec<Name<'db>>,
    file_index: FileIndex,
    #[allow(dead_code)] // TODO remove this
    parent: Option<&'a NameBinder<'db, 'a>>,
}

impl<'db, 'a> NameBinder<'db, 'a> {
    fn new(
        mypy_compatible: bool,
        tree: &'db Tree,
        type_: NameBinderType,
        scope_node: NodeIndex,
        symbol_table: &'a SymbolTable,
        points: &'db Points,
        complex_points: &'db ComplexValues,
        issues: &'db Diagnostics,
        star_imports: &'db RefCell<Vec<StarImport>>,
        file_index: FileIndex,
        parent: Option<&'a Self>,
    ) -> Self {
        Self {
            mypy_compatible,
            tree,
            type_,
            scope_node,
            symbol_table,
            points,
            complex_points,
            issues,
            star_imports,
            unordered_references: vec![],
            unresolved_nodes: vec![],
            names_to_be_resolved_in_parent: vec![],
            unresolved_self_vars: vec![],
            annotation_names: vec![],
            file_index,
            parent,
        }
    }

    pub(crate) fn with_global_binder(
        mypy_compatible: bool,
        tree: &'db Tree,
        symbol_table: &'a SymbolTable,
        points: &'db Points,
        complex_points: &'db ComplexValues,
        issues: &'db Diagnostics,
        star_imports: &'db RefCell<Vec<StarImport>>,
        file_index: FileIndex,
        func: impl FnOnce(&mut NameBinder<'db, 'db>),
    ) where
        'a: 'db,
    {
        let mut binder = NameBinder::new(
            mypy_compatible,
            tree,
            NameBinderType::Global,
            0,
            symbol_table,
            points,
            complex_points,
            issues,
            star_imports,
            file_index,
            None,
        );
        func(&mut binder);
        binder.close();
        while let Some(class_def) = binder.unresolved_self_vars.pop() {
            binder.index_self_vars(class_def);
        }
        for annotation_name in &binder.annotation_names {
            binder.try_to_process_reference(*annotation_name);
        }
    }

    fn with_nested(
        &mut self,
        type_: NameBinderType,
        scope_node: NodeIndex,
        symbol_table: &'_ SymbolTable,
        func: impl FnOnce(&mut NameBinder<'db, '_>),
    ) {
        let mut name_binder = NameBinder::new(
            self.mypy_compatible,
            self.tree,
            type_,
            scope_node,
            symbol_table,
            self.points,
            self.complex_points,
            self.issues,
            self.star_imports,
            self.file_index,
            Some(self),
        );
        func(&mut name_binder);
        name_binder.close();
        let NameBinder {
            unresolved_nodes,
            names_to_be_resolved_in_parent,
            annotation_names,
            unresolved_self_vars,
            ..
        } = name_binder;
        self.unresolved_self_vars.extend(unresolved_self_vars);
        self.unresolved_nodes.extend(
            names_to_be_resolved_in_parent
                .into_iter()
                .map(Unresolved::Name),
        );
        self.unresolved_nodes.extend(unresolved_nodes);
        for annotation_name in annotation_names {
            if !try_to_process_reference_for_symbol_table(
                symbol_table,
                self.file_index,
                self.points,
                annotation_name,
            ) {
                self.annotation_names.push(annotation_name);
            }
        }
    }

    pub(crate) fn add_issue(&self, node_index: NodeIndex, type_: IssueType) {
        let maybe_ignored = self.tree.node_type_ignore_comment(node_index);
        let issue = Issue { type_, node_index };
        match self.issues.add_if_not_ignored(issue, maybe_ignored) {
            Ok(issue) => debug!("New name binder issue: {:?}", issue.type_),
            Err(issue) => debug!("New ignored name binder issue: {:?}", issue.type_),
        }
    }

    fn add_new_definition(&self, name_def: NameDefinition<'db>, point: Point, in_base_scope: bool) {
        let replaced = self.symbol_table.add_or_replace_symbol(name_def.name());
        if let Some(replaced) = replaced {
            self.points.set(
                name_def.name_index(),
                Point::new_multi_definition(replaced, Locality::File),
            );
        }
        self.points.set(name_def.index(), point);
    }

    fn add_point_definition(
        &mut self,
        name_def: NameDefinition<'db>,
        type_: Specific,
        in_base_scope: bool,
    ) {
        self.add_new_definition(
            name_def,
            Point::new_simple_specific(type_, Locality::Stmt),
            in_base_scope,
        );
    }

    fn add_redirect_definition(
        &mut self,
        name_def: NameDefinition<'db>,
        node_index: NodeIndex,
        in_base_scope: bool,
    ) {
        self.add_new_definition(
            name_def,
            Point::new_redirect(self.file_index, node_index, Locality::Stmt),
            in_base_scope,
        );
    }

    pub(crate) fn index_file(&mut self, file_node: File<'db>) {
        self.index_stmts(file_node.iter_stmts(), true, true);
    }

    fn index_block(&mut self, block: Block<'db>, ordered: bool, in_base_scope: bool) -> NodeIndex {
        // Returns the latest return/yield index
        // Theory:
        // - while_stmt, for_stmt: ignore order (at least mostly)
        // - match_stmt, if_stmt, try_stmt (only in coresponding blocks and after)
        // - sync_for_if_clause: reversed order and only in scope
        // - lambda: only in scope
        // - function_def, class_def: ignore
        match block.unpack() {
            BlockContent::OneLine(simple) => {
                self.index_simple_stmts(simple, ordered, in_base_scope)
            }
            BlockContent::Indented(stmts) => self.index_stmts(stmts, ordered, in_base_scope),
        }
    }

    fn index_stmts(
        &mut self,
        stmts: StmtIterator<'db>,
        ordered: bool,
        in_base_scope: bool,
    ) -> NodeIndex {
        let mut latest_return_or_yield = 0;
        for stmt in stmts {
            let return_or_yield = match stmt.unpack() {
                StmtContent::SimpleStmts(s) => self.index_simple_stmts(s, ordered, in_base_scope),
                StmtContent::FunctionDef(func) => {
                    self.index_function_name_and_param_defaults(
                        func,
                        ordered,
                        in_base_scope,
                        None,  // decorators
                        false, // is_async
                    );
                    0
                }
                StmtContent::ClassDef(class) => {
                    self.index_class(class, false, in_base_scope);
                    0
                }
                StmtContent::Decorated(decorated) => {
                    match decorated.decoratee() {
                        Decoratee::FunctionDef(func) => {
                            self.index_function_name_and_param_defaults(
                                func,
                                ordered,
                                in_base_scope,
                                Some(decorated.decorators()),
                                false, // is_async
                            );
                        }
                        Decoratee::AsyncFunctionDef(func) => {
                            self.index_function_name_and_param_defaults(
                                func,
                                ordered,
                                in_base_scope,
                                Some(decorated.decorators()),
                                true, // is_async
                            );
                        }
                        Decoratee::ClassDef(cls) => {
                            self.index_class(cls, true, in_base_scope);
                        }
                    }
                    0
                }
                StmtContent::IfStmt(if_stmt) => self.index_if_stmt(if_stmt, ordered),
                StmtContent::ForStmt(for_stmt) => self.index_for_stmt(for_stmt, ordered),
                StmtContent::TryStmt(try_stmt) => self.index_try_stmt(try_stmt, ordered),
                StmtContent::WhileStmt(while_stmt) => self.index_while_stmt(while_stmt, ordered),
                StmtContent::WithStmt(with_stmt) => self.index_with_stmt(with_stmt, ordered),
                StmtContent::MatchStmt(match_stmt) => self.index_match_stmt(match_stmt, ordered),
                StmtContent::AsyncStmt(async_stmt) => match async_stmt.unpack() {
                    AsyncStmtContent::FunctionDef(function_def) => {
                        self.index_function_name_and_param_defaults(
                            function_def,
                            ordered,
                            in_base_scope,
                            None, // decorators
                            true,
                        );
                        0
                    }
                    AsyncStmtContent::ForStmt(for_stmt) => self.index_for_stmt(for_stmt, ordered),
                    AsyncStmtContent::WithStmt(with_stmt) => {
                        self.index_with_stmt(with_stmt, ordered)
                    }
                },
                StmtContent::Newline => 0,
            };
            latest_return_or_yield =
                self.merge_latest_return_or_yield(latest_return_or_yield, return_or_yield);
        }
        latest_return_or_yield
    }

    fn merge_latest_return_or_yield(&self, first: NodeIndex, mut second: NodeIndex) -> NodeIndex {
        if first != 0 && second != 0 {
            loop {
                let point = self.points.get(second);
                let node_index = point.node_index();
                if node_index == 0 {
                    // Now that we have the first node in the chain of the second nodes, link that
                    // to the first one (like a linked list)
                    self.points.set(
                        second,
                        Point::new_node_analysis_with_node_index(Locality::File, first),
                    );
                    break;
                } else {
                    assert!(node_index < second);
                    second = node_index;
                }
            }
        }
        if second == 0 {
            first
        } else {
            second
        }
    }

    fn close(&mut self) {
        if self.type_ != NameBinderType::Class {
            while let Some(n) = self.unresolved_nodes.pop() {
                match n {
                    Unresolved::Name(name) => {
                        if !self.try_to_process_reference(name) {
                            self.names_to_be_resolved_in_parent.push(name);
                        }
                    }
                    Unresolved::FunctionDef { func, is_method } => {
                        let symbol_table = SymbolTable::default();
                        self.with_nested(
                            NameBinderType::Function,
                            func.index(),
                            &symbol_table,
                            |binder| binder.index_function_body(func, is_method),
                        );
                    }
                    Unresolved::Lambda(lambda) => {
                        let symbol_table = SymbolTable::default();
                        self.with_nested(
                            NameBinderType::Lambda,
                            lambda.index(),
                            &symbol_table,
                            |binder| binder.index_lambda(lambda),
                        );
                    }
                    Unresolved::Comprehension(comp) => self.index_comprehension(comp, true),
                    Unresolved::DictComprehension(comp) => {
                        self.index_dict_comprehension(comp, true)
                    }
                };
            }
        }
        self.index_unordered_references();
        debug_assert_eq!(self.unordered_references.len(), 0);
    }

    fn index_simple_stmts(
        &mut self,
        simple_stmts: SimpleStmts<'db>,
        ordered: bool,
        in_base_scope: bool,
    ) -> NodeIndex {
        let mut latest_return_or_yield = 0;
        for simple_stmt in simple_stmts.iter() {
            let r = if let Some(assignment) = simple_stmt.maybe_assignment() {
                let unpacked = assignment.unpack_with_simple_targets();
                // First we have to index the right side, before we can begin indexing the left
                // side.
                match &unpacked {
                    AssignmentContentWithSimpleTargets::Normal(_, right)
                    | AssignmentContentWithSimpleTargets::WithAnnotation(_, _, Some(right))
                    | AssignmentContentWithSimpleTargets::AugAssign(_, _, right) => {
                        let latest = match right {
                            AssignmentRightSide::YieldExpr(yield_expr) => {
                                self.index_non_block_node(yield_expr, ordered, in_base_scope)
                            }
                            AssignmentRightSide::StarExpressions(star_exprs) => {
                                self.index_non_block_node(star_exprs, ordered, in_base_scope)
                            }
                        };
                        latest_return_or_yield =
                            self.merge_latest_return_or_yield(latest_return_or_yield, latest);
                    }
                    _ => (),
                };
                match unpacked {
                    AssignmentContentWithSimpleTargets::Normal(targets, _) => {
                        for target in targets {
                            let l = self.index_non_block_node(&target, ordered, in_base_scope);
                            latest_return_or_yield =
                                self.merge_latest_return_or_yield(latest_return_or_yield, l);
                        }
                        0
                    }
                    AssignmentContentWithSimpleTargets::WithAnnotation(target, annotation, _) => {
                        self.index_annotation_expression(&annotation.expression());
                        self.index_non_block_node(&target, ordered, in_base_scope)
                    }
                    AssignmentContentWithSimpleTargets::AugAssign(target, _, _) => {
                        self.index_non_block_node(&target, ordered, in_base_scope)
                    }
                }
            } else if let Some(import) = simple_stmt.maybe_import_from() {
                match import.unpack_targets() {
                    ImportFromTargets::Star(star) => {
                        self.star_imports.borrow_mut().push(StarImport {
                            scope: self.scope_node,
                            import_from_node: import.index(),
                            star_node: star.index(),
                        })
                    }
                    ImportFromTargets::Iterator(targets) => {
                        for target in targets {
                            self.index_non_block_node(&target, ordered, in_base_scope);
                        }
                    }
                };
                0
            } else {
                self.index_non_block_node(&simple_stmt, ordered, in_base_scope)
            };
            latest_return_or_yield = self.merge_latest_return_or_yield(latest_return_or_yield, r);
        }
        latest_return_or_yield
    }

    fn index_for_stmt(&mut self, for_stmt: ForStmt<'db>, ordered: bool) -> NodeIndex {
        let mut latest_return_or_yield = 0;
        let (star_targets, star_expressions, block, else_block) = for_stmt.unpack();
        let latest = self.index_non_block_node(&star_targets, ordered, false);
        latest_return_or_yield = self.merge_latest_return_or_yield(latest_return_or_yield, latest);
        let latest = self.index_non_block_node(&star_expressions, ordered, false);
        latest_return_or_yield = self.merge_latest_return_or_yield(latest_return_or_yield, latest);

        let latest = self.index_block(block, false, false);
        latest_return_or_yield = self.merge_latest_return_or_yield(latest_return_or_yield, latest);

        if ordered {
            self.index_unordered_references();
        }
        if let Some(else_block) = else_block {
            let latest = self.index_block(else_block.block(), ordered, false);
            latest_return_or_yield =
                self.merge_latest_return_or_yield(latest_return_or_yield, latest);
        }
        latest_return_or_yield
    }

    fn index_while_stmt(&mut self, while_stmt: WhileStmt<'db>, ordered: bool) -> NodeIndex {
        let mut latest_return_or_yield = 0;
        let (condition, block, else_block) = while_stmt.unpack();
        let latest = self.index_non_block_node(&condition, ordered, false);
        latest_return_or_yield = self.merge_latest_return_or_yield(latest_return_or_yield, latest);
        let latest = self.index_block(block, false, false);
        latest_return_or_yield = self.merge_latest_return_or_yield(latest_return_or_yield, latest);
        if ordered {
            self.index_unordered_references();
        }
        if let Some(else_block) = else_block {
            // "else" ":" block
            let latest = self.index_block(else_block.block(), ordered, false);
            latest_return_or_yield =
                self.merge_latest_return_or_yield(latest_return_or_yield, latest);
        }
        latest_return_or_yield
    }

    fn index_with_stmt(&mut self, with_stmt: WithStmt<'db>, ordered: bool) -> NodeIndex {
        let mut latest_return_or_yield = 0;
        let (with_items, block) = with_stmt.unpack();
        for with_item in with_items.iter() {
            let latest = self.index_non_block_node(&with_item, ordered, false);
            latest_return_or_yield =
                self.merge_latest_return_or_yield(latest_return_or_yield, latest);
        }
        let latest = self.index_block(block, ordered, false);
        self.merge_latest_return_or_yield(latest_return_or_yield, latest)
    }

    fn index_if_stmt(&mut self, if_stmt: IfStmt<'db>, ordered: bool) -> NodeIndex {
        let mut latest_return_or_yield = 0;
        for if_block in if_stmt.iter_blocks() {
            let latest = match if_block {
                IfBlockType::If(expr, block) => {
                    let latest = self.index_non_block_node(&expr, ordered, false);
                    latest_return_or_yield =
                        self.merge_latest_return_or_yield(latest_return_or_yield, latest);
                    self.index_block(block, ordered, false)
                }
                IfBlockType::Else(block) => self.index_block(block, ordered, false),
            };
            latest_return_or_yield =
                self.merge_latest_return_or_yield(latest_return_or_yield, latest);
        }
        latest_return_or_yield
    }

    fn index_try_stmt(&mut self, try_stmt: TryStmt<'db>, ordered: bool) -> NodeIndex {
        let mut latest_return_or_yield = 0;
        for b in try_stmt.iter_blocks() {
            let latest = match b {
                TryBlockType::Try(block) => self.index_block(block, ordered, false),
                TryBlockType::Except(except) => {
                    let (expression, name_def, block) = except.unpack();
                    if let Some(expression) = expression {
                        let latest = self.index_non_block_node(&expression, ordered, false);
                        latest_return_or_yield =
                            self.merge_latest_return_or_yield(latest_return_or_yield, latest);
                        if let Some(name_def) = name_def {
                            self.add_new_definition(name_def, Point::new_uncalculated(), false)
                        }
                    }
                    self.index_block(block, ordered, false)
                }
                TryBlockType::Else(else_) => self.index_block(else_.block(), ordered, false),
                TryBlockType::Finally(finally) => self.index_block(finally.block(), ordered, false),
            };
            latest_return_or_yield =
                self.merge_latest_return_or_yield(latest_return_or_yield, latest);
        }
        latest_return_or_yield
    }

    fn index_class(&mut self, class: ClassDef<'db>, is_decorated: bool, in_base_scope: bool) {
        let class_symbol_table = SymbolTable::default();
        let self_symbol_table = SymbolTable::default();
        self.with_nested(
            NameBinderType::Class,
            class.index(),
            &class_symbol_table,
            |binder| {
                let (arguments, block) = class.unpack();
                if let Some(arguments) = arguments {
                    binder.index_non_block_node(&arguments, true, true);
                }
                binder.index_block(block, true, true);
            },
        );
        self.unresolved_self_vars.push(class);
        self.complex_points.insert(
            self.points,
            class.index(),
            ComplexPoint::Class(Box::new(ClassStorage {
                class_symbol_table,
                self_symbol_table,
                parent_scope: match self.type_ {
                    NameBinderType::Global => ParentScope::Module,
                    NameBinderType::Class => ParentScope::Class(self.scope_node),
                    NameBinderType::Function => ParentScope::Function(self.scope_node),
                    _ => unreachable!(),
                },
                promote_to: Default::default(),
            })),
            Locality::File,
        );
        // Need to first index the class, because the class body does not have access to
        // the class name.
        /*
        TODO reenable this maybe?
        if is_decorated {
            self.add_point_definition(
                class.name_definition(),
                Specific::LazyInferredClass,
                in_base_scope,
            );
        } else {
        */
        self.add_new_definition(
            class.name_definition(),
            Point::new_uncalculated(),
            in_base_scope,
        );
        //}
    }

    fn index_self_vars(&mut self, class: ClassDef<'db>) {
        let symbol_table = match self
            .complex_points
            .get(self.points.get(class.index()).complex_index())
        {
            ComplexPoint::Class(storage) => &storage.self_symbol_table,
            _ => unreachable!(),
        };
        for (self_name, name) in class.search_potential_self_assignments() {
            if self.is_self_param(self_name) {
                symbol_table.add_or_replace_symbol(name);
            }
        }
    }

    fn is_self_param(&self, name: Name<'db>) -> bool {
        let point = self.points.get(name.index());
        if point.type_() == PointType::Redirect {
            let param_index = point.node_index();
            // Points to the name and not the name definition, therefore check that.
            // It should be safe to check the index before, because the name binder only ever
            // redirects to ame definitions.
            let param_point = self.points.get(param_index - 1);
            if param_point.calculated()
                && param_point.type_() == PointType::Specific
                && param_point.specific() == Specific::SelfParam
            {
                return true;
            }
        }
        false
    }

    fn index_match_stmt(&mut self, match_stmt: MatchStmt<'db>, ordered: bool) -> NodeIndex {
        todo!("match_stmt")
    }

    fn index_non_block_node<T: InterestingNodeSearcher<'db>>(
        &mut self,
        node: &T,
        ordered: bool,
        in_base_scope: bool,
    ) -> NodeIndex {
        self.index_non_block_node_full(node, ordered, in_base_scope, false)
    }

    fn index_annotation_expression(&mut self, node: &Expression<'db>) -> NodeIndex {
        self.index_non_block_node_full(node, true, true, true)
    }

    #[inline]
    fn index_non_block_node_full<T: InterestingNodeSearcher<'db>>(
        &mut self,
        node: &T,
        ordered: bool,
        in_base_scope: bool,
        from_annotation: bool,
    ) -> NodeIndex {
        let mut latest_return_or_yield = 0;
        for n in node.search_interesting_nodes() {
            match n {
                InterestingNode::Name(name) => {
                    match name.parent() {
                        NameParent::Atom => {
                            if from_annotation {
                                self.annotation_names.push(name);
                            } else {
                                self.maybe_add_reference(name, ordered);
                            }
                        }
                        NameParent::NameDefinition(name_def) => {
                            if name_def.is_not_primary() {
                                // The types are inferred later.
                                self.add_new_definition(
                                    name_def,
                                    Point::new_uncalculated(),
                                    in_base_scope,
                                )
                            }
                        }
                        NameParent::GlobalStmt => {
                            //self.maybe_add_reference(name, ordered);
                            dbg!("TODO unhandled global");
                        }
                        NameParent::NonlocalStmt => {
                            // TODO nonlocal
                        }
                        _ => {
                            // All other names are not references or part of imports and should be
                            // resolved later.
                        }
                    }
                }
                InterestingNode::YieldExpr(n) => {
                    self.index_return_or_yield(&mut latest_return_or_yield, n.index());
                }
                InterestingNode::ReturnStmt(n) => {
                    if self.type_ != NameBinderType::Function {
                        self.add_issue(
                            n.index(),
                            IssueType::StmtOutsideFunction { keyword: "return" },
                        )
                    }
                    self.index_return_or_yield(&mut latest_return_or_yield, n.index());
                }
                InterestingNode::Lambda(lambda) => {
                    self.index_lambda_param_defaults(lambda, ordered);
                    self.unresolved_nodes.push(Unresolved::Lambda(lambda));
                }
                InterestingNode::Comprehension(comp) => {
                    // Index the first expression of a comprehension, which is always executed
                    // in the current scope.
                    if comp.is_generator() {
                        self.unresolved_nodes.push(Unresolved::Comprehension(comp));
                    } else {
                        self.index_comprehension(comp, ordered);
                    }
                }
                InterestingNode::DictComprehension(comp) => {
                    self.index_dict_comprehension(comp, ordered);
                }
            }
        }
        latest_return_or_yield
    }

    fn index_return_or_yield(&self, latest_return_or_yield: &mut NodeIndex, node_index: NodeIndex) {
        let keyword_index = node_index + 1;
        self.points.set(
            keyword_index,
            Point::new_node_analysis_with_node_index(Locality::File, *latest_return_or_yield),
        );
        *latest_return_or_yield = keyword_index
    }

    fn index_comprehension(&mut self, comp: Comprehension<'db>, ordered: bool) {
        // TODO the ordered argument is not used here currently and it should probably be used.
        let (expr, for_if_clauses) = comp.unpack();
        let mut clauses = for_if_clauses.iter();
        self.index_comprehension_clause(&expr, &clauses.next().unwrap(), &mut clauses)
    }

    fn index_dict_comprehension(&mut self, comp: DictComprehension<'db>, ordered: bool) {
        let (expr, for_if_clauses) = comp.unpack();
        let mut clauses = for_if_clauses.iter();
        self.index_comprehension_clause(&expr, &clauses.next().unwrap(), &mut clauses)
    }

    fn index_comprehension_clause(
        &mut self,
        expr: &CommonComprehensionExpression<'db>,
        clause: &ForIfClause<'db>,
        clauses: &mut ForIfClauseIterator<'db>,
    ) {
        let targets = match clause {
            ForIfClause::Sync(sync_for_if_clause) | ForIfClause::Async(sync_for_if_clause) => {
                let (targets, from, ifs) = sync_for_if_clause.unpack();
                self.index_non_block_node(&from, true, false);
                for if_ in ifs {
                    self.index_non_block_node(&if_, true, false);
                }
                targets
            }
        };
        // TODO this is not exactly correct for named expressions and their scopes.
        let symbol_table = SymbolTable::default();
        self.with_nested(
            NameBinderType::Comprehension,
            clause.index(),
            &symbol_table,
            |binder| {
                binder.index_non_block_node(&targets, true, false);

                if let Some(clause) = clauses.next() {
                    binder.index_comprehension_clause(expr, &clause, clauses);
                } else {
                    match expr {
                        CommonComprehensionExpression::Single(named_expr) => {
                            binder.index_non_block_node(named_expr, true, false)
                        }
                        CommonComprehensionExpression::DictKeyValue(dict_key_value) => {
                            binder.index_non_block_node(dict_key_value, true, false)
                        }
                    };
                }
            },
        );
    }

    fn index_function_name_and_param_defaults(
        &mut self,
        func: FunctionDef<'db>,
        ordered: bool,
        in_base_scope: bool,
        decorators: Option<Decorators<'db>>,
        is_async: bool,
    ) {
        // If there is no parent, this does not have to be resolved immediately in theory, but for
        // now we just do.
        self.unresolved_nodes.push(Unresolved::FunctionDef {
            func,
            is_method: self.type_ == NameBinderType::Class,
        });

        let (name_def, params, return_annotation, _) = func.unpack();
        let mut param_count = 0;
        for param in params.iter() {
            // expressions are resolved immediately while annotations are inferred at the
            // end of a module.
            if let Some(annotation) = param.annotation() {
                self.index_annotation_expression(&annotation.expression());
            }
            if let Some(expression) = param.default() {
                self.index_non_block_node(&expression, ordered, false);
            }
            param_count += 1;
        }
        if let Some(return_annotation) = return_annotation {
            // This is the -> annotation
            self.index_annotation_expression(&return_annotation.expression());
        }

        let mut is_overload = false;
        if let Some(decorators) = decorators {
            for decorator in decorators.iter() {
                self.index_non_block_node(&decorator, ordered, false);
                let expression = decorator.named_expression().expression();
                if let ExpressionContent::ExpressionPart(ExpressionPart::Atom(atom)) =
                    expression.unpack()
                {
                    if let AtomContent::Name(name) = atom.unpack() {
                        match name.as_str() {
                            "overload" => is_overload = true,
                            _ => (),
                        }
                    }
                }
            }
        }

        if self.type_ == NameBinderType::Class && param_count == 0 {
            self.add_issue(func.index(), IssueType::MethodWithoutArguments)
        }

        let file_index = self.file_index;
        let maybe_overload = self.maybe_overload(name_def.as_code());
        if is_overload {
            let current_link = PointLink::new(file_index, func.index());

            let new_overload = if let Some((old_overload_index, overload)) = maybe_overload {
                if let Some(implementing) = overload.implementing_function {
                    self.add_issue(
                        implementing.node_index,
                        IssueType::OverloadImplementationNotLast,
                    )
                }
                let new = overload.add_another_overload(current_link);
                // Reset the old overload definition, there should only be one.
                self.points.set(
                    old_overload_index,
                    Point::new_simple_specific(Specific::OverloadUnreachable, Locality::File),
                );
                new
            } else {
                // TODO add is_async for function types
                Overload {
                    functions: Box::new([current_link]),
                    implementing_function: None,
                }
            };
            self.complex_points.insert(
                self.points,
                name_def.index(),
                ComplexPoint::FunctionOverload(Box::new(new_overload)),
                Locality::File,
            );
            self.symbol_table.add_or_replace_symbol(name_def.name());
        } else {
            // Check for implementing functions of overloads
            if let Some((old_overload_index, o)) = maybe_overload {
                if o.implementing_function.is_none() {
                    is_overload = true;
                    let mut new_overload = o.clone();
                    new_overload.implementing_function =
                        Some(PointLink::new(self.file_index, func.index()));

                    self.complex_points.insert(
                        self.points,
                        name_def.index(),
                        ComplexPoint::FunctionOverload(Box::new(new_overload)),
                        Locality::File,
                    );
                    // Reset the old overload definition, there should only be one.
                    self.points.set(
                        old_overload_index,
                        Point::new_simple_specific(Specific::OverloadUnreachable, Locality::File),
                    );
                    self.symbol_table.add_or_replace_symbol(name_def.name());
                }
            }

            if !is_overload {
                if let Some(decorators) = decorators {
                    self.add_point_definition(name_def, Specific::DecoratedFunction, in_base_scope)
                } else {
                    self.add_redirect_definition(name_def, func.index(), true);
                }
            }
        }
        self.points.set(
            func.index(),
            Point::new_simple_specific(
                if self.type_ != NameBinderType::Function || return_annotation.is_some() {
                    Specific::Function
                } else {
                    Specific::Closure
                },
                Locality::Stmt,
            ),
        );
    }

    fn maybe_overload(&self, name: &str) -> Option<(NodeIndex, &Overload)> {
        if let Some(index) = self.symbol_table.lookup_symbol(name) {
            let name_def_index = index - 1;
            let point = self.points.get(name_def_index); // Lookup on NameDefinition
            if let Some(complex_index) = point.maybe_complex_index() {
                if let ComplexPoint::FunctionOverload(o) = self.complex_points.get(complex_index) {
                    return Some((name_def_index, o));
                }
            }
        }
        None
    }

    pub(crate) fn index_function_body(&mut self, func: FunctionDef<'db>, is_method: bool) {
        // Function name was indexed already.
        let (_, params, _, block) = func.unpack();

        self.index_param_name_defs(
            params.iter().map(|param| param.name_definition()),
            is_method,
        );

        let latest_return_index = self.index_block(block, true, true);
        // It's kind of hard to know where to store the latest reference statement.
        self.points.set(
            func.index() + 1,
            Point::new_node_analysis_with_node_index(
                Locality::ClassOrFunction,
                latest_return_index,
            ),
        );
    }

    fn index_param_name_defs(
        &mut self,
        mut names: impl Iterator<Item = NameDefinition<'db>>,
        is_method: bool,
    ) {
        if is_method {
            if let Some(name_def) = names.next() {
                self.add_point_definition(name_def, Specific::SelfParam, true);
            }
        }
        for name_def in names {
            self.add_point_definition(name_def, Specific::Param, true);
        }
    }

    fn index_lambda_param_defaults(&mut self, lambda: Lambda<'db>, ordered: bool) {
        // lambda: "lambda" [lambda_parameters] ":" expression
        for param in lambda.params() {
            if let Some(default) = param.default() {
                self.index_non_block_node(&default, ordered, false);
            }
        }
    }

    fn index_lambda(&mut self, lambda: Lambda<'db>) {
        let (params, expr) = lambda.unpack();
        self.index_param_name_defs(params.map(|param| param.name_definition()), false);
        self.index_non_block_node(&expr, true, true);
    }

    #[inline]
    fn maybe_add_reference(&mut self, name: Name<'db>, ordered: bool) {
        if !ordered || self.mypy_compatible && self.type_ != NameBinderType::Class {
            self.unordered_references.push(name);
        } else if !self.try_to_process_reference(name) {
            self.names_to_be_resolved_in_parent.push(name);
        }
    }

    #[inline]
    fn try_to_process_reference(&self, name: Name<'db>) -> bool {
        try_to_process_reference_for_symbol_table(
            self.symbol_table,
            self.file_index,
            self.points,
            name,
        )
    }

    fn index_unordered_references(&mut self) {
        for &name in &self.unordered_references {
            if !self.try_to_process_reference(name) {
                self.names_to_be_resolved_in_parent.push(name);
            }
        }
        self.unordered_references.truncate(0);
    }
}

#[inline]
fn try_to_process_reference_for_symbol_table(
    symbol_table: &SymbolTable,
    file_index: FileIndex,
    points: &Points,
    name: Name,
) -> bool {
    let point = {
        if let Some(definition) = symbol_table.lookup_symbol(name.as_str()) {
            Point::new_redirect(file_index, definition, Locality::File)
        } else {
            return false;
        }
    };
    points.set(name.index(), point);
    true
}
