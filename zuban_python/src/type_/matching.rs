use crate::{
    database::{ComplexPoint, MetaclassState},
    debug,
    inference_state::InferenceState,
    matching::{matches_params, params::has_overlapping_params, Match, Matcher, MismatchReason},
    node_ref::NodeRef,
    type_::{CallableLike, CallableParams, TupleTypeArguments, TypeOrTypeVarTuple, Variance},
    type_helpers::{Class, TypeOrClass},
};

use super::{CallableContent, ClassGenerics, TupleContent, Type, TypeVarKind, UnionType};

impl Type {
    pub fn overlaps(&self, i_s: &InferenceState, other: &Self) -> bool {
        match other {
            Type::TypeVar(t2_usage) => {
                return match &t2_usage.type_var.kind {
                    TypeVarKind::Unrestricted => true,
                    TypeVarKind::Bound(bound) => self.overlaps(i_s, bound),
                    TypeVarKind::Constraints(constraints) => {
                        constraints.iter().all(|r2| self.overlaps(i_s, r2))
                    }
                }
            }
            Type::Union(union_type2) => return union_type2.iter().any(|t| self.overlaps(i_s, t)),
            Type::Any => return false, // This is a fallback
            _ => (),
        }

        match self {
            Type::Class(c) => match other {
                Type::Class(c) => Self::overlaps_class(i_s, c.class(i_s.db), c.class(i_s.db)),
                _ => false,
            },
            Type::Type(t1) => match other {
                Type::Type(t2) => t1.overlaps(i_s, t2),
                _ => false,
            },
            Type::Callable(c1) => match other {
                Type::Callable(c2) => {
                    c1.result_type.overlaps(i_s, &c2.result_type)
                        && has_overlapping_params(i_s, &c1.params, &c2.params)
                }
                Type::Type(t2) => self.overlaps(i_s, &t2),
                _ => false,
            },
            Type::Any => true,
            Type::Never => todo!(),
            Type::Literal(literal1) => match other {
                Type::Literal(literal2) => literal1.value(i_s.db) == literal2.value(i_s.db),
                _ => i_s
                    .db
                    .python_state
                    .literal_type(&literal1.kind)
                    .overlaps(i_s, other),
            },
            Type::None => matches!(other, Type::None),
            Type::TypeVar(t1) => match &t1.type_var.kind {
                TypeVarKind::Unrestricted => true,
                TypeVarKind::Bound(bound) => bound.overlaps(i_s, other),
                TypeVarKind::Constraints(constraints) => todo!("{other:?}"),
            },
            Type::Tuple(t1) => match other {
                Type::Tuple(t2) => Self::overlaps_tuple(i_s, t1, t2),
                _ => false,
            },
            Type::Union(union) => union.iter().any(|t| t.overlaps(i_s, other)),
            Type::FunctionOverload(intersection) => todo!(),
            Type::NewType(_) => todo!(),
            Type::RecursiveAlias(_) => todo!(),
            Type::Self_ => false, // TODO this is wrong
            Type::ParamSpecArgs(usage) => todo!(),
            Type::ParamSpecKwargs(usage) => todo!(),
            Type::Module(file_index) => todo!(),
            Type::Namespace(file_index) => todo!(),
            Type::Dataclass(_) => todo!(),
            Type::TypedDict(_) => todo!(),
            Type::NamedTuple(_) => todo!(),
            Type::Enum(_) => todo!(),
            Type::EnumMember(_) => todo!(),
            Type::Super { .. } => todo!(),
            Type::CustomBehavior(_) => false,
        }
    }

    fn matches_internal(
        &self,
        i_s: &InferenceState,
        matcher: &mut Matcher,
        value_type: &Self,
        variance: Variance,
    ) -> Match {
        match self {
            Type::Class(c) => Self::matches_class_against_type(
                i_s,
                matcher,
                &c.class(i_s.db),
                value_type,
                variance,
            ),
            Type::Type(t1) => match value_type {
                Type::Type(t2) => t1.matches(i_s, matcher, t2, variance).similar_if_false(),
                _ => Match::new_false(),
            },
            Type::TypeVar(t1) => {
                if matcher.is_matching_reverse() {
                    Match::new_false()
                } else {
                    matcher.match_or_add_type_var(i_s, t1, value_type, variance)
                }
            }
            Type::Callable(c1) => {
                Self::matches_callable_against_arbitrary(i_s, matcher, c1, value_type, variance)
            }
            Type::None => matches!(value_type, Type::None).into(),
            Type::Any if matcher.is_matching_reverse() => {
                debug!("TODO write a test for this. (reverse matching any)");
                matcher.set_all_contained_type_vars_to_any(i_s, self);
                Match::True { with_any: true }
            }
            Type::Any => Match::new_true(),
            Type::Never => matches!(value_type, Type::Never).into(),
            Type::Tuple(t1) => match value_type {
                Type::Tuple(t2) => {
                    Self::matches_tuple(i_s, matcher, t1, t2, variance).similar_if_false()
                }
                Type::NamedTuple(t2) => {
                    Self::matches_tuple(i_s, matcher, t1, &t2.as_tuple(), variance)
                        .similar_if_false()
                }
                _ => Match::new_false(),
            },
            Type::Union(union_type1) => {
                self.matches_union(i_s, matcher, union_type1, value_type, variance)
            }
            Type::FunctionOverload(overload) if variance == Variance::Invariant => self
                .matches_internal(i_s, matcher, value_type, Variance::Covariant)
                .or(|| value_type.matches_internal(i_s, matcher, self, Variance::Covariant)),
            Type::FunctionOverload(overload) => Match::all(overload.iter_functions(), |c1| {
                Self::matches_callable_against_arbitrary(i_s, matcher, c1, value_type, variance)
            }),
            Type::Literal(literal1) => match value_type {
                Type::Literal(literal2) => {
                    (literal1.value(i_s.db) == literal2.value(i_s.db)).into()
                }
                _ => Match::new_false(),
            },
            Type::NewType(new_type1) => match value_type {
                Type::NewType(new_type2) => (new_type1 == new_type2).into(),
                _ => Match::new_false(),
            },
            t1 @ Type::RecursiveAlias(rec1) => {
                match value_type {
                    t2 @ Type::Class(_) => {
                        // Classes like aliases can also be recursive in mypy, like `class B(List[B])`.
                        matcher.avoid_recursion(t1, t2, |matcher| {
                            let g = rec1.calculated_type(i_s.db);
                            g.matches_internal(i_s, matcher, value_type, variance)
                        })
                    }
                    t @ Type::RecursiveAlias(rec2) => matcher.avoid_recursion(t1, t, |matcher| {
                        let t1 = rec1.calculated_type(i_s.db);
                        let t2 = rec2.calculated_type(i_s.db);
                        t1.matches_internal(i_s, matcher, &t2, variance)
                    }),
                    _ => {
                        let g = rec1.calculated_type(i_s.db);
                        g.matches_internal(i_s, matcher, value_type, variance)
                    }
                }
            }
            Type::Self_ => match value_type {
                Type::Self_ => Match::new_true(),
                _ => Match::new_false(),
            },
            Type::ParamSpecArgs(usage1) => match value_type {
                Type::ParamSpecArgs(usage2) => (usage1 == usage2).into(),
                _ => Match::new_false(),
            },
            Type::ParamSpecKwargs(usage1) => match value_type {
                Type::ParamSpecKwargs(usage2) => (usage1 == usage2).into(),
                _ => Match::new_false(),
            },
            Type::Dataclass(d1) => match value_type {
                Type::Dataclass(d2) => {
                    let c1 = d1.class(i_s.db);
                    let c2 = d2.class(i_s.db);
                    Self::matches_class(i_s, matcher, &c1, &c2, Variance::Covariant)
                }
                _ => Match::new_false(),
            },
            Type::TypedDict(d1) => match value_type {
                Type::TypedDict(d2) => {
                    let mut matches = Match::new_true();
                    for m1 in d1.members.iter() {
                        if let Some(m2) = d2.find_member(i_s.db, m1.name.as_str(i_s.db)) {
                            if m1.required != m2.required {
                                return Match::new_false().similar_if_false();
                            }
                            matches &= m1.type_.is_same_type(i_s, matcher, &m2.type_);
                        } else {
                            return Match::new_false().similar_if_false();
                        }
                    }
                    matches.similar_if_false()
                }
                _ => Match::new_false(),
            },
            Type::NamedTuple(nt1) => match value_type {
                Type::NamedTuple(nt2) => {
                    let c1 = &nt1.__new__;
                    let c2 = &nt2.__new__;
                    if !c1.type_vars.is_empty() || !c2.type_vars.is_empty() {
                        todo!()
                    } else {
                        (c1.defined_at == c2.defined_at).into()
                    }
                }
                _ => Match::new_false(),
            },
            Type::Enum(e1) => match value_type {
                Type::Enum(e2) => (e1 == e2).into(),
                Type::EnumMember(member) => (e1 == &member.enum_).into(),
                _ => Match::new_false(),
            },
            Type::EnumMember(m1) => match value_type {
                Type::EnumMember(m2) => (m1.is_same_member(m2)).into(),
                _ => Match::new_false(),
            },
            Type::Module(file_index) => Match::new_false(),
            Type::Namespace(file_index) => todo!(),
            Type::Super { .. } => todo!(),
            Type::CustomBehavior(_) => Match::new_false(),
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
        let mut m = Match::new_false();
        for (_, t2) in value_type.mro(i_s.db) {
            m = match t2 {
                TypeOrClass::Class(c2) => match self.maybe_class(i_s.db) {
                    Some(c1) => Self::matches_class(i_s, matcher, &c1, &c2, Variance::Covariant),
                    None => {
                        // TODO performance: This might be slow, because it always
                        // allocates when e.g.  Foo is passed to def x(f: Foo | None): ...
                        // This is a bit unfortunate, especially because it loops over the
                        // mro and allocates every time.
                        let t2 = c2.as_db_type(i_s.db);
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
        let result = m
            .or(|| {
                self.check_protocol_and_other_side(i_s, matcher, value_type, Variance::Covariant)
            })
            .or(|| {
                if let Some(class2) = value_type.maybe_class(i_s.db) {
                    if class2.incomplete_mro(i_s.db) && self.maybe_class(i_s.db).is_some() {
                        debug!(
                            "Match of class, because base class is incomplete: {}",
                            class2.format_short(i_s.db)
                        );
                        return Match::new_true();
                    }
                    if !matcher.ignore_promotions() {
                        return self.check_promotion(i_s, matcher, class2.node_ref);
                    }
                } else if let Type::Literal(literal) = value_type {
                    if !matcher.ignore_promotions() {
                        return self.check_promotion(
                            i_s,
                            matcher,
                            i_s.db
                                .python_state
                                .literal_instance(&literal.kind)
                                .class
                                .node_ref,
                        );
                    }
                }
                Match::new_false()
            });
        debug!(
            "Match covariant {} :> {} -> {:?}",
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
                &Type::new_class(cls_node_ref.as_link(), ClassGenerics::None),
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
            "Match invariant {} ≡ {} -> {:?}",
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
        let mut m = Match::new_false();
        // 2. Check if it is a class with a protocol
        if let Some(class1) = self.maybe_class(i_s.db) {
            // TODO this should probably be checked before normal mro checking?!
            if class1.is_protocol(i_s.db) {
                m = matcher.avoid_recursion(self, value_type, |matcher| {
                    class1.check_protocol_match(i_s, matcher, value_type, variance)
                });
                if m.bool() {
                    return m;
                }
            }
        }
        // 3. Check if the value_type is special like Any or a Typevar and needs to be checked
        //    again.
        match value_type {
            Type::Any if matcher.is_matching_reverse() => return Match::new_true(),
            Type::Any => {
                matcher.set_all_contained_type_vars_to_any(i_s, self);
                return Match::True { with_any: true };
            }
            Type::None if !i_s.db.python_state.project.strict_optional => return Match::new_true(),
            Type::TypeVar(t2) => {
                if matcher.is_matching_reverse() {
                    return matcher.match_or_add_type_var(i_s, t2, self, variance.invert());
                }
                if variance == Variance::Covariant {
                    match &t2.type_var.kind {
                        TypeVarKind::Unrestricted => (),
                        TypeVarKind::Bound(bound) => {
                            let m = self.simple_matches(i_s, bound, variance);
                            if m.bool() {
                                return m;
                            }
                        }
                        TypeVarKind::Constraints(constraints) => {
                            let m = constraints
                                .iter()
                                .all(|r| self.simple_matches(i_s, r, variance).bool());
                            if m {
                                return Match::new_true();
                            }
                        }
                    }
                }
            }
            // Necessary to e.g. match int to Literal[1, 2]
            Type::Union(u2)
                if variance == Variance::Covariant
                // Union matching was already done.
                && !self.is_union_like() =>
            {
                if matcher.is_matching_reverse() {
                    debug!("TODO matching reverse?");
                }
                let mut result: Option<Match> = None;
                for t in u2.iter() {
                    let r = self.matches(i_s, matcher, t, variance);
                    if !r.bool() {
                        return r.bool().into();
                    } else if let Some(old) = result {
                        result = Some(old & r)
                    } else {
                        result = Some(r)
                    }
                }
                return result.unwrap();
            }
            Type::NewType(n2) => {
                let t = n2.type_(i_s);
                return self.matches(i_s, matcher, t, variance);
            }
            Type::Never if variance == Variance::Covariant => return Match::new_true(), // Never is assignable to anything
            Type::Self_ if variance == Variance::Covariant => {
                if let Some(cls) = i_s.current_class() {
                    return self.simple_matches(i_s, &cls.as_db_type(i_s.db), variance);
                }
            }
            Type::Module(_) => {
                m = m.or(|| {
                    self.matches(
                        i_s,
                        matcher,
                        &i_s.db.python_state.module_db_type().into(),
                        variance,
                    )
                })
            }
            _ => (),
        }
        m
    }

    fn matches_union(
        &self,
        i_s: &InferenceState,
        matcher: &mut Matcher,
        u1: &UnionType,
        value_type: &Self,
        variance: Variance,
    ) -> Match {
        match value_type {
            Type::TypeVar(type_var2) if matcher.is_matching_reverse() => {
                matcher.match_or_add_type_var(i_s, type_var2, self, variance.invert())
            }
            Type::Union(u2) => match variance {
                Variance::Covariant => {
                    let mut matches = Match::new_true();
                    for g2 in u2.iter() {
                        matches &=
                            Match::any(u1.iter(), |g1| g1.matches(i_s, matcher, &g2, variance))
                    }
                    matches
                }
                Variance::Invariant => {
                    self.is_super_type_of(i_s, matcher, value_type)
                        & self.is_sub_type_of(i_s, matcher, value_type)
                }
                Variance::Contravariant => unreachable!(),
            },
            _ => match variance {
                Variance::Covariant => {
                    Match::any(u1.iter(), |g| g.matches(i_s, matcher, value_type, variance))
                }
                Variance::Invariant => {
                    Match::all(u1.iter(), |g| g.matches(i_s, matcher, value_type, variance))
                }
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
        let type_vars = class1.type_vars(i_s);
        if !type_vars.is_empty() {
            let result = class1
                .generics()
                .matches(i_s, matcher, class2.generics(), type_vars, variance)
                .similar_if_false();
            if !result.bool() {
                let mut check = |i_s: &InferenceState, n| {
                    let t1 = class1.nth_type_argument(i_s.db, n);
                    if matches!(t1, Type::Any) {
                        return false;
                    }
                    let t2 = class2.nth_type_argument(i_s.db, n);
                    if matches!(t2, Type::Any) {
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
        Match::new_true()
    }

    fn matches_class_against_type(
        i_s: &InferenceState,
        matcher: &mut Matcher,
        class1: &Class,
        value_type: &Type,
        variance: Variance,
    ) -> Match {
        match value_type {
            Type::Class(c2) => {
                Self::matches_class(i_s, matcher, class1, &c2.class(i_s.db), variance)
            }
            Type::Type(t2) => {
                if let Type::Class(c2) = t2.as_ref() {
                    match c2.class(i_s.db).use_cached_class_infos(i_s.db).metaclass {
                        MetaclassState::Some(link) => {
                            return class1.as_db_type(i_s.db).matches(
                                i_s,
                                matcher,
                                &Type::new_class(link, ClassGenerics::None),
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
            Type::Literal(literal) if variance == Variance::Covariant => {
                Self::matches_class_against_type(
                    i_s,
                    matcher,
                    class1,
                    &i_s.db.python_state.literal_type(&literal.kind),
                    variance,
                )
            }
            _ => Match::new_false(),
        }
    }

    fn matches_callable_against_arbitrary(
        i_s: &InferenceState,
        matcher: &mut Matcher,
        c1: &CallableContent,
        value_type: &Self,
        variance: Variance,
    ) -> Match {
        debug_assert_ne!(variance, Variance::Contravariant);
        match value_type {
            Type::FunctionOverload(overload) if variance == Variance::Covariant => {
                if matcher.is_matching_reverse() {
                    todo!()
                }
                Match::any(overload.iter_functions(), |c2| {
                    Self::matches_callable(i_s, matcher, c1, c2)
                })
            }
            Type::Type(t2) if c1.params == CallableParams::Any => {
                c1.result_type.is_super_type_of(i_s, matcher, t2)
            }
            _ => match value_type.maybe_callable(i_s) {
                Some(CallableLike::Callable(c2)) => Self::matches_callable(i_s, matcher, c1, &c2),
                Some(CallableLike::Overload(overload)) if variance == Variance::Covariant => {
                    Self::matches_callable_against_arbitrary(
                        i_s,
                        matcher,
                        c1,
                        &Type::FunctionOverload(overload),
                        variance,
                    )
                }
                _ => Match::new_false(),
            },
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
            if !c2.type_vars.is_empty() {
                let mut matcher = Matcher::new_reverse_callable_matcher(c2);
                return Self::matches_callable(i_s, &mut matcher, c1, c2);
            }
        }
        c1.result_type
            .is_super_type_of(i_s, matcher, &c2.result_type)
            & matches_params(
                i_s,
                matcher,
                &c1.params,
                &c2.params,
                (!c2.type_vars.is_empty()).then(|| (&c2.type_vars, c2.defined_at)),
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
        match_tuple_type_arguments(i_s, matcher, &t1.args, &t2.args, variance)
    }

    fn overlaps_tuple(i_s: &InferenceState, t1: &TupleContent, t2: &TupleContent) -> bool {
        use TupleTypeArguments::*;
        match (&t1.args, &t2.args) {
            (FixedLength(ts1), FixedLength(ts2)) => {
                let mut value_generics = ts2.iter();
                let mut overlaps = true;
                for type1 in ts1.iter() {
                    /*
                    if matcher.might_have_defined_type_vars() {
                        match type1 {
                            Type::TypeVarLike(t) if t.is_type_var_tuple() => {
                                todo!()
                            }
                            _ => (),
                        }
                    }
                    */
                    if let Some(type2) = value_generics.next() {
                        match (type1, type2) {
                            (TypeOrTypeVarTuple::Type(t1), TypeOrTypeVarTuple::Type(t2)) => {
                                overlaps &= t1.overlaps(i_s, &t2);
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
            (ArbitraryLength(t1), ArbitraryLength(t2)) => t1.overlaps(i_s, t2),
            (ArbitraryLength(t1), FixedLength(ts2)) => ts2.iter().all(|t2| match t2 {
                TypeOrTypeVarTuple::Type(t2) => t1.overlaps(i_s, t2),
                TypeOrTypeVarTuple::TypeVarTuple(t2) => {
                    todo!()
                }
            }),
            (FixedLength(ts1), ArbitraryLength(t2)) => ts1.iter().all(|t1| match t1 {
                TypeOrTypeVarTuple::Type(t1) => t1.overlaps(i_s, &t2),
                TypeOrTypeVarTuple::TypeVarTuple(t1) => {
                    todo!()
                }
            }),
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
                            matches &= t1.matches(i_s, matcher, t2, variance);
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
        (ArbitraryLength(t1), ArbitraryLength(t2), _) => t1.matches(i_s, matcher, t2, variance),
        (tup1_args @ FixedLength(ts1), tup2_args @ ArbitraryLength(t2), _) => Match::new_false(),
        (ArbitraryLength(t1), FixedLength(ts2), Variance::Invariant) => {
            todo!()
        }
        (ArbitraryLength(t1), FixedLength(ts2), _) => ts2
            .iter()
            .all(|g2| match g2 {
                TypeOrTypeVarTuple::Type(t2) => t1.matches(i_s, matcher, t2, variance).bool(),
                TypeOrTypeVarTuple::TypeVarTuple(_) => {
                    todo!()
                }
            })
            .into(),
    }
}
