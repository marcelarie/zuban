use parsa_python_ast::{
    FunctionDef, FunctionParent, NodeIndex, Param as ASTParam, ParamIterator as ASTParamIterator,
    ParamKind, ReturnAnnotation, ReturnOrYield,
};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::fmt;
use std::rc::Rc;

use super::{Instance, Module};
use crate::arguments::{Argument, ArgumentIterator, ArgumentKind, Arguments, KnownArguments};
use crate::database::{
    CallableContent, CallableParam, CallableParams, ClassGenerics, ComplexPoint, Database, DbType,
    DoubleStarredParamSpecific, GenericItem, GenericsList, Locality, Overload, ParamSpecUsage,
    ParamSpecific, Point, PointLink, Specific, StarredParamSpecific, StringSlice, TupleContent,
    TupleTypeArguments, TypeOrTypeVarTuple, TypeVar, TypeVarLike, TypeVarLikeUsage, TypeVarLikes,
    TypeVarManager, TypeVarName, TypeVarUsage, Variance,
};
use crate::diagnostics::IssueType;
use crate::file::{
    use_cached_annotation_type, File, PythonFile, TypeComputation, TypeComputationOrigin,
    TypeVarCallbackReturn,
};
use crate::inference_state::InferenceState;
use crate::inferred::Inferred;
use crate::matching::params::{
    InferrableParamIterator2, Param, WrappedDoubleStarred, WrappedParamSpecific, WrappedStarred,
};
use crate::matching::{
    calculate_class_init_type_vars_and_return, calculate_function_type_vars_and_return,
    ArgumentIndexWithParam, CalculatedTypeArguments, Generic, Generics, LookupResult, OnTypeError,
    ResultContext, SignatureMatch, Type,
};
use crate::node_ref::NodeRef;
use crate::type_helpers::Class;
use crate::{base_qualified_name, debug};

#[derive(Clone, Copy)]
pub struct Function<'a, 'class> {
    pub node_ref: NodeRef<'a>,
    pub class: Option<Class<'class>>,
}

impl fmt::Debug for Function<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Function")
            .field("file", self.node_ref.file)
            .field("node", &self.node())
            .finish()
    }
}

impl<'db: 'a, 'a, 'class> Function<'a, 'class> {
    // Functions use the following points:
    // - "def" to redirect to the first return/yield
    // - "function_def_parameters" to save calculated type vars
    // - "(" for decorator caching
    pub fn new(node_ref: NodeRef<'a>, class: Option<Class<'class>>) -> Self {
        Self { node_ref, class }
    }

    pub fn node(&self) -> FunctionDef<'a> {
        FunctionDef::by_index(&self.node_ref.file.tree, self.node_ref.node_index)
    }

    pub fn return_annotation(&self) -> Option<ReturnAnnotation> {
        self.node().return_annotation()
    }

    pub fn iter_inferrable_params<'b>(
        &self,
        db: &'db Database,
        args: &'b dyn Arguments<'db>,
        skip_first_param: bool,
    ) -> InferrableParamIterator<'db, 'b>
    where
        'a: 'b,
    {
        let mut params = self.node().params().iter();
        if skip_first_param {
            params.next();
        }
        InferrableParamIterator::new(db, self.node_ref.file, params, args.iter())
    }

    pub fn iter_args_with_params<'b, AI: Iterator<Item = Argument<'db, 'b>>>(
        &self,
        db: &'db Database,
        args: AI,
        skip_first_param: bool,
    ) -> InferrableParamIterator2<
        'db,
        'b,
        impl Iterator<Item = FunctionParam<'b>>,
        FunctionParam<'b>,
        AI,
    >
    where
        'a: 'b,
    {
        let mut params = self.iter_params();
        if skip_first_param {
            params.next();
        }
        InferrableParamIterator2::new(db, params, args)
    }

    pub fn infer_param(
        &self,
        i_s: &InferenceState<'db, '_>,
        param_name_def_index: NodeIndex,
        args: &dyn Arguments<'db>,
    ) -> Inferred {
        let func_node =
            FunctionDef::from_param_name_def_index(&self.node_ref.file.tree, param_name_def_index);
        //let temporary_args;
        //let temporary_func;
        let (check_args, func) = if func_node.index() == self.node_ref.node_index {
            (args, self)
        } else {
            debug!("TODO untyped param");
            return Inferred::new_unknown();
        };
        for param in func.iter_inferrable_params(i_s.db, check_args, false) {
            if param.is_at(param_name_def_index) {
                return param.infer(i_s).unwrap_or_else(Inferred::new_unknown);
            }
        }
        unreachable!("{param_name_def_index:?}");
    }

    fn execute_without_annotation(
        &self,
        i_s: &InferenceState<'db, '_>,
        args: &dyn Arguments<'db>,
    ) -> Inferred {
        if i_s.db.python_state.project.mypy_compatible {
            return Inferred::new_any();
        }
        if self.is_generator() {
            todo!("Maybe not check here, because this could be precalculated and cached");
        }
        let inner_i_s = i_s.with_func_and_args(self, args);
        for return_or_yield in self.iter_return_or_yield() {
            match return_or_yield {
                ReturnOrYield::Return(ret) =>
                // TODO multiple returns, this is an early exit
                {
                    if let Some(star_expressions) = ret.star_expressions() {
                        return self
                            .node_ref
                            .file
                            .inference(&inner_i_s)
                            .infer_star_expressions(star_expressions, &mut ResultContext::Unknown)
                            .resolve_untyped_function_return(&inner_i_s);
                    } else {
                        todo!()
                    }
                }
                ReturnOrYield::Yield(yield_expr) => unreachable!(),
            }
        }
        Inferred::new_none()
    }

    fn iter_return_or_yield(&self) -> ReturnOrYieldIterator<'a> {
        let def_point = self.node_ref.file.points.get(self.node_ref.node_index + 1);
        let first_return_or_yield = def_point.node_index();
        ReturnOrYieldIterator {
            file: self.node_ref.file,
            next_node_index: first_return_or_yield,
        }
    }

    fn is_generator(&self) -> bool {
        for return_or_yield in self.iter_return_or_yield() {
            if let ReturnOrYield::Yield(_) = return_or_yield {
                return true;
            }
        }
        false
    }

    pub fn type_vars(&self, i_s: &InferenceState<'db, '_>) -> Option<&'a TypeVarLikes> {
        // To save the generics just use the ( operator's storage.
        // + 1 for def; + 2 for name + 1 for (...)
        let type_var_reference = self.node_ref.add_to_node_index(4);
        if type_var_reference.point().calculated() {
            if let Some(complex) = type_var_reference.complex() {
                match complex {
                    ComplexPoint::TypeVarLikes(vars) => return Some(vars),
                    _ => unreachable!(),
                }
            }
            return None;
        }
        let func_node = self.node();
        let implicit_optional = i_s.db.python_state.project.implicit_optional;
        let mut inference = self.node_ref.file.inference(i_s);
        let in_result_type = Cell::new(false);
        let mut unbound_type_vars = vec![];
        let mut on_type_var = |i_s: &InferenceState,
                               manager: &TypeVarManager,
                               type_var: TypeVarLike,
                               current_callable: Option<_>| {
            self.class
                .and_then(|class| {
                    class
                        .type_vars(i_s)
                        .and_then(|t| t.find(type_var.clone(), class.node_ref.as_link()))
                        .map(TypeVarCallbackReturn::TypeVarLike)
                })
                .unwrap_or_else(|| {
                    if in_result_type.get()
                        && manager.position(&type_var).is_none()
                        && current_callable.is_none()
                    {
                        unbound_type_vars.push(type_var);
                    }
                    TypeVarCallbackReturn::NotFound
                })
        };
        let mut type_computation = TypeComputation::new(
            &mut inference,
            self.node_ref.as_link(),
            &mut on_type_var,
            TypeComputationOrigin::ParamTypeCommentOrAnnotation,
        );
        for param in func_node.params().iter() {
            if let Some(annotation) = param.annotation() {
                let mut is_implicit_optional = false;
                if implicit_optional {
                    if let Some(default) = param.default() {
                        if default.as_code() == "None" {
                            is_implicit_optional = true;
                        }
                    }
                }
                type_computation.cache_annotation(annotation, is_implicit_optional);
            }
        }
        if let Some(return_annot) = func_node.return_annotation() {
            in_result_type.set(true);
            type_computation.cache_return_annotation(return_annot);
        }
        let type_vars = type_computation.into_type_vars(|inf, recalculate_type_vars| {
            for param in func_node.params().iter() {
                if let Some(annotation) = param.annotation() {
                    inf.recalculate_annotation_type_vars(annotation.index(), recalculate_type_vars);
                }
            }
            if let Some(return_annot) = func_node.return_annotation() {
                inf.recalculate_annotation_type_vars(return_annot.index(), recalculate_type_vars);
            }
        });
        if !unbound_type_vars.is_empty() {
            if let DbType::TypeVar(t) = self.result_type(i_s).as_ref() {
                if unbound_type_vars.contains(&TypeVarLike::TypeVar(t.type_var.clone())) {
                    let node_ref = NodeRef::new(
                        self.node_ref.file,
                        func_node.return_annotation().unwrap().expression().index(),
                    );
                    node_ref.add_typing_issue(i_s, IssueType::TypeVarInReturnButNotArgument);
                    if let Some(bound) = t.type_var.bound.as_ref() {
                        node_ref.add_typing_issue(
                            i_s,
                            IssueType::Note(
                                format!(
                                    "Consider using the upper bound \"{}\" instead",
                                    Type::new(bound).format_short(i_s.db)
                                )
                                .into(),
                            ),
                        );
                    }
                }
            }
        }
        match type_vars.len() {
            0 => type_var_reference.set_point(Point::new_node_analysis(Locality::Todo)),
            _ => type_var_reference
                .insert_complex(ComplexPoint::TypeVarLikes(type_vars), Locality::Todo),
        }
        debug_assert!(type_var_reference.point().calculated());
        self.type_vars(i_s)
    }

    fn remap_param_spec(
        &self,
        i_s: &InferenceState,
        mut pre_params: Vec<CallableParam>,
        usage: &ParamSpecUsage,
    ) -> CallableParams {
        let into_types = |mut types: Vec<_>, pre_params: Vec<CallableParam>| {
            types.extend(
                pre_params
                    .into_iter()
                    .map(|p| p.param_specific.expect_positional_db_type()),
            );
            Rc::from(types)
        };
        match self.class {
            Some(c) if c.node_ref.as_link() == usage.in_definition => match c
                .generics()
                .nth_usage(i_s.db, &TypeVarLikeUsage::ParamSpec(Cow::Borrowed(usage)))
            {
                Generic::ParamSpecArgument(p) => match p.into_owned().params {
                    CallableParams::Any => CallableParams::Any,
                    CallableParams::Simple(params) => {
                        // Performance issue: Rc -> Vec check https://github.com/rust-lang/rust/issues/93610#issuecomment-1528108612
                        pre_params.extend(params.iter().cloned());
                        CallableParams::Simple(Rc::from(pre_params))
                    }
                    CallableParams::WithParamSpec(pre, p) => {
                        // Performance issue: Rc -> Vec check https://github.com/rust-lang/rust/issues/93610#issuecomment-1528108612
                        let types: Vec<_> = Vec::from(pre.as_ref());
                        CallableParams::WithParamSpec(into_types(types, pre_params), p)
                    }
                },
                _ => unreachable!(),
            },
            _ => {
                let types = vec![];
                CallableParams::WithParamSpec(into_types(types, pre_params), usage.clone())
            }
        }
    }

    pub fn decorated(&self, i_s: &InferenceState<'db, '_>) -> Inferred {
        // To save the generics just use the ( operator's storage.
        // + 1 for def; + 2 for name + 1 for (...) + 1 for (
        let decorator_ref = self.node_ref.add_to_node_index(5);
        if decorator_ref.point().calculated() {
            return self
                .node_ref
                .file
                .inference(i_s)
                .check_point_cache(decorator_ref.node_index)
                .unwrap();
        }
        let node = self.node();
        let FunctionParent::Decorated(decorated) = node.parent() else {
            unreachable!();
        };
        let mut new_inf = Inferred::from_saved_node_ref(self.node_ref);
        for decorator in decorated.decorators().iter_reverse() {
            let i = self
                .node_ref
                .file
                .inference(i_s)
                .infer_named_expression(decorator.named_expression());
            // TODO check if it's an function without a return annotation and
            // abort in that case.
            new_inf = i.execute(
                i_s,
                &KnownArguments::new(
                    &new_inf,
                    NodeRef::new(self.node_ref.file, decorator.index()),
                ),
            );
        }
        if let DbType::Callable(callable_content) = new_inf.as_type(i_s).as_ref() {
            let mut callable_content = (**callable_content).clone();
            callable_content.name = Some(self.name_string_slice());
            callable_content.class_name = self.class.map(|c| c.name_string_slice());
            Inferred::from_type(DbType::Callable(Rc::new(callable_content))).save_redirect(
                i_s,
                decorator_ref.file,
                decorator_ref.node_index,
            )
        } else {
            new_inf.save_redirect(i_s, decorator_ref.file, decorator_ref.node_index)
        }
    }

    pub fn as_callable(
        &self,
        i_s: &InferenceState,
        first: FirstParamProperties,
    ) -> CallableContent {
        let mut params = self.iter_params().peekable();
        let mut self_type_var_usage = None;
        let defined_at = self.node_ref.as_link();
        let mut type_vars = self.type_vars(i_s).cloned(); // Cache annotation types
        let mut type_vars = if let Some(type_vars) = type_vars.take() {
            type_vars.into_vec()
        } else {
            vec![]
        };
        match first {
            FirstParamProperties::MethodAccessedOnClass => {
                let mut needs_self_type_variable =
                    self.result_type(i_s).has_explicit_self_type(i_s.db);
                for param in self.iter_params().skip(1) {
                    if let Some(t) = param.annotation(i_s) {
                        needs_self_type_variable |= t.has_explicit_self_type(i_s.db);
                    }
                }
                if needs_self_type_variable {
                    let self_type_var = Rc::new(TypeVar {
                        name_string: TypeVarName::Self_,
                        restrictions: Box::new([]),
                        bound: Some(self.class.unwrap().as_db_type(i_s.db)),
                        variance: Variance::Invariant,
                    });
                    self_type_var_usage = Some(TypeVarUsage {
                        in_definition: defined_at,
                        type_var: self_type_var.clone(),
                        index: 0.into(),
                    });
                    type_vars.insert(0, TypeVarLike::TypeVar(self_type_var));
                }
            }
            FirstParamProperties::Skip(_) => {
                params.next();
            }
            FirstParamProperties::None => (),
        }
        let self_type_var_usage = self_type_var_usage.as_ref();

        let as_db_type = |i_s: &InferenceState, t: Type| {
            let Some(func_class) = self.class else {
                return t.as_db_type()
            };
            t.replace_type_var_likes_and_self(
                i_s.db,
                &mut |mut usage| {
                    let in_definition = usage.in_definition();
                    if in_definition == func_class.node_ref.as_link() {
                        func_class
                            .generics()
                            .nth_usage(i_s.db, &usage)
                            .into_generic_item(i_s.db)
                    } else if in_definition == defined_at {
                        if self_type_var_usage.is_some() {
                            usage.add_to_index(1);
                        }
                        usage.into_generic_item()
                    } else {
                        // This can happen for example if the return value is a Callable with its
                        // own type vars.
                        usage.into_generic_item()
                    }
                },
                &mut || {
                    if let Some(self_type_var_usage) = self_type_var_usage {
                        DbType::TypeVar(self_type_var_usage.clone())
                    } else if let FirstParamProperties::Skip(instance) = first {
                        instance.class.as_db_type(i_s.db)
                    } else {
                        DbType::Self_
                    }
                },
            )
        };
        let mut callable =
            self.internal_as_db_type(i_s, params, self_type_var_usage.is_some(), as_db_type);
        callable.type_vars = (!type_vars.is_empty()).then(|| TypeVarLikes::from_vec(type_vars));
        callable
    }

    pub fn as_db_type(&self, i_s: &InferenceState, first: FirstParamProperties) -> DbType {
        DbType::Callable(Rc::new(self.as_callable(i_s, first)))
    }

    pub fn as_type(&self, i_s: &InferenceState<'db, '_>) -> Type<'a> {
        Type::owned(self.as_db_type(i_s, FirstParamProperties::None))
    }

    pub fn classmethod_as_db_type(
        &self,
        i_s: &InferenceState,
        class: &Class,
        class_generics_not_defined_yet: bool,
    ) -> DbType {
        let mut class_method_type_var_usage = None;
        let mut params = self.iter_params();
        let defined_at = self.node_ref.as_link();
        let mut type_vars = self.type_vars(i_s).cloned(); // Cache annotation types
        let mut type_vars = if let Some(type_vars) = type_vars.take() {
            type_vars.into_vec()
        } else {
            vec![]
        };
        if let Some(param) = params.next() {
            if let Some(t) = param.annotation(i_s) {
                match t.as_ref() {
                    DbType::Type(t) => {
                        if let DbType::TypeVar(usage) = t.as_ref() {
                            class_method_type_var_usage = Some(usage.clone());
                            type_vars.remove(0);
                        }
                    }
                    _ => todo!(),
                }
            }
        }

        let type_vars = RefCell::new(type_vars);

        let ensure_classmethod_type_var_like = |tvl| {
            let pos = type_vars.borrow().iter().position(|t| t == &tvl);
            let position = pos.unwrap_or_else(|| {
                type_vars.borrow_mut().push(tvl.clone());
                type_vars.borrow().len() - 1
            });
            tvl.as_type_var_like_usage(position.into(), defined_at)
                .into_generic_item()
        };
        let get_class_method_class = || {
            if class_generics_not_defined_yet {
                DbType::Class(
                    class.node_ref.as_link(),
                    match class.use_cached_type_vars(i_s.db) {
                        Some(tvls) => ClassGenerics::List(GenericsList::new_generics(
                            tvls.iter()
                                .map(|tvl| ensure_classmethod_type_var_like(tvl.clone()))
                                .collect(),
                        )),
                        None => ClassGenerics::None,
                    },
                )
            } else {
                class.as_db_type(i_s.db)
            }
        };
        let as_db_type = |i_s: &InferenceState, t: Type| {
            let Some(func_class) = self.class else {
                return t.as_db_type()
            };
            t.replace_type_var_likes_and_self(
                i_s.db,
                &mut |mut usage| {
                    let in_definition = usage.in_definition();
                    if in_definition == func_class.node_ref.as_link() {
                        let result = func_class
                            .generics()
                            .nth_usage(i_s.db, &usage)
                            .into_generic_item(i_s.db);
                        // We need to remap again, because in generics of classes will be
                        // generic in the function of the classmethod, see for example
                        // `testGenericClassMethodUnboundOnClass`.
                        if class_generics_not_defined_yet {
                            return result.replace_type_var_likes(
                                i_s.db,
                                &mut |usage| {
                                    if usage.in_definition() == class.node_ref.as_link() {
                                        let tvl = usage.as_type_var_like();
                                        ensure_classmethod_type_var_like(tvl)
                                    } else {
                                        usage.into_generic_item()
                                    }
                                },
                                &mut || todo!(),
                            );
                        }
                        result
                    } else if in_definition == defined_at {
                        if let Some(u) = &class_method_type_var_usage {
                            if u.index == usage.index() {
                                return GenericItem::TypeArgument(get_class_method_class());
                            } else {
                                usage.add_to_index(-1);
                                todo!()
                            }
                        }
                        usage.into_generic_item()
                    } else {
                        // This can happen for example if the return value is a Callable with its
                        // own type vars.
                        usage.into_generic_item()
                    }
                },
                #[allow(clippy::redundant_closure)] // This is a clippy bug
                &mut || get_class_method_class(),
            )
        };
        let mut callable = self.internal_as_db_type(i_s, params, false, as_db_type);
        let type_vars = type_vars.into_inner();
        callable.type_vars = (!type_vars.is_empty()).then(|| TypeVarLikes::from_vec(type_vars));
        DbType::Callable(Rc::new(callable))
    }

    fn internal_as_db_type(
        &self,
        i_s: &InferenceState,
        params: impl Iterator<Item = FunctionParam<'a>>,
        has_self_type_var_usage: bool,
        mut as_db_type: impl FnMut(&InferenceState, Type) -> DbType,
    ) -> CallableContent {
        let mut params = params.peekable();
        let result_type = self.result_type(i_s);
        let result_type = as_db_type(i_s, result_type);

        let return_result = |params| CallableContent {
            name: Some(self.name_string_slice()),
            class_name: self.class.map(|c| c.name_string_slice()),
            defined_at: self.node_ref.as_link(),
            params,
            type_vars: None,
            result_type,
        };

        let mut new_params = vec![];
        let mut had_param_spec_args = false;
        let file_index = self.node_ref.file_index();
        while let Some(p) = params.next() {
            let specific = p.specific(i_s.db);
            let mut as_t = |t: Option<Type>| {
                t.map(|t| as_db_type(i_s, t)).unwrap_or({
                    let name_ref =
                        NodeRef::new(self.node_ref.file, p.param.name_definition().index());
                    if name_ref.point().maybe_specific() == Some(Specific::SelfParam) {
                        if has_self_type_var_usage {
                            DbType::Self_
                        } else {
                            i_s.current_class().unwrap().as_db_type(i_s.db)
                        }
                    } else {
                        DbType::Any
                    }
                })
            };
            let param_specific = match specific {
                WrappedParamSpecific::PositionalOnly(t) => ParamSpecific::PositionalOnly(as_t(t)),
                WrappedParamSpecific::PositionalOrKeyword(t) => {
                    ParamSpecific::PositionalOrKeyword(as_t(t))
                }
                WrappedParamSpecific::KeywordOnly(t) => ParamSpecific::KeywordOnly(as_t(t)),
                WrappedParamSpecific::Starred(WrappedStarred::ArbitraryLength(t)) => {
                    ParamSpecific::Starred(StarredParamSpecific::ArbitraryLength(as_t(t)))
                }
                WrappedParamSpecific::Starred(WrappedStarred::ParamSpecArgs(u1)) => {
                    match params.peek().map(|p| p.specific(i_s.db)) {
                        Some(WrappedParamSpecific::DoubleStarred(
                            WrappedDoubleStarred::ParamSpecKwargs(u2),
                        )) if u1 == u2 => {
                            had_param_spec_args = true;
                            continue;
                        }
                        _ => todo!(),
                    }
                }
                WrappedParamSpecific::DoubleStarred(WrappedDoubleStarred::ValueType(t)) => {
                    ParamSpecific::DoubleStarred(DoubleStarredParamSpecific::ValueType(as_t(t)))
                }
                WrappedParamSpecific::DoubleStarred(WrappedDoubleStarred::ParamSpecKwargs(u)) => {
                    if !had_param_spec_args {
                        todo!()
                    }
                    return return_result(self.remap_param_spec(i_s, new_params, u));
                }
            };
            new_params.push(CallableParam {
                param_specific,
                has_default: p.has_default(),
                name: Some({
                    let n = p.param.name_definition();
                    StringSlice::new(file_index, n.start(), n.end())
                }),
            });
        }
        return_result(CallableParams::Simple(Rc::from(new_params)))
    }

    pub fn name_string_slice(&self) -> StringSlice {
        let name = self.node().name();
        StringSlice::new(self.node_ref.file_index(), name.start(), name.end())
    }

    pub fn iter_params(&self) -> impl Iterator<Item = FunctionParam<'a>> {
        self.node().params().iter().map(|param| FunctionParam {
            file: self.node_ref.file,
            param,
        })
    }

    pub fn first_param_annotation_type(&self, i_s: &InferenceState<'db, '_>) -> Option<Type> {
        self.iter_params().next().unwrap().annotation(i_s)
    }

    pub(super) fn execute_internal(
        &self,
        i_s: &InferenceState<'db, '_>,
        args: &dyn Arguments<'db>,
        on_type_error: OnTypeError<'db, '_>,
        class: Option<&Class>,
        result_context: &mut ResultContext,
    ) -> Inferred {
        let return_annotation = self.return_annotation();
        let func_type_vars = return_annotation.and_then(|_| self.type_vars(i_s));
        let calculated_type_vars = calculate_function_type_vars_and_return(
            i_s,
            class,
            *self,
            args.iter(),
            &|| args.as_node_ref(),
            false,
            func_type_vars,
            self.node_ref.as_link(),
            result_context,
            Some(on_type_error),
        );
        if let Some(return_annotation) = return_annotation {
            self.apply_type_args_in_return_annotation(
                i_s,
                calculated_type_vars,
                class,
                return_annotation,
            )
        } else {
            self.execute_without_annotation(i_s, args)
        }
    }

    fn apply_type_args_in_return_annotation(
        &self,
        i_s: &InferenceState<'db, '_>,
        calculated_type_vars: CalculatedTypeArguments,
        class: Option<&Class>,
        return_annotation: ReturnAnnotation,
    ) -> Inferred {
        // We check first if type vars are involved, because if they aren't we can reuse the
        // annotation expression cache instead of recalculating.
        if NodeRef::new(self.node_ref.file, return_annotation.index())
            .point()
            .maybe_specific()
            == Some(Specific::AnnotationOrTypeCommentWithTypeVars)
        {
            debug!(
                "Inferring generics for {}{}",
                self.class
                    .map(|c| format!("{}.", c.format_short(i_s.db)))
                    .unwrap_or_else(|| "".to_owned()),
                self.name()
            );
            self.node_ref
                .file
                .inference(i_s)
                .use_cached_return_annotation_type(return_annotation)
                .execute_and_resolve_type_vars(
                    i_s,
                    self.class.as_ref(),
                    class,
                    &calculated_type_vars,
                )
        } else {
            self.node_ref
                .file
                .inference(i_s)
                .use_cached_return_annotation(return_annotation)
        }
    }

    pub fn diagnostic_string(&self, class: Option<&Class>) -> Box<str> {
        match class {
            Some(class) => {
                if self.name() == "__init__" {
                    format!("{:?}", class.name()).into()
                } else {
                    format!("{:?} of {:?}", self.name(), self.class.unwrap().name()).into()
                }
            }
            None => format!("{:?}", self.name()).into(),
        }
    }

    pub fn result_type(&self, i_s: &InferenceState<'db, '_>) -> Type<'a> {
        self.return_annotation()
            .map(|a| {
                self.node_ref
                    .file
                    .inference(i_s)
                    .use_cached_return_annotation_type(a)
            })
            .unwrap_or_else(|| Type::new(&DbType::Any))
    }

    fn format_overload_variant(&self, i_s: &InferenceState, is_init: bool) -> Box<str> {
        // Make sure annotations/type vars are calculated
        self.type_vars(i_s);

        let node = self.node();
        let ret = match node.return_annotation() {
            Some(annotation) => self
                .node_ref
                .file
                .inference(i_s)
                .use_cached_return_annotation_type(annotation),
            None => Type::new(&DbType::Any),
        };
        format_pretty_function_like(
            i_s,
            self.class,
            self.class.is_some()
                && self
                    .iter_params()
                    .next()
                    .is_some_and(|t| t.annotation(i_s).is_none()),
            self.name(),
            self.type_vars(i_s),
            self.iter_params(),
            (!is_init).then_some(ret),
        )
    }

    pub fn execute(
        &self,
        i_s: &InferenceState<'db, '_>,
        args: &dyn Arguments<'db>,
        result_context: &mut ResultContext,
        on_type_error: OnTypeError<'db, '_>,
    ) -> Inferred {
        if let Some(class) = &self.class {
            self.execute_internal(
                &i_s.with_class_context(class),
                args,
                on_type_error,
                Some(class),
                result_context,
            )
        } else {
            self.execute_internal(i_s, args, on_type_error, None, result_context)
        }
    }

    pub fn lookup(
        &self,
        i_s: &InferenceState,
        node_ref: Option<NodeRef>,
        name: &str,
    ) -> LookupResult {
        debug!("TODO Function lookup");
        LookupResult::None
    }

    pub fn qualified_name(&self, db: &'a Database) -> String {
        base_qualified_name!(self, db, self.name())
    }

    fn module(&self) -> Module<'a> {
        Module::new(self.node_ref.file)
    }

    pub fn name(&self) -> &str {
        let func = FunctionDef::by_index(&self.node_ref.file.tree, self.node_ref.node_index);
        func.name().as_str()
    }
}

#[derive(Copy, Clone)]
pub enum FirstParamProperties<'a> {
    Skip(&'a Instance<'a>),
    MethodAccessedOnClass,
    None,
}

struct ReturnOrYieldIterator<'a> {
    file: &'a PythonFile,
    next_node_index: NodeIndex,
}

impl<'a> Iterator for ReturnOrYieldIterator<'a> {
    type Item = ReturnOrYield<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.next_node_index == 0 {
            None
        } else {
            let point = self.file.points.get(self.next_node_index);
            let index = self.next_node_index;
            self.next_node_index = point.node_index();
            Some(ReturnOrYield::by_index(&self.file.tree, index - 1))
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FunctionParam<'x> {
    file: &'x PythonFile,
    param: ASTParam<'x>,
}

impl<'db: 'x, 'x> FunctionParam<'x> {
    pub fn annotation(&self, i_s: &InferenceState<'db, '_>) -> Option<Type<'x>> {
        self.param
            .annotation()
            .map(|annotation| use_cached_annotation_type(i_s.db, self.file, annotation))
    }
}

impl<'x> Param<'x> for FunctionParam<'x> {
    fn has_default(&self) -> bool {
        self.param.default().is_some()
    }

    fn name(&self, db: &'x Database) -> Option<&str> {
        Some(self.param.name_definition().as_code())
    }

    fn specific<'db: 'x>(&self, db: &'db Database) -> WrappedParamSpecific<'x> {
        let t = self
            .param
            .annotation()
            .map(|annotation| use_cached_annotation_type(db, self.file, annotation));
        fn dbt<'a>(t: Option<&Type<'a>>) -> Option<&'a DbType> {
            t.and_then(|t| t.maybe_borrowed_db_type())
        }
        match self.kind(db) {
            ParamKind::PositionalOnly => WrappedParamSpecific::PositionalOnly(t),
            ParamKind::PositionalOrKeyword => WrappedParamSpecific::PositionalOrKeyword(t),
            ParamKind::KeywordOnly => WrappedParamSpecific::KeywordOnly(t),
            ParamKind::Starred => WrappedParamSpecific::Starred(match dbt(t.as_ref()) {
                Some(DbType::ParamSpecArgs(ref param_spec_usage)) => {
                    WrappedStarred::ParamSpecArgs(param_spec_usage)
                }
                _ => WrappedStarred::ArbitraryLength(t.map(|t| {
                    let DbType::Tuple(t) = t.maybe_borrowed_db_type().unwrap() else {
                        unreachable!()
                    };
                    match t.args.as_ref().unwrap() {
                        TupleTypeArguments::FixedLength(..) => todo!(),
                        TupleTypeArguments::ArbitraryLength(t) => Type::new(t),
                    }
                }))
            }),
            ParamKind::DoubleStarred => WrappedParamSpecific::DoubleStarred(match dbt(t.as_ref()) {
                Some(DbType::ParamSpecKwargs(param_spec_usage)) => {
                    WrappedDoubleStarred::ParamSpecKwargs(param_spec_usage)
                }
                _ => WrappedDoubleStarred::ValueType(t.map(|t| {
                    let DbType::Class(_, ClassGenerics::List(generics)) = t.maybe_borrowed_db_type().unwrap() else {
                        unreachable!()
                    };
                    let GenericItem::TypeArgument(t) = &generics[1.into()] else {
                        unreachable!();
                    };
                    Type::new(t)
                }))
            })
        }
    }

    fn func_annotation_link(&self) -> Option<PointLink> {
        self.param
            .annotation()
            .map(|a| PointLink::new(self.file.file_index(), a.index()))
    }

    fn kind(&self, db: &Database) -> ParamKind {
        let mut t = self.param.type_();
        if t == ParamKind::PositionalOrKeyword
            && db.python_state.project.mypy_compatible
            && is_private(self.param.name_definition().as_code())
        {
            // Mypy treats __ params as positional only
            t = ParamKind::PositionalOnly
        }
        t
    }
}

pub fn is_private(name: &str) -> bool {
    name.starts_with("__") && !name.ends_with("__")
}

pub struct InferrableParamIterator<'db, 'a> {
    db: &'db Database,
    arguments: ArgumentIterator<'db, 'a>,
    params: ASTParamIterator<'a>,
    file: &'a PythonFile,
    unused_keyword_arguments: Vec<Argument<'db, 'a>>,
}

impl<'db, 'a> InferrableParamIterator<'db, 'a> {
    fn new(
        db: &'db Database,
        file: &'a PythonFile,
        params: ASTParamIterator<'a>,
        arguments: ArgumentIterator<'db, 'a>,
    ) -> Self {
        Self {
            db,
            arguments,
            file,
            params,
            unused_keyword_arguments: vec![],
        }
    }

    fn next_argument(&mut self, param: &FunctionParam<'a>) -> ParamInput<'db, 'a> {
        for (i, unused) in self.unused_keyword_arguments.iter().enumerate() {
            match &unused.kind {
                ArgumentKind::Keyword { key, .. } => {
                    if *key == param.name(self.db).unwrap() {
                        return ParamInput::Argument(self.unused_keyword_arguments.remove(i));
                    }
                }
                _ => unreachable!(),
            }
        }
        match param.kind(self.db) {
            ParamKind::PositionalOrKeyword => {
                for argument in &mut self.arguments {
                    match argument.kind {
                        ArgumentKind::Keyword { key, .. } => {
                            if key == param.name(self.db).unwrap() {
                                return ParamInput::Argument(argument);
                            } else {
                                self.unused_keyword_arguments.push(argument);
                            }
                        }
                        _ => return ParamInput::Argument(argument),
                    }
                }
            }
            ParamKind::KeywordOnly => {
                for argument in &mut self.arguments {
                    match argument.kind {
                        ArgumentKind::Keyword { key, .. } => {
                            if key == param.name(self.db).unwrap() {
                                return ParamInput::Argument(argument);
                            } else {
                                self.unused_keyword_arguments.push(argument);
                            }
                        }
                        _ => todo!(),
                    }
                }
            }
            ParamKind::PositionalOnly => todo!(),
            ParamKind::Starred => {
                let mut args = vec![];
                for argument in &mut self.arguments {
                    if argument.is_keyword_argument() {
                        self.unused_keyword_arguments.push(argument);
                        break;
                    }
                    args.push(argument)
                }
                return ParamInput::Tuple(args.into_boxed_slice());
            }
            ParamKind::DoubleStarred => todo!(),
        }
        for argument in &mut self.arguments {
            // TODO check param type here and make sure that it makes sense.
        }
        ParamInput::None
    }
}

impl<'db, 'a> Iterator for InferrableParamIterator<'db, 'a> {
    type Item = InferrableParam<'db, 'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.params.next().map(|param| {
            let param = FunctionParam {
                file: self.file,
                param,
            };
            let argument = self.next_argument(&param);
            InferrableParam { param, argument }
        })
    }
}

#[derive(Debug)]
enum ParamInput<'db, 'a> {
    Argument(Argument<'db, 'a>),
    Tuple(Box<[Argument<'db, 'a>]>),
    None,
}

#[derive(Debug)]
pub struct InferrableParam<'db, 'a> {
    pub param: FunctionParam<'a>,
    argument: ParamInput<'db, 'a>,
}

impl<'db> InferrableParam<'db, '_> {
    fn is_at(&self, index: NodeIndex) -> bool {
        self.param.param.name_definition().index() == index
    }

    pub fn has_argument(&self) -> bool {
        !matches!(self.argument, ParamInput::None)
    }

    pub fn infer(&self, i_s: &InferenceState<'db, '_>) -> Option<Inferred> {
        if !matches!(&self.argument, ParamInput::None) {
            debug!("Infer param {:?}", self.param.name(i_s.db));
        }
        match &self.argument {
            ParamInput::Argument(arg) => Some(arg.infer(i_s, &mut ResultContext::Unknown)),
            ParamInput::Tuple(args) => {
                let mut list = vec![];
                for arg in args.iter() {
                    if arg.in_args_or_kwargs_and_arbitrary_len() {
                        todo!()
                    }
                    list.push(TypeOrTypeVarTuple::Type(
                        arg.infer(i_s, &mut ResultContext::Unknown)
                            .class_as_db_type(i_s),
                    ))
                }
                let t = TupleContent::new_fixed_length(list.into_boxed_slice());
                Some(Inferred::from_type(DbType::Tuple(Rc::new(t))))
            }
            ParamInput::None => None,
        }
    }
}

#[derive(Debug)]
pub struct OverloadedFunction<'a> {
    node_ref: NodeRef<'a>,
    overload: &'a Overload,
    class: Option<Class<'a>>,
}

pub enum OverloadResult<'a> {
    Single(Function<'a, 'a>),
    Union(DbType),
    NotFound,
}

#[derive(Debug)]
pub enum UnionMathResult {
    FirstSimilarIndex(usize),
    Match {
        first_similar_index: usize,
        result: DbType,
    },
    NoMatch,
}

impl<'db: 'a, 'a> OverloadedFunction<'a> {
    pub fn new(node_ref: NodeRef<'a>, overload: &'a Overload, class: Option<Class<'a>>) -> Self {
        Self {
            node_ref,
            overload,
            class,
        }
    }

    pub(super) fn find_matching_function(
        &self,
        i_s: &InferenceState<'db, '_>,
        args: &dyn Arguments<'db>,
        class: Option<&Class>,
        search_init: bool, // TODO this feels weird, maybe use a callback?
        result_context: &mut ResultContext,
        on_type_error: OnTypeError<'db, '_>,
    ) -> OverloadResult<'a> {
        let match_signature = |i_s: &InferenceState<'db, '_>,
                               result_context: &mut ResultContext,
                               function: Function<'a, 'a>| {
            let func_type_vars = function.type_vars(i_s);
            if search_init {
                calculate_class_init_type_vars_and_return(
                    i_s,
                    class.unwrap(),
                    function,
                    args.iter(),
                    &|| args.as_node_ref(),
                    result_context,
                    None,
                )
            } else {
                calculate_function_type_vars_and_return(
                    i_s,
                    class,
                    function,
                    args.iter(),
                    &|| args.as_node_ref(),
                    false,
                    func_type_vars,
                    function.node_ref.as_link(),
                    result_context,
                    None,
                )
            }
        };
        let has_already_calculated_class_generics = search_init
            && !matches!(
                class.unwrap().generics(),
                Generics::None | Generics::NotDefinedYet
            );
        let mut first_arbitrary_length_not_handled = None;
        let mut first_similar = None;
        let mut multi_any_match: Option<(_, _, Box<_>)> = None;
        let mut had_error_in_func = None;
        for (i, link) in self.overload.functions.iter().enumerate() {
            let function = Function::new(NodeRef::from_link(i_s.db, *link), self.class);
            let (calculated_type_args, had_error) =
                i_s.do_overload_check(|i_s| match_signature(i_s, result_context, function));
            if had_error && had_error_in_func.is_none() {
                had_error_in_func = Some(function);
            }
            match calculated_type_args.matches {
                SignatureMatch::True {
                    arbitrary_length_handled,
                } => {
                    if multi_any_match.is_some() {
                        // This means that there was an explicit any in a param.
                        return OverloadResult::NotFound;
                    } else if !arbitrary_length_handled {
                        if first_arbitrary_length_not_handled.is_none() {
                            first_arbitrary_length_not_handled =
                                Some((calculated_type_args.type_arguments, function));
                        }
                    } else {
                        debug!(
                            "Decided overload for {} (called on #{}): {:?}",
                            self.name(),
                            args.as_node_ref().line(),
                            function.node().short_debug()
                        );
                        args.reset_cache();
                        return OverloadResult::Single(function);
                    }
                }
                SignatureMatch::TrueWithAny { argument_indices } => {
                    // TODO there could be three matches or more?
                    // TODO maybe merge list[any] and list[int]
                    if let Some((_, _, ref old_indices)) = multi_any_match {
                        // If multiple signatures match because of Any, we should just return
                        // without an error message, there is no clear choice, i.e. it's ambiguous,
                        // but there should also not be an error.
                        if are_any_arguments_ambiguous_in_overload(
                            i_s.db,
                            old_indices,
                            &argument_indices,
                        ) {
                            if had_error {
                                args.reset_cache();
                                // Need to run the whole thing again to generate errors, because
                                // the function is not going to be checked.
                                match_signature(i_s, result_context, function);
                                todo!("Add a test")
                            }
                            debug!(
                                "Decided overload with any for {} (called on #{}): {:?}",
                                self.name(),
                                args.as_node_ref().line(),
                                function.node().short_debug()
                            );
                            args.reset_cache();
                            return OverloadResult::NotFound;
                        }
                    } else {
                        multi_any_match = Some((
                            calculated_type_args.type_arguments,
                            function,
                            argument_indices,
                        ))
                    }
                }
                SignatureMatch::False { similar: true } => {
                    if first_similar.is_none() {
                        first_similar = Some(function)
                    }
                }
                SignatureMatch::False { similar: false } => (),
            }
            args.reset_cache();
        }
        if let Some((type_arguments, function, _)) = multi_any_match {
            debug!(
                "Decided overload with any fallback for {} (called on #{}): {:?}",
                self.name(),
                args.as_node_ref().line(),
                function.node().short_debug()
            );
            return OverloadResult::Single(function);
        }
        if let Some((type_arguments, function)) = first_arbitrary_length_not_handled {
            return OverloadResult::Single(function);
        }
        if first_similar.is_none() && args.has_a_union_argument(i_s) {
            let mut non_union_args = vec![];
            match self.check_union_math(
                i_s,
                result_context,
                args.iter(),
                &mut non_union_args,
                args.as_node_ref(),
                search_init,
                class,
            ) {
                UnionMathResult::Match { result, .. } => return OverloadResult::Union(result),
                UnionMathResult::FirstSimilarIndex(index) => {
                    first_similar = Some(Function::new(
                        NodeRef::from_link(i_s.db, self.overload.functions[index]),
                        self.class,
                    ))
                }
                UnionMathResult::NoMatch => (),
            }
        }
        if let Some(function) = first_similar {
            // In case of similar params, we simply use the first similar overload and calculate
            // its diagnostics and return its types.
            // This is also how mypy does it. See `check_overload_call` (9943444c7)
            let calculated_type_args = match_signature(i_s, result_context, function);
            return OverloadResult::Single(function);
        } else {
            let function = Function::new(
                NodeRef::from_link(i_s.db, self.overload.functions[0]),
                self.class,
            );
            if let Some(on_overload_mismatch) = on_type_error.on_overload_mismatch {
                on_overload_mismatch(i_s, class)
            } else {
                let t = IssueType::OverloadMismatch {
                    name: function.diagnostic_string(self.class.as_ref()),
                    args: args.iter().into_argument_types(i_s),
                    variants: self.variants(i_s, search_init),
                };
                args.as_node_ref().add_typing_issue(i_s, t);
            }
        }
        if let Some(function) = had_error_in_func {
            // Need to run the whole thing again to generate errors, because the function is not
            // going to be checked.
            match_signature(i_s, result_context, function);
        }
        OverloadResult::NotFound
    }

    fn check_union_math<'x>(
        &self,
        i_s: &InferenceState<'db, '_>,
        result_context: &mut ResultContext,
        mut args: ArgumentIterator<'db, 'x>,
        non_union_args: &mut Vec<Argument<'db, 'x>>,
        args_node_ref: NodeRef,
        search_init: bool,
        class: Option<&Class>,
    ) -> UnionMathResult {
        if let Some(next_arg) = args.next() {
            let inf = next_arg.infer(i_s, result_context);
            if inf.is_union(i_s.db) {
                // TODO this is shit
                let nxt_arg: &'x Argument<'db, 'x> = unsafe { std::mem::transmute(&next_arg) };
                non_union_args.push(Argument {
                    index: next_arg.index,
                    kind: ArgumentKind::Overridden {
                        original: nxt_arg,
                        inferred: Inferred::new_unknown(),
                    },
                });
                let DbType::Union(u) = inf.as_type(i_s).into_db_type() else {
                    unreachable!()
                };
                let mut unioned = DbType::Never;
                let mut first_similar = None;
                let mut mismatch = false;
                for entry in u.entries.into_vec().into_iter() {
                    let non_union_args_len = non_union_args.len();
                    non_union_args.last_mut().unwrap().kind = ArgumentKind::Overridden {
                        original: nxt_arg,
                        inferred: Inferred::from_type(entry.type_),
                    };
                    let r = self.check_union_math(
                        i_s,
                        result_context,
                        args.clone(),
                        non_union_args,
                        args_node_ref,
                        search_init,
                        class,
                    );
                    if let UnionMathResult::Match {
                        first_similar_index,
                        ..
                    }
                    | UnionMathResult::FirstSimilarIndex(first_similar_index) = r
                    {
                        if first_similar
                            .map(|f| f > first_similar_index)
                            .unwrap_or(true)
                        {
                            first_similar = Some(first_similar_index);
                        }
                    }
                    match r {
                        UnionMathResult::Match { result, .. } if !mismatch => {
                            unioned.union_in_place(i_s.db, result);
                        }
                        _ => mismatch = true,
                    };
                    non_union_args.truncate(non_union_args_len);
                }
                if mismatch {
                    if let Some(first_similar_index) = first_similar {
                        UnionMathResult::FirstSimilarIndex(first_similar_index)
                    } else {
                        UnionMathResult::NoMatch
                    }
                } else {
                    UnionMathResult::Match {
                        result: unioned,
                        first_similar_index: first_similar.unwrap(),
                    }
                }
            } else {
                non_union_args.push(next_arg);
                self.check_union_math(
                    i_s,
                    result_context,
                    args,
                    non_union_args,
                    args_node_ref,
                    search_init,
                    class,
                )
            }
        } else {
            let mut first_similar = None;
            for (i, link) in self.overload.functions.iter().enumerate() {
                let function = Function::new(NodeRef::from_link(i_s.db, *link), self.class);
                let (calculated_type_args, had_error) = i_s.do_overload_check(|i_s| {
                    if search_init {
                        calculate_class_init_type_vars_and_return(
                            i_s,
                            class.unwrap(),
                            function,
                            non_union_args.clone().into_iter(),
                            &|| args_node_ref,
                            result_context,
                            None,
                        )
                    } else {
                        calculate_function_type_vars_and_return(
                            i_s,
                            class,
                            function,
                            non_union_args.clone().into_iter(),
                            &|| args_node_ref,
                            false,
                            function.type_vars(i_s),
                            function.node_ref.as_link(),
                            result_context,
                            None,
                        )
                    }
                });
                if had_error {
                    todo!()
                }
                match calculated_type_args.matches {
                    SignatureMatch::True { .. } => {
                        if search_init {
                            todo!()
                        } else if let Some(return_annotation) = function.return_annotation() {
                            return UnionMathResult::Match {
                                result: function
                                    .apply_type_args_in_return_annotation(
                                        i_s,
                                        calculated_type_args,
                                        class,
                                        return_annotation,
                                    )
                                    .class_as_db_type(i_s),
                                first_similar_index: i,
                            };
                        } else {
                            todo!()
                        }
                    }
                    SignatureMatch::TrueWithAny { argument_indices } => todo!(),
                    SignatureMatch::False { similar: true } if first_similar.is_none() => {
                        first_similar = Some(i);
                    }
                    SignatureMatch::False { .. } => (),
                }
            }
            if let Some(first_similar) = first_similar {
                UnionMathResult::FirstSimilarIndex(first_similar)
            } else {
                UnionMathResult::NoMatch
            }
        }
    }

    fn variants(&self, i_s: &InferenceState<'db, '_>, is_init: bool) -> Box<[Box<str>]> {
        self.overload
            .functions
            .iter()
            .map(|link| {
                let func = Function::new(NodeRef::from_link(i_s.db, *link), self.class);
                func.format_overload_variant(i_s, is_init)
            })
            .collect()
    }

    fn fallback_type(&self, i_s: &InferenceState<'db, '_>) -> Inferred {
        let mut t: Option<Type> = None;
        for link in self.overload.functions.iter() {
            let func = Function::new(NodeRef::from_link(i_s.db, *link), self.class);
            let f_t = func.result_type(i_s);
            if let Some(old_t) = t.take() {
                t = Some(old_t.merge_matching_parts(i_s.db, func.result_type(i_s)))
            } else {
                t = Some(f_t);
            }
        }
        Inferred::from_type(t.unwrap().into_db_type())
    }

    pub fn as_db_type(&self, i_s: &InferenceState<'db, '_>, first: FirstParamProperties) -> DbType {
        DbType::FunctionOverload(
            self.overload
                .functions
                .iter()
                .map(|link| {
                    let function = Function::new(NodeRef::from_link(i_s.db, *link), self.class);
                    function.as_callable(i_s, first)
                })
                .collect(),
        )
    }

    pub fn as_type(&self, i_s: &InferenceState<'db, '_>) -> Type<'a> {
        Type::owned(self.as_db_type(i_s, FirstParamProperties::None))
    }

    pub(super) fn execute_internal(
        &self,
        i_s: &InferenceState<'db, '_>,
        args: &dyn Arguments<'db>,
        on_type_error: OnTypeError<'db, '_>,
        class: Option<&Class>,
        result_context: &mut ResultContext,
    ) -> Inferred {
        debug!("Execute overloaded function {}", self.name());
        match self.find_matching_function(i_s, args, class, false, result_context, on_type_error) {
            OverloadResult::Single(func) => func.execute(i_s, args, result_context, on_type_error),
            OverloadResult::Union(t) => Inferred::from_type(t),
            OverloadResult::NotFound => self.fallback_type(i_s),
        }
    }

    pub fn execute(
        &self,
        i_s: &InferenceState<'db, '_>,
        args: &dyn Arguments<'db>,
        result_context: &mut ResultContext,
        on_type_error: OnTypeError<'db, '_>,
    ) -> Inferred {
        self.execute_internal(i_s, args, on_type_error, None, result_context)
    }

    fn name(&self) -> &str {
        self.node_ref.as_code()
    }
}

fn are_any_arguments_ambiguous_in_overload(
    db: &Database,
    a: &[ArgumentIndexWithParam],
    b: &[ArgumentIndexWithParam],
) -> bool {
    // This function checks if an argument with an Any (like List[Any]) makes it unclear which
    // overload would need to be chosen. Please have a look at the test
    // `testOverloadWithOverlappingItemsAndAnyArgument4` for more information.
    for p1 in a {
        for p2 in b {
            if p1.argument_index == p2.argument_index {
                let n1 = NodeRef::from_link(db, p1.param_annotation_link);
                let n2 = NodeRef::from_link(db, p2.param_annotation_link);

                let t1 = use_cached_annotation_type(db, n1.file, n1.as_annotation()).as_db_type();
                let t2 = use_cached_annotation_type(db, n2.file, n2.as_annotation()).as_db_type();
                if t1 != t2 {
                    return true;
                }
            }
        }
    }
    false
}

pub fn format_pretty_function_like<'db: 'x, 'x, P: Param<'x>>(
    i_s: &InferenceState<'db, '_>,
    class: Option<Class>,
    avoid_self_annotation: bool,
    name: &str,
    type_vars: Option<&TypeVarLikes>,
    params: impl Iterator<Item = P>,
    return_type: Option<Type>,
) -> Box<str> {
    let format_type = |t: Type| {
        if let Some(func_class) = class {
            let t = t.replace_type_var_likes_and_self(
                i_s.db,
                &mut |usage| {
                    let in_definition = usage.in_definition();
                    if in_definition == func_class.node_ref.as_link() {
                        func_class
                            .generics()
                            .nth_usage(i_s.db, &usage)
                            .into_generic_item(i_s.db)
                    } else {
                        usage.into_generic_item()
                    }
                },
                &mut || todo!(),
            );
            t.format_short(i_s.db)
        } else {
            t.format_short(i_s.db)
        }
    };

    let mut previous_kind = None;
    let mut args = params
        .enumerate()
        .map(|(i, p)| {
            let annotation_str = match p.specific(i_s.db) {
                WrappedParamSpecific::PositionalOnly(t)
                | WrappedParamSpecific::PositionalOrKeyword(t)
                | WrappedParamSpecific::KeywordOnly(t)
                | WrappedParamSpecific::Starred(WrappedStarred::ArbitraryLength(t))
                | WrappedParamSpecific::DoubleStarred(WrappedDoubleStarred::ValueType(t)) => {
                    t.map(format_type)
                }
                WrappedParamSpecific::Starred(WrappedStarred::ParamSpecArgs(u)) => todo!(),
                WrappedParamSpecific::DoubleStarred(WrappedDoubleStarred::ParamSpecKwargs(u)) => {
                    todo!()
                }
            };
            let current_kind = p.kind(i_s.db);
            let stars = match current_kind {
                ParamKind::Starred => "*",
                ParamKind::DoubleStarred => "**",
                _ => "",
            };
            let mut out = if i == 0 && avoid_self_annotation && stars.is_empty() {
                p.name(i_s.db).unwrap().to_owned()
            } else {
                let mut out = if current_kind == ParamKind::PositionalOnly {
                    annotation_str.unwrap_or_else(|| Box::from("Any")).into()
                } else {
                    format!(
                        "{stars}{}: {}",
                        p.name(i_s.db).unwrap(),
                        annotation_str.as_deref().unwrap_or("Any")
                    )
                };
                if previous_kind == Some(ParamKind::PositionalOnly)
                    && current_kind != ParamKind::PositionalOnly
                {
                    out = format!("/, {out}")
                }
                out
            };
            if p.has_default() {
                out += " = ...";
            }
            previous_kind = Some(current_kind);
            out
        })
        .collect::<Vec<_>>()
        .join(", ");
    if previous_kind == Some(ParamKind::PositionalOnly) {
        args += ", /";
    }
    let type_var_string = type_vars.map(|type_vars| {
        format!(
            "[{}] ",
            type_vars
                .iter()
                .map(|t| match t {
                    TypeVarLike::TypeVar(t) => {
                        let mut s = t.name(i_s.db).to_owned();
                        if let Some(bound) = &t.bound {
                            s += &format!(" <: {}", Type::new(bound).format_short(i_s.db));
                        } else if !t.restrictions.is_empty() {
                            s += &format!(
                                " in ({})",
                                t.restrictions
                                    .iter()
                                    .map(|t| Type::new(t).format_short(i_s.db))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            );
                        }
                        s
                    }
                    TypeVarLike::TypeVarTuple(t) => todo!(),
                    TypeVarLike::ParamSpec(s) => todo!(),
                })
                .collect::<Vec<_>>()
                .join(", "),
        )
    });
    let type_var_str = type_var_string.as_deref().unwrap_or("");
    let result_string = return_type.map(format_type);

    if let Some(result_string) = result_string {
        format!("def {type_var_str}{name}({args}) -> {result_string}").into()
    } else {
        format!("def {type_var_str}{name}({args})").into()
    }
}
