use parsa_python_ast::ParamKind;

use super::super::params::{
    InferrableParamIterator2, Param, ParamArgument, WrappedDoubleStarred, WrappedParamSpecific,
    WrappedStarred,
};
use super::super::{
    replace_class_type_vars, ArgumentIndexWithParam, FormatData, Generic, Generics, Match, Matcher,
    MismatchReason, ResultContext, SignatureMatch, Type,
};
use super::bound::TypeVarBound;
use super::type_var_matcher::{BoundKind, FunctionOrCallable, TypeVarMatcher};
use crate::arguments::{ArgumentIterator, ArgumentKind, Arguments};
use crate::database::{
    CallableContent, CallableParams, DbType, GenericItem, GenericsList, PointLink, TypeVarLike,
    TypeVarLikeUsage, TypeVarLikes,
};
use crate::debug;
use crate::diagnostics::IssueType;
use crate::inference_state::InferenceState;
use crate::node_ref::NodeRef;
use crate::value::{Class, FirstParamProperties, Function, Instance, OnTypeError, Value};

pub fn calculate_class_init_type_vars_and_return<'db>(
    i_s: &mut InferenceState<'db, '_>,
    class: &Class,
    function: Function,
    args: &dyn Arguments<'db>,
    result_context: &mut ResultContext,
    on_type_error: Option<OnTypeError<'db, '_>>,
) -> CalculatedTypeArguments {
    debug!(
        "Calculate type vars for class {} ({})",
        class.name(),
        function.name(),
    );
    let has_generics = !matches!(class.generics, Generics::None | Generics::Any);
    let type_vars = class.type_vars(i_s);
    // Function type vars need to be calculated, so annotations are used.
    let func_type_vars = function.type_vars(i_s);

    let func_or_callable = FunctionOrCallable::Function(function);
    let match_in_definition;
    let mut matcher = if has_generics {
        match_in_definition = function.node_ref.as_link();
        get_matcher(
            Some(class),
            func_or_callable,
            match_in_definition,
            func_type_vars,
        )
    } else {
        match_in_definition = class.node_ref.as_link();
        get_matcher(
            Some(class),
            func_or_callable,
            match_in_definition,
            type_vars,
        )
    };

    if let Some(t) = function.first_param_annotation_type(i_s) {
        let mut class = *class;
        // The generics of the class are Any, until we actually execute this function and check the
        // __init__.
        debug_assert!(matches!(class.generics, Generics::Any));
        class.generics = Generics::Self_ {
            class_definition: class.node_ref.as_link(),
            type_var_likes: class.type_vars(i_s),
        };
        let matches = Type::Class(class).is_super_type_of(
            &mut i_s.with_class_context(&class),
            &mut matcher,
            &t,
        );
        if let Match::False { similar, .. } = matches {
            return CalculatedTypeArguments {
                in_definition: match_in_definition,
                matches: SignatureMatch::False { similar },
                type_arguments: None,
            };
        }
    }
    if has_generics {
        let mut type_arguments = calculate_type_vars(
            i_s,
            matcher,
            Some(class),
            func_or_callable,
            None,
            args,
            true,
            func_type_vars,
            match_in_definition,
            result_context,
            on_type_error,
        );
        type_arguments.type_arguments = class.generics_as_list(i_s.db);
        type_arguments
    } else {
        calculate_type_vars(
            i_s,
            matcher,
            Some(class),
            func_or_callable,
            Some(class),
            args,
            true,
            type_vars,
            match_in_definition,
            result_context,
            on_type_error,
        )
    }
}

#[derive(Debug)]
pub struct CalculatedTypeArguments {
    pub in_definition: PointLink,
    pub matches: SignatureMatch,
    pub type_arguments: Option<GenericsList>,
}

impl CalculatedTypeArguments {
    pub fn lookup_type_var_usage(
        &self,
        i_s: &mut InferenceState,
        class: Option<&Class>,
        usage: TypeVarLikeUsage,
    ) -> GenericItem {
        if self.in_definition == usage.in_definition() {
            return if let Some(fm) = &self.type_arguments {
                fm[usage.index()].clone()
            } else {
                // TODO we are just passing the type vars again. Does this make sense?
                // usage.into_generic_item()
                todo!()
            };
        }
        if let Some(c) = class {
            if usage.in_definition() == c.node_ref.as_link() {
                return c
                    .generics()
                    .nth_usage(i_s.db, &usage)
                    .into_generic_item(i_s.db);
            }
        }
        usage.into_generic_item()
    }
}

pub fn calculate_function_type_vars_and_return<'db>(
    i_s: &mut InferenceState<'db, '_>,
    class: Option<&Class>,
    function: Function,
    args: &dyn Arguments<'db>,
    skip_first_param: bool,
    type_vars: Option<&TypeVarLikes>,
    match_in_definition: PointLink,
    result_context: &mut ResultContext,
    on_type_error: Option<OnTypeError<'db, '_>>,
) -> CalculatedTypeArguments {
    debug!(
        "Calculate type vars for {} in {}",
        function.name(),
        class.map(|c| c.name()).unwrap_or("-")
    );
    let func_or_callable = FunctionOrCallable::Function(function);
    calculate_type_vars(
        i_s,
        get_matcher(class, func_or_callable, match_in_definition, type_vars),
        class,
        func_or_callable,
        None,
        args,
        skip_first_param,
        type_vars,
        match_in_definition,
        result_context,
        on_type_error,
    )
}

pub fn calculate_callable_type_vars_and_return<'db>(
    i_s: &mut InferenceState<'db, '_>,
    class: Option<&Class>,
    callable: &CallableContent,
    args: &dyn Arguments<'db>,
    result_context: &mut ResultContext,
    on_type_error: OnTypeError<'db, '_>,
) -> CalculatedTypeArguments {
    let func_or_callable = FunctionOrCallable::Callable(callable);
    let type_vars = callable.type_vars.as_ref();
    calculate_type_vars(
        i_s,
        get_matcher(class, func_or_callable, callable.defined_at, type_vars),
        None,
        func_or_callable,
        None,
        args,
        false,
        type_vars,
        callable.defined_at,
        result_context,
        Some(on_type_error),
    )
}

fn get_matcher<'a>(
    class: Option<&'a Class>,
    func_or_callable: FunctionOrCallable<'a>,
    match_in_definition: PointLink,
    type_vars: Option<&TypeVarLikes>,
) -> Matcher<'a> {
    let matcher = type_vars.map(|t| TypeVarMatcher::new(match_in_definition, t.len()));
    Matcher::new(class, func_or_callable, matcher)
}

fn add_generics_from_result_context_class(
    i_s: &mut InferenceState,
    matcher: &mut Matcher,
    type_vars: &TypeVarLikes,
    result_class: &Class,
) {
    let mut calculating = matcher.iter_calculated_type_vars();
    for (type_var_like, g) in type_vars
        .iter()
        .zip(result_class.generics().iter(i_s.clone()))
    {
        let calculated = calculating.next().unwrap();
        match g {
            Generic::TypeArgument(g) => {
                if !g.is_any() {
                    let mut bound = TypeVarBound::new(
                        g.as_db_type(i_s.db),
                        match type_var_like {
                            TypeVarLike::TypeVar(t) => t.variance,
                            _ => unreachable!(),
                        },
                    );
                    bound.invert_bounds();
                    calculated.type_ = BoundKind::TypeVar(bound);
                    calculated.defined_by_result_context = true;
                }
            }
            Generic::TypeVarTuple(ts) => {
                calculated.type_ = BoundKind::TypeVarTuple(ts.into_owned());
                calculated.defined_by_result_context = true;
            }
            Generic::ParamSpecArgument(p) => {
                calculated.type_ = BoundKind::ParamSpecArgument(p.into_owned());
                calculated.defined_by_result_context = true;
            }
        };
    }
}

fn calculate_type_vars<'db>(
    i_s: &mut InferenceState<'db, '_>,
    mut matcher: Matcher,
    class: Option<&Class>,
    func_or_callable: FunctionOrCallable,
    expected_return_class: Option<&Class>,
    args: &dyn Arguments<'db>,
    skip_first_param: bool,
    type_vars: Option<&TypeVarLikes>,
    match_in_definition: PointLink,
    result_context: &mut ResultContext,
    on_type_error: Option<OnTypeError<'db, '_>>,
) -> CalculatedTypeArguments {
    if matcher.type_var_matcher.is_some() {
        result_context.with_type_if_exists_and_replace_type_var_likes(i_s, |i_s, type_| {
            if let Some(class) = expected_return_class {
                // This is kind of a special case. Since __init__ has no return annotation, we simply
                // check if the classes match and then push the generics there.
                if let Some(type_vars) = class.type_vars(i_s) {
                    type_.on_any_class(
                        i_s,
                        &mut Matcher::default(),
                        &mut |i_s, _, result_class| {
                            for (_, t) in class.mro(i_s) {
                                if let Some(class) = t.maybe_class(i_s.db) {
                                    if result_class.node_ref == class.node_ref {
                                        add_generics_from_result_context_class(
                                            i_s,
                                            &mut matcher,
                                            type_vars,
                                            result_class,
                                        );
                                        return true;
                                    }
                                }
                            }
                            false
                        },
                    );
                }
            } else {
                let result_type = match func_or_callable {
                    FunctionOrCallable::Function(f) => f.result_type(i_s),
                    FunctionOrCallable::Callable(c) => Type::new(&c.result_type),
                };
                // Fill the type var arguments from context
                result_type.is_sub_type_of(i_s, &mut matcher, type_);
                for calculated in matcher.iter_calculated_type_vars() {
                    let has_any = match &calculated.type_ {
                        BoundKind::TypeVar(
                            TypeVarBound::Invariant(t)
                            | TypeVarBound::Lower(t)
                            | TypeVarBound::Upper(t),
                        ) => t.has_any(i_s),
                        BoundKind::TypeVar(TypeVarBound::LowerAndUpper(t1, t2)) => {
                            t1.has_any(i_s) | t2.has_any(i_s)
                        }
                        BoundKind::TypeVarTuple(ts) => ts.args.has_any(i_s),
                        BoundKind::ParamSpecArgument(params) => todo!(),
                        BoundKind::Uncalculated => continue,
                    };
                    if has_any {
                        calculated.type_ = BoundKind::Uncalculated
                    } else {
                        calculated.defined_by_result_context = true;
                    }
                }
            }
        });
    }
    let matches = match func_or_callable {
        FunctionOrCallable::Function(function) => {
            // Make sure the type vars are properly pre-calculated, because we are using type
            // vars from in use_cached_annotation_type.
            function.type_vars(i_s);
            calculate_type_vars_for_params(
                i_s,
                &mut matcher,
                class,
                func_or_callable,
                args,
                on_type_error,
                function.iter_args_with_params(i_s.db, args, skip_first_param),
            )
        }
        FunctionOrCallable::Callable(callable) => match &callable.params {
            CallableParams::Simple(params) => calculate_type_vars_for_params(
                i_s,
                &mut matcher,
                None,
                func_or_callable,
                args,
                on_type_error,
                InferrableParamIterator2::new(i_s.db, params.iter(), args.iter_arguments()),
            ),
            CallableParams::Any => SignatureMatch::True,
            CallableParams::WithParamSpec(pre_types, param_spec) => {
                let mut args = args.iter_arguments();
                if !pre_types.is_empty() {
                    dbg!(pre_types, args.collect::<Vec<_>>());
                    todo!()
                }
                if let Some(arg) = args.next() {
                    if let ArgumentKind::ParamSpec { usage, .. } = &arg.kind {
                        if usage.in_definition == param_spec.in_definition {
                            SignatureMatch::True
                        } else if class.is_none() {
                            SignatureMatch::False { similar: false }
                        } else {
                            dbg!(class);
                            todo!("{:?}, {:?}", param_spec.in_definition, usage.in_definition)
                        }
                    } else {
                        todo!()
                    }
                } else {
                    todo!()
                }
            }
        },
    };
    let type_arguments = matcher.type_var_matcher.and_then(|m| {
        type_vars.map(|type_vars| {
            GenericsList::new_generics(
                m.calculated_type_vars
                    .into_iter()
                    .zip(type_vars.iter())
                    .map(|(c, type_var_like)| c.into_generic_item(i_s.db, type_var_like))
                    .collect(),
            )
        })
    });
    if cfg!(feature = "zuban_debug") {
        if let Some(type_arguments) = &type_arguments {
            let callable_description: String;
            debug!(
                "Calculated type vars: {}[{}]",
                match &func_or_callable {
                    FunctionOrCallable::Function(function) => function.name(),
                    FunctionOrCallable::Callable(callable) => {
                        callable_description = callable.format(&FormatData::new_short(i_s.db));
                        &callable_description
                    }
                },
                type_arguments.format(&FormatData::new_short(i_s.db)),
            );
        }
    }
    CalculatedTypeArguments {
        in_definition: match_in_definition,
        matches,
        type_arguments,
    }
}

pub fn match_arguments_against_params<
    'db: 'x,
    'x,
    'c,
    P: Param<'x>,
    AI: ArgumentIterator<'db, 'x>,
>(
    i_s: &mut InferenceState<'db, '_>,
    matcher: &mut Matcher,
    class: Option<&Class>,
    func_or_callable: FunctionOrCallable,
    args_node_ref: &impl Fn() -> NodeRef<'c>,
    on_type_error: Option<OnTypeError<'db, '_>>,
    mut args_with_params: InferrableParamIterator2<'db, 'x, impl Iterator<Item = P>, P, AI>,
) -> SignatureMatch {
    let diagnostic_string = |prefix: &str| match func_or_callable {
        FunctionOrCallable::Function(f) => {
            Some((prefix.to_owned() + &f.diagnostic_string(class)).into())
        }
        FunctionOrCallable::Callable(c) => c.name.map(|n| {
            let mut string = format!("{prefix}\"{}\"", n.as_str(i_s.db));
            if let Some(class_name) = c.class_name {
                string += &format!(" of \"{}\"", class_name.as_str(i_s.db));
            }
            string.into()
        }),
    };
    let should_generate_errors = on_type_error.is_some();
    let mut missing_params = vec![];
    let mut argument_indices_with_any = vec![];
    let mut matches = Match::new_true();
    for (i, p) in args_with_params.by_ref().enumerate() {
        if matches!(p.argument, ParamArgument::None) && !p.param.has_default() {
            matches = Match::new_false();
            if should_generate_errors {
                missing_params.push(p.param);
            }
            continue;
        }
        match p.argument {
            ParamArgument::Argument(ref argument) => {
                let specific = p.param.specific(i_s);
                let annotation_type = match &specific {
                    WrappedParamSpecific::PositionalOnly(t)
                    | WrappedParamSpecific::PositionalOrKeyword(t)
                    | WrappedParamSpecific::KeywordOnly(t)
                    | WrappedParamSpecific::Starred(WrappedStarred::ArbitraryLength(t))
                    | WrappedParamSpecific::DoubleStarred(WrappedDoubleStarred::ValueType(t)) => {
                        match t {
                            Some(t) => t,
                            None => continue,
                        }
                    }
                    WrappedParamSpecific::Starred(WrappedStarred::ParamSpecArgs(_))
                    | WrappedParamSpecific::DoubleStarred(WrappedDoubleStarred::ParamSpecKwargs(
                        _,
                    )) => {
                        unreachable!()
                    }
                };
                let value = if matcher.might_have_defined_type_vars() {
                    argument.infer(
                        i_s,
                        &mut ResultContext::WithMatcher {
                            type_: annotation_type,
                            matcher,
                        },
                    )
                } else {
                    argument.infer(i_s, &mut ResultContext::Known(annotation_type))
                };
                let m = annotation_type.error_if_not_matches_with_matcher(
                    i_s,
                    matcher,
                    &value,
                    on_type_error.as_ref().map(|on_type_error| {
                        |i_s: &mut InferenceState<'db, '_>, mut t1, t2, reason: &MismatchReason| {
                            let node_ref = argument.as_node_ref().to_db_lifetime(i_s.db);
                            if let Some(starred) = node_ref.maybe_starred_expression() {
                                t1 = format!(
                                    "*{}",
                                    node_ref
                                        .file
                                        .inference(i_s)
                                        .infer_expression(starred.expression())
                                        .format_short(i_s)
                                )
                                .into()
                            } else if let Some(double_starred) =
                                node_ref.maybe_double_starred_expression()
                            {
                                t1 = format!(
                                    "**{}",
                                    node_ref
                                        .file
                                        .inference(i_s)
                                        .infer_expression(double_starred.expression())
                                        .format_short(i_s)
                                )
                                .into()
                            }
                            match reason {
                                MismatchReason::CannotInferTypeArgument(index) => {
                                    node_ref.add_typing_issue(
                                        i_s.db,
                                        IssueType::CannotInferTypeArgument {
                                            index: *index,
                                            callable: diagnostic_string("")
                                                .unwrap_or_else(|| Box::from("Callable")),
                                        },
                                    );
                                }
                                MismatchReason::ConstraintMismatch { expected, type_var } => {
                                    node_ref.add_typing_issue(
                                        i_s.db,
                                        IssueType::InvalidTypeVarValue {
                                            type_var_name: Box::from(type_var.name(i_s.db)),
                                            func: diagnostic_string("")
                                                .unwrap_or_else(|| Box::from("function")),
                                            actual: expected.format(&FormatData::new_short(i_s.db)),
                                        },
                                    );
                                }
                                _ => (on_type_error.callback)(
                                    i_s,
                                    class,
                                    &diagnostic_string,
                                    argument,
                                    t1,
                                    t2,
                                ),
                            };
                            node_ref
                        }
                    }),
                );
                if matches!(m, Match::True { with_any: true }) {
                    if let Some(param_annotation_link) = p.param.func_annotation_link() {
                        // This is never reached when matching callables
                        argument_indices_with_any.push(ArgumentIndexWithParam {
                            argument_index: argument.index,
                            param_annotation_link,
                        })
                    }
                }
                matches &= m
            }
            ParamArgument::ParamSpecArgs(param_spec, args) => {
                matches &= match matcher.match_param_spec_arguments(
                    i_s,
                    &param_spec,
                    args,
                    class,
                    func_or_callable,
                    args_node_ref,
                    on_type_error,
                ) {
                    SignatureMatch::True => Match::new_true(),
                    SignatureMatch::TrueWithAny { .. } => todo!(),
                    SignatureMatch::False { similar } => Match::False {
                        similar,
                        reason: MismatchReason::None,
                    },
                }
            }
            ParamArgument::None => (),
        }
    }
    let add_keyword_argument_issue = |reference: NodeRef, name| {
        let s = if let FunctionOrCallable::Function(function) = func_or_callable {
            if function.iter_params().any(|p| {
                p.name(i_s.db) == Some(name)
                    && matches!(
                        p.kind(i_s.db),
                        ParamKind::PositionalOrKeyword | ParamKind::KeywordOnly
                    )
            }) {
                format!(
                    "{} gets multiple values for keyword argument {name:?}",
                    function.diagnostic_string(class),
                )
            } else {
                format!(
                    "Unexpected keyword argument {name:?} for {}",
                    function.diagnostic_string(class),
                )
            }
        } else {
            debug!("TODO this keyword param could also exist");
            format!("Unexpected keyword argument {name:?}")
        };
        reference.add_typing_issue(i_s.db, IssueType::ArgumentIssue(s.into()));
    };
    if args_with_params.too_many_positional_arguments {
        matches = Match::new_false();
        if should_generate_errors || true {
            // TODO remove true and add test
            let mut s = "Too many positional arguments".to_owned();
            s += diagnostic_string(" for ").as_deref().unwrap_or("");
            args_node_ref().add_typing_issue(i_s.db, IssueType::ArgumentIssue(s.into()));
        }
    } else if args_with_params.has_unused_arguments() {
        matches = Match::new_false();
        if should_generate_errors {
            let mut too_many = false;
            for arg in args_with_params.arguments {
                match arg.kind {
                    ArgumentKind::Keyword { key, node_ref, .. } => {
                        add_keyword_argument_issue(node_ref, key)
                    }
                    _ => {
                        too_many = true;
                        break;
                    }
                }
            }
            if too_many {
                let mut s = "Too many arguments".to_owned();
                s += diagnostic_string(" for ").as_deref().unwrap_or("");
                args_node_ref().add_typing_issue(i_s.db, IssueType::ArgumentIssue(s.into()));
            }
        }
    } else if args_with_params.has_unused_keyword_arguments() && should_generate_errors {
        for unused in args_with_params.unused_keyword_arguments {
            match unused.kind {
                ArgumentKind::Keyword { key, node_ref, .. } => {
                    add_keyword_argument_issue(node_ref, key)
                }
                _ => unreachable!(),
            }
        }
    } else if should_generate_errors {
        let mut missing_positional = vec![];
        for param in &missing_params {
            if let Some(param_name) = param.name(i_s.db) {
                if param.kind(i_s.db) == ParamKind::KeywordOnly {
                    let mut s = format!("Missing named argument {:?}", param_name);
                    s += diagnostic_string(" for ").as_deref().unwrap_or("");
                    args_node_ref().add_typing_issue(i_s.db, IssueType::ArgumentIssue(s.into()));
                } else {
                    missing_positional.push(format!("\"{param_name}\""));
                }
            } else {
                let mut s = "Too few arguments".to_owned();
                s += diagnostic_string(" for ").as_deref().unwrap_or("");
                args_node_ref().add_typing_issue(i_s.db, IssueType::ArgumentIssue(s.into()));
            }
        }
        if let Some(mut s) = match &missing_positional[..] {
            [] => None,
            [param_name] => Some(format!(
                "Missing positional argument {} in call",
                param_name
            )),
            _ => Some(format!(
                "Missing positional arguments {} in call",
                missing_positional.join(", ")
            )),
        } {
            s += diagnostic_string(" to ").as_deref().unwrap_or("");
            args_node_ref().add_typing_issue(i_s.db, IssueType::ArgumentIssue(s.into()));
        };
    }
    match matches {
        Match::True { with_any: false } => SignatureMatch::True,
        Match::True { with_any: true } => SignatureMatch::TrueWithAny {
            argument_indices: argument_indices_with_any.into(),
        },
        Match::False { similar, .. } => SignatureMatch::False { similar },
    }
}

fn calculate_type_vars_for_params<'db: 'x, 'x, P: Param<'x>, AI: ArgumentIterator<'db, 'x>>(
    i_s: &mut InferenceState<'db, '_>,
    matcher: &mut Matcher,
    class: Option<&Class>,
    func_or_callable: FunctionOrCallable,
    args: &dyn Arguments<'db>,
    on_type_error: Option<OnTypeError<'db, '_>>,
    args_with_params: InferrableParamIterator2<'db, 'x, impl Iterator<Item = P>, P, AI>,
) -> SignatureMatch {
    match_arguments_against_params(
        i_s,
        matcher,
        class,
        func_or_callable,
        &|| args.as_node_ref(),
        on_type_error,
        args_with_params,
    )
}

pub fn create_signature_without_self(
    i_s: &mut InferenceState,
    func: Function,
    instance: Instance,
    expected_type: &Type,
) -> Option<DbType> {
    let type_vars = func.type_vars(i_s);
    let type_vars_len = type_vars.map(|t| t.len()).unwrap_or(0);
    let mut matcher = Matcher::new_reverse_function_matcher(Some(&instance.class), func, type_vars);
    let instance_t = instance.as_type(i_s);
    let match_ = matcher.match_reverse(|m| expected_type.is_super_type_of(i_s, m, &instance_t));
    if !match_.bool() {
        return None;
    }
    let mut t = func.as_db_type(i_s, FirstParamProperties::Skip);
    if let Some(type_vars) = type_vars {
        let DbType::Callable(callable_content) = &mut t else {
            unreachable!();
        };
        let mut old_type_vars = std::mem::replace(&mut callable_content.type_vars, None)
            .unwrap()
            .into_vec();
        let calculated = matcher.unwrap_calculated_type_args();
        for (i, c) in calculated.iter().enumerate().rev() {
            if c.calculated() {
                old_type_vars.remove(i);
            }
        }
        if !old_type_vars.is_empty() {
            callable_content.type_vars = Some(TypeVarLikes::from_vec(old_type_vars));
        }
        t = t.replace_type_var_likes_and_self(
            i_s.db,
            &mut |usage| {
                let index = usage.index().as_usize();
                if usage.in_definition() == func.node_ref.as_link() {
                    let c = &calculated[index];
                    if c.calculated() {
                        return (*c).clone().into_generic_item(i_s.db, &type_vars[index]);
                    }
                }
                let new_index = calculated
                    .iter()
                    .take(index)
                    .filter(|c| !c.calculated())
                    .count();
                usage.into_generic_item_with_new_index(new_index.into())
            },
            &mut || DbType::Self_,
        );
    }
    // TODO this should not be run separately, we do two replacements here.
    Some(replace_class_type_vars(i_s, &t, &instance.class))
}
