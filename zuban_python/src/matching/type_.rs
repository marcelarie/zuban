use std::borrow::Cow;
use std::rc::Rc;

use super::params::has_overlapping_params;
use super::{
    calculate_callable_type_vars_and_return, matches_params, CalculatedTypeArguments, FormatData,
    Generics, IteratorContent, LookupResult, Match, Matcher, MismatchReason, OnLookupError,
    OnTypeError, ResultContext,
};
use crate::arguments::Arguments;
use crate::database::{
    CallableContent, CallableParam, CallableParams, ClassGenerics, ClassType, ComplexPoint,
    Database, DbType, DoubleStarredParamSpecific, GenericItem, GenericsList, MetaclassState,
    NamedTuple, ParamSpecArgument, ParamSpecTypeVars, ParamSpecUsage, ParamSpecific, PointLink,
    RecursiveAlias, StarredParamSpecific, TupleContent, TupleTypeArguments, TypeAlias,
    TypeArguments, TypeOrTypeVarTuple, TypeVarLike, TypeVarLikeUsage, TypeVarLikes, TypeVarManager,
    TypeVarTupleUsage, TypeVarUsage, UnionEntry, UnionType, Variance,
};
use crate::debug;
use crate::diagnostics::IssueType;
use crate::getitem::SliceType;
use crate::inference_state::InferenceState;
use crate::inferred::Inferred;
use crate::node_ref::NodeRef;
use crate::type_helpers::{
    Callable, Class, Instance, Module, MroIterator, NamedTupleValue, Tuple, TypeOrClass, TypingType,
};
use crate::utils::rc_unwrap_or_clone;

pub type ReplaceTypeVarLike<'x> = &'x mut dyn FnMut(TypeVarLikeUsage) -> GenericItem;
pub type ReplaceSelf<'x> = &'x mut dyn FnMut() -> DbType;

#[derive(Debug, Clone)]
#[allow(clippy::enum_variant_names)]
pub struct Type<'a>(Cow<'a, DbType>);

impl<'x> From<&'x DbType> for Type<'x> {
    fn from(item: &'x DbType) -> Self {
        Self(Cow::Borrowed(item))
    }
}

impl From<DbType> for Type<'static> {
    fn from(item: DbType) -> Self {
        Self(Cow::Owned(item))
    }
}

impl<'a> Type<'a> {
    pub fn new(t: &'a DbType) -> Self {
        Self(Cow::Borrowed(t))
    }

    pub fn owned(t: DbType) -> Self {
        Self(Cow::Owned(t))
    }

    pub fn union(self, db: &Database, other: Self) -> Self {
        Self::owned(self.into_db_type().union(db, other.into_db_type()))
    }

    pub fn into_db_type(self) -> DbType {
        self.0.into_owned()
    }

    pub fn as_db_type(&self) -> DbType {
        self.0.as_ref().clone()
    }

    pub fn as_ref(&self) -> &DbType {
        self.0.as_ref()
    }

    #[inline]
    pub fn maybe_class(&self, db: &'a Database) -> Option<Class<'_>> {
        match self.as_ref() {
            DbType::Class(link, generics) => Some(Class::from_db_type(db, *link, generics)),
            _ => None,
        }
    }

    #[inline]
    pub fn maybe_type_of_class(&self, db: &'a Database) -> Option<Class<'_>> {
        if let DbType::Type(t) = self.as_ref() {
            if let DbType::Class(link, generics) = t.as_ref() {
                return Some(Class::from_db_type(db, *link, generics));
            }
        }
        None
    }

    #[inline]
    pub fn expect_borrowed_class(&self, db: &'a Database) -> Class<'a> {
        match self.0 {
            Cow::Borrowed(t) => match t {
                DbType::Class(link, generics) => Class::from_db_type(db, *link, generics),
                _ => unreachable!(),
            },
            Cow::Owned(DbType::Class(link, ref generics)) => Class::from_position(
                NodeRef::from_link(db, link),
                Generics::from_non_list_class_generics(db, generics),
                None,
            ),
            _ => unreachable!(),
        }
    }

    pub fn maybe_callable(
        &self,
        i_s: &InferenceState,
        include_non_callables: bool,
    ) -> Option<Cow<'a, CallableContent>> {
        match self.0 {
            Cow::Borrowed(DbType::Callable(c)) => Some(Cow::Borrowed(c)),
            _ => match self.as_ref() {
                DbType::Callable(c) if include_non_callables => {
                    Some(Cow::Owned(c.as_ref().clone()))
                }
                DbType::Type(t) if include_non_callables => match t.as_ref() {
                    DbType::Class(link, generics) => {
                        todo!()
                    }
                    _ => None,
                },
                DbType::Any => Some(Cow::Owned(CallableContent::new_any())),
                DbType::Class(link, generics) if include_non_callables => {
                    let cls = Class::from_db_type(i_s.db, *link, generics);
                    Instance::new(cls, None)
                        .lookup(i_s, None, "__call__")
                        .into_maybe_inferred()
                        .and_then(|i| {
                            i.as_type(i_s)
                                .maybe_callable(i_s, include_non_callables)
                                .map(|c| Cow::Owned(c.into_owned()))
                        })
                }
                _ => None,
            },
        }
    }

    pub fn maybe_borrowed_db_type(&self) -> Option<&'a DbType> {
        match self.0 {
            Cow::Borrowed(t) => Some(t),
            _ => None,
        }
    }

    pub fn overlaps(&self, i_s: &InferenceState, other: &Self) -> bool {
        match other.as_ref() {
            DbType::TypeVar(t2_usage) => {
                return if let Some(bound) = &t2_usage.type_var.bound {
                    self.overlaps(i_s, &bound.into())
                } else if !t2_usage.type_var.restrictions.is_empty() {
                    t2_usage
                        .type_var
                        .restrictions
                        .iter()
                        .all(|r2| self.overlaps(i_s, &r2.into()))
                } else {
                    true
                };
            }
            DbType::Union(union_type2) => {
                return union_type2.iter().any(|t| self.overlaps(i_s, &t.into()))
            }
            DbType::Any => return false, // This is a fallback
            _ => (),
        }

        match self.as_ref() {
            DbType::Class(link, generics) => {
                let class1 = Class::from_db_type(i_s.db, *link, generics);
                match other.as_ref() {
                    DbType::Class(l, g) => {
                        Self::overlaps_class(i_s, class1, Class::from_db_type(i_s.db, *l, g))
                    }
                    _ => false,
                }
            }
            DbType::Type(t1) => match other.as_ref() {
                DbType::Type(t2) => Type::new(t1).overlaps(i_s, &Type::new(t2)),
                _ => false,
            },
            DbType::Callable(c1) => match other.as_ref() {
                DbType::Callable(c2) => {
                    Type::new(&c1.result_type).overlaps(i_s, &Type::new(&c2.result_type))
                        && has_overlapping_params(i_s, &c1.params, &c2.params)
                }
                DbType::Type(t2) => Type::new(self.as_ref()).overlaps(i_s, &Type::new(t2)),
                _ => false,
            },
            DbType::Any => true,
            DbType::Never => todo!(),
            DbType::Literal(literal1) => match other.as_ref() {
                DbType::Literal(literal2) => literal1.value(i_s.db) == literal2.value(i_s.db),
                _ => i_s
                    .db
                    .python_state
                    .literal_type(literal1.kind)
                    .overlaps(i_s, other),
            },
            DbType::None => matches!(other.as_ref(), DbType::None),
            DbType::TypeVar(t1) => {
                if let Some(db_t) = &t1.type_var.bound {
                    Type::new(db_t).overlaps(i_s, other)
                } else if !t1.type_var.restrictions.is_empty() {
                    todo!("{other:?}")
                } else {
                    true
                }
            }
            DbType::Tuple(t1) => match other.as_ref() {
                DbType::Tuple(t2) => Self::overlaps_tuple(i_s, t1, t2),
                _ => false,
            },
            DbType::Union(union) => union.iter().any(|t| Type::new(t).overlaps(i_s, other)),
            DbType::FunctionOverload(intersection) => todo!(),
            DbType::NewType(_) => todo!(),
            DbType::RecursiveAlias(_) => todo!(),
            DbType::Self_ => false, // TODO this is wrong
            DbType::ParamSpecArgs(usage) => todo!(),
            DbType::ParamSpecKwargs(usage) => todo!(),
            DbType::Module(file_index) => todo!(),
            DbType::NamedTuple(_) => todo!(),
        }
    }

    fn matches_internal(
        &self,
        i_s: &InferenceState,
        matcher: &mut Matcher,
        value_type: &Self,
        variance: Variance,
    ) -> Match {
        match self.as_ref() {
            DbType::Class(link, generics) => Self::matches_class_against_type(
                i_s,
                matcher,
                &Class::from_db_type(i_s.db, *link, generics),
                value_type,
                variance,
            ),
            DbType::Type(t1) => match value_type.as_ref() {
                DbType::Type(t2) => Type::new(t1)
                    .matches(i_s, matcher, &Type::new(t2), variance)
                    .similar_if_false(),
                _ => Match::new_false(),
            },
            DbType::TypeVar(t1) => {
                if matcher.is_matching_reverse() {
                    Match::new_false()
                } else {
                    matcher.match_or_add_type_var(i_s, t1, value_type, variance)
                }
            }
            DbType::Callable(c1) => match value_type.as_ref() {
                DbType::Type(t2) => match t2.as_ref() {
                    DbType::Class(link, generics) => {
                        let cls = Class::from_db_type(i_s.db, *link, generics);
                        // TODO the __init__ should actually be looked up on the original class, not
                        // the subclass
                        let lookup = cls.lookup(i_s, None, "__init__");
                        if let LookupResult::GotoName(_, init) = lookup {
                            let c2 = init.as_type(i_s).into_db_type();
                            if let DbType::Callable(c2) = c2 {
                                let type_vars2 = cls.type_vars(i_s);
                                // Since __init__ does not have a return, We need to check the params
                                // of the __init__ functions and the class as a return type separately.
                                return Type::new(&c1.result_type).is_super_type_of(
                                    i_s,
                                    matcher,
                                    &Type::new(t2),
                                ) & matches_params(
                                    i_s,
                                    matcher,
                                    &c1.params,
                                    &c2.params,
                                    type_vars2.map(|t| (t, cls.node_ref.as_link())),
                                    Variance::Contravariant,
                                    true, // Ignore the first param
                                );
                            }
                        }
                        Match::new_false()
                    }
                    _ => {
                        if matches!(&c1.params, CallableParams::Any) {
                            Type::new(&c1.result_type).is_super_type_of(
                                i_s,
                                matcher,
                                &Type::new(t2.as_ref()),
                            )
                        } else {
                            Match::new_false()
                        }
                    }
                },
                DbType::Callable(c2) => Self::matches_callable(i_s, matcher, c1, c2),
                DbType::FunctionOverload(overload) if variance == Variance::Covariant => {
                    if matcher.is_matching_reverse() {
                        todo!()
                    }
                    overload
                        .iter()
                        .any(|c2| Self::matches_callable(i_s, matcher, c1, c2).bool())
                        .into()
                }
                _ => Match::new_false(),
            },
            DbType::None => matches!(value_type.as_ref(), DbType::None).into(),
            DbType::Any if matcher.is_matching_reverse() => {
                debug!("TODO write a test for this.");
                matcher.set_all_contained_type_vars_to_any(i_s, &self.as_ref());
                Match::True { with_any: true }
            }
            DbType::Any => Match::new_true(),
            DbType::Never => Match::new_false(),
            DbType::Tuple(t1) => match value_type.as_ref() {
                DbType::Tuple(t2) => {
                    Self::matches_tuple(i_s, matcher, t1, t2, variance).similar_if_false()
                }
                DbType::NamedTuple(t2) => {
                    Self::matches_tuple(i_s, matcher, t1, t2.as_tuple(), variance)
                        .similar_if_false()
                }
                _ => Match::new_false(),
            },
            DbType::Union(union_type1) => {
                self.matches_union(i_s, matcher, union_type1, value_type, variance)
            }
            DbType::FunctionOverload(intersection) => todo!(),
            DbType::Literal(literal1) => {
                debug_assert!(!literal1.implicit);
                match value_type.as_ref() {
                    DbType::Literal(literal2) => {
                        (literal1.value(i_s.db) == literal2.value(i_s.db)).into()
                    }
                    _ => Match::new_false(),
                }
            }
            DbType::NewType(new_type1) => match value_type.as_ref() {
                DbType::NewType(new_type2) => (new_type1 == new_type2).into(),
                _ => Match::new_false(),
            },
            t1 @ DbType::RecursiveAlias(rec1) => {
                match value_type.as_ref() {
                    t2 @ DbType::Class(link, generics) => {
                        // Classes like aliases can also be recursive in mypy, like `class B(List[B])`.
                        matcher.avoid_recursion(t1, t2.clone(), |matcher| {
                            let g = rec1.calculated_db_type(i_s.db);
                            Type::new(g).matches_internal(i_s, matcher, value_type, variance)
                        })
                    }
                    t @ DbType::RecursiveAlias(rec2) => {
                        matcher.avoid_recursion(t1, t.clone(), |matcher| {
                            let t1 = rec1.calculated_db_type(i_s.db);
                            let t2 = rec2.calculated_db_type(i_s.db);
                            Type::new(t1).matches_internal(i_s, matcher, &Type::new(t2), variance)
                        })
                    }
                    _ => {
                        let g = rec1.calculated_db_type(i_s.db);
                        Type::new(g).matches_internal(i_s, matcher, value_type, variance)
                    }
                }
            }
            DbType::Self_ => match value_type.as_ref() {
                DbType::Self_ => Match::new_true(),
                _ => Match::new_false(),
            },
            DbType::ParamSpecArgs(usage1) => match value_type.as_ref() {
                DbType::ParamSpecArgs(usage2) => (usage1 == usage2).into(),
                _ => Match::new_false(),
            },
            DbType::ParamSpecKwargs(usage1) => match value_type.as_ref() {
                DbType::ParamSpecKwargs(usage2) => (usage1 == usage2).into(),
                _ => Match::new_false(),
            },
            DbType::NamedTuple(nt1) => match value_type.as_ref() {
                DbType::NamedTuple(nt2) => {
                    let c1 = &nt1.constructor;
                    let c2 = &nt2.constructor;
                    if c1.type_vars.is_some() || c2.type_vars.is_some() {
                        todo!()
                    } else {
                        (c1.defined_at == c2.defined_at).into()
                    }
                }
                _ => Match::new_false(),
            },
            DbType::Module(file_index) => Match::new_false(),
        }
    }

    pub fn is_sub_type_of(
        &self,
        i_s: &InferenceState,
        matcher: &mut Matcher,
        value_type: &Self,
    ) -> Match {
        matcher.match_reverse(|matcher| value_type.is_super_type_of(i_s, matcher, self))
    }

    pub fn is_simple_sub_type_of(&self, i_s: &InferenceState, value_type: &Self) -> Match {
        self.is_sub_type_of(i_s, &mut Matcher::default(), value_type)
    }

    pub fn is_simple_super_type_of(&self, i_s: &InferenceState, value_type: &Self) -> Match {
        self.is_super_type_of(i_s, &mut Matcher::default(), value_type)
    }

    pub fn is_super_type_of(
        &self,
        i_s: &InferenceState,
        matcher: &mut Matcher,
        value_type: &Self,
    ) -> Match {
        // 1. Check if the type is part of the mro.
        let m = match value_type.mro(i_s.db) {
            Some(mro) => {
                for (_, t2) in mro {
                    let m = match t2 {
                        TypeOrClass::Class(c2) => match self.maybe_class(i_s.db) {
                            Some(c1) => {
                                Self::matches_class(i_s, matcher, &c1, &c2, Variance::Covariant)
                            }
                            None => {
                                // TODO performance: This might be slow, because it always
                                // allocates when e.g.  Foo is passed to def x(f: Foo | None): ...
                                // This is a bit unfortunate, especially because it loops over the
                                // mro and allocates every time.
                                let t2 = Type::owned(c2.as_db_type(i_s.db));
                                self.matches_internal(i_s, matcher, &t2, Variance::Covariant)
                            }
                        },
                        TypeOrClass::Type(t2) => {
                            self.matches_internal(i_s, matcher, &t2, Variance::Covariant)
                        }
                    };
                    if !matches!(
                        m,
                        Match::False {
                            reason: MismatchReason::None,
                            similar: false
                        }
                    ) {
                        return m;
                    }
                }
                Match::new_false()
            }
            None => {
                let m = self.matches_internal(i_s, matcher, value_type, Variance::Covariant);
                m.or(|| self.matches_object_class(i_s.db, value_type))
            }
        };
        let result = m
            .or(|| {
                self.check_protocol_and_other_side(i_s, matcher, value_type, Variance::Covariant)
            })
            .or(|| {
                if let Some(class2) = value_type.maybe_class(i_s.db) {
                    if !matcher.ignore_promotions() {
                        return self.check_promotion(i_s, matcher, class2.node_ref);
                    }
                }
                Match::new_false()
            });
        debug!(
            "Match covariant {} against {} -> {:?}",
            self.format_short(i_s.db),
            value_type.format_short(i_s.db),
            result
        );
        result
    }

    #[inline]
    pub fn check_promotion(
        &self,
        i_s: &InferenceState,
        matcher: &mut Matcher,
        class2_node_ref: NodeRef,
    ) -> Match {
        let ComplexPoint::Class(storage) = class2_node_ref.complex().unwrap() else {
            unreachable!()
        };
        if let Some(promote_to) = storage.promote_to.get() {
            let cls_node_ref = NodeRef::from_link(i_s.db, promote_to);
            self.is_same_type(
                i_s,
                matcher,
                &Type::owned(DbType::Class(cls_node_ref.as_link(), ClassGenerics::None)),
            )
            .or(|| self.check_promotion(i_s, matcher, cls_node_ref))
        } else {
            Match::new_false()
        }
    }

    pub fn is_simple_same_type(&self, i_s: &InferenceState, value_type: &Self) -> Match {
        self.is_same_type(i_s, &mut Matcher::default(), value_type)
    }

    pub fn is_same_type(
        &self,
        i_s: &InferenceState,
        matcher: &mut Matcher,
        value_type: &Self,
    ) -> Match {
        let m = self.matches_internal(i_s, matcher, value_type, Variance::Invariant);
        let result = m.or(|| {
            self.check_protocol_and_other_side(i_s, matcher, value_type, Variance::Invariant)
        });
        debug!(
            "Match invariant {} against {} -> {:?}",
            self.format_short(i_s.db),
            value_type.format_short(i_s.db),
            result
        );
        result
    }

    pub fn simple_matches(
        &self,
        i_s: &InferenceState,
        value_type: &Self,
        variance: Variance,
    ) -> Match {
        self.matches(i_s, &mut Matcher::default(), value_type, variance)
    }

    pub fn matches(
        &self,
        i_s: &InferenceState,
        matcher: &mut Matcher,
        value_type: &Self,
        variance: Variance,
    ) -> Match {
        match variance {
            Variance::Covariant => self.is_super_type_of(i_s, matcher, value_type),
            Variance::Invariant => self.is_same_type(i_s, matcher, value_type),
            Variance::Contravariant => self.is_sub_type_of(i_s, matcher, value_type),
        }
    }

    fn check_protocol_and_other_side(
        &self,
        i_s: &InferenceState,
        matcher: &mut Matcher,
        value_type: &Self,
        variance: Variance,
    ) -> Match {
        // 2. Check if it is a class with a protocol
        if let Some(class1) = self.maybe_class(i_s.db) {
            // TODO this should probably be checked before normal mro checking?!
            if class1.use_cached_class_infos(i_s.db).class_type == ClassType::Protocol {
                if let Some(class2) = value_type.maybe_class(i_s.db) {
                    return class1.check_protocol_match(i_s, matcher, class2, variance);
                }
            }
        }
        // 3. Check if the value_type is special like Any or a Typevar and needs to be checked
        //    again.
        match value_type.as_ref() {
            DbType::Any if matcher.is_matching_reverse() => return Match::new_true(),
            DbType::Any => {
                matcher.set_all_contained_type_vars_to_any(i_s, &self.as_ref());
                return Match::True { with_any: true };
            }
            DbType::None if !i_s.db.python_state.project.strict_optional => {
                return Match::new_true()
            }
            DbType::TypeVar(t2) => {
                if matcher.is_matching_reverse() {
                    return matcher.match_or_add_type_var(i_s, t2, self, variance.invert());
                }
                if variance == Variance::Covariant {
                    if let Some(bound) = &t2.type_var.bound {
                        let m = self.simple_matches(i_s, &Type::new(bound), variance);
                        if m.bool() {
                            return m;
                        }
                    } else if !t2.type_var.restrictions.is_empty() {
                        let m = t2
                            .type_var
                            .restrictions
                            .iter()
                            .all(|r| self.simple_matches(i_s, &Type::new(r), variance).bool());
                        if m {
                            return Match::new_true();
                        }
                    }
                }
            }
            // Necessary to e.g. match int to Literal[1, 2]
            DbType::Union(u2)
                if variance == Variance::Covariant
                // Union matching was already done.
                && !self.is_union() =>
            {
                if matcher.is_matching_reverse() {
                    debug!("TODO matching reverse?");
                }
                let mut result: Option<Match> = None;
                for t in u2.iter() {
                    let r = self.matches(i_s, matcher, &Type::new(t), variance);
                    if !r.bool() {
                        return r.bool().into();
                    } else {
                        if let Some(old) = result {
                            result = Some(old & r)
                        } else {
                            result = Some(r)
                        }
                    }
                }
                return result.unwrap();
            }
            DbType::NewType(n2) => {
                let t = n2.type_(i_s);
                return self.matches(i_s, matcher, &Type::new(t), variance);
            }
            DbType::Never if variance == Variance::Covariant => return Match::new_true(), // Never is assignable to anything
            DbType::Self_ if variance == Variance::Covariant => {
                return self.simple_matches(
                    i_s,
                    &Type::owned(i_s.current_class().unwrap().as_db_type(i_s.db)),
                    variance,
                )
            }
            DbType::Module(_) => {
                return self.matches(
                    i_s,
                    matcher,
                    &i_s.db.python_state.module_db_type().into(),
                    variance,
                )
            }
            _ => (),
        }
        Match::new_false()
    }

    pub fn mro<'db: 'x, 'x>(&'x self, db: &'db Database) -> Option<MroIterator<'db, '_>> {
        match self.as_ref() {
            DbType::Class(link, generics) => Some(Class::from_db_type(db, *link, generics).mro(db)),
            DbType::Tuple(tup) => Some({
                let tuple_class = db.python_state.tuple_class(db, tup);
                MroIterator::new(
                    db,
                    TypeOrClass::Type(Type::new(self.as_ref())),
                    tuple_class.generics,
                    tuple_class.use_cached_class_infos(db).mro.iter(),
                    false,
                )
            }),
            _ => None,
        }
    }

    fn matches_object_class(&self, db: &Database, value_type: &Type) -> Match {
        self.maybe_class(db)
            .map(|c| {
                let m = c.is_object_class(db);
                if m.bool() && matches!(value_type.as_ref(), DbType::Any) {
                    Match::True { with_any: true }
                } else {
                    m
                }
            })
            .unwrap_or_else(Match::new_false)
    }

    fn matches_union(
        &self,
        i_s: &InferenceState,
        matcher: &mut Matcher,
        u1: &UnionType,
        value_type: &Self,
        variance: Variance,
    ) -> Match {
        match value_type.as_ref() {
            DbType::Union(u2) => match variance {
                Variance::Covariant => {
                    let mut matches = true;
                    for g2 in u2.iter() {
                        let t2 = Type::new(g2);
                        matches &= u1
                            .iter()
                            .any(|g1| Type::new(g1).matches(i_s, matcher, &t2, variance).bool())
                    }
                    matches.into()
                }
                Variance::Invariant => {
                    self.is_super_type_of(i_s, matcher, value_type)
                        & self.is_sub_type_of(i_s, matcher, value_type)
                }
                Variance::Contravariant => unreachable!(),
            },
            _ => match variance {
                Variance::Covariant => u1
                    .iter()
                    .any(|g| {
                        Type::new(g)
                            .matches(i_s, matcher, value_type, variance)
                            .bool()
                    })
                    .into(),
                Variance::Invariant => u1
                    .iter()
                    .all(|g| {
                        Type::new(g)
                            .matches(i_s, matcher, value_type, variance)
                            .bool()
                    })
                    .into(),
                Variance::Contravariant => unreachable!(),
            },
        }
    }

    fn matches_class(
        i_s: &InferenceState,
        matcher: &mut Matcher,
        class1: &Class,
        class2: &Class,
        variance: Variance,
    ) -> Match {
        if class1.node_ref != class2.node_ref {
            return Match::new_false();
        }
        if let Some(type_vars) = class1.type_vars(i_s) {
            let c1_generics = class1.generics();
            let c2_generics = class2.generics();
            let result = c1_generics
                .matches(i_s, matcher, c2_generics, type_vars, variance)
                .similar_if_false();
            if !result.bool() {
                let mut check = |i_s: &InferenceState, n| {
                    let type_var_like = &type_vars[n];
                    let t1 = c1_generics.nth_type_argument(i_s.db, type_var_like, n);
                    if t1.is_any() {
                        return false;
                    }
                    let t2 = c2_generics.nth_type_argument(i_s.db, type_var_like, n);
                    if t2.is_any() {
                        return false;
                    }
                    t1.matches(i_s, matcher, &t2, variance).bool()
                };
                if class1.node_ref == i_s.db.python_state.list_node_ref() && check(i_s, 0) {
                    return Match::False {
                        similar: true,
                        reason: MismatchReason::SequenceInsteadOfListNeeded,
                    };
                } else if class1.node_ref == i_s.db.python_state.dict_node_ref() && check(i_s, 1) {
                    return Match::False {
                        similar: true,
                        reason: MismatchReason::MappingInsteadOfDictNeeded,
                    };
                }
            }
            return result;
        }
        return Match::new_true();
    }

    fn matches_class_against_type(
        i_s: &InferenceState,
        matcher: &mut Matcher,
        class1: &Class,
        value_type: &Type,
        variance: Variance,
    ) -> Match {
        match value_type.as_ref() {
            DbType::Class(link, generics) => {
                let class2 = Class::from_db_type(i_s.db, *link, generics);
                Self::matches_class(i_s, matcher, class1, &class2, variance)
            }
            DbType::Type(t2) => {
                if let DbType::Class(c2, generics2) = t2.as_ref() {
                    let class2 = Class::from_db_type(i_s.db, *c2, generics2);
                    match class2.use_cached_class_infos(i_s.db).metaclass {
                        MetaclassState::Some(link) => {
                            return Type::owned(class1.as_db_type(i_s.db)).matches(
                                i_s,
                                matcher,
                                &Type::owned(DbType::Class(link, ClassGenerics::None)),
                                variance,
                            );
                        }
                        MetaclassState::Unknown => {
                            todo!()
                        }
                        MetaclassState::None => (),
                    }
                }
                Match::new_false()
            }
            DbType::Literal(literal) if variance == Variance::Covariant => {
                Self::matches_class_against_type(
                    i_s,
                    matcher,
                    class1,
                    &i_s.db.python_state.literal_type(literal.kind),
                    variance,
                )
            }
            _ => Match::new_false(),
        }
    }

    fn matches_callable(
        i_s: &InferenceState,
        matcher: &mut Matcher,
        c1: &CallableContent,
        c2: &CallableContent,
    ) -> Match {
        // TODO This if is weird.
        if !matcher.has_type_var_matcher() {
            if let Some(c2_type_vars) = c2.type_vars.as_ref() {
                let mut matcher = Matcher::new_reverse_callable_matcher(c2, c2_type_vars.len());
                return Type::matches_callable(i_s, &mut matcher, c1, c2);
            }
        }
        Type::new(&c1.result_type).is_super_type_of(i_s, matcher, &Type::new(&c2.result_type))
            & matches_params(
                i_s,
                matcher,
                &c1.params,
                &c2.params,
                c2.type_vars.as_ref().map(|t| (t, c2.defined_at)),
                Variance::Contravariant,
                false,
            )
    }

    fn matches_tuple(
        i_s: &InferenceState,
        matcher: &mut Matcher,
        t1: &TupleContent,
        t2: &TupleContent,
        variance: Variance,
    ) -> Match {
        if let Some(t1_args) = &t1.args {
            if let Some(t2_args) = &t2.args {
                return match_tuple_type_arguments(i_s, matcher, t1_args, t2_args, variance);
            } else {
                // TODO maybe set generics to any?
            }
        }
        Match::new_true()
    }

    fn overlaps_tuple(i_s: &InferenceState, t1: &TupleContent, t2: &TupleContent) -> bool {
        use TupleTypeArguments::*;
        match (&t1.args, &t2.args) {
            (Some(FixedLength(ts1)), Some(FixedLength(ts2))) => {
                let mut value_generics = ts2.iter();
                let mut overlaps = true;
                for type1 in ts1.iter() {
                    /*
                    if matcher.might_have_defined_type_vars() {
                        match type1 {
                            DbType::TypeVarLike(t) if t.is_type_var_tuple() => {
                                todo!()
                            }
                            _ => (),
                        }
                    }
                    */
                    if let Some(type2) = value_generics.next() {
                        match (type1, type2) {
                            (TypeOrTypeVarTuple::Type(t1), TypeOrTypeVarTuple::Type(t2)) => {
                                overlaps &= Type::new(t1).overlaps(i_s, &Type::new(t2));
                            }
                            _ => todo!(),
                        }
                    } else {
                        overlaps = false;
                    }
                }
                if value_generics.next().is_some() {
                    overlaps = false;
                }
                overlaps
            }
            (Some(ArbitraryLength(t1)), Some(ArbitraryLength(t2))) => {
                Type::new(t1.as_ref()).overlaps(i_s, &Type::new(t2.as_ref()))
            }
            (Some(ArbitraryLength(t1)), Some(FixedLength(ts2))) => {
                let t1 = Type::new(t1.as_ref());
                ts2.iter().all(|t2| match t2 {
                    TypeOrTypeVarTuple::Type(t2) => t1.overlaps(i_s, &Type::new(t2)),
                    TypeOrTypeVarTuple::TypeVarTuple(t2) => {
                        todo!()
                    }
                })
            }
            (Some(FixedLength(ts1)), Some(ArbitraryLength(t2))) => {
                let t2 = Type::new(t2.as_ref());
                ts1.iter().all(|t1| match t1 {
                    TypeOrTypeVarTuple::Type(t1) => Type::new(t1).overlaps(i_s, &t2),
                    TypeOrTypeVarTuple::TypeVarTuple(t1) => {
                        todo!()
                    }
                })
            }
            _ => true,
        }
    }

    pub fn overlaps_class(i_s: &InferenceState, class1: Class, class2: Class) -> bool {
        let check = {
            #[inline]
            |i_s: &InferenceState, t1: &Type, t2: &Type| {
                t1.maybe_class(i_s.db)
                    .and_then(|c1| {
                        t2.maybe_class(i_s.db).map(|c2| {
                            c1.node_ref == c2.node_ref && {
                                let type_vars = c1.type_vars(i_s);
                                c1.generics().overlaps(i_s, c2.generics(), type_vars)
                            }
                        })
                    })
                    .unwrap_or(false)
            }
        };

        for (_, c1) in class1.mro(i_s.db) {
            if let TypeOrClass::Class(c1) = c1 {
                if Self::overlaps_class(i_s, c1, class2) {
                    return true;
                }
            }
        }
        for (_, c2) in class2.mro(i_s.db) {
            if let TypeOrClass::Class(c2) = c2 {
                if Self::overlaps_class(i_s, class1, c2) {
                    return true;
                }
            }
        }
        false
    }

    pub fn error_if_not_matches<'db>(
        &self,
        i_s: &InferenceState<'db, '_>,
        value: &Inferred,
        callback: impl FnOnce(&InferenceState<'db, '_>, Box<str>, Box<str>) -> NodeRef<'db>,
    ) {
        self.error_if_not_matches_with_matcher(
            i_s,
            &mut Matcher::default(),
            value,
            Some(
                |i_s: &InferenceState<'db, '_>, t1, t2, reason: &MismatchReason| {
                    callback(i_s, t1, t2)
                },
            ),
        );
    }

    pub fn error_if_not_matches_with_matcher<'db>(
        &self,
        i_s: &InferenceState<'db, '_>,
        matcher: &mut Matcher,
        value: &Inferred,
        callback: Option<
            impl FnOnce(&InferenceState<'db, '_>, Box<str>, Box<str>, &MismatchReason) -> NodeRef<'db>,
        >,
    ) -> Match {
        let value_type = value.as_type(i_s);
        let matches = self.is_super_type_of(i_s, matcher, &value_type);
        if let Match::False { ref reason, .. } = matches {
            let mut fmt1 = FormatData::new_short(i_s.db);
            let mut fmt2 = FormatData::with_matcher(i_s.db, matcher);
            let mut input = value_type.format(&fmt1);
            let mut wanted = self.format(&fmt2);
            if input == wanted {
                fmt1.enable_verbose();
                fmt2.enable_verbose();
                input = value_type.format(&fmt1);
                wanted = self.format(&fmt2);
            }
            debug!(
                "Mismatch between {input:?} and {wanted:?} -> {:?}",
                &matches
            );
            if let Some(callback) = callback {
                let node_ref = callback(i_s, input, wanted, reason);
                match reason {
                    MismatchReason::SequenceInsteadOfListNeeded => {
                        node_ref.add_typing_issue(
                            i_s,
                            IssueType::InvariantNote {
                                actual: "List",
                                maybe: "Sequence",
                            },
                        );
                    }
                    MismatchReason::MappingInsteadOfDictNeeded => {
                        node_ref.add_typing_issue(
                            i_s,
                            IssueType::InvariantNote {
                                actual: "Dict",
                                maybe: "Mapping",
                            },
                        );
                    }
                    MismatchReason::ProtocolMismatches { notes } => {
                        for note in notes.iter() {
                            node_ref.add_typing_issue(i_s, IssueType::Note(note.clone()));
                        }
                    }
                    _ => (),
                }
            }
        }
        matches
    }

    pub fn try_to_resemble_context(
        self,
        i_s: &InferenceState,
        matcher: &mut Matcher,
        t: &Self,
    ) -> DbType {
        use TupleTypeArguments::*;
        match t.as_ref() {
            DbType::Class(c1, generics) => {
                let class = Class::from_db_type(i_s.db, *c1, generics);
                if let Some(mro) = self.mro(i_s.db) {
                    for (_, type_or_class) in mro {
                        match type_or_class {
                            TypeOrClass::Class(value_class) => {
                                if Self::matches_class(
                                    i_s,
                                    &mut Matcher::default(),
                                    &class,
                                    &value_class,
                                    Variance::Covariant,
                                )
                                .bool()
                                {
                                    return value_class.as_db_type(i_s.db);
                                }
                            }
                            TypeOrClass::Type(value_type) => {
                                if Self::matches_class_against_type(
                                    i_s,
                                    &mut Matcher::default(),
                                    &class,
                                    &value_type,
                                    Variance::Covariant,
                                )
                                .bool()
                                {
                                    return value_type.into_db_type();
                                }
                            }
                        }
                    }
                }
            }
            DbType::Tuple(t1) => {
                if let DbType::Tuple(t2) = self.as_ref() {
                    match (&t1.args, &t2.args) {
                        (Some(FixedLength(ts1)), Some(FixedLength(ts2))) => {
                            return DbType::Tuple(Rc::new(TupleContent::new_fixed_length(
                                ts1.iter()
                                    .zip(ts2.iter())
                                    .map(|(g1, g2)| match (g1, g2) {
                                        (
                                            TypeOrTypeVarTuple::Type(g1),
                                            TypeOrTypeVarTuple::Type(g2),
                                        ) => TypeOrTypeVarTuple::Type(
                                            Type::new(g2).try_to_resemble_context(
                                                i_s,
                                                matcher,
                                                &Type::new(g1),
                                            ),
                                        ),
                                        _ => todo!(),
                                    })
                                    .collect(),
                            )))
                        }
                        (Some(ArbitraryLength(t1)), Some(ArbitraryLength(t2))) => {
                            return DbType::Tuple(Rc::new(TupleContent::new_arbitrary_length(
                                Type::new(t1.as_ref()).try_to_resemble_context(
                                    i_s,
                                    matcher,
                                    &Type::new(t2.as_ref()),
                                ),
                            )))
                        }
                        (Some(ArbitraryLength(t1)), Some(FixedLength(ts2))) => {
                            /*
                            let t1 = Type::new(t1);
                            generics2
                                .iter()
                                .all(|g2| {
                                    let t2 = Type::new(g2);
                                    t1.is_super_type_of(i_s, matcher, &t2).bool()
                                })
                                .into()
                            */
                            todo!()
                        }
                        (Some(FixedLength(ts1)), Some(ArbitraryLength(t2))) => unreachable!(),
                        _ => (),
                    }
                }
            }
            DbType::TypeVar(_) => {
                if matcher.might_have_defined_type_vars() {
                    let t = Type::owned(
                        matcher.replace_type_var_likes_for_nested_context(i_s.db, t.as_ref()),
                    );
                    return self.try_to_resemble_context(i_s, &mut Matcher::default(), &t);
                }
            }
            DbType::Type(t1) => todo!(),
            _ => (),
        }
        self.into_db_type()
    }

    pub fn execute_and_resolve_type_vars(
        &self,
        i_s: &InferenceState,
        class: Option<&Class>,
        self_class: Option<&Class>,
        calculated_type_args: &CalculatedTypeArguments,
    ) -> Inferred {
        let db_type = self.internal_resolve_type_vars(i_s, class, self_class, calculated_type_args);
        debug!(
            "Resolved type vars: {}",
            Type::new(&db_type).format_short(i_s.db)
        );
        Inferred::from_type(db_type)
    }

    fn internal_resolve_type_vars(
        &self,
        i_s: &InferenceState,
        class: Option<&Class>,
        self_class: Option<&Class>,
        calculated_type_args: &CalculatedTypeArguments,
    ) -> DbType {
        let mut replace_self = || {
            self_class
                .map(|c| c.as_db_type(i_s.db))
                .unwrap_or(DbType::Self_)
        };
        self.replace_type_var_likes_and_self(
            i_s.db,
            &mut |t| calculated_type_args.lookup_type_var_usage(i_s, class, t),
            &mut replace_self,
        )
    }

    pub fn replace_type_var_likes(
        &self,
        db: &Database,
        callable: &mut impl FnMut(TypeVarLikeUsage) -> GenericItem,
    ) -> DbType {
        self.replace_type_var_likes_and_self(db, callable, &mut || DbType::Self_)
    }

    pub fn replace_type_var_likes_and_self(
        &self,
        db: &Database,
        callable: ReplaceTypeVarLike,
        replace_self: ReplaceSelf,
    ) -> DbType {
        let remap_tuple_likes = |args: &TupleTypeArguments,
                                 callable: ReplaceTypeVarLike,
                                 replace_self: ReplaceSelf| {
            match args {
                TupleTypeArguments::FixedLength(ts) => {
                    let mut new_args = vec![];
                    for g in ts.iter() {
                        match g {
                            TypeOrTypeVarTuple::Type(t) => new_args.push(TypeOrTypeVarTuple::Type(
                                Type::new(t).replace_type_var_likes_and_self(
                                    db,
                                    callable,
                                    replace_self,
                                ),
                            )),
                            TypeOrTypeVarTuple::TypeVarTuple(t) => {
                                match callable(TypeVarLikeUsage::TypeVarTuple(Cow::Borrowed(t))) {
                                    GenericItem::TypeArguments(new) => {
                                        new_args.extend(match new.args {
                                            TupleTypeArguments::FixedLength(fixed) => {
                                                fixed.into_vec().into_iter()
                                            }
                                            TupleTypeArguments::ArbitraryLength(t) => {
                                                match ts.len() {
                                                    // TODO this might be wrong with different data types??
                                                    1 => {
                                                        return TupleTypeArguments::ArbitraryLength(
                                                            t,
                                                        )
                                                    }
                                                    _ => todo!(),
                                                }
                                            }
                                        })
                                    }
                                    x => unreachable!("{x:?}"),
                                }
                            }
                        }
                    }
                    TupleTypeArguments::FixedLength(new_args.into())
                }
                TupleTypeArguments::ArbitraryLength(t) => {
                    TupleTypeArguments::ArbitraryLength(Box::new(
                        Type::new(t).replace_type_var_likes_and_self(db, callable, replace_self),
                    ))
                }
            }
        };
        let remap_generics = |generics: &GenericsList| {
            GenericsList::new_generics(
                generics
                    .iter()
                    .map(|g| match g {
                        GenericItem::TypeArgument(t) => {
                            GenericItem::TypeArgument(Type::new(t).replace_type_var_likes_and_self(
                                db,
                                callable,
                                replace_self,
                            ))
                        }
                        GenericItem::TypeArguments(ts) => {
                            GenericItem::TypeArguments(TypeArguments {
                                args: remap_tuple_likes(&ts.args, callable, replace_self),
                            })
                        }
                        GenericItem::ParamSpecArgument(p) => {
                            let mut type_vars = p.type_vars.clone().map(|t| t.type_vars.into_vec());
                            GenericItem::ParamSpecArgument(ParamSpecArgument::new(
                                Self::remap_callable_params(
                                    db,
                                    &p.params,
                                    &mut type_vars,
                                    p.type_vars.as_ref().map(|t| t.in_definition),
                                    callable,
                                    replace_self,
                                )
                                .0,
                                type_vars.map(|t| ParamSpecTypeVars {
                                    type_vars: TypeVarLikes::from_vec(t),
                                    in_definition: p.type_vars.as_ref().unwrap().in_definition,
                                }),
                            ))
                        }
                    })
                    .collect(),
            )
        };
        match self.as_ref() {
            DbType::Any => DbType::Any,
            DbType::None => DbType::None,
            DbType::Never => DbType::Never,
            DbType::Class(link, generics) => {
                DbType::Class(*link, generics.map_list(remap_generics))
            }
            DbType::FunctionOverload(callables) => DbType::FunctionOverload(
                callables
                    .iter()
                    .map(|c| {
                        Self::replace_type_var_likes_and_self_for_callable(
                            c,
                            db,
                            callable,
                            replace_self,
                        )
                    })
                    .collect(),
            ),
            DbType::Union(u) => {
                let mut entries: Vec<UnionEntry> = Vec::with_capacity(u.entries.len());
                let mut add = |type_, format_index| {
                    if matches!(type_, DbType::None) && !db.python_state.project.strict_optional {
                        return;
                    }
                    // Simplify duplicates & subclass removal
                    let i_s = InferenceState::new(db);
                    let mut matcher = Matcher::with_ignored_promotions();
                    match &type_ {
                        DbType::RecursiveAlias(r1) if r1.generics.is_some() => {
                            // Recursive aliases need special handling, because the normal subtype
                            // checking will call this function again if generics are available to
                            // cache the type. In that case we just avoid complex matching and use
                            // a simple heuristic. This won't affect correctness, it might just
                            // display a bigger union than necessary.
                            for entry in entries.iter() {
                                if let DbType::RecursiveAlias(r2) = &entry.type_ {
                                    if r1 == r2 {
                                        return;
                                    }
                                }
                            }
                        }
                        _ => {
                            let t = Type::new(&type_);
                            for entry in entries.iter_mut() {
                                let current = Type::new(&entry.type_);
                                if entry.type_.has_any(&i_s) || type_.has_any(&i_s) {
                                    if entry.type_ == type_ {
                                        return;
                                    }
                                } else {
                                    match &entry.type_ {
                                        DbType::RecursiveAlias(r) if r.generics.is_some() => (),
                                        _ => {
                                            if current
                                                .is_super_type_of(&i_s, &mut matcher, &t)
                                                .bool()
                                            {
                                                return; // Type is already in the union
                                            }
                                            if current.is_sub_type_of(&i_s, &mut matcher, &t).bool()
                                            {
                                                // The new type is more general and therefore needs to be used.
                                                entry.type_ = type_;
                                                return;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    entries.push(UnionEntry {
                        type_,
                        format_index,
                    })
                };
                for entry in u.entries.iter() {
                    match Type::new(&entry.type_).replace_type_var_likes_and_self(
                        db,
                        callable,
                        replace_self,
                    ) {
                        DbType::Union(inner) => {
                            for inner_entry in inner.entries.into_vec().into_iter() {
                                match inner_entry.type_ {
                                    DbType::Union(_) => unreachable!(),
                                    type_ => add(type_, entry.format_index),
                                }
                            }
                        }
                        type_ => add(type_, entry.format_index),
                    }
                }
                match entries.len() {
                    0 => DbType::None,
                    1 => entries.into_iter().next().unwrap().type_,
                    _ => {
                        let mut union = UnionType {
                            entries: entries.into_boxed_slice(),
                            format_as_optional: u.format_as_optional,
                        };
                        union.sort_for_priority();
                        DbType::Union(union)
                    }
                }
            }
            DbType::TypeVar(t) => match callable(TypeVarLikeUsage::TypeVar(Cow::Borrowed(t))) {
                GenericItem::TypeArgument(t) => t,
                GenericItem::TypeArguments(ts) => unreachable!(),
                GenericItem::ParamSpecArgument(params) => todo!(),
            },
            DbType::Type(t) => DbType::Type(Rc::new(Type::new(t).replace_type_var_likes_and_self(
                db,
                callable,
                replace_self,
            ))),
            DbType::Tuple(content) => DbType::Tuple(match &content.args {
                Some(args) => Rc::new(TupleContent::new(remap_tuple_likes(
                    args,
                    callable,
                    replace_self,
                ))),
                None => TupleContent::new_empty(),
            }),
            DbType::Callable(content) => {
                DbType::Callable(Rc::new(Self::replace_type_var_likes_and_self_for_callable(
                    content,
                    db,
                    callable,
                    replace_self,
                )))
            }
            DbType::Literal { .. } => self.as_db_type(),
            DbType::NewType(t) => DbType::NewType(t.clone()),
            DbType::RecursiveAlias(rec) => DbType::RecursiveAlias(Rc::new(RecursiveAlias::new(
                rec.link,
                rec.generics.as_ref().map(remap_generics),
            ))),
            DbType::Module(file_index) => DbType::Module(*file_index),
            DbType::Self_ => replace_self(),
            DbType::ParamSpecArgs(usage) => todo!(),
            DbType::ParamSpecKwargs(usage) => todo!(),
            DbType::NamedTuple(nt) => {
                let mut constructor = nt.constructor.as_ref().clone();
                let CallableParams::Simple(params) = &constructor.params else {
                    unreachable!();
                };
                constructor.params = CallableParams::Simple(
                    params
                        .iter()
                        .map(|param| {
                            let ParamSpecific::PositionalOrKeyword(t) = &param.param_specific else {
                        unreachable!()
                    };
                            CallableParam {
                                param_specific: ParamSpecific::PositionalOrKeyword(
                                    Type::new(t).replace_type_var_likes_and_self(
                                        db,
                                        callable,
                                        replace_self,
                                    ),
                                ),
                                has_default: param.has_default,
                                name: param.name,
                            }
                        })
                        .collect(),
                );
                DbType::NamedTuple(Rc::new(NamedTuple::new(nt.name, constructor)))
            }
        }
    }

    pub fn replace_type_var_likes_and_self_for_callable(
        c: &CallableContent,
        db: &Database,
        callable: ReplaceTypeVarLike,
        replace_self: ReplaceSelf,
    ) -> CallableContent {
        let mut type_vars = c.type_vars.clone().map(|t| t.into_vec());
        let (params, remap_data) = Self::remap_callable_params(
            db,
            &c.params,
            &mut type_vars,
            Some(c.defined_at),
            callable,
            replace_self,
        );
        let mut result_type =
            Type::new(&c.result_type).replace_type_var_likes_and_self(db, callable, replace_self);
        if let Some(remap_data) = remap_data {
            result_type = Type::new(&result_type).replace_type_var_likes_and_self(
                db,
                &mut |usage| Self::remap_param_spec_inner(usage, c.defined_at, remap_data),
                replace_self,
            );
        }
        CallableContent {
            name: c.name,
            class_name: c.class_name,
            defined_at: c.defined_at,
            type_vars: type_vars.map(TypeVarLikes::from_vec),
            params,
            result_type,
        }
    }

    fn rewrite_late_bound_callables_for_callable(
        c: &CallableContent,
        manager: &TypeVarManager,
    ) -> CallableContent {
        let type_vars = manager
            .iter()
            .filter_map(|t| {
                (t.most_outer_callable == Some(c.defined_at)).then(|| t.type_var_like.clone())
            })
            .collect::<Rc<_>>();
        CallableContent {
            name: c.name,
            class_name: c.class_name,
            defined_at: c.defined_at,
            type_vars: (!type_vars.is_empty()).then_some(TypeVarLikes::new(type_vars)),
            params: match &c.params {
                CallableParams::Simple(params) => CallableParams::Simple(
                    params
                        .iter()
                        .map(|p| CallableParam {
                            param_specific: match &p.param_specific {
                                ParamSpecific::PositionalOnly(t) => ParamSpecific::PositionalOnly(
                                    Type::new(t).rewrite_late_bound_callables(manager),
                                ),
                                ParamSpecific::PositionalOrKeyword(t) => {
                                    ParamSpecific::PositionalOrKeyword(
                                        Type::new(t).rewrite_late_bound_callables(manager),
                                    )
                                }
                                ParamSpecific::KeywordOnly(t) => ParamSpecific::KeywordOnly(
                                    Type::new(t).rewrite_late_bound_callables(manager),
                                ),
                                ParamSpecific::Starred(s) => ParamSpecific::Starred(match s {
                                    StarredParamSpecific::ArbitraryLength(t) => {
                                        StarredParamSpecific::ArbitraryLength(
                                            Type::new(t).rewrite_late_bound_callables(manager),
                                        )
                                    }
                                    StarredParamSpecific::ParamSpecArgs(_) => todo!(),
                                }),
                                ParamSpecific::DoubleStarred(d) => {
                                    ParamSpecific::DoubleStarred(match d {
                                        DoubleStarredParamSpecific::ValueType(t) => {
                                            DoubleStarredParamSpecific::ValueType(
                                                Type::new(t).rewrite_late_bound_callables(manager),
                                            )
                                        }
                                        DoubleStarredParamSpecific::ParamSpecKwargs(_) => {
                                            todo!()
                                        }
                                    })
                                }
                            },
                            has_default: p.has_default,
                            name: p.name,
                        })
                        .collect(),
                ),
                CallableParams::Any => CallableParams::Any,
                CallableParams::WithParamSpec(types, param_spec) => CallableParams::WithParamSpec(
                    types
                        .iter()
                        .map(|t| Type::new(t).rewrite_late_bound_callables(manager))
                        .collect(),
                    manager.remap_param_spec(param_spec),
                ),
            },
            result_type: Type::new(&c.result_type).rewrite_late_bound_callables(manager),
        }
    }

    fn remap_callable_params(
        db: &Database,
        params: &CallableParams,
        type_vars: &mut Option<Vec<TypeVarLike>>,
        in_definition: Option<PointLink>,
        callable: ReplaceTypeVarLike,
        replace_self: ReplaceSelf,
    ) -> (CallableParams, Option<(PointLink, usize)>) {
        let mut remap_data = None;
        let new_params = match params {
            CallableParams::Simple(params) => CallableParams::Simple(
                params
                    .iter()
                    .map(|p| CallableParam {
                        param_specific: match &p.param_specific {
                            ParamSpecific::PositionalOnly(t) => ParamSpecific::PositionalOnly(
                                Type::new(t).replace_type_var_likes_and_self(
                                    db,
                                    callable,
                                    replace_self,
                                ),
                            ),
                            ParamSpecific::PositionalOrKeyword(t) => {
                                ParamSpecific::PositionalOrKeyword(
                                    Type::new(t).replace_type_var_likes_and_self(
                                        db,
                                        callable,
                                        replace_self,
                                    ),
                                )
                            }
                            ParamSpecific::KeywordOnly(t) => ParamSpecific::KeywordOnly(
                                Type::new(t).replace_type_var_likes_and_self(
                                    db,
                                    callable,
                                    replace_self,
                                ),
                            ),
                            ParamSpecific::Starred(s) => ParamSpecific::Starred(match s {
                                StarredParamSpecific::ArbitraryLength(t) => {
                                    StarredParamSpecific::ArbitraryLength(
                                        Type::new(t).replace_type_var_likes_and_self(
                                            db,
                                            callable,
                                            replace_self,
                                        ),
                                    )
                                }
                                StarredParamSpecific::ParamSpecArgs(_) => todo!(),
                            }),
                            ParamSpecific::DoubleStarred(d) => {
                                ParamSpecific::DoubleStarred(match d {
                                    DoubleStarredParamSpecific::ValueType(t) => {
                                        DoubleStarredParamSpecific::ValueType(
                                            Type::new(t).replace_type_var_likes_and_self(
                                                db,
                                                callable,
                                                replace_self,
                                            ),
                                        )
                                    }
                                    DoubleStarredParamSpecific::ParamSpecKwargs(_) => {
                                        todo!()
                                    }
                                })
                            }
                        },
                        has_default: p.has_default,
                        name: p.name,
                    })
                    .collect(),
            ),
            CallableParams::Any => CallableParams::Any,
            CallableParams::WithParamSpec(types, param_spec) => {
                let result = callable(TypeVarLikeUsage::ParamSpec(Cow::Borrowed(param_spec)));
                let GenericItem::ParamSpecArgument(mut new) = result else {
                    unreachable!()
                };
                if let Some(new_spec_type_vars) = new.type_vars {
                    if let Some(in_definition) = in_definition {
                        let type_var_len = type_vars.as_ref().map(|t| t.len()).unwrap_or(0);
                        remap_data = Some((new_spec_type_vars.in_definition, type_var_len));
                        let new_params = Type::remap_callable_params(
                            db,
                            &new.params,
                            &mut None,
                            None,
                            &mut |usage| {
                                Type::remap_param_spec_inner(
                                    usage,
                                    in_definition,
                                    remap_data.unwrap(),
                                )
                            },
                            replace_self,
                        );
                        if let Some(type_vars) = type_vars.as_mut() {
                            type_vars.extend(new_spec_type_vars.type_vars.into_vec());
                        } else {
                            *type_vars = Some(new_spec_type_vars.type_vars.into_vec());
                        }
                        new.params = new_params.0;
                    } else {
                        debug_assert!(type_vars.is_none());
                        *type_vars = Some(new_spec_type_vars.type_vars.into_vec());
                        todo!("Can probably just be removed")
                    }
                }
                if types.is_empty() {
                    new.params
                } else {
                    match new.params {
                        CallableParams::Simple(params) => {
                            // Performance issue: Rc -> Vec check https://github.com/rust-lang/rust/issues/93610#issuecomment-1528108612
                            let mut params = Vec::from(params.as_ref());
                            params.splice(
                                0..0,
                                types.iter().map(|t| CallableParam {
                                    param_specific: ParamSpecific::PositionalOnly(
                                        Type::new(t).replace_type_var_likes_and_self(
                                            db,
                                            callable,
                                            replace_self,
                                        ),
                                    ),
                                    name: None,
                                    has_default: false,
                                }),
                            );
                            CallableParams::Simple(Rc::from(params))
                        }
                        CallableParams::Any => CallableParams::Any,
                        CallableParams::WithParamSpec(new_types, p) => {
                            let mut types: Vec<DbType> = types
                                .iter()
                                .map(|t| {
                                    Type::new(t).replace_type_var_likes_and_self(
                                        db,
                                        callable,
                                        replace_self,
                                    )
                                })
                                .collect();
                            // Performance issue: Rc -> Vec check https://github.com/rust-lang/rust/issues/93610#issuecomment-1528108612
                            types.extend(new_types.iter().cloned());
                            CallableParams::WithParamSpec(types.into(), p)
                        }
                    }
                }
            }
        };
        (new_params, remap_data)
    }

    fn remap_param_spec_inner(
        mut usage: TypeVarLikeUsage,
        in_definition: PointLink,
        remap_data: (PointLink, usize),
    ) -> GenericItem {
        if usage.in_definition() == remap_data.0 {
            usage.update_in_definition_and_index(
                in_definition,
                (usage.index().as_usize() + remap_data.1).into(),
            );
        }
        usage.into_generic_item()
    }

    pub fn rewrite_late_bound_callables(&self, manager: &TypeVarManager) -> DbType {
        let rewrite_generics = |generics: &GenericsList| {
            GenericsList::new_generics(
                generics
                    .iter()
                    .map(|g| match g {
                        GenericItem::TypeArgument(t) => GenericItem::TypeArgument(
                            Type::new(t).rewrite_late_bound_callables(manager),
                        ),
                        GenericItem::TypeArguments(ts) => todo!(),
                        GenericItem::ParamSpecArgument(p) => GenericItem::ParamSpecArgument({
                            debug_assert!(p.type_vars.is_none());
                            ParamSpecArgument {
                                params: match &p.params {
                                    CallableParams::WithParamSpec(types, param_spec) => {
                                        CallableParams::WithParamSpec(
                                            types
                                                .iter()
                                                .map(|t| {
                                                    Type::new(t)
                                                        .rewrite_late_bound_callables(manager)
                                                })
                                                .collect(),
                                            manager.remap_param_spec(param_spec),
                                        )
                                    }
                                    CallableParams::Simple(x) => unreachable!(),
                                    CallableParams::Any => unreachable!(),
                                },
                                type_vars: p.type_vars.clone(),
                            }
                        }),
                    })
                    .collect(),
            )
        };
        match self.as_ref() {
            DbType::Any => DbType::Any,
            DbType::None => DbType::None,
            DbType::Never => DbType::Never,
            DbType::Class(link, generics) => {
                DbType::Class(*link, generics.map_list(rewrite_generics))
            }
            DbType::Union(u) => DbType::Union(UnionType {
                entries: u
                    .entries
                    .iter()
                    .map(|e| UnionEntry {
                        type_: Type::new(&e.type_).rewrite_late_bound_callables(manager),
                        format_index: e.format_index,
                    })
                    .collect(),
                format_as_optional: u.format_as_optional,
            }),
            DbType::FunctionOverload(callables) => DbType::FunctionOverload(
                callables
                    .iter()
                    .map(|e| Self::rewrite_late_bound_callables_for_callable(e, manager))
                    .collect(),
            ),
            DbType::TypeVar(t) => DbType::TypeVar(manager.remap_type_var(t)),
            DbType::Type(db_type) => DbType::Type(Rc::new(
                Type::new(db_type).rewrite_late_bound_callables(manager),
            )),
            DbType::Tuple(content) => DbType::Tuple(match &content.args {
                Some(TupleTypeArguments::FixedLength(ts)) => {
                    Rc::new(TupleContent::new_fixed_length(
                        ts.iter()
                            .map(|g| match g {
                                TypeOrTypeVarTuple::Type(t) => TypeOrTypeVarTuple::Type(
                                    Type::new(t).rewrite_late_bound_callables(manager),
                                ),
                                TypeOrTypeVarTuple::TypeVarTuple(t) => {
                                    TypeOrTypeVarTuple::TypeVarTuple(
                                        manager.remap_type_var_tuple(t),
                                    )
                                }
                            })
                            .collect(),
                    ))
                }
                Some(TupleTypeArguments::ArbitraryLength(t)) => {
                    Rc::new(TupleContent::new_arbitrary_length(
                        Type::new(t).rewrite_late_bound_callables(manager),
                    ))
                }
                None => TupleContent::new_empty(),
            }),
            DbType::Literal { .. } => self.as_db_type(),
            DbType::Callable(content) => DbType::Callable(Rc::new(
                Self::rewrite_late_bound_callables_for_callable(content, manager),
            )),
            DbType::NewType(_) => todo!(),
            DbType::RecursiveAlias(recursive_alias) => {
                DbType::RecursiveAlias(Rc::new(RecursiveAlias::new(
                    recursive_alias.link,
                    recursive_alias.generics.as_ref().map(rewrite_generics),
                )))
            }
            DbType::Self_ => DbType::Self_,
            DbType::ParamSpecArgs(usage) => todo!(),
            DbType::ParamSpecKwargs(usage) => todo!(),
            DbType::NamedTuple(_) => todo!(),
            DbType::Module(file_index) => DbType::Module(*file_index),
        }
    }

    pub fn execute<'db>(
        &self,
        i_s: &InferenceState<'db, '_>,
        inferred_from: Option<&Inferred>,
        args: &dyn Arguments<'db>,
        result_context: &mut ResultContext,
        on_type_error: OnTypeError<'db, '_>,
    ) -> Inferred {
        match self.as_ref() {
            DbType::Class(link, generics) => {
                let cls = Class::from_db_type(i_s.db, *link, generics);
                Instance::new(cls, inferred_from).execute(i_s, args, result_context, on_type_error)
            }
            DbType::Type(cls) => {
                execute_type_of_type(i_s, args, result_context, on_type_error, cls.as_ref())
            }
            DbType::Union(union) => Inferred::gather_union(i_s, |gather| {
                for entry in union.iter() {
                    gather(Type::new(entry).execute(i_s, None, args, result_context, on_type_error))
                }
            }),
            t @ DbType::Callable(content) => Callable::new(t, content).execute_internal(
                i_s,
                args,
                on_type_error,
                None,
                result_context,
            ),
            DbType::Any | DbType::Never => {
                args.iter().calculate_diagnostics(i_s);
                Inferred::new_unknown()
            }
            _ => {
                let t = self.format_short(i_s.db);
                args.as_node_ref().add_typing_issue(
                    i_s,
                    IssueType::NotCallable {
                        type_: format!("\"{}\"", t).into(),
                    },
                );
                Inferred::new_unknown()
            }
        }
    }

    pub fn iter_on_borrowed<'slf>(
        &self,
        i_s: &InferenceState<'a, '_>,
        from: NodeRef,
    ) -> IteratorContent<'a> {
        match self.maybe_borrowed_db_type() {
            Some(DbType::Class(l, g)) => {
                Instance::new(Class::from_db_type(i_s.db, *l, g), None).iter(i_s, from)
            }
            Some(DbType::Tuple(content)) => Tuple::new(content).iter(i_s, from),
            Some(DbType::NamedTuple(nt)) => NamedTupleValue::new(i_s.db, nt).iter(i_s, from),
            Some(DbType::Any | DbType::Never) => IteratorContent::Any,
            Some(DbType::Union(union)) => {
                let mut items = vec![];
                for t in union.iter() {
                    items.push(Type::new(t).iter_on_borrowed(i_s, from));
                }
                IteratorContent::Union(items)
            }
            Some(DbType::TypeVar(tv)) if tv.type_var.bound.is_some() => {
                Type::new(tv.type_var.bound.as_ref().unwrap()).iter_on_borrowed(i_s, from)
            }
            Some(DbType::NewType(n)) => Type::new(n.type_(i_s)).iter_on_borrowed(i_s, from),
            Some(DbType::Self_) => todo!(), //Instance::new(*i_s.current_class().unwrap(), None).iter(i_s, from),
            _ => {
                if let DbType::Class(l, ClassGenerics::None) = self.as_ref() {
                    return Instance::new(
                        Class::from_db_type(i_s.db, *l, &ClassGenerics::None),
                        None,
                    )
                    .iter(i_s, from);
                }
                let t = self.format_short(i_s.db);
                from.add_typing_issue(
                    i_s,
                    IssueType::NotIterable {
                        type_: format!("\"{}\"", t).into(),
                    },
                );
                IteratorContent::Any
            }
        }
    }

    pub fn on_any_class(
        &self,
        i_s: &InferenceState,
        matcher: &mut Matcher,
        callable: &mut impl FnMut(&InferenceState, &mut Matcher, &Class) -> bool,
    ) -> bool {
        match self.as_ref() {
            DbType::Class(link, generics) => {
                let class = Class::from_db_type(i_s.db, *link, generics);
                callable(i_s, matcher, &class)
            }
            DbType::Union(union_type) => union_type
                .iter()
                .any(|t| Type::new(t).on_any_class(i_s, matcher, callable)),
            db_type @ DbType::TypeVar(_) => {
                if matcher.might_have_defined_type_vars() {
                    Type::owned(matcher.replace_type_var_likes_for_nested_context(i_s.db, db_type))
                        .on_any_class(i_s, matcher, callable)
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    pub fn common_base_type(&self, i_s: &InferenceState, other: &Self) -> DbType {
        match (self.maybe_class(i_s.db), other.maybe_class(i_s.db)) {
            (Some(c1), Some(c2)) => {
                for (_, c1) in c1.mro(i_s.db) {
                    match c1 {
                        TypeOrClass::Type(c1) => (),
                        TypeOrClass::Class(c1) => {
                            for (_, c2) in c2.mro(i_s.db) {
                                let TypeOrClass::Class(c2) = c2 else {
                                    continue
                                };
                                let m = Self::matches_class(
                                    i_s,
                                    &mut Matcher::default(),
                                    &c1,
                                    &c2,
                                    Variance::Invariant,
                                );
                                if m.bool() {
                                    return c1.as_db_type(i_s.db);
                                }
                            }
                        }
                    }
                }
                unreachable!("object is always a common base class")
            }
            (None, None) => {
                // TODO this should also be done for function/callable and callable/function and
                // not only callable/callable
                if let DbType::Callable(c1) = self.as_ref() {
                    if let DbType::Callable(c2) = other.as_ref() {
                        return DbType::Class(
                            i_s.db.python_state.function_point_link(),
                            ClassGenerics::None,
                        );
                    }
                }
            }
            _ => (),
        }
        i_s.db.python_state.object_db_type()
    }

    pub fn check_duplicate_base_class(&self, db: &Database, other: &Self) -> Option<Box<str>> {
        match (self.as_ref(), other.as_ref()) {
            (DbType::Class(link1, generics), DbType::Class(link2, _)) => (link1 == link2)
                .then(|| Box::from(Class::from_db_type(db, *link1, generics).name())),
            (DbType::Type(_), DbType::Type(_)) => Some(Box::from("type")),
            (DbType::Tuple(_), DbType::Tuple(_)) => Some(Box::from("tuple")),
            (DbType::Callable(_), DbType::Callable(_)) => Some(Box::from("callable")),
            _ => None,
        }
    }

    pub fn is_union(&self) -> bool {
        matches!(self.as_ref(), DbType::Union(_))
    }

    pub fn is_any(&self) -> bool {
        matches!(self.as_ref(), DbType::Any)
    }

    pub fn has_any(&self, i_s: &InferenceState) -> bool {
        self.0.has_any(i_s)
    }

    pub fn has_explicit_self_type(&self, db: &Database) -> bool {
        self.as_ref().has_self_type()
    }

    #[inline]
    pub fn run_on_each_union_type(&self, callable: &mut impl FnMut(&Type)) {
        match self.as_ref() {
            DbType::Union(union) => {
                for t in union.iter() {
                    Type::new(t).run_on_each_union_type(callable)
                }
            }
            DbType::Never => (),
            _ => callable(self),
        }
    }

    #[inline]
    pub fn run_after_lookup_on_each_union_member<'db>(
        &self,
        i_s: &InferenceState<'db, '_>,
        from_inferred: Option<&Inferred>,
        from: Option<NodeRef>,
        name: &str,
        callable: &mut impl FnMut(&Type, LookupResult),
    ) {
        match self.as_ref() {
            DbType::Class(link, generics) => {
                let cls = Class::from_db_type(i_s.db, *link, generics);
                callable(
                    self,
                    Instance::new(cls, from_inferred).lookup(i_s, from, name),
                )
            }
            DbType::Any => callable(self, LookupResult::any()),
            DbType::None => callable(
                self,
                i_s.db
                    .python_state
                    .none_type()
                    .lookup_without_error(i_s, from, name),
            ),
            DbType::Literal(literal) => {
                let t = i_s.db.python_state.literal_type(literal.kind);
                callable(self, t.lookup_without_error(i_s, from, name))
            }
            t @ DbType::TypeVar(tv) => {
                if !tv.type_var.restrictions.is_empty() {
                    debug!("TODO type var values");
                    /*
                    for db_type in self.type_var_usage.type_var.restrictions.iter() {
                        return match db_type {
                            DbType::Class(link) => Instance::new(
                                Class::(NodeRef::from_link(i_s.db, *link), Generics::NotDefinedYet, None)
                                    .unwrap(),
                                &Inferred::from_type(DbType::Class(*link, None)),
                            )
                            .lookup(i_s, name),
                            _ => todo!("{:?}", db_type),
                        }
                    }
                    */
                }
                if let Some(t) = &tv.type_var.bound {
                    Type::new(t)
                        .run_after_lookup_on_each_union_member(i_s, None, from, name, callable);
                    /*
                    if matches!(result, LookupResult::None) {
                        debug!(
                            "Item \"{}\" of the upper bound \"{}\" of type variable \"{}\" has no attribute \"{}\"",
                            db_type.format_short(i_s.db),
                            Type::new(db_type).format_short(i_s.db),
                            tv.type_var.name(i_s.db),
                            name,
                        );
                    }
                    result
                    */
                } else {
                    let s = &i_s.db.python_state;
                    // TODO it's kind of stupid that we recreate an instance object here all the time, we
                    // should just use a precreated object() from somewhere.
                    callable(
                        self,
                        Instance::new(s.object_class(), None).lookup(i_s, from, name),
                    )
                }
            }
            DbType::Tuple(tup) => callable(self, Tuple::new(tup).lookup(i_s, from, name)),
            DbType::Union(union) => {
                for t in union.iter() {
                    Type::new(t)
                        .run_after_lookup_on_each_union_member(i_s, None, from, name, callable)
                }
            }
            DbType::Type(t) => match t.as_ref() {
                DbType::Union(union) => {
                    debug_assert!(!union.entries.is_empty());
                    for t in union.iter() {
                        callable(self, TypingType::new(i_s.db, t).lookup(i_s, None, name))
                    }
                }
                DbType::Any => callable(self, LookupResult::any()),
                _ => callable(self, TypingType::new(i_s.db, t).lookup(i_s, from, name)),
            },
            t @ DbType::Callable(c) => callable(self, Callable::new(t, c).lookup(i_s, from, name)),
            DbType::Module(file_index) => {
                let file = i_s.db.loaded_python_file(*file_index);
                callable(self, Module::new(file).lookup(i_s, from, name))
            }
            DbType::Self_ => {
                let current_class = i_s.current_class().unwrap();
                let type_var_likes = current_class.type_vars(i_s);
                callable(
                    self,
                    Instance::new(
                        Class::from_position(
                            current_class.node_ref.to_db_lifetime(i_s.db),
                            Generics::Self_ {
                                class_definition: current_class.node_ref.as_link(),
                                type_var_likes,
                            },
                            None,
                        ),
                        from_inferred,
                    )
                    .lookup(i_s, from, name),
                )
            }
            DbType::NamedTuple(nt) => callable(
                self,
                NamedTupleValue::new(i_s.db, nt).lookup(i_s, from, name),
            ),
            DbType::Never => (),
            DbType::NewType(new_type) => Type::new(&new_type.type_(i_s))
                .run_after_lookup_on_each_union_member(i_s, None, from, name, callable),
            _ => todo!("{self:?}"),
        }
    }

    pub fn lookup_without_error<'db>(
        &self,
        i_s: &InferenceState<'db, '_>,
        from: Option<NodeRef>,
        name: &str,
    ) -> LookupResult {
        self.lookup_helper(i_s, from, name, &|_| ())
    }

    pub fn lookup_with_error<'db>(
        &self,
        i_s: &InferenceState<'db, '_>,
        from: NodeRef,
        name: &str,
        on_lookup_error: OnLookupError,
    ) -> LookupResult {
        self.lookup_helper(i_s, Some(from), name, on_lookup_error)
    }

    #[inline]
    fn lookup_helper<'db>(
        &self,
        i_s: &InferenceState<'db, '_>,
        from: Option<NodeRef>,
        name: &str,
        on_lookup_error: OnLookupError,
    ) -> LookupResult {
        let mut result: Option<LookupResult> = None;
        self.run_after_lookup_on_each_union_member(
            i_s,
            None,
            from,
            name,
            &mut |t, lookup_result| {
                if matches!(lookup_result, LookupResult::None) {
                    on_lookup_error(t);
                }
                result = Some(if let Some(l) = result.take() {
                    LookupResult::UnknownName(
                        l.into_inferred().union(i_s, lookup_result.into_inferred()),
                    )
                } else {
                    lookup_result
                })
            },
        );
        result.unwrap_or_else(|| todo!())
    }

    pub fn lookup_symbol(&self, i_s: &InferenceState, name: &str) -> LookupResult {
        match self.as_ref() {
            DbType::Class(c, generics) => todo!(),
            DbType::Tuple(t) => LookupResult::None, // TODO this probably omits index/count
            DbType::NamedTuple(nt) => NamedTupleValue::new(i_s.db, nt).lookup(i_s, None, name),
            DbType::Callable(t) => todo!(),
            _ => todo!("{name:?} {self:?}"),
        }
    }

    pub fn get_item(
        &self,
        i_s: &InferenceState,
        from_inferred: Option<&Inferred>,
        slice_type: &SliceType,
        result_context: &mut ResultContext,
    ) -> Inferred {
        match self.as_ref() {
            DbType::Class(link, generics) => {
                let cls = Class::from_db_type(i_s.db, *link, generics);
                Instance::new(cls, from_inferred).get_item(i_s, slice_type, result_context)
            }
            DbType::Any => Inferred::new_any(),
            DbType::Tuple(tup) => Tuple::new(tup).get_item(i_s, slice_type, result_context),
            DbType::NamedTuple(nt) => {
                NamedTupleValue::new(i_s.db, nt).get_item(i_s, slice_type, result_context)
            }
            DbType::Union(union) => Inferred::gather_union(i_s, |callable| {
                for t in union.iter() {
                    callable(Type::new(t).get_item(i_s, None, slice_type, result_context))
                }
            }),
            t @ DbType::TypeVar(tv) => {
                if let Some(db_type) = &tv.type_var.bound {
                    Type::new(db_type).get_item(i_s, None, slice_type, result_context)
                } else {
                    todo!()
                }
            }
            DbType::Type(t) => match t.as_ref() {
                DbType::Class(link, generics) => Class::from_db_type(i_s.db, *link, generics)
                    .get_item(i_s, slice_type, result_context),
                _ => {
                    slice_type
                        .as_node_ref()
                        .add_typing_issue(i_s, IssueType::OnlyClassTypeApplication);
                    Inferred::new_any()
                }
            },
            DbType::NewType(new_type) => {
                Type::new(new_type.type_(i_s)).get_item(i_s, None, slice_type, result_context)
            }
            DbType::RecursiveAlias(r) => Type::new(r.calculated_db_type(i_s.db)).get_item(
                i_s,
                None,
                slice_type,
                result_context,
            ),
            DbType::Callable(_) => {
                slice_type
                    .as_node_ref()
                    .add_typing_issue(i_s, IssueType::OnlyClassTypeApplication);
                Inferred::new_unknown()
            }
            DbType::FunctionOverload(_) => {
                slice_type
                    .as_node_ref()
                    .add_typing_issue(i_s, IssueType::OnlyClassTypeApplication);
                todo!("Please write a test that checks this");
            }
            DbType::None => {
                debug!("TODO None[...]");
                Inferred::new_any()
            }
            _ => todo!("get_item not implemented for {self:?}"),
        }
    }

    pub fn merge_matching_parts(self, db: &Database, other: Self) -> Self {
        // TODO performance there's a lot of into_db_type here, that should not really be
        /*
        if self.as_ref() == other.as_ref() {
            return self;
        }
        */
        // necessary.
        let merge_generics = |g1: ClassGenerics, g2: ClassGenerics| {
            if matches!(g1, ClassGenerics::None) {
                return ClassGenerics::None;
            }
            ClassGenerics::List(GenericsList::new_generics(
                // Performance issue: clone could probably be removed. Rc -> Vec check
                // https://github.com/rust-lang/rust/issues/93610#issuecomment-1528108612
                Generics::from_class_generics(db, &g1)
                    .iter(db)
                    .zip(Generics::from_class_generics(db, &g2).iter(db))
                    .map(|(gi1, gi2)| gi1.merge_matching_parts(db, gi2))
                    .collect(),
            ))
        };
        use TupleTypeArguments::*;
        Type::owned(match self.into_db_type() {
            DbType::Class(link1, g1) => match other.into_db_type() {
                DbType::Class(link2, g2) if link1 == link2 => {
                    DbType::Class(link1, merge_generics(g1, g2))
                }
                _ => DbType::Any,
            },
            DbType::Union(u1) => match other.into_db_type() {
                DbType::Union(u2) if u1.iter().all(|x| u2.iter().any(|y| x == y)) => {
                    DbType::Union(u1)
                }
                _ => DbType::Any,
            },
            DbType::Tuple(c1) => match other.into_db_type() {
                DbType::Tuple(c2) => {
                    let c1 = rc_unwrap_or_clone(c1);
                    let c2 = rc_unwrap_or_clone(c2);
                    DbType::Tuple(match (c1.args, c2.args) {
                        (Some(FixedLength(ts1)), Some(FixedLength(ts2)))
                            if ts1.len() == ts2.len() =>
                        {
                            Rc::new(TupleContent::new_fixed_length(
                                ts1.into_vec()
                                    .into_iter()
                                    .zip(ts2.into_vec().into_iter())
                                    .map(|(t1, t2)| match (t1, t2) {
                                        (
                                            TypeOrTypeVarTuple::Type(t1),
                                            TypeOrTypeVarTuple::Type(t2),
                                        ) => TypeOrTypeVarTuple::Type(
                                            Type::owned(t1)
                                                .merge_matching_parts(db, Type::owned(t2))
                                                .into_db_type(),
                                        ),
                                        (t1, t2) => match t1 == t2 {
                                            true => t1,
                                            false => todo!(),
                                        },
                                    })
                                    .collect(),
                            ))
                        }
                        (Some(ArbitraryLength(t1)), Some(ArbitraryLength(t2))) => {
                            Rc::new(TupleContent::new_arbitrary_length(
                                Type::owned(*t1)
                                    .merge_matching_parts(db, t2.as_ref().into())
                                    .into_db_type(),
                            ))
                        }
                        _ => TupleContent::new_empty(),
                    })
                }
                _ => DbType::Any,
            },
            DbType::Callable(content1) => match other.into_db_type() {
                DbType::Callable(content2) => DbType::Callable(Rc::new(CallableContent {
                    name: content1.name.or(content2.name),
                    class_name: content1.class_name.or(content2.class_name),
                    defined_at: content1.defined_at,
                    type_vars: None,
                    params: CallableParams::Any,
                    result_type: DbType::Any,
                })),
                _ => DbType::Any,
            },
            _ => DbType::Any,
        })
    }

    pub fn format(&self, format_data: &FormatData) -> Box<str> {
        self.as_ref().format(format_data)
    }

    pub fn format_short(&self, db: &Database) -> Box<str> {
        self.format(&FormatData::new_short(db))
    }
}

pub fn match_tuple_type_arguments(
    i_s: &InferenceState,
    matcher: &mut Matcher,
    t1: &TupleTypeArguments,
    t2: &TupleTypeArguments,
    variance: Variance,
) -> Match {
    if matcher.is_matching_reverse() {
        return matcher.match_reverse(|matcher| {
            match_tuple_type_arguments(i_s, matcher, t2, t1, variance.invert())
        });
    }
    use TupleTypeArguments::*;
    if matcher.might_have_defined_type_vars() {
        if let Some(ts) = t1.has_type_var_tuple() {
            return matcher.match_type_var_tuple(i_s, ts, t2, variance);
        }
    }
    match (t1, t2, variance) {
        (tup1_args @ FixedLength(ts1), tup2_args @ FixedLength(ts2), _) => {
            if ts1.len() == ts2.len() {
                let mut matches = Match::new_true();
                for (t1, t2) in ts1.iter().zip(ts2.iter()) {
                    match (t1, t2) {
                        (TypeOrTypeVarTuple::Type(t1), TypeOrTypeVarTuple::Type(t2)) => {
                            matches &=
                                Type::new(t1).matches(i_s, matcher, &Type::new(t2), variance);
                        }
                        (
                            TypeOrTypeVarTuple::TypeVarTuple(t1),
                            TypeOrTypeVarTuple::TypeVarTuple(t2),
                        ) => matches &= (t1 == t2).into(),
                        _ => todo!("{t1:?} {t2:?}"),
                    }
                }
                matches
            } else {
                Match::new_false()
            }
        }
        (ArbitraryLength(t1), ArbitraryLength(t2), _) => {
            Type::new(t1).matches(i_s, matcher, &Type::new(t2), variance)
        }
        (tup1_args @ FixedLength(ts1), tup2_args @ ArbitraryLength(t2), _) => Match::new_false(),
        (ArbitraryLength(t1), FixedLength(ts2), Variance::Invariant) => {
            todo!()
        }
        (ArbitraryLength(t1), FixedLength(ts2), Variance::Covariant) => {
            let t1 = Type::new(t1);
            ts2.iter()
                .all(|g2| match g2 {
                    TypeOrTypeVarTuple::Type(t2) => {
                        t1.is_super_type_of(i_s, matcher, &Type::new(t2)).bool()
                    }
                    TypeOrTypeVarTuple::TypeVarTuple(_) => {
                        todo!()
                    }
                })
                .into()
        }
        (_, _, _) => unreachable!(),
    }
}

pub fn common_base_type<'x, I: Iterator<Item = &'x TypeOrTypeVarTuple>>(
    i_s: &InferenceState,
    mut ts: I,
) -> DbType {
    if let Some(first) = ts.next() {
        let mut result = match first {
            TypeOrTypeVarTuple::Type(t) => Cow::Borrowed(t),
            TypeOrTypeVarTuple::TypeVarTuple(_) => return i_s.db.python_state.object_db_type(),
        };
        for t in ts {
            let t = match t {
                TypeOrTypeVarTuple::Type(t) => t,
                TypeOrTypeVarTuple::TypeVarTuple(_) => return i_s.db.python_state.object_db_type(),
            };
            result = Cow::Owned(Type(result).common_base_type(i_s, &Type::new(t)));
        }
        result.into_owned()
    } else {
        DbType::Never
    }
}

pub fn execute_type_of_type<'db>(
    i_s: &InferenceState<'db, '_>,
    args: &dyn Arguments<'db>,
    result_context: &mut ResultContext,
    on_type_error: OnTypeError<'db, '_>,
    type_: &DbType,
) -> Inferred {
    match type_ {
        #![allow(unreachable_code)]
        // TODO remove this
        tuple @ DbType::Tuple(tuple_content) => {
            debug!("TODO this does not check the arguments");
            return Inferred::from_type(tuple.clone());
            // TODO reenable this
            let mut args_iterator = args.iter();
            let (arg, inferred_tup) = if let Some(arg) = args_iterator.next() {
                let inf = arg.infer(i_s, &mut ResultContext::Known(&Type::new(tuple)));
                (arg, inf)
            } else {
                debug!("TODO this does not check the arguments");
                return Inferred::from_type(tuple.clone());
            };
            if args_iterator.next().is_some() {
                todo!()
            }
            Type::new(tuple).error_if_not_matches(
                i_s,
                &inferred_tup,
                |i_s: &InferenceState<'db, '_>, t1, t2| {
                    (on_type_error.callback)(i_s, None, &|_| todo!(), &arg, t1, t2);
                    args.as_node_ref().to_db_lifetime(i_s.db)
                },
            );
            Inferred::from_type(tuple.clone())
        }
        DbType::Class(link, generics_list) => Class::from_db_type(i_s.db, *link, generics_list)
            .execute(i_s, args, result_context, on_type_error),
        DbType::TypeVar(t) => {
            if let Some(bound) = &t.type_var.bound {
                execute_type_of_type(i_s, args, result_context, on_type_error, bound);
                Inferred::from_type(type_.clone())
            } else {
                todo!("{t:?}")
            }
        }
        DbType::NewType(n) => {
            let mut iterator = args.iter();
            if let Some(first) = iterator.next() {
                if iterator.next().is_some() {
                    todo!()
                }
                // TODO match
                Inferred::from_type(type_.clone())
            } else {
                todo!()
            }
        }
        DbType::Self_ => {
            i_s.current_class()
                .unwrap()
                .execute(i_s, args, result_context, on_type_error);
            Inferred::from_type(DbType::Self_)
        }
        DbType::Any => Inferred::new_any(),
        DbType::NamedTuple(nt) => {
            let calculated_type_vars = calculate_callable_type_vars_and_return(
                i_s,
                None,
                &nt.constructor,
                args.iter(),
                &|| args.as_node_ref(),
                &mut ResultContext::Unknown,
                on_type_error,
            );
            debug!("TODO use generics for namedtuple");
            Inferred::from_type(DbType::NamedTuple(nt.clone()))
        }
        _ => todo!("{:?}", type_),
    }
}

impl TypeAlias {
    pub fn as_db_type_and_set_type_vars_any(&self, db: &Database) -> DbType {
        if self.is_recursive() {
            return DbType::RecursiveAlias(Rc::new(RecursiveAlias::new(
                self.location,
                self.type_vars.as_ref().map(|type_vars| {
                    GenericsList::new_generics(
                        type_vars
                            .iter()
                            .map(|tv| tv.as_any_generic_item())
                            .collect(),
                    )
                }),
            )));
        }
        let db_type = self.db_type_if_valid();
        if self.type_vars.is_none() {
            db_type.clone()
        } else {
            Type::new(db_type).replace_type_var_likes(db, &mut |t| match t.in_definition()
                == self.location
            {
                true => t.as_type_var_like().as_any_generic_item(),
                false => t.into_generic_item(),
            })
        }
    }

    pub fn replace_type_var_likes(
        &self,
        db: &Database,
        remove_recursive_wrapper: bool,
        callable: &mut impl FnMut(TypeVarLikeUsage) -> GenericItem,
    ) -> Cow<DbType> {
        if self.is_recursive() && !remove_recursive_wrapper {
            return Cow::Owned(DbType::RecursiveAlias(Rc::new(RecursiveAlias::new(
                self.location,
                self.type_vars.as_ref().map(|type_vars| {
                    GenericsList::new_generics(
                        type_vars
                            .iter()
                            .enumerate()
                            .map(|(i, type_var_like)| match type_var_like {
                                TypeVarLike::TypeVar(type_var) => {
                                    callable(TypeVarLikeUsage::TypeVar(Cow::Owned(TypeVarUsage {
                                        type_var: type_var.clone(),
                                        index: i.into(),
                                        in_definition: self.location,
                                    })))
                                }
                                TypeVarLike::TypeVarTuple(t) => callable(
                                    TypeVarLikeUsage::TypeVarTuple(Cow::Owned(TypeVarTupleUsage {
                                        type_var_tuple: t.clone(),
                                        index: i.into(),
                                        in_definition: self.location,
                                    })),
                                ),
                                TypeVarLike::ParamSpec(p) => callable(TypeVarLikeUsage::ParamSpec(
                                    Cow::Owned(ParamSpecUsage {
                                        param_spec: p.clone(),
                                        index: i.into(),
                                        in_definition: self.location,
                                    }),
                                )),
                            })
                            .collect(),
                    )
                }),
            ))));
        }
        let db_type = self.db_type_if_valid();
        if self.type_vars.is_none() {
            Cow::Borrowed(db_type)
        } else {
            Cow::Owned(Type::new(db_type).replace_type_var_likes(db, callable))
        }
    }
}

impl GenericItem {
    pub fn replace_type_var_likes(
        &self,
        db: &Database,
        callable: &mut impl FnMut(TypeVarLikeUsage) -> GenericItem,
        replace_self: ReplaceSelf,
    ) -> Self {
        match self {
            Self::TypeArgument(t) => Self::TypeArgument(
                Type::new(t).replace_type_var_likes_and_self(db, callable, replace_self),
            ),
            Self::TypeArguments(_) => todo!(),
            Self::ParamSpecArgument(_) => todo!(),
        }
    }
}

impl RecursiveAlias {
    pub fn calculated_db_type<'db: 'slf, 'slf>(&'slf self, db: &'db Database) -> &'slf DbType {
        let alias = self.type_alias(db);
        if self.generics.is_none() {
            alias.db_type_if_valid()
        } else {
            self.calculated_db_type.get_or_init(|| {
                self.type_alias(db)
                    .replace_type_var_likes(db, true, &mut |t| {
                        self.generics
                            .as_ref()
                            .map(|g| g.nth(t.index()).unwrap().clone())
                            .unwrap()
                    })
                    .into_owned()
            })
        }
    }
}
