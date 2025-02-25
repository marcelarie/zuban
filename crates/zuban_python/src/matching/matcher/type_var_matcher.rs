use std::borrow::Cow;

use parsa_python_cst::ParamKind;

use super::{
    super::{Match, MismatchReason},
    bound::{Bound, BoundKind},
};
use crate::{
    database::{Database, PointLink},
    debug,
    format_data::{FormatData, ParamsStyle},
    inference_state::InferenceState,
    matching::MatcherFormatResult,
    params::Param,
    type_::{
        AnyCause, CallableParams, GenericItem, GenericsList, NeverCause, ParamType, Type,
        TypeVarKind, TypeVarLike, TypeVarLikeUsage, TypeVarLikes, TypeVarUsage, Variance,
    },
    type_helpers::{Callable, Class, Function},
    utils::join_with_commas,
};

#[derive(Debug, Clone, Copy)]
pub enum FunctionOrCallable<'a> {
    Function(Function<'a, 'a>),
    Callable(Callable<'a>),
}

impl<'db: 'a, 'a> FunctionOrCallable<'a> {
    pub fn return_type(&self, i_s: &InferenceState<'db, '_>) -> Cow<'a, Type> {
        match self {
            Self::Function(f) => f.return_type(i_s),
            Self::Callable(c) => Cow::Borrowed(&c.content.return_type),
        }
    }

    pub fn diagnostic_string(&self, db: &Database) -> Option<String> {
        match self {
            Self::Function(f) => Some(f.diagnostic_string()),
            Self::Callable(c) => c.diagnostic_string(db),
        }
    }

    pub fn defined_at(&self) -> PointLink {
        match self {
            Self::Function(function) => function.node_ref.as_link(),
            Self::Callable(callable) => callable.content.defined_at,
        }
    }

    pub fn type_vars(&self, i_s: &InferenceState<'db, '_>) -> &'a TypeVarLikes {
        match self {
            Self::Function(function) => function.type_vars(i_s.db),
            Self::Callable(c) => &c.content.type_vars,
        }
    }

    pub fn class(&self) -> Option<Class<'a>> {
        match self {
            Self::Function(function) => function.class,
            Self::Callable(c) => c.defined_in,
        }
    }

    pub fn first_self_or_class_annotation(
        &self,
        i_s: &InferenceState<'db, '_>,
    ) -> Option<Cow<Type>> {
        match self {
            Self::Function(function) => function.first_param_annotation_type(i_s),
            Self::Callable(c) => c
                .content
                .kind
                .had_first_self_or_class_annotation()
                .then(|| Cow::Owned(c.content.first_positional_type().unwrap())),
        }
    }

    pub fn has_keyword_param_with_name(&self, db: &Database, name: &str) -> bool {
        match self {
            Self::Function(function) => function.iter_params().any(|p| {
                p.name(db) == Some(name)
                    && matches!(
                        p.kind(db),
                        ParamKind::PositionalOrKeyword | ParamKind::KeywordOnly
                    )
            }),
            Self::Callable(c) => match &c.content.params {
                CallableParams::Simple(params) => params.iter().any(|p| {
                    p.name.as_ref().is_some_and(|n| {
                        n.as_str(db) == name
                            && matches!(
                                p.type_,
                                ParamType::PositionalOrKeyword(_) | ParamType::KeywordOnly(_)
                            )
                    })
                }),
                _ => false,
            },
        }
    }
}

#[derive(Debug, Default, Clone)]
pub(super) struct CalculatingTypeArg {
    pub(super) type_: Bound,
    pub(super) unresolved_transitive_constraints: Vec<Bound>,
    pub(super) defined_by_result_context: bool,
    pub(super) uninferrable: bool,
    pub(super) has_any_in_context: bool,
}

impl CalculatingTypeArg {
    pub fn calculated(&self) -> bool {
        !matches!(self.type_, Bound::Uncalculated { .. })
    }

    pub fn merge_full(&mut self, i_s: &InferenceState, other: Self) -> Match {
        self.defined_by_result_context |= other.defined_by_result_context;
        self.merge(i_s, other.type_)
    }

    pub fn merge(&mut self, i_s: &InferenceState, other: Bound) -> Match {
        if self.type_ == other {
            return Match::new_true();
        }
        let mut m = Match::new_true();
        if let Bound::Uncalculated { fallback } = &self.type_ {
            if !matches!(&other, Bound::Uncalculated { .. }) || fallback.is_none() {
                self.type_ = other;
                if self.type_.is_any() {
                    return Match::True { with_any: true };
                }
            }
            return Match::new_true();
        }
        let (t, variance) = match other {
            Bound::Upper(t) => (t, Variance::Contravariant),
            Bound::Lower(t) => (t, Variance::Covariant),
            Bound::Invariant(t) => (t, Variance::Invariant),
            Bound::UpperAndLower(upper, t) => {
                m = self.merge_or_mismatch(i_s, upper, Variance::Contravariant);
                (t, Variance::Covariant)
            }
            Bound::Uncalculated { .. } => return Match::new_true(),
        };
        let m = m & self.merge_or_mismatch(i_s, t, variance);
        if !m.bool() && !self.defined_by_result_context {
            self.uninferrable = true;
        }
        m
    }

    fn merge_or_mismatch(
        &mut self,
        i_s: &InferenceState,
        other: BoundKind,
        variance: Variance,
    ) -> Match {
        // First check if the value is between the bounds.
        let matches = match &mut self.type_ {
            Bound::Invariant(t) => {
                let m = t.is_simple_same_type(i_s, &other);
                if m.bool() {
                    if self.has_any_in_context {
                        self.type_ = Bound::new(other, variance);
                        self.has_any_in_context = false;
                    }
                    return m; // In the false case we still have to check for the variance cases.
                }
                m
            }
            Bound::Upper(upper) => upper.is_simple_super_type_of(i_s, &other),
            Bound::Lower(lower) => lower.is_simple_sub_type_of(i_s, &other),
            Bound::UpperAndLower(upper, lower) => {
                upper.is_simple_super_type_of(i_s, &other)
                    & lower.is_simple_sub_type_of(i_s, &other)
            }
            Bound::Uncalculated { .. } => unreachable!(),
        };
        if matches.bool() {
            // If we are between the bounds we might need to update lower/upper bounds
            match variance {
                Variance::Invariant => self.type_ = Bound::Invariant(other),
                Variance::Covariant => self.update_lower_bound(i_s, other),
                Variance::Contravariant => self.update_upper_bound(other),
            }
            matches
        } else {
            // If we are not between the lower and upper bound, but the value is co or
            // contravariant, it can still be valid.
            match variance {
                Variance::Invariant => matches,
                Variance::Covariant => match &mut self.type_ {
                    Bound::Lower(t) => {
                        let m = t.is_simple_super_type_of(i_s, &other);
                        if let Some(new) = t.common_base_type(i_s, &other) {
                            *t = new;
                            Match::new_true()
                        } else {
                            m
                        }
                    }
                    Bound::Invariant(t) => t.is_simple_super_type_of(i_s, &other),
                    Bound::Upper(_) => matches,
                    Bound::UpperAndLower(upper, lower) => {
                        let m = lower.is_simple_super_type_of(i_s, &other);
                        if let Some(new) = lower.common_base_type(i_s, &other) {
                            if upper.is_simple_super_type_of(i_s, &new).bool() {
                                *lower = new;
                                return Match::new_true();
                            }
                        }
                        m
                    }
                    Bound::Uncalculated { .. } => unreachable!(),
                },
                Variance::Contravariant => match &mut self.type_ {
                    Bound::Upper(t) => {
                        let m = t.is_simple_sub_type_of(i_s, &other);
                        if let Some(new) = t.common_sub_type(i_s, &other) {
                            *t = new;
                            Match::new_true()
                        } else {
                            m
                        }
                    }
                    Bound::Invariant(t) => t.is_simple_sub_type_of(i_s, &other),
                    Bound::UpperAndLower(upper, lower) => {
                        let m = upper.is_simple_sub_type_of(i_s, &other);
                        if let Some(new) = upper.common_sub_type(i_s, &other) {
                            if lower.is_simple_sub_type_of(i_s, &new).bool() {
                                *upper = new;
                                return Match::new_true();
                            }
                        }
                        m
                    }
                    Bound::Lower(_) => matches,
                    Bound::Uncalculated { .. } => unreachable!(),
                },
            }
        }
    }

    fn update_upper_bound(&mut self, upper: BoundKind) {
        self.type_ = match &self.type_ {
            Bound::Upper(_) => Bound::Upper(upper),
            Bound::Lower(lower) => {
                if &upper == lower {
                    Bound::Invariant(upper)
                } else {
                    Bound::UpperAndLower(upper, lower.clone())
                }
            }
            Bound::UpperAndLower(_, lower) => {
                if &upper == lower {
                    Bound::Invariant(upper)
                } else {
                    Bound::UpperAndLower(upper, lower.clone())
                }
            }
            _ => unreachable!(),
        };
    }

    fn update_lower_bound(&mut self, i_s: &InferenceState, lower: BoundKind) {
        let common = |b: &BoundKind, lower: BoundKind| {
            b.common_base_type(i_s, &lower)
                .expect("It feels like this should never happend, because matching happened before")
        };
        self.type_ = match &self.type_ {
            Bound::Lower(old) => Bound::Lower(common(old, lower)),
            Bound::Upper(upper) => {
                if upper == &lower {
                    Bound::Invariant(lower)
                } else {
                    Bound::UpperAndLower(upper.clone(), lower)
                }
            }
            Bound::UpperAndLower(upper, old) => {
                let new_lower = common(old, lower);
                if upper == &new_lower {
                    Bound::Invariant(new_lower)
                } else {
                    Bound::UpperAndLower(upper.clone(), new_lower)
                }
            }
            _ => unreachable!(),
        };
    }

    pub fn into_generic_item(self, db: &Database, type_var_like: &TypeVarLike) -> GenericItem {
        if self.uninferrable {
            return type_var_like.as_any_generic_item();
        }
        self.type_.into_generic_item(db, |fallback| {
            if let Some(fallback) = fallback {
                GenericItem::TypeArg(fallback)
            } else {
                type_var_like.as_never_generic_item(NeverCause::Inference)
            }
        })
    }
}

#[derive(Debug, Clone)]
pub struct TypeVarMatcher {
    pub(super) calculating_type_args: Vec<CalculatingTypeArg>,
    pub(super) type_var_likes: TypeVarLikes,
    pub(super) match_in_definition: PointLink,
}

impl TypeVarMatcher {
    pub fn new(match_in_definition: PointLink, type_var_likes: TypeVarLikes) -> Self {
        Self::new_internal(match_in_definition, type_var_likes)
    }

    fn new_internal(match_in_definition: PointLink, type_var_likes: TypeVarLikes) -> Self {
        // We could allocate on stack as described here:
        // https://stackoverflow.com/questions/27859822/is-it-possible-to-have-stack-allocated-arrays-with-the-size-determined-at-runtim
        let mut calculated_type_vars = vec![];
        calculated_type_vars.resize_with(type_var_likes.len(), Default::default);
        Self {
            calculating_type_args: calculated_type_vars,
            match_in_definition,
            type_var_likes,
        }
    }

    pub fn set_all_contained_type_vars_to_any(
        &mut self,
        search_type_vars: impl FnOnce(&mut dyn FnMut(TypeVarLikeUsage)),
        matcher_index: u32,
        cause: AnyCause,
    ) {
        search_type_vars(&mut |usage: TypeVarLikeUsage| {
            if usage.in_definition() == self.match_in_definition {
                let temporary_matcher_id = usage.temporary_matcher_id();
                if temporary_matcher_id == 0 || temporary_matcher_id == matcher_index {
                    let current = &mut self.calculating_type_args[usage.index().as_usize()];
                    if !current.calculated() {
                        current.type_.set_to_any(&usage.as_type_var_like(), cause)
                    }
                }
            }
        });
    }

    pub fn match_or_add_type_var(
        &mut self,
        i_s: &InferenceState,
        type_var_usage: &TypeVarUsage,
        value_type: &Type,
        variance: Variance,
    ) -> Match {
        debug_assert_eq!(type_var_usage.in_definition, self.match_in_definition);
        let current = &mut self.calculating_type_args[type_var_usage.index.as_usize()];
        // Before setting the type var, we need to check if the constraints match.
        let constraint_mismatch = |current: &mut CalculatingTypeArg| {
            if !current.calculated() {
                current.type_ = Bound::Uncalculated {
                    fallback: Some(value_type.clone()),
                };
            }
            Match::False {
                reason: MismatchReason::ConstraintMismatch {
                    expected: value_type.clone(),
                    type_var: type_var_usage.type_var.clone(),
                },
                similar: false,
            }
        };
        match type_var_usage.type_var.kind(i_s.db) {
            TypeVarKind::Unrestricted => (),
            TypeVarKind::Bound(bound) => {
                if !bound.is_simple_super_type_of(i_s, value_type).bool() {
                    debug!(
                        "Mismatched constraint {} :> {}",
                        bound.format_short(i_s.db),
                        value_type.format_short(i_s.db)
                    );
                    return constraint_mismatch(current);
                }
            }
            TypeVarKind::Constraints(constraints) => {
                match check_constraints(i_s, constraints, value_type, variance) {
                    Ok(bound) => {
                        let mut m = if current.calculated() {
                            current.merge(i_s, bound)
                        } else {
                            current.type_ = bound;
                            if value_type.is_any() {
                                Match::True { with_any: true }
                            } else {
                                Match::new_true()
                            }
                        };
                        if let Match::False {
                            reason: reason @ MismatchReason::None,
                            ..
                        } = &mut m
                        {
                            // This means that if the constraints are (x, y) and we set
                            *reason = MismatchReason::ConstraintAlreadySet;
                        }
                        return m;
                    }
                    Err(()) => return constraint_mismatch(current),
                }
            }
        }
        current.merge(
            i_s,
            Bound::new(BoundKind::TypeVar(value_type.clone()), variance),
        )
    }

    pub fn debug_format(&self, db: &Database) -> String {
        join_with_commas(self.calculating_type_args.iter().map(|arg| {
            let formatted = arg.type_.format_with_fallback(
                &FormatData::new_short(db),
                ParamsStyle::CallableParams,
                |_| MatcherFormatResult::Str("?".into()),
            );
            let MatcherFormatResult::Str(s) = formatted else {
                unreachable!()
            };
            s.into()
        }))
    }

    pub fn into_generics_list(self, db: &Database) -> GenericsList {
        GenericsList::new_generics(
            self.calculating_type_args
                .into_iter()
                .zip(self.type_var_likes.iter())
                .map(|(c, type_var_like)| c.into_generic_item(db, type_var_like))
                .collect(),
        )
    }

    pub fn has_unresolved_transitive_constraints(&self) -> bool {
        self.calculating_type_args
            .iter()
            .any(|calculated| !calculated.unresolved_transitive_constraints.is_empty())
    }
}

fn check_constraints<'x>(
    i_s: &InferenceState,
    constraints: impl Iterator<Item = &'x Type> + Clone,
    value_type: &Type,
    variance: Variance,
) -> Result<Bound, ()> {
    if let Type::TypeVar(t2) = value_type {
        if let TypeVarKind::Constraints(mut constraints2) = t2.type_var.kind(i_s.db) {
            if constraints2.all(|r2| {
                constraints
                    .clone()
                    .any(|r1| r1.is_simple_super_type_of(i_s, r2).bool())
            }) {
                return Ok(Bound::Invariant(BoundKind::TypeVar(value_type.clone())));
            } else {
                return Err(());
            }
        }
    }
    let mut matched_constraint = None;
    for constraint in constraints {
        let m = constraint.simple_matches(i_s, value_type, variance);
        if m.bool() {
            if matched_constraint.is_some() {
                // This means that any is involved and multiple constraints
                // are matching. Therefore just return Any.
                return Ok(Bound::Invariant(BoundKind::TypeVar(Type::Any(
                    AnyCause::Todo,
                ))));
            }
            if value_type.has_any(i_s) {
                matched_constraint = Some(constraint);
            } else {
                return Ok(Bound::Invariant(BoundKind::TypeVar(constraint.clone())));
            }
        }
    }
    if let Some(constraint) = matched_constraint {
        return Ok(Bound::Invariant(BoundKind::TypeVar(constraint.clone())));
    }
    Err(())
}
