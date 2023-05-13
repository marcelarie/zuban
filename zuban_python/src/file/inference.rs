use std::cell::Cell;
use std::rc::Rc;

use parsa_python_ast::*;

use super::{on_argument_type_error, File, PythonFile};
use crate::arguments::{CombinedArguments, KnownArguments, NoArguments, SimpleArguments};
use crate::database::{
    CallableContent, CallableParams, ComplexPoint, DbType, FileIndex, GenericItem, GenericsList,
    Literal, LiteralKind, Locality, ParamSpecific, Point, PointLink, PointType, Specific,
    TupleContent, TypeOrTypeVarTuple,
};
use crate::debug;
use crate::diagnostics::IssueType;
use crate::getitem::SliceType;
use crate::imports::{find_ancestor, global_import};
use crate::inference_state::InferenceState;
use crate::inferred::{Inferred, UnionValue};
use crate::matching::{FormatData, Generics, ResultContext, Type};
use crate::node_ref::NodeRef;
use crate::utils::debug_indent;
use crate::value::{Class, Function, Instance, LookupResult, Module, OnTypeError};

pub struct Inference<'db: 'file, 'file, 'i_s> {
    pub(super) file: &'file PythonFile,
    pub(super) file_index: FileIndex,
    pub(super) i_s: &'i_s InferenceState<'db, 'i_s>,
}

macro_rules! check_point_cache_with {
    ($vis:vis $name:ident, $func:path, $ast:ident $(, $result_context:ident )?) => {
        $vis fn $name(&mut self, node: $ast $(, $result_context : &mut ResultContext)?) -> $crate::inferred::Inferred {
            debug_indent(|| {
                if let Some(inferred) = self.check_point_cache(node.index()) {
                    debug!(
                        "{} {:?} (#{}, {}:{}) from cache: {}",
                        stringify!($name),
                        node.short_debug(),
                        self.file.byte_to_line_column(node.start()).0,
                        self.file.file_index(),
                        node.index(),
                        {
                            let point = self.file.points.get(node.index());
                            match point.type_() {
                                PointType::Specific => format!("{:?}", point.specific()),
                                PointType::Redirect => format!("Redirect {}:{}", point.file_index(), point.node_index()),
                                _ => format!("{:?}", point.type_()),
                            }
                        },
                    );
                    inferred
                } else {
                    debug!(
                        "{} {:?} (#{}, {}:{})",
                        stringify!($name),
                        node.short_debug(),
                        self.file.byte_to_line_column(node.start()).0,
                        self.file.file_index(),
                        node.index(),
                    );
                    $func(self, node $(, $result_context)?)
                }
            })
        }
    }
}

impl<'db, 'file, 'i_s> Inference<'db, 'file, 'i_s> {
    fn cache_simple_stmts_name(&mut self, simple_stmts: SimpleStmts, name_def: NodeRef) {
        debug!(
            "Infer stmt (#{}, {}:{}): {:?}",
            self.file.byte_to_line_column(simple_stmts.start()).0,
            self.file.file_index(),
            simple_stmts.index(),
            simple_stmts.short_debug().trim()
        );
        name_def.set_point(Point::new_calculating());
        for simple_stmt in simple_stmts.iter() {
            match simple_stmt.unpack() {
                SimpleStmtContent::Assignment(assignment) => {
                    self.cache_assignment_nodes(assignment);
                }
                SimpleStmtContent::ImportFrom(import_from) => {
                    self.cache_import_from(import_from);
                }
                SimpleStmtContent::ImportName(import_name) => {
                    self.cache_import_name(import_name);
                }
                _ => unreachable!("Found {simple_stmt:?}"),
            }
        }
    }

    fn cache_stmt_name(&mut self, stmt: Stmt, name_def: NodeRef) {
        debug!(
            "Infer stmt (#{}, {}:{}): {:?}",
            self.file.byte_to_line_column(stmt.start()).0,
            self.file.file_index(),
            stmt.index(),
            stmt.short_debug().trim()
        );
        match stmt.unpack() {
            StmtContent::ForStmt(for_stmt) => {
                name_def.set_point(Point::new_calculating());
                let (star_targets, star_exprs, _, _) = for_stmt.unpack();
                // Performance: We probably do not need to calculate diagnostics just for
                // calculating the names.
                self.cache_for_stmt_names(star_targets, star_exprs);
            }
            StmtContent::ClassDef(cls) => self.cache_class(name_def, cls),
            StmtContent::Decorated(decorated) => match decorated.decoratee() {
                Decoratee::ClassDef(cls) => {
                    // TODO this just ignores decorators? Should this even be reachable?
                    self.cache_class(name_def, cls)
                }
                _ => unreachable!(),
            },
            _ => unreachable!("Found type {:?}", stmt.short_debug()),
        }
    }

    pub fn cache_class(&mut self, name_def: NodeRef, class_node: ClassDef) {
        if !name_def.point().calculated() {
            let definition = NodeRef::new(self.file, class_node.index());
            let ComplexPoint::Class(cls_storage) = definition.complex().unwrap() else {
                unreachable!()
            };

            debug_assert!(!name_def.point().calculated());
            // We can redirect now, because we are going to calculate the class infos.
            name_def.set_point(Point::new_redirect(
                self.file_index,
                class_node.index(),
                Locality::Todo,
            ));

            let class = Class::new(definition, cls_storage, Generics::NotDefinedYet, None);
            // Make sure the type vars are properly pre-calculated
            class.ensure_calculated_class_infos(self.i_s);
        }
    }

    pub(super) fn cache_import_name(&mut self, imp: ImportName) {
        if self.file.points.get(imp.index()).calculated() {
            return;
        }

        for dotted_as_name in imp.iter_dotted_as_names() {
            match dotted_as_name.unpack() {
                DottedAsNameContent::Simple(name_def, _) => {
                    self.global_import(
                        name_def.as_code(),
                        name_def.index(),
                        Some(name_def.name_index()),
                    );
                }
                DottedAsNameContent::WithAs(dotted_name, as_name_def) => {
                    let file_index = self.infer_import_dotted_name(dotted_name, None);
                    debug_assert!(!self.file.points.get(as_name_def.index()).calculated());
                    let point = if let Some(file_index) = file_index {
                        Point::new_file_reference(file_index, Locality::Todo)
                    } else {
                        Point::new_unknown(self.file.file_index(), Locality::Todo)
                    };
                    self.file.points.set(as_name_def.index(), point);
                    self.file.points.set(as_name_def.name().index(), point);
                }
            }
        }

        self.file
            .points
            .set(imp.index(), Point::new_node_analysis(Locality::Todo));
    }

    pub(super) fn cache_import_from(&mut self, imp: ImportFrom) {
        if self.file.points.get(imp.index()).calculated() {
            return;
        }

        let (level, dotted_name) = imp.level_with_dotted_name();
        let maybe_level_file = (level > 0)
            .then(|| {
                find_ancestor(self.i_s.db, self.file, level).or_else(|| {
                    NodeRef::new(self.file, imp.index())
                        .add_typing_issue(self.i_s, IssueType::NoParentModule);
                    None
                })
            })
            .flatten();
        let from_part_file_index = match dotted_name {
            Some(dotted_name) => self.infer_import_dotted_name(dotted_name, maybe_level_file),
            None => maybe_level_file,
        };

        match imp.unpack_targets() {
            ImportFromTargets::Star(keyword) => {
                // Nothing to do here, was calculated earlier
                let point = match from_part_file_index {
                    Some(file_index) => Point::new_file_reference(file_index, Locality::Todo),
                    None => Point::new_unknown(self.file.file_index(), Locality::Todo),
                };
                self.file.points.set(keyword.index(), point);
            }
            ImportFromTargets::Iterator(targets) => {
                // as names should have been calculated earlier
                let import_file = from_part_file_index.map(|f| self.i_s.db.loaded_python_file(f));
                for target in targets {
                    let (import_name, name_def) = target.unpack();

                    let point = if let Some(import_file) = import_file {
                        let module = Module::new(self.i_s.db, import_file);

                        if let Some(link) = import_file.lookup_global(import_name.as_str()) {
                            link.into_point_redirect()
                        } else if let Some(file_index) = import_file
                            .package_dir
                            .as_ref()
                            // TODO this dir is unused???
                            .and_then(|dir| module.sub_module(self.i_s.db, import_name.as_str()))
                        {
                            self.i_s
                                .db
                                .add_invalidates(file_index, self.file.file_index());
                            Point::new_file_reference(file_index, Locality::Todo)
                        } else if let Some(link) = import_file
                            .inference(self.i_s)
                            .lookup_from_star_import(import_name.as_str(), false)
                        {
                            Point::new_redirect(link.file, link.node_index, Locality::Todo)
                        } else {
                            NodeRef::new(self.file, import_name.index()).add_typing_issue(
                                self.i_s,
                                IssueType::ImportAttributeError {
                                    module_name: Box::from(module.name()),
                                    name: Box::from(import_name.as_str()),
                                },
                            );
                            Point::new_unknown(import_file.file_index(), Locality::Todo)
                        }
                    } else {
                        Point::new_unknown(self.file.file_index(), Locality::Todo)
                    };
                    self.file.points.set(import_name.index(), point);
                    self.file.points.set(name_def.index(), point);
                    self.file.points.set(name_def.name().index(), point);
                }
            }
        }
        self.file
            .points
            .set(imp.index(), Point::new_node_analysis(Locality::Todo));
    }

    fn global_import(
        &self,
        name: &str,
        index: NodeIndex,
        second_index: Option<NodeIndex>,
    ) -> Option<FileIndex> {
        let file_index = global_import(self.i_s.db, self.file.file_index(), name);
        let point = if let Some(file_index) = file_index {
            self.i_s
                .db
                .add_invalidates(file_index, self.file.file_index());
            debug!(
                "Global import {name:?}: {:?}",
                self.i_s.db.file_path(file_index)
            );
            Point::new_file_reference(file_index, Locality::DirectExtern)
        } else {
            let node_ref = NodeRef::new(self.file, index);
            node_ref.add_typing_issue(
                self.i_s,
                IssueType::ModuleNotFound {
                    module_name: Box::from(name),
                },
            );
            Point::new_unknown(self.file.file_index(), Locality::Todo)
        };
        self.file.points.set(index, point);
        if let Some(second_index) = second_index {
            self.file.points.set(second_index, point);
        }
        file_index
    }

    fn infer_import_dotted_name(
        &mut self,
        dotted: DottedName,
        base: Option<FileIndex>,
    ) -> Option<FileIndex> {
        let infer_name = |i_s, file_index, name: Name| {
            let file = self.i_s.db.loaded_python_file(file_index);
            let module = Module::new(self.i_s.db, file);
            let result = module.sub_module(self.i_s.db, name.as_str());
            if let Some(imported) = result {
                debug!(
                    "Imported {:?} for {:?}",
                    self.i_s
                        .db
                        .loaded_python_file(imported)
                        .file_path(self.i_s.db),
                    dotted.as_code(),
                );
            } else {
                let node_ref = NodeRef::new(self.file, name.index());
                let m = format!("{}.{}", module.name().to_owned(), name.as_str()).into();
                node_ref.add_typing_issue(i_s, IssueType::ModuleNotFound { module_name: m });
            }
            result
        };
        match dotted.unpack() {
            DottedNameContent::Name(name) => {
                if let Some(base) = base {
                    infer_name(self.i_s, base, name)
                } else {
                    self.global_import(name.as_str(), name.index(), None)
                }
            }
            DottedNameContent::DottedName(dotted_name, name) => self
                .infer_import_dotted_name(dotted_name, base)
                .and_then(|file_index| infer_name(self.i_s, file_index, name)),
        }
    }

    fn original_definition(&mut self, assignment: Assignment) -> Option<Inferred> {
        // TODO shouldn't this be merged/using infer_single_target

        // TODO it's weird that we unpack assignments here again.
        if let AssignmentContent::Normal(targets, _) = assignment.unpack() {
            for target in targets {
                match target {
                    Target::Name(name_def) => {
                        let point = self.file.points.get(name_def.name_index());
                        if point.calculated() {
                            debug_assert_eq!(
                                point.type_(),
                                PointType::MultiDefinition,
                                "{target:?}"
                            );
                            let mut first_definition = point.node_index();
                            loop {
                                let point = self.file.points.get(first_definition);
                                if point.calculated() && point.type_() == PointType::MultiDefinition
                                {
                                    first_definition = point.node_index();
                                } else {
                                    break;
                                }
                            }
                            let inferred = self.infer_name_by_index(first_definition);
                            return Some(inferred);
                        }
                    }
                    Target::NameExpression(primary_target, name_def_node) => {
                        /*
                        if let PrimaryTargetOrAtom::Atom(atom) = primary_target.first() {
                            // TODO this is completely wrong!!!
                            if atom.as_code() == "self" {
                                continue;
                            }
                        }
                        return Some(self.infer_primary_target(primary_target));
                        */
                    }
                    _ => (),
                }
            }
        }
        None
    }

    fn set_calculating_on_target(&mut self, target: Target) {
        match target {
            Target::Name(name_def) => {
                self.file
                    .points
                    .set(name_def.index(), Point::new_calculating());
            }
            Target::NameExpression(primary_target, name_def_node) => (),
            Target::IndexExpression(t) => (),
            Target::Tuple(targets) => {
                for target in targets {
                    self.set_calculating_on_target(target);
                }
            }
            Target::Starred(s) => self.set_calculating_on_target(s.as_target()),
        }
    }

    pub(super) fn cache_assignment_nodes(&mut self, assignment: Assignment) {
        let node_ref = NodeRef::new(self.file, assignment.index()).to_db_lifetime(self.i_s.db);
        if node_ref.point().calculated() {
            return;
        }
        let right_side = match assignment.unpack() {
            AssignmentContent::Normal(targets, right_side) => {
                for target in targets {
                    self.set_calculating_on_target(target);
                }
                Some(right_side)
            }
            AssignmentContent::WithAnnotation(target, _, right_side) => {
                self.set_calculating_on_target(target);
                right_side
            }
            AssignmentContent::AugAssign(target, aug_assign, right_side) => Some(right_side),
        };
        let on_type_error = |i_s: &InferenceState, got, expected| -> NodeRef {
            // In cases of stubs when an ellipsis is given, it's not an error.
            if self.file.is_stub(i_s.db) {
                // Right side always exists, because it was compared and there was an error because
                // of it.
                if let AssignmentRightSide::StarExpressions(star_exprs) = right_side.unwrap() {
                    if let StarExpressionContent::Expression(expr) = star_exprs.unpack() {
                        if expr.is_ellipsis_literal() {
                            return node_ref;
                        }
                    }
                }
            }
            node_ref.add_typing_issue(i_s, IssueType::IncompatibleAssignment { got, expected });
            node_ref
        };
        match assignment.unpack() {
            AssignmentContent::Normal(targets, right_side) => {
                let type_comment_result = self.check_for_type_comment(assignment);

                let is_definition = type_comment_result.is_some();
                let right = if let Some((r, type_)) = type_comment_result {
                    let right = self
                        .infer_assignment_right_side(right_side, &mut ResultContext::Known(&type_));
                    type_.error_if_not_matches(self.i_s, &right, on_type_error);
                    r
                } else {
                    let original_def = self.original_definition(assignment);
                    let result_type = original_def.as_ref().map(|inf| inf.as_type(self.i_s));
                    let mut result_context = match &result_type {
                        Some(t) => ResultContext::Known(t),
                        None => ResultContext::AssignmentNewDefinition,
                    };
                    self.infer_assignment_right_side(right_side, &mut result_context)
                };
                for target in targets {
                    self.assign_targets(target, right.clone(), node_ref, is_definition)
                }
            }
            AssignmentContent::WithAnnotation(target, annotation, right_side) => {
                self.ensure_cached_annotation(annotation);
                match self.file.points.get(annotation.index()).maybe_specific() {
                    Some(Specific::TypingTypeAlias) => {
                        debug!("TODO TypeAlias calculation, does this make sense?");
                        self.compute_explicit_type_assignment(assignment)
                    }
                    Some(Specific::TypingFinal) => {
                        if let Some(right_side) = right_side {
                            let right = self.infer_assignment_right_side(
                                right_side,
                                &mut ResultContext::ExpectLiteral,
                            );
                            self.assign_single_target(
                                target,
                                &right.clone(),
                                true,
                                |i_s, index| {
                                    right.save_redirect(i_s, self.file, index);
                                },
                            );
                        } else {
                            todo!()
                        }
                    }
                    _ => {
                        if let Some(right_side) = right_side {
                            let t = self.use_cached_annotation_type(annotation);
                            let right = self.infer_assignment_right_side(
                                right_side,
                                &mut ResultContext::Known(&t),
                            );
                            t.error_if_not_matches(self.i_s, &right, on_type_error);
                        }
                        let inf_annot = self.use_cached_annotation(annotation);
                        self.assign_single_target(target, &inf_annot, true, |_, index| {
                            self.file.points.set(
                                index,
                                Point::new_redirect(
                                    self.file.file_index(),
                                    annotation.index(),
                                    Locality::Todo,
                                ),
                            );
                        })
                    }
                }
            }
            AssignmentContent::AugAssign(target, aug_assign, right_side) => {
                let (inplace, normal, reverse) = aug_assign.magic_methods();
                let right =
                    self.infer_assignment_right_side(right_side, &mut ResultContext::Unknown);
                let left = self.infer_single_target(target);
                let result = left.lookup_and_execute_with_details(
                    self.i_s,
                    node_ref,
                    normal,
                    &KnownArguments::new(&right, node_ref),
                    &|i_s, type_| {
                        let left = type_.format_short(i_s.db);
                        node_ref.add_typing_issue(
                            i_s,
                            IssueType::UnsupportedLeftOperand {
                                operand: Box::from(aug_assign.operand()),
                                left,
                            },
                        );
                    },
                    OnTypeError::new(&|i_s, class, function, arg, right, wanted| {
                        arg.as_node_ref().add_typing_issue(
                            i_s,
                            IssueType::UnsupportedOperand {
                                operand: Box::from(aug_assign.operand()),
                                left: class.unwrap().format_short(i_s.db),
                                right,
                            },
                        )
                    }),
                );
                if let AssignmentContent::AugAssign(target, _, _) = assignment.unpack() {
                    self.assign_single_target(target, &result, false, |_, index| {
                        // There is no need to save this, because it's never used
                    })
                } else {
                    unreachable!()
                }
            }
        }
        self.file
            .points
            .set(assignment.index(), Point::new_node_analysis(Locality::Todo));
    }

    fn infer_assignment_right_side(
        &mut self,
        right: AssignmentRightSide,
        result_context: &mut ResultContext,
    ) -> Inferred {
        match right {
            AssignmentRightSide::StarExpressions(star_exprs) => {
                self.infer_star_expressions(star_exprs, result_context)
            }
            AssignmentRightSide::YieldExpr(yield_expr) => match yield_expr.unpack() {
                YieldExprContent::StarExpressions(s) => todo!(),
                YieldExprContent::YieldFrom(y) => todo!(),
            },
        }
    }

    fn infer_single_target(&mut self, target: Target) -> Inferred {
        match target {
            // TODO it's a bit weird that we cannot just call self.infer_name_definition here
            Target::Name(name_def) => self.infer_name_reference(name_def.name()),
            Target::NameExpression(primary_target, name_def_node) => {
                todo!()
            }
            Target::IndexExpression(t) => self.infer_primary_target(t),
            Target::Tuple(_) | Target::Starred(_) => unreachable!(),
        }
    }

    fn assign_single_target(
        &mut self,
        target: Target,
        value: &Inferred,
        is_definition: bool,
        save: impl FnOnce(&InferenceState, NodeIndex),
    ) {
        match target {
            Target::Name(name_def) => {
                let point = self.file.points.get(name_def.name_index());
                if point.calculated() {
                    debug_assert_eq!(point.type_(), PointType::MultiDefinition, "{target:?}");
                    let mut first_definition = point.node_index();
                    loop {
                        let point = self.file.points.get(first_definition);
                        if point.calculated() && point.type_() == PointType::MultiDefinition {
                            first_definition = point.node_index();
                        } else {
                            break;
                        }
                    }
                    let inferred = self.infer_name_by_index(first_definition);
                    inferred.as_type(self.i_s).error_if_not_matches(
                        self.i_s,
                        value,
                        |i_s, got, expected| {
                            let node_ref =
                                NodeRef::new(self.file, name_def.index()).to_db_lifetime(i_s.db);
                            node_ref.add_typing_issue(
                                i_s,
                                IssueType::IncompatibleAssignment { got, expected },
                            );
                            node_ref
                        },
                    );
                }
                save(self.i_s, name_def.index());
            }
            Target::NameExpression(primary_target, name_definition) => {
                if primary_target.as_code().contains("self") {
                    // TODO here we should do something as well.
                } else {
                    let i_s = self.i_s;
                    if is_definition {
                        NodeRef::new(self.file, primary_target.index())
                            .add_typing_issue(i_s, IssueType::InvalidTypeDeclaration);
                    }
                    let base = self.infer_primary_target_or_atom(primary_target.first());
                    let node_ref = NodeRef::new(self.file, primary_target.index());
                    base.as_type(i_s).run_on_each_union_type(&mut |t| {
                        if let Some(cls) = t.maybe_class(i_s.db) {
                            if Instance::new(cls, None).checked_set_descriptor(
                                i_s,
                                node_ref,
                                name_definition.name(),
                                value,
                            ) {
                                return;
                            }
                        }
                        t.maybe_type_of_class(i_s.db)
                            .and_then(|c| {
                                // We need to handle class descriptors separately, because
                                // there the __get__ descriptor should not be applied.
                                c.lookup_with_or_without_descriptors(
                                    i_s,
                                    Some(node_ref),
                                    name_definition.as_code(),
                                    false,
                                )
                                .into_maybe_inferred()
                            })
                            .unwrap_or_else(|| {
                                t.lookup_with_error(
                                    i_s,
                                    node_ref,
                                    name_definition.as_code(),
                                    &|i_s, t| {
                                        add_attribute_error(
                                            i_s,
                                            node_ref,
                                            t,
                                            name_definition.name(),
                                        )
                                    },
                                )
                                .into_inferred()
                            })
                            .as_type(i_s)
                            .error_if_not_matches(i_s, value, |i_s, got, expected| {
                                let node_ref = NodeRef::new(self.file, primary_target.index())
                                    .to_db_lifetime(i_s.db);
                                node_ref.add_typing_issue(
                                    i_s,
                                    IssueType::IncompatibleAssignment { got, expected },
                                );
                                node_ref
                            });
                    });
                }
                // This mostly needs to be saved for self names
                save(self.i_s, name_definition.index());
            }
            Target::IndexExpression(primary_target) => {
                let base = self.infer_primary_target_or_atom(primary_target.first());
                if is_definition {
                    NodeRef::new(self.file, primary_target.index())
                        .add_typing_issue(self.i_s, IssueType::UnexpectedTypeDeclaration);
                }
                let PrimaryContent::GetItem(slice_type) = primary_target.second() else {
                    unreachable!();
                };
                let node_ref = NodeRef::new(self.file, primary_target.index());
                let slice = SliceType::new(self.file, primary_target.index(), slice_type);
                let args = slice.as_args(*self.i_s);
                debug!("Set Item on {}", base.format_short(self.i_s));
                base.lookup_and_execute_with_details(
                    self.i_s,
                    node_ref,
                    "__setitem__",
                    &CombinedArguments::new(&args, &KnownArguments::new(value, node_ref)),
                    &|i_s, _| {
                        debug!("TODO __setitem__ not found");
                    },
                    OnTypeError::new(&|i_s, class, function, arg, actual, expected| {
                        arg.as_node_ref().add_typing_issue(
                            i_s,
                            IssueType::InvalidGetItem {
                                actual,
                                type_: class.unwrap().format_short(i_s.db),
                                expected,
                            },
                        )
                    }),
                );
            }
            Target::Tuple(_) | Target::Starred(_) => unreachable!(),
        }
    }

    pub(super) fn assign_targets(
        &mut self,
        target: Target,
        value: Inferred,
        value_node_ref: NodeRef,
        is_definition: bool,
    ) {
        match target {
            Target::Tuple(mut targets) => {
                let mut value_iterator = value.save_and_iter(self.i_s, value_node_ref);
                let mut counter = 0;
                while let Some(target) = targets.next() {
                    counter += 1;
                    if let Target::Starred(star_target) = target {
                        let (stars, normal) = targets.clone().remaining_stars_and_normal_count();
                        if stars > 0 {
                            todo!()
                        } else if let Some(len) = value_iterator.len() {
                            let fetch = len - normal;
                            let union = Inferred::gather_union(self.i_s, |callable| {
                                for _ in 0..(len - normal) {
                                    callable(value_iterator.next(self.i_s).unwrap());
                                }
                            });

                            let generic = union.class_as_db_type(self.i_s);
                            let list = Inferred::create_instance(
                                self.i_s.db.python_state.list_node_ref().as_link(),
                                Some(Rc::new([GenericItem::TypeArgument(generic)])),
                            );
                            self.assign_targets(
                                star_target.as_target(),
                                list,
                                value_node_ref,
                                is_definition,
                            );
                        } else if value_iterator.len().is_none() {
                            let value = value_iterator.next(self.i_s).unwrap();
                            let list = Inferred::create_instance(
                                self.i_s.db.python_state.list_node_ref().as_link(),
                                Some(Rc::new([GenericItem::TypeArgument(
                                    value.class_as_db_type(self.i_s),
                                )])),
                            );
                            self.assign_targets(
                                star_target.as_target(),
                                list,
                                value_node_ref,
                                is_definition,
                            );
                        } else {
                            todo!()
                        }
                    } else if let Some(value) = value_iterator.next(self.i_s) {
                        self.assign_targets(target, value, value_node_ref, is_definition)
                    } else {
                        let original_counter = counter;
                        self.assign_targets(
                            target,
                            Inferred::new_any(),
                            value_node_ref,
                            is_definition,
                        );
                        for target in targets {
                            counter += 1;
                            self.assign_targets(
                                target,
                                Inferred::new_any(),
                                value_node_ref,
                                is_definition,
                            );
                        }
                        value_node_ref.add_typing_issue(
                            self.i_s,
                            IssueType::TooFewValuesToUnpack {
                                actual: original_counter - 1,
                                expected: counter,
                            },
                        );
                        break;
                    }
                }
            }
            Target::Starred(n) => {
                todo!("Star tuple unpack");
            }
            _ => self.assign_single_target(target, &value, is_definition, |i_s, index| {
                value
                    .clone()
                    .maybe_save_redirect(i_s, self.file, index, true);
            }),
        };
    }

    pub fn infer_star_expressions(
        &mut self,
        exprs: StarExpressions,
        result_context: &mut ResultContext,
    ) -> Inferred {
        match exprs.unpack() {
            StarExpressionContent::Expression(expr) => {
                if true {
                    self.infer_expression_with_context(expr, result_context)
                } else {
                    // TODO use this somewhere
                    /*
                    debug!("Found {} type vars in {}", type_vars.len(), expr.as_code());
                    ComplexPoint::TypeAlias(Box::new(TypeAlias {
                        type_vars: type_vars.into_boxed_slice(),
                        db_type: self.infer_expression(expr).as_db_type(self.i_s),
                    }))
                    */
                    todo!()
                }
            }
            StarExpressionContent::StarExpression(expr) => {
                NodeRef::new(self.file, expr.index())
                    .add_typing_issue(self.i_s, IssueType::StarredExpressionOnlyNoTarget);
                Inferred::new_any()
            }
            StarExpressionContent::Tuple(tuple) => self
                .infer_tuple_iterator(tuple.iter())
                .save_redirect(self.i_s, self.file, tuple.index()),
        }
    }

    pub fn infer_named_expression(&mut self, named_expr: NamedExpression) -> Inferred {
        self.infer_named_expression_with_context(named_expr, &mut ResultContext::Unknown)
    }

    pub fn infer_named_expression_with_context(
        &mut self,
        named_expr: NamedExpression,
        result_context: &mut ResultContext,
    ) -> Inferred {
        match named_expr.unpack() {
            NamedExpressionContent::Expression(expr)
            | NamedExpressionContent::Definition(_, expr) => {
                self.infer_expression_with_context(expr, result_context)
            }
        }
    }

    pub fn infer_expression(&mut self, expr: Expression) -> Inferred {
        self.infer_expression_with_context(expr, &mut ResultContext::Unknown)
    }

    check_point_cache_with!(
        pub infer_expression_with_context,
        Self::infer_expression_without_cache,
        Expression,
        result_context
    );
    fn infer_expression_without_cache(
        &mut self,
        expr: Expression,
        result_context: &mut ResultContext,
    ) -> Inferred {
        let inferred = match expr.unpack() {
            ExpressionContent::ExpressionPart(n) => self.infer_expression_part(n, result_context),
            ExpressionContent::Lambda(l) => self.infer_lambda(l, result_context),
            ExpressionContent::Ternary(t) => {
                let (if_, condition, else_) = t.unpack();
                let else_inf = self.infer_expression(else_);
                self.infer_expression_part(if_, &mut ResultContext::Unknown)
                    .types_union(self.i_s, else_inf, result_context)
            }
        };
        // We only save the result if nothing is there, yet. It could be that we pass this function
        // twice, when for example a class F(List[X]) is created, where X = F and X is defined
        // before F, this might happen.
        inferred.maybe_save_redirect(self.i_s, self.file, expr.index(), true)
    }

    pub fn infer_expression_part(
        &mut self,
        node: ExpressionPart,
        result_context: &mut ResultContext,
    ) -> Inferred {
        match node {
            ExpressionPart::Atom(atom) => self.infer_atom(atom, result_context),
            ExpressionPart::Primary(primary) => self.infer_primary(primary, result_context),
            ExpressionPart::Sum(sum) => self.infer_operation(sum.as_operation()),
            ExpressionPart::Term(term) => self.infer_operation(term.as_operation()),
            ExpressionPart::Power(power) => self.infer_operation(power.as_operation()),
            ExpressionPart::ShiftExpr(shift) => self.infer_operation(shift.as_operation()),
            ExpressionPart::BitwiseOr(or) => {
                self.infer_expression_part(or.as_operation().left, &mut ResultContext::Unknown);
                self.infer_expression_part(or.as_operation().right, &mut ResultContext::Unknown);
                debug!("TODO Use: self.infer_operation(or.as_operation())");
                Inferred::new_any()
            }
            ExpressionPart::BitwiseAnd(and) => self.infer_operation(and.as_operation()),
            ExpressionPart::BitwiseXor(xor) => self.infer_operation(xor.as_operation()),
            ExpressionPart::Disjunction(or) => {
                let (first, second) = or.unpack();
                let first = self.infer_expression_part(first, &mut ResultContext::Unknown);
                let second = self.infer_expression_part(second, &mut ResultContext::Unknown);
                Inferred::create_instance(
                    self.i_s.db.python_state.builtins_point_link("bool"),
                    None,
                )
            }
            ExpressionPart::Conjunction(and) => {
                let (first, second) = and.unpack();
                let first = self.infer_expression_part(first, &mut ResultContext::Unknown);
                let second = self.infer_expression_part(second, &mut ResultContext::Unknown);
                Inferred::create_instance(
                    self.i_s.db.python_state.builtins_point_link("bool"),
                    None,
                )
            }
            ExpressionPart::Inversion(inversion) => {
                let expr = inversion.expression();
                self.infer_expression_part(expr, &mut ResultContext::Unknown);
                Inferred::create_instance(
                    self.i_s.db.python_state.builtins_point_link("bool"),
                    None,
                )
            }
            ExpressionPart::Comparisons(cmps) => {
                Inferred::gather_types_union(|gather| {
                    for cmp in cmps.iter() {
                        let result = match cmp {
                            ComparisonContent::Equals(first, op, second)
                            | ComparisonContent::NotEquals(first, op, second) => {
                                let first =
                                    self.infer_expression_part(first, &mut ResultContext::Unknown);
                                let second =
                                    self.infer_expression_part(second, &mut ResultContext::Unknown);
                                let from = NodeRef::new(self.file, op.index());
                                // TODO this does not implement __ne__ for NotEquals
                                first.lookup_and_execute(
                                    self.i_s,
                                    from,
                                    "__eq__",
                                    &KnownArguments::new(&second, from),
                                    &|_, _| todo!(),
                                )
                            }
                            ComparisonContent::Is(first, _, second)
                            | ComparisonContent::IsNot(first, _, second) => {
                                let first =
                                    self.infer_expression_part(first, &mut ResultContext::Unknown);
                                let second =
                                    self.infer_expression_part(second, &mut ResultContext::Unknown);
                                Inferred::create_instance(
                                    self.i_s.db.python_state.builtins_point_link("bool"),
                                    None,
                                )
                            }
                            ComparisonContent::In(first, op, second)
                            | ComparisonContent::NotIn(first, op, second) => {
                                let first =
                                    self.infer_expression_part(first, &mut ResultContext::Unknown);
                                let second =
                                    self.infer_expression_part(second, &mut ResultContext::Unknown);
                                let from = NodeRef::new(self.file, op.index());
                                second.run_after_lookup_on_each_union_member(
                                    self.i_s,
                                    Some(from),
                                    "__contains__",
                                    &mut |r_type, lookup_result| {
                                        if let Some(method) = lookup_result.into_maybe_inferred() {
                                            method.execute_with_details(
                                                self.i_s,
                                                &KnownArguments::new(&first, from),
                                                &mut ResultContext::Unknown,
                                                OnTypeError::new(&|i_s, _, _, _, got, _| {
                                                    let right = r_type.format_short(i_s.db);
                                                    from.add_typing_issue(
                                                        i_s,
                                                        IssueType::UnsupportedOperand {
                                                            operand: Box::from("in"),
                                                            left: got,
                                                            right,
                                                        },
                                                    );
                                                }),
                                            );
                                        } else {
                                            let t = r_type
                                                .lookup_with_error(
                                                    self.i_s,
                                                    from,
                                                    "__iter__",
                                                    &|i_s, _| {
                                                        let right = second.format_short(i_s);
                                                        from.add_typing_issue(
                                                            i_s,
                                                            IssueType::UnsupportedIn { right },
                                                        )
                                                    },
                                                )
                                                .into_inferred()
                                                .execute(self.i_s, &NoArguments::new(from))
                                                .lookup_and_execute(
                                                    self.i_s,
                                                    from,
                                                    "__next__",
                                                    &NoArguments::new(from),
                                                    &|_, _| todo!(),
                                                )
                                                .as_type(self.i_s)
                                                .error_if_not_matches(
                                                    self.i_s,
                                                    &first,
                                                    |i_s, got, _| {
                                                        let t = IssueType::UnsupportedOperand {
                                                            operand: Box::from("in"),
                                                            left: got,
                                                            right: r_type.format_short(i_s.db),
                                                        };
                                                        from.add_typing_issue(i_s, t);
                                                        from.to_db_lifetime(i_s.db)
                                                    },
                                                );
                                        }
                                    },
                                );
                                Inferred::create_instance(
                                    self.i_s.db.python_state.builtins_point_link("bool"),
                                    None,
                                )
                            }
                            ComparisonContent::Operation(op) => self.infer_operation(op),
                        };
                        gather(self.i_s, result)
                    }
                })
            }
            ExpressionPart::Factor(f) => {
                let (operand, right) = f.unpack();
                let method_name = match operand.as_code() {
                    "-" => {
                        if let ExpressionPart::Atom(atom) = right {
                            if let AtomContent::Int(i) = atom.unpack() {
                                return if let Some(i) = self.parse_int(i, result_context) {
                                    Inferred::execute_db_type(
                                        self.i_s,
                                        DbType::Literal(Literal {
                                            kind: LiteralKind::Int(-i),
                                            implicit: true,
                                        }),
                                    )
                                } else {
                                    let point =
                                        Point::new_simple_specific(Specific::Int, Locality::Todo);
                                    Inferred::new_and_save(self.file, f.index(), point)
                                };
                            }
                        }
                        "__neg__"
                    }
                    "+" => "__pos__",
                    "~" => "__invert__",
                    _ => unreachable!(),
                };
                let inf = self.infer_expression_part(
                    right,
                    &mut match result_context.is_literal_context(self.i_s) {
                        false => ResultContext::Unknown,
                        true => ResultContext::ExpectLiteral,
                    },
                );
                if operand.as_code() == "-" && result_context.is_literal_context(self.i_s) {
                    match inf.maybe_literal(self.i_s.db) {
                        UnionValue::Single(literal) => {
                            if let LiteralKind::Int(i) = &literal.kind {
                                return Inferred::execute_db_type(
                                    self.i_s,
                                    DbType::Literal(Literal {
                                        kind: LiteralKind::Int(-i),
                                        implicit: true,
                                    }),
                                );
                            }
                        }
                        UnionValue::Multiple(literals) => todo!(),
                        UnionValue::Any => (),
                    }
                }
                let node_ref = NodeRef::new(self.file, f.index());
                inf.lookup_and_execute(
                    self.i_s,
                    node_ref,
                    method_name,
                    &NoArguments::new(node_ref),
                    &|i_s, type_| {
                        let operand = match operand.as_code() {
                            "~" => "~",
                            "-" => "unary -",
                            "+" => "unary +",
                            _ => unreachable!(),
                        };
                        let got = type_.format_short(i_s.db);
                        node_ref.add_typing_issue(
                            i_s,
                            IssueType::UnsupportedOperandForUnary { operand, got },
                        )
                    },
                )
            }
            ExpressionPart::AwaitPrimary(_) => todo!(),
        }
    }

    pub fn infer_lambda(&mut self, lambda: Lambda, result_context: &mut ResultContext) -> Inferred {
        result_context
            .with_type_if_exists_and_replace_type_var_likes(
                self.i_s,
                |i_s: &InferenceState<'db, '_>, type_| {
                    if let Some(DbType::Callable(c)) = type_.maybe_db_type() {
                        let i_s = i_s.with_lambda_callable(c);
                        let (params, expr) = lambda.unpack();
                        let rt = Type::new(&c.result_type);
                        let result = self
                            .file
                            .inference(&i_s)
                            .infer_expression_without_cache(expr, &mut ResultContext::Known(&rt));
                        let mut c = (**c).clone();
                        c.result_type = result.as_type(&i_s).into_db_type(i_s.db);
                        Inferred::execute_db_type(&i_s, DbType::Callable(Rc::new(c)))
                    } else {
                        todo!()
                    }
                },
            )
            .unwrap_or_else(|| {
                let (params, expr) = lambda.unpack();
                if params.count() == 0 {
                    let result =
                        self.infer_expression_without_cache(expr, &mut ResultContext::Unknown);
                    let c = CallableContent {
                        name: None,
                        class_name: None,
                        defined_at: PointLink::new(self.file.file_index(), lambda.index()),
                        type_vars: None,
                        params: CallableParams::Simple(Rc::new([])),
                        result_type: result.class_as_db_type(self.i_s),
                    };
                    Inferred::execute_db_type(self.i_s, DbType::Callable(Rc::new(c)))
                } else {
                    todo!()
                }
            })
    }

    fn infer_operation(&mut self, op: Operation) -> Inferred {
        let left = self.infer_expression_part(op.left, &mut ResultContext::Unknown);
        self.infer_detailed_operation(op, left)
    }

    fn infer_detailed_operation(&mut self, op: Operation, left: Inferred) -> Inferred {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum LookupError {
            NoError,
            LeftError,
            ShortCircuit,
            BothSidesError,
        }

        let right = self.infer_expression_part(op.right, &mut ResultContext::Unknown);
        let node_ref = NodeRef::new(self.file, op.index);
        let mut had_error = false;
        let i_s = self.i_s;
        let result = Inferred::gather_union(i_s, |add_to_union| {
            left.run_after_lookup_on_each_union_member(
                i_s,
                Some(node_ref),
                op.magic_method,
                &mut |l_type, lookup_result| {
                    let left_op_method = l_type
                        .lookup_without_error(self.i_s, Some(node_ref), op.magic_method)
                        .into_maybe_inferred();
                    right.as_type(i_s).run_on_each_union_type(&mut |r_type| {
                        let error = Cell::new(LookupError::NoError);
                        if let Some(left) = left_op_method.as_ref() {
                            let had_left_error = Cell::new(false);
                            let right_inf = Inferred::execute_db_type_allocation_todo(i_s, r_type);
                            let result = left.execute_with_details(
                                i_s,
                                &KnownArguments::new(&right_inf, node_ref),
                                &mut ResultContext::Unknown,
                                OnTypeError {
                                    on_overload_mismatch: Some(&|_, _| had_left_error.set(true)),
                                    callback: &|_, _, _, _, _, _| had_left_error.set(true),
                                },
                            );
                            if !had_left_error.get() {
                                return add_to_union(result);
                            }
                        }
                        if op.shortcut_when_same_type {
                            if let Some(left_instance) = l_type.maybe_class(i_s.db) {
                                if let Some(right_instance) = r_type.maybe_class(i_s.db) {
                                    if left_instance.node_ref == right_instance.node_ref {
                                        error.set(LookupError::ShortCircuit);
                                    }
                                }
                            }
                        }
                        let result = if error.get() != LookupError::ShortCircuit {
                            let left_inf = Inferred::execute_db_type_allocation_todo(i_s, l_type);
                            r_type
                                .lookup_with_error(
                                    i_s,
                                    node_ref,
                                    op.reverse_magic_method,
                                    &|i_s, _| {
                                        if left_op_method.as_ref().is_some() {
                                            error.set(LookupError::BothSidesError);
                                        } else {
                                            error.set(LookupError::LeftError);
                                        }
                                    },
                                )
                                .into_inferred()
                                .execute_with_details(
                                    i_s,
                                    &KnownArguments::new(&left_inf, node_ref),
                                    &mut ResultContext::Unknown,
                                    OnTypeError {
                                        on_overload_mismatch: Some(&|_, _| {
                                            error.set(LookupError::BothSidesError)
                                        }),
                                        callback: &|_, _, _, _, _, _| {
                                            error.set(LookupError::BothSidesError)
                                        },
                                    },
                                )
                        } else {
                            Inferred::new_unknown()
                        };
                        add_to_union(match error.get() {
                            LookupError::NoError => result,
                            LookupError::BothSidesError => {
                                had_error = true;
                                let t = IssueType::UnsupportedOperand {
                                    operand: Box::from(op.operand),
                                    left: l_type.format_short(i_s.db),
                                    right: r_type.format_short(i_s.db),
                                };
                                node_ref.add_typing_issue(i_s, t);
                                Inferred::new_unknown()
                            }
                            LookupError::LeftError | LookupError::ShortCircuit => {
                                had_error = true;
                                let left = l_type.format_short(i_s.db);
                                node_ref.add_typing_issue(
                                    i_s,
                                    IssueType::UnsupportedLeftOperand {
                                        operand: Box::from(op.operand),
                                        left,
                                    },
                                );
                                Inferred::new_unknown()
                            }
                        })
                    })
                },
            )
        });
        if had_error {
            let note = match (left.is_union(self.i_s.db), right.is_union(self.i_s.db)) {
                (false, false) => return result,
                (true, false) => {
                    format!("Left operand is of type {:?}", left.format_short(self.i_s),).into()
                }
                (false, true) => format!(
                    "Right operand is of type {:?}",
                    right.format_short(self.i_s),
                )
                .into(),
                (true, true) => Box::from("Both left and right operands are unions"),
            };
            node_ref.add_typing_issue(self.i_s, IssueType::Note(note));
        }
        debug!(
            "Operation between {} and {} results in {}",
            left.format_short(i_s),
            right.format_short(i_s),
            result.format_short(i_s)
        );
        result
    }

    pub fn infer_primary(
        &mut self,
        primary: Primary,
        result_context: &mut ResultContext,
    ) -> Inferred {
        let base = self.infer_primary_or_atom(primary.first());
        let result = self.infer_primary_or_primary_t_content(
            base,
            primary.index(),
            primary.second(),
            false,
            result_context,
        );
        /*
         * TODO reenable this? see test testNewAnalyzerAliasToNotReadyNestedClass2
        debug!(
            "Infer primary {} as {}",
            primary.short_debug(),
            result.format_short(self.i_s)
        );
        */
        result
    }

    fn infer_primary_or_primary_t_content(
        &mut self,
        base: Inferred,
        node_index: NodeIndex,
        second: PrimaryContent,
        is_target: bool,
        result_context: &mut ResultContext,
    ) -> Inferred {
        let node_ref = NodeRef::new(self.file, node_index);
        match second {
            PrimaryContent::Attribute(name) => {
                debug!("Lookup {}.{}", base.format_short(self.i_s), name.as_str());
                let lookup = base.as_type(self.i_s).lookup_with_error(
                    self.i_s,
                    node_ref,
                    name.as_str(),
                    &|i_s, t| add_attribute_error(i_s, node_ref, t, name),
                );
                match &lookup {
                    LookupResult::GotoName(link, inferred) => {
                        // TODO this is not correct, because there can be multiple runs, so setting
                        // it here can be overwritten.
                        self.file.points.set(
                            name.index(),
                            Point::new_redirect(link.file, link.node_index, Locality::Todo),
                        );
                    }
                    LookupResult::FileReference(file_index) => {
                        self.file.points.set(
                            name.index(),
                            Point::new_file_reference(*file_index, Locality::Todo),
                        );
                    }
                    LookupResult::UnknownName(_) | LookupResult::None => (),
                };
                lookup.into_inferred()
            }
            PrimaryContent::Execution(details) => {
                let f = self.file;
                let args = SimpleArguments::new(*self.i_s, f, node_index, details);
                base.execute_with_details(
                    self.i_s,
                    &args,
                    result_context,
                    OnTypeError::new(&on_argument_type_error),
                )
            }
            PrimaryContent::GetItem(slice_type) => {
                let f = self.file;
                // TODO enable this debug
                //debug!("Get Item on {}", base.format_short(self.i_s));
                base.get_item(
                    self.i_s,
                    &SliceType::new(f, node_index, slice_type),
                    result_context,
                )
            }
        }
    }

    pub fn infer_primary_or_atom(&mut self, p: PrimaryOrAtom) -> Inferred {
        match p {
            PrimaryOrAtom::Primary(primary) => {
                self.infer_primary(primary, &mut ResultContext::Unknown)
            }
            PrimaryOrAtom::Atom(atom) => self.infer_atom(atom, &mut ResultContext::Unknown),
        }
    }

    check_point_cache_with!(pub infer_atom, Self::_infer_atom, Atom, result_context);
    fn _infer_atom(&mut self, atom: Atom, result_context: &mut ResultContext) -> Inferred {
        let check_literal = |i_s, index, non_literal: Specific, literal| {
            let specific = if result_context.is_literal_context(i_s) {
                literal
            } else {
                non_literal
            };
            let point = Point::new_simple_specific(specific, Locality::Todo);
            Inferred::new_and_save(self.file, index, point)
        };

        use AtomContent::*;
        let specific = match atom.unpack() {
            Name(n) => return self.infer_name_reference(n),
            Int(i) => match self.parse_int(i, result_context) {
                Some(_) => {
                    let point = Point::new_simple_specific(Specific::IntLiteral, Locality::Todo);
                    return Inferred::new_and_save(self.file, i.index(), point);
                }
                None => Specific::Int,
            },
            Float(_) => Specific::Float,
            Complex(_) => Specific::Complex,
            Strings(s_o_b) => {
                for string in s_o_b.iter() {
                    if let StringType::FString(f) = string {
                        self.calc_fstring_diagnostics(f)
                    }
                }
                if let Some(s) = s_o_b.maybe_single_string_literal() {
                    return check_literal(
                        self.i_s,
                        s.index(),
                        Specific::String,
                        Specific::StringLiteral,
                    );
                } else {
                    Specific::String
                }
            }
            Bytes(b) => {
                return check_literal(self.i_s, b.index(), Specific::Bytes, Specific::BytesLiteral)
            }
            NoneLiteral => Specific::None,
            Bool(b) => {
                return check_literal(self.i_s, b.index(), Specific::Bool, Specific::BoolLiteral)
            }
            Ellipsis => Specific::Ellipsis,
            List(list) => {
                if let Some(result) = self.infer_list_literal_from_context(list, result_context) {
                    return result.save_redirect(self.i_s, self.file, atom.index());
                }
                let result = match list.unpack() {
                    elements @ StarLikeExpressionIterator::Elements(_) => {
                        self.create_list_or_set_generics(elements)
                    }
                    StarLikeExpressionIterator::Empty => GenericItem::TypeArgument(DbType::Any), // TODO shouldn't this be Never?
                };
                return Inferred::execute_db_type(
                    self.i_s,
                    DbType::Class(
                        self.i_s.db.python_state.builtins_point_link("list"),
                        Some(GenericsList::new_generics(Rc::new([result]))),
                    ),
                );
            }
            ListComprehension(_) => {
                debug!("TODO ANY INSTEAD OF ACTUAL VALUE IN COMPREHENSION");
                return Inferred::execute_db_type(
                    self.i_s,
                    DbType::Class(
                        self.i_s.db.python_state.builtins_point_link("list"),
                        Some(GenericsList::new_generics(Rc::new([
                            GenericItem::TypeArgument(DbType::Any),
                        ]))),
                    ),
                );
            }
            Dict(dict) => {
                let generics = self.create_dict_generics(dict, result_context);
                return Inferred::execute_db_type(
                    self.i_s,
                    DbType::Class(
                        self.i_s.db.python_state.builtins_point_link("dict"),
                        Some(generics),
                    ),
                );
            }
            DictComprehension(_) => todo!(),
            Set(set) => {
                if let elements @ StarLikeExpressionIterator::Elements(_) = set.unpack() {
                    return Inferred::create_instance(
                        self.i_s.db.python_state.builtins_point_link("set"),
                        Some(Rc::new([self.create_list_or_set_generics(elements)])),
                    )
                    .save_redirect(self.i_s, self.file, atom.index());
                } else {
                    todo!()
                }
            }
            SetComprehension(_) => todo!(),
            Tuple(tuple) => {
                return self.infer_tuple_iterator(tuple.iter()).save_redirect(
                    self.i_s,
                    self.file,
                    atom.index(),
                )
            }
            GeneratorComprehension(_) => Specific::GeneratorComprehension,
            YieldExpr(_) => todo!(),
            NamedExpression(named_expression) => {
                return self.infer_named_expression_with_context(named_expression, result_context)
            }
        };
        let point = Point::new_simple_specific(specific, Locality::Todo);
        Inferred::new_and_save(self.file, atom.index(), point)
    }

    fn infer_tuple_iterator<'x>(
        &mut self,
        iterator: impl Iterator<Item = StarLikeExpression<'x>>,
    ) -> Inferred {
        let mut generics = vec![];
        for e in iterator {
            match e {
                StarLikeExpression::NamedExpression(e) => generics.push(TypeOrTypeVarTuple::Type(
                    self.infer_named_expression(e).class_as_db_type(self.i_s),
                )),
                StarLikeExpression::Expression(e) => generics.push(TypeOrTypeVarTuple::Type(
                    self.infer_expression(e).class_as_db_type(self.i_s),
                )),
                StarLikeExpression::StarNamedExpression(e) => {
                    let inferred = self
                        .infer_expression_part(e.expression_part(), &mut ResultContext::Unknown);
                    let mut iterator =
                        inferred.save_and_iter(self.i_s, NodeRef::new(self.file, e.index()));
                    if iterator.len().is_some() {
                        while let Some(inf) = iterator.next(self.i_s) {
                            generics.push(TypeOrTypeVarTuple::Type(inf.class_as_db_type(self.i_s)))
                        }
                    } else {
                        todo!()
                    }
                }
                StarLikeExpression::StarExpression(e) => {
                    todo!()
                }
            }
        }
        let content = TupleContent::new_fixed_length(generics.into_boxed_slice());
        debug!(
            "Inferred: {}",
            content.format(&FormatData::new_short(self.i_s.db))
        );
        Inferred::execute_db_type(self.i_s, DbType::Tuple(Rc::new(content)))
    }

    check_point_cache_with!(pub infer_primary_target, Self::_infer_primary_target, PrimaryTarget);
    fn _infer_primary_target(&mut self, primary_target: PrimaryTarget) -> Inferred {
        let first = self.infer_primary_target_or_atom(primary_target.first());
        self.infer_primary_or_primary_t_content(
            first,
            primary_target.index(),
            primary_target.second(),
            true,
            &mut ResultContext::Unknown,
        )
        .save_redirect(self.i_s, self.file, primary_target.index())
    }

    fn infer_primary_target_or_atom(&mut self, t: PrimaryTargetOrAtom) -> Inferred {
        match t {
            PrimaryTargetOrAtom::Atom(atom) => self.infer_atom(atom, &mut ResultContext::Unknown),
            PrimaryTargetOrAtom::PrimaryTarget(p) => self.infer_primary_target(p),
        }
    }

    check_point_cache_with!(pub infer_name_reference, Self::_infer_name_reference, Name);
    fn _infer_name_reference(&mut self, name: Name) -> Inferred {
        // If it's not inferred already through the name binder, it's either a star import, a
        // builtin or really missing.
        let name_str = name.as_str();
        if let Some(point_link) = self.lookup_from_star_import(name_str, true) {
            self.file.points.set(
                name.index(),
                Point::new_redirect(point_link.file, point_link.node_index, Locality::Todo),
            );
            return self.infer_name_reference(name);
        }
        let point = if name_str == "reveal_type" {
            Point::new_simple_specific(Specific::RevealTypeFunction, Locality::Stmt)
        } else if let Some(link) = self
            .i_s
            .db
            .python_state
            .builtins()
            .lookup_global(name.as_str())
        {
            debug_assert!(link.file != self.file_index || link.node_index != name.index());
            link.into_point_redirect()
        } else {
            // The builtin module should really not have any issues.
            debug_assert!(
                self.file_index != self.i_s.db.python_state.builtins().file_index(),
                "{:?}",
                name
            );
            // TODO check star imports
            NodeRef::new(self.file, name.index()).add_typing_issue(
                self.i_s,
                IssueType::NameError {
                    name: Box::from(name.as_str()),
                },
            );
            if self
                .i_s
                .db
                .python_state
                .typing()
                .lookup_global(name_str)
                .is_some()
            {
                // TODO what about underscore or other vars?
                NodeRef::new(self.file, name.index()).add_typing_issue(
                    self.i_s,
                    IssueType::Note(
                        format!(
                            "Did you forget to import it from \"typing\"? \
                         (Suggestion: \"from typing import {name_str}\")",
                        )
                        .into(),
                    ),
                );
            }
            Point::new_unknown(self.file_index, Locality::Todo)
        };
        self.file.points.set(name.index(), point);
        debug_assert!(self.file.points.get(name.index()).calculated());
        self.infer_name_reference(name)
    }

    fn lookup_from_star_import(&mut self, name: &str, check_local: bool) -> Option<PointLink> {
        if !name.starts_with('_') {
            for star_import in self.file.star_imports.borrow().iter() {
                // TODO these feel a bit weird and do not include parent functions (when in a
                // closure)
                if !(star_import.scope == 0
                    || check_local
                        && self
                            .i_s
                            .current_function()
                            .map(|f| f.node_ref.node_index == star_import.scope)
                            .or_else(|| {
                                self.i_s
                                    .current_class()
                                    .map(|c| c.node_ref.node_index == star_import.scope)
                            })
                            .unwrap_or(false))
                {
                    continue;
                }
                if let Some(other_file) = star_import.to_file(self) {
                    if let Some(symbol) = other_file.symbol_table.lookup_symbol(name) {
                        return Some(PointLink::new(other_file.file_index(), symbol));
                    }
                    if let Some(l) = other_file
                        .inference(self.i_s)
                        .lookup_from_star_import(name, false)
                    {
                        return Some(l);
                    }
                }
            }
        }
        if let Some(super_file) = &self.file.super_file {
            if let Some(func) = self.i_s.current_function() {
                debug!("TODO lookup in func of sub file")
            } else if let Some(class) = self.i_s.current_class() {
                debug!("TODO lookup in class of sub file")
            }

            let super_file = self.i_s.db.loaded_python_file(*super_file);
            if let Some(symbol) = super_file.symbol_table.lookup_symbol(name) {
                return Some(PointLink::new(super_file.file_index(), symbol));
            }
            super_file
                .inference(self.i_s)
                .lookup_from_star_import(name, false)
        } else {
            None
        }
    }

    pub fn check_point_cache(&mut self, node_index: NodeIndex) -> Option<Inferred> {
        let point = self.file.points.get(node_index);
        let result = point
            .calculated()
            .then(|| match point.type_() {
                PointType::Redirect => {
                    let file_index = point.file_index();
                    let next_node_index = point.node_index();
                    debug_assert!(
                        file_index != self.file.file_index() || next_node_index != node_index,
                        "{file_index}:{node_index}"
                    );
                    let infer = |inference: &mut Inference| {
                        let point = inference.file.points.get(next_node_index);
                        inference
                            .check_point_cache(next_node_index)
                            .unwrap_or_else(|| {
                                let name =
                                    Name::maybe_by_index(&inference.file.tree, next_node_index);
                                if let Some(name) = name {
                                    inference.infer_name(name)
                                } else if let Some(expr) = Expression::maybe_by_index(
                                    &inference.file.tree,
                                    next_node_index,
                                ) {
                                    inference.infer_expression_without_cache(
                                        expr,
                                        &mut ResultContext::Unknown,
                                    )
                                } else if let Some(annotation) = Annotation::maybe_by_index(
                                    &inference.file.tree,
                                    next_node_index,
                                ) {
                                    todo!()
                                    // inference.cache_annotation(annotation)
                                } else {
                                    todo!(
                                        "{}",
                                        NodeRef::new(inference.file, next_node_index)
                                            .debug_info(self.i_s.db)
                                    )
                                }
                            })
                    };
                    if file_index == self.file_index {
                        infer(self)
                    } else {
                        infer(
                            &mut self
                                .i_s
                                .db
                                .loaded_python_file(file_index)
                                .inference(self.i_s),
                        )
                    }
                }
                PointType::Specific => match point.specific() {
                    specific @ (Specific::Param | Specific::SelfParam) => {
                        let name_def = NameDefinition::by_index(&self.file.tree, node_index);
                        // Performance: This could be improved by not needing to lookup all the
                        // parents all the time.
                        match name_def.function_or_lambda_ancestor().unwrap() {
                            FunctionOrLambda::Function(func) => {
                                let func = Function::new(
                                    NodeRef::new(self.file, func.index()),
                                    self.i_s.current_class().copied(),
                                );
                                func.type_vars(self.i_s);

                                if let Some(annotation) = name_def.maybe_param_annotation() {
                                    self.use_cached_annotation(annotation)
                                } else if let Some((function, args)) = self.i_s.current_execution()
                                {
                                    if specific == Specific::SelfParam {
                                        if func.node_ref.point().maybe_specific().unwrap()
                                            == Specific::ClassMethod
                                        {
                                            Inferred::execute_db_type(
                                                self.i_s,
                                                DbType::Type(Rc::new(DbType::Self_)),
                                            )
                                        } else {
                                            Inferred::new_saved(self.file, node_index, point)
                                        }
                                    } else {
                                        function.infer_param(self.i_s, node_index, args)
                                    }
                                } else if specific == Specific::SelfParam {
                                    todo!("Inferred::new_saved(self.file, node_index, point)")
                                } else {
                                    todo!("{:?} {:?}", self.i_s, specific)
                                }
                            }
                            FunctionOrLambda::Lambda(lambda) => {
                                for (i, p) in lambda.params().enumerate() {
                                    if p.name_definition().index() == node_index {
                                        if let Some(current_callable) =
                                            self.i_s.current_lambda_callable()
                                        {
                                            return match &current_callable.params {
                                                CallableParams::Simple(ps) => {
                                                    if let Some(p2) = ps.get(i) {
                                                        if let ParamSpecific::PositionalOnly(t) =
                                                            &p2.param_specific
                                                        {
                                                            if p.type_()
                                                                == ParamKind::PositionalOrKeyword
                                                            {
                                                                Inferred::execute_db_type(
                                                                    self.i_s,
                                                                    t.clone(),
                                                                )
                                                            } else {
                                                                todo!()
                                                            }
                                                        } else {
                                                            todo!()
                                                        }
                                                    } else {
                                                        todo!()
                                                    }
                                                }
                                                CallableParams::Any => Inferred::new_any(),
                                                CallableParams::WithParamSpec(_, _) => todo!(),
                                            };
                                        } else {
                                            todo!()
                                        }
                                    }
                                }
                                unreachable!()
                            }
                        }
                    }
                    Specific::LazyInferredFunction => {
                        let name_def = NameDefinition::by_index(&self.file.tree, node_index);
                        let FunctionOrLambda::Function(func) =
                            name_def.function_or_lambda_ancestor().unwrap() else
                        {
                            unreachable!();
                        };
                        let func = Function::new(
                            NodeRef::new(self.file, func.index()),
                            self.i_s.current_class().copied(),
                        );
                        // Caches the decorated inference on properly
                        func.decorated(self.i_s)
                    }
                    Specific::LazyInferredClass => {
                        // TODO this does not analyze decorators
                        let name_def = NameDefinition::by_index(&self.file.tree, node_index);
                        let class = name_def.expect_class_def();
                        // Avoid overwriting multi definitions
                        if self.file.points.get(name_def.name().index()).type_()
                            == PointType::MultiDefinition
                        {
                            todo!()
                        }
                        self.file.points.set(
                            name_def.index(),
                            Point::new_redirect(self.file_index, class.index(), Locality::Todo),
                        );
                        debug_assert!(self.file.points.get(node_index).calculated());
                        todo!();
                        //self.check_point_cache(node_index).unwrap()
                    }
                    _ => Inferred::new_saved(self.file, node_index, point),
                },
                PointType::MultiDefinition => {
                    // TODO for now we use Mypy's way of resolving multiple names, which means that
                    // it always uses the first name.
                    /*
                    let inferred = self.infer_name(Name::by_index(&self.file.tree, point.node_index()));
                    // Check for the cache of name_definition
                    let name_def = NameDefinition::by_index(&self.file.tree, node_index - 1);
                    inferred.union(self.infer_multi_definition(name_def))
                    */
                    self.check_point_cache(point.node_index())
                        .unwrap_or_else(|| {
                            self.infer_name(Name::by_index(&self.file.tree, point.node_index()))
                        })
                }
                PointType::Complex | PointType::Unknown | PointType::FileReference => {
                    Inferred::new_saved(self.file, node_index, point)
                }
                PointType::NodeAnalysis => {
                    panic!("Invalid state, should not happen {node_index:?}");
                }
            })
            .or_else(|| {
                if point.calculating() {
                    let node_ref = NodeRef::new(self.file, node_index);
                    node_ref.set_point(Point::new_simple_specific(Specific::Cycle, Locality::Todo));
                    Some(Inferred::new_cycle())
                } else {
                    None
                }
            });
        result
    }

    pub fn infer_name_by_index(&mut self, node_index: NodeIndex) -> Inferred {
        self.infer_name(Name::by_index(&self.file.tree, node_index))
    }

    pub fn infer_name(&mut self, name: Name) -> Inferred {
        let point = self.file.points.get(name.index());
        if point.calculated() && point.type_() == PointType::MultiDefinition {
            // We are trying to infer the name here. We don't have to follow the multi definition,
            // because the cache handling takes care of that.
            println!("TODO Is this branch still needed???");
            //self.infer_multi_definition(name.name_definition().unwrap())
        }
        if point.calculated() {
            if let Some(inf) = self.check_point_cache(name.index()) {
                return inf;
            }
        }
        match name.name_definition() {
            Some(name_def) => self.infer_name_definition(name_def),
            None => {
                todo!()
                /* TODO maybe use this???
                if name_def.is_reference() {
                    // References are not calculated by the name binder for star imports and
                    // lookups.
                    if let Some(primary) = name_def.maybe_primary_parent() {
                        return self.infer_primary(primary);
                    } else {
                        todo!(
                            "star import {} {name_def:?} {:?}",
                            self.file.file_path(self.i_s.db),
                            self.file.byte_to_line_column(name_def.start())
                        )
                    }
                } else {
                }
                */
            }
        }
    }

    check_point_cache_with!(pub infer_name_definition, Self::_infer_name_definition, NameDefinition);
    fn _infer_name_definition(&mut self, name_def: NameDefinition) -> Inferred {
        let stmt_like = name_def.expect_stmt_like_ancestor();

        if !self.file.points.get(stmt_like.index()).calculated() {
            match stmt_like {
                StmtLike::SimpleStmts(s) => {
                    self.cache_simple_stmts_name(s, NodeRef::new(self.file, name_def.index()));
                }
                StmtLike::Stmt(stmt) => {
                    self.cache_stmt_name(stmt, NodeRef::new(self.file, name_def.index()));
                }
                _ => todo!("{stmt_like:?}"),
            }
        }
        debug_assert!(
            self.file.points.get(name_def.index()).calculated(),
            "{name_def:?}",
        );
        self.infer_name_definition(name_def)
    }

    pub fn infer_by_node_index(&mut self, node_index: NodeIndex) -> Inferred {
        self.check_point_cache(node_index)
            .unwrap_or_else(|| todo!())
    }

    pub fn infer_comprehension(&mut self, comprehension: Comprehension) -> Inferred {
        let (expr, for_if_clauses) = comprehension.unpack();
        let clauses = for_if_clauses.iter();
        todo!()
    }
}

fn add_attribute_error<'db>(
    i_s: &InferenceState<'db, '_>,
    node_ref: NodeRef,
    t: &Type,
    name: Name,
) {
    let object = if matches!(t.maybe_db_type(), Some(DbType::Module(_))) {
        Box::from("Module")
    } else {
        format!("{:?}", t.format_short(i_s.db)).into()
    };
    node_ref.add_typing_issue(
        i_s,
        IssueType::AttributeError {
            object,
            name: Box::from(name.as_str()),
        },
    );
}
