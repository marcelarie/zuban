use std::{
    cell::OnceCell,
    hash::{Hash, Hasher},
    rc::Rc,
};

use super::{
    utils::method_with_fallback, CallableContent, CallableParam, CallableParams, CustomBehavior,
    DbString, FormatStyle, GenericsList, LookupResult, NeverCause, ParamType, RecursiveType,
    StringSlice, Type, TypeVarLikeUsage, TypeVarLikes,
};
use crate::{
    arguments::{ArgKind, Args, InferredArg},
    database::{Database, PointLink},
    diagnostics::IssueKind,
    file::infer_string_index,
    format_data::{AvoidRecursionFor, FormatData},
    getitem::{SliceType, SliceTypeContent},
    inference_state::InferenceState,
    inferred::{AttributeKind, Inferred},
    matching::{ErrorStrs, LookupKind, Match, Matcher, MismatchReason, OnTypeError, ResultContext},
    type_helpers::{Class, Instance, InstanceLookupOptions, LookupDetails},
    utils::join_with_commas,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypedDictMember {
    pub name: StringSlice,
    pub type_: Type,
    pub required: bool,
    pub read_only: bool,
}

impl TypedDictMember {
    pub fn replace_type(&self, callable: impl FnOnce(&Type) -> Type) -> Self {
        Self {
            name: self.name,
            type_: callable(&self.type_),
            required: self.required,
            read_only: self.read_only,
        }
    }

    pub fn as_keyword_param(&self) -> CallableParam {
        CallableParam {
            type_: ParamType::KeywordOnly(self.type_.clone()),
            name: Some(DbString::StringSlice(self.name)),
            has_default: !self.required,
            might_have_type_vars: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum TypedDictGenerics {
    None,
    NotDefinedYet(TypeVarLikes),
    Generics(GenericsList),
}

#[derive(Debug, Clone, Eq)]
pub(crate) struct TypedDict {
    pub name: Option<StringSlice>,
    members: OnceCell<Box<[TypedDictMember]>>,
    pub defined_at: PointLink,
    pub generics: TypedDictGenerics,
    pub is_final: bool,
}

impl TypedDict {
    pub fn new(
        name: Option<StringSlice>,
        members: Box<[TypedDictMember]>,
        defined_at: PointLink,
        generics: TypedDictGenerics,
    ) -> Rc<Self> {
        Rc::new(Self {
            name,
            members: OnceCell::from(members),
            defined_at,
            generics,
            is_final: false,
        })
    }

    pub fn new_definition(
        name: StringSlice,
        members: Box<[TypedDictMember]>,
        defined_at: PointLink,
        type_var_likes: TypeVarLikes,
    ) -> Rc<Self> {
        let generics = if type_var_likes.is_empty() {
            TypedDictGenerics::None
        } else {
            TypedDictGenerics::NotDefinedYet(type_var_likes)
        };
        Rc::new(Self {
            name: Some(name),
            members: OnceCell::from(members),
            defined_at,
            generics,
            is_final: false,
        })
    }

    pub fn new_class_definition(
        name: StringSlice,
        defined_at: PointLink,
        type_var_likes: TypeVarLikes,
        is_final: bool,
    ) -> Rc<Self> {
        let generics = if type_var_likes.is_empty() {
            TypedDictGenerics::None
        } else {
            TypedDictGenerics::NotDefinedYet(type_var_likes)
        };
        Rc::new(Self {
            name: Some(name),
            members: OnceCell::new(),
            defined_at,
            generics,
            is_final,
        })
    }

    pub fn late_initialization_of_members(&self, members: Box<[TypedDictMember]>) {
        debug_assert!(!matches!(self.generics, TypedDictGenerics::Generics(_)));
        self.members.set(members).unwrap()
    }

    pub fn apply_generics(&self, db: &Database, generics: TypedDictGenerics) -> Rc<Self> {
        let mut members = OnceCell::new();
        if let TypedDictGenerics::Generics(generics) = &generics {
            if let Some(ms) = self.members.get() {
                members = OnceCell::from(Self::remap_members_with_generics(db, ms, generics))
            }
        }
        Rc::new(TypedDict {
            name: self.name,
            members,
            defined_at: self.defined_at,
            generics,
            is_final: self.is_final,
        })
    }

    fn remap_members_with_generics(
        db: &Database,
        original_members: &[TypedDictMember],
        generics: &GenericsList,
    ) -> Box<[TypedDictMember]> {
        original_members
            .iter()
            .map(|m| {
                m.replace_type(|_| {
                    m.type_
                        .replace_type_var_likes(db, &mut |usage| {
                            Some(generics[usage.index()].clone())
                        })
                        .unwrap_or_else(|| m.type_.clone())
                })
            })
            .collect()
    }

    pub fn has_calculated_members(&self, db: &Database) -> bool {
        let members = self.members.get().map(|m| m.as_ref());
        if members.is_none() && matches!(&self.generics, TypedDictGenerics::Generics(_)) {
            let class = Class::from_non_generic_link(db, self.defined_at);
            let original_typed_dict = class.maybe_typed_dict().unwrap();
            if original_typed_dict.has_calculated_members(db) {
                return true;
            }
        }
        members.is_some()
    }

    pub fn calculating(&self) -> bool {
        self.members.get().is_none()
    }

    pub fn members(&self, db: &Database) -> &[TypedDictMember] {
        self.members.get().unwrap_or_else(|| {
            let TypedDictGenerics::Generics(list) = &self.generics else {
                unreachable!()
            };
            let class = Class::from_non_generic_link(db, self.defined_at);
            let original_typed_dict = class.maybe_typed_dict().unwrap();
            // The members are not pre-calculated, because there existed recursions where the
            // members of the original class were not calculated at that point. Therefore do that
            // now.
            let new_members = Self::remap_members_with_generics(
                db,
                original_typed_dict.members.get().unwrap(),
                list,
            );
            let result = self.members.set(new_members);
            debug_assert_eq!(result, Ok(()));
            self.members.get().unwrap()
        })
    }

    pub fn iter_required_members(
        &self,
        db: &Database,
    ) -> impl Iterator<Item = &'_ TypedDictMember> {
        self.members(db).iter().filter(|member| member.required)
    }

    pub fn iter_optional_members(
        &self,
        db: &Database,
    ) -> impl Iterator<Item = &'_ TypedDictMember> {
        self.members(db).iter().filter(|member| !member.required)
    }

    pub fn find_member(&self, db: &Database, name: &str) -> Option<&TypedDictMember> {
        self.members(db).iter().find(|p| p.name.as_str(db) == name)
    }

    fn qualified_name(&self, db: &Database) -> Option<String> {
        let name = self.name?;
        let module = db.loaded_python_file(name.file_index).qualified_name(db);
        Some(format!("{module}.{}", name.as_str(db)))
    }

    pub fn union(&self, i_s: &InferenceState, other: &Self) -> Type {
        let mut members: Vec<_> = self.members(i_s.db).into();
        'outer: for m2 in other.members(i_s.db).iter() {
            for m1 in members.iter() {
                if m1.name.as_str(i_s.db) == m2.name.as_str(i_s.db) {
                    if m1.required != m2.required
                        || !m1.type_.is_simple_same_type(i_s, &m2.type_).bool()
                    {
                        return Type::Never(NeverCause::Other);
                    }
                    continue 'outer;
                }
            }
            members.push(m2.clone());
        }
        Type::TypedDict(Self::new(
            None,
            members.into_boxed_slice(),
            self.defined_at,
            TypedDictGenerics::None,
        ))
    }

    pub fn intersection(&self, i_s: &InferenceState, other: &Self) -> Rc<TypedDict> {
        let mut new_members = vec![];
        for m1 in self.members(i_s.db).iter() {
            for m2 in other.members(i_s.db).iter() {
                if m1.name.as_str(i_s.db) == m2.name.as_str(i_s.db)
                    && m1.required == m2.required
                    && m1.type_.is_simple_same_type(i_s, &m2.type_).bool()
                {
                    new_members.push(m1.clone());
                }
            }
        }
        Self::new(
            None,
            new_members.into_boxed_slice(),
            self.defined_at,
            TypedDictGenerics::None,
        )
    }

    pub fn name_or_fallback(&self, format_data: &FormatData) -> String {
        if let Some(name) = self.name {
            let name = name.as_str(format_data.db);
            match &self.generics {
                TypedDictGenerics::Generics(list) => {
                    // Mypy seems to format TypedDicts with generics always with a qualified name.
                    let name = self
                        .qualified_name(format_data.db)
                        .unwrap_or_else(|| name.into());
                    format!("{name}[{}]", list.format(format_data))
                }
                _ => name.into(),
            }
        } else {
            self.format_full(format_data, None)
        }
    }

    pub fn format(&self, format_data: &FormatData) -> String {
        match format_data.style {
            FormatStyle::MypyRevealType => {
                self.format_full(format_data, self.qualified_name(format_data.db).as_deref())
            }
            FormatStyle::Short if !format_data.should_format_qualified(self.defined_at) => {
                self.name_or_fallback(format_data)
            }
            _ => self
                .qualified_name(format_data.db)
                .unwrap_or_else(|| self.name_or_fallback(format_data)),
        }
    }

    pub fn format_full(&self, format_data: &FormatData, name: Option<&str>) -> String {
        match format_data.with_seen_recursive_type(AvoidRecursionFor::TypedDict(self.defined_at)) {
            Ok(format_data) => {
                let params = join_with_commas(self.members(format_data.db).iter().map(|p| {
                    format!(
                        "'{}'{}{}: {}",
                        p.name.as_str(format_data.db),
                        match p.required {
                            true => "",
                            false => "?",
                        },
                        match p.read_only {
                            true => "=",
                            false => "",
                        },
                        p.type_.format(&format_data)
                    )
                }));
                if let Some(name) = name {
                    format!("TypedDict('{name}', {{{params}}})")
                } else {
                    format!("TypedDict({{{params}}})")
                }
            }
            Err(()) => "...".to_string(),
        }
    }

    pub(crate) fn get_item(
        &self,
        i_s: &InferenceState,
        slice_type: &SliceType,
        add_issue: &dyn Fn(IssueKind),
    ) -> Inferred {
        match slice_type.unpack() {
            SliceTypeContent::Simple(simple) => infer_string_index(
                i_s,
                simple,
                |key| {
                    Some({
                        if let Some(member) = self.find_member(i_s.db, key) {
                            Inferred::from_type(member.type_.clone())
                        } else {
                            add_issue(IssueKind::TypedDictHasNoKeyForGet {
                                typed_dict: self.format(&FormatData::new_short(i_s.db)).into(),
                                key: key.into(),
                            });
                            Inferred::new_any_from_error()
                        }
                    })
                },
                || add_access_key_must_be_string_literal_issue(i_s.db, self, add_issue),
            ),
            _ => {
                add_access_key_must_be_string_literal_issue(i_s.db, self, add_issue);
                Inferred::new_any_from_error()
            }
        }
    }

    pub fn replace(
        &self,
        generics: TypedDictGenerics,
        mut callable: &mut impl FnMut(&Type) -> Type,
    ) -> Rc<Self> {
        Rc::new(TypedDict {
            name: self.name,
            members: if let Some(members) = self.members.get() {
                OnceCell::from(
                    members
                        .iter()
                        .map(|m| m.replace_type(&mut callable))
                        .collect::<Box<_>>(),
                )
            } else {
                OnceCell::new()
            },
            defined_at: self.defined_at,
            generics,
            is_final: self.is_final,
        })
    }

    pub fn matches(
        &self,
        i_s: &InferenceState,
        matcher: &mut Matcher,
        other: &Self,
        read_only: bool,
    ) -> Match {
        let mut matches = Match::new_true();
        for m1 in self.members(i_s.db).iter() {
            if let Some(m2) = other.find_member(i_s.db, m1.name.as_str(i_s.db)) {
                // Required must match except if the wanted type is also read-only (and therefore
                // may not be modified afterwards
                if m1.required != m2.required && !(m1.read_only && !m1.required) {
                    return Match::new_false();
                }
                if !m1.read_only && m2.read_only {
                    return Match::new_false();
                }
                if m1.read_only {
                    matches &= m1.type_.is_super_type_of(i_s, matcher, &m2.type_);
                } else {
                    // When matching mutable fields, the type must be the exact same, because
                    // modifications propagate from one to the other TypedDict.
                    matches &= m1.type_.is_same_type(i_s, matcher, &m2.type_);
                }
            } else if !read_only || m1.required {
                return Match::new_false();
            }
        }
        matches
    }

    pub fn search_type_vars<C: FnMut(TypeVarLikeUsage) + ?Sized>(&self, found_type_var: &mut C) {
        if let TypedDictGenerics::Generics(list) = &self.generics {
            list.search_type_vars(found_type_var)
        }
    }

    pub fn has_any_internal(
        &self,
        i_s: &InferenceState,
        already_checked: &mut Vec<Rc<RecursiveType>>,
    ) -> bool {
        self.members(i_s.db)
            .iter()
            .any(|m| m.type_.has_any_internal(i_s, already_checked))
    }
}

pub fn rc_typed_dict_as_callable(db: &Database, slf: Rc<TypedDict>) -> CallableContent {
    CallableContent::new_simple(
        slf.name.map(DbString::StringSlice),
        None,
        slf.defined_at,
        match &slf.generics {
            TypedDictGenerics::None | TypedDictGenerics::Generics(_) => {
                db.python_state.empty_type_var_likes.clone()
            }
            TypedDictGenerics::NotDefinedYet(type_vars) => type_vars.clone(),
        },
        CallableParams::Simple(
            slf.members
                .get()
                .unwrap()
                .iter()
                .map(|m| m.as_keyword_param())
                .collect(),
        ),
        Type::TypedDict(slf.clone()),
    )
}

impl PartialEq for TypedDict {
    fn eq(&self, other: &Self) -> bool {
        self.defined_at == other.defined_at && self.generics == other.generics
    }
}

impl Hash for TypedDict {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.defined_at.hash(state);
        self.generics.hash(state);
    }
}

fn add_access_key_must_be_string_literal_issue(
    db: &Database,
    td: &TypedDict,
    add_issue: impl FnOnce(IssueKind),
) {
    add_issue(IssueKind::TypedDictAccessKeyMustBeStringLiteral {
        keys: join_with_commas(
            td.members(db)
                .iter()
                .map(|member| format!("\"{}\"", member.name.as_str(db))),
        )
        .into(),
    })
}

pub(crate) fn typed_dict_setdefault<'db>(
    i_s: &InferenceState<'db, '_>,
    args: &dyn Args<'db>,
    result_context: &mut ResultContext,
    on_type_error: OnTypeError,
    bound: Option<&Type>,
) -> Inferred {
    let Type::TypedDict(td) = bound.unwrap() else {
        unreachable!();
    };
    typed_dict_method_with_fallback(
        i_s,
        args,
        result_context,
        on_type_error,
        td,
        "setdefault",
        typed_dict_setdefault_internal,
    )
}

fn typed_dict_setdefault_internal<'db>(
    i_s: &InferenceState<'db, '_>,
    td: &TypedDict,
    args: &dyn Args<'db>,
) -> Option<Inferred> {
    let mut iterator = args.iter(i_s.mode);
    let first_arg = iterator.next()?;
    let second_arg = iterator.next();
    if iterator.next().is_some() {
        return None;
    }
    let default = match &second_arg {
        Some(second) => match &second.kind {
            ArgKind::Positional(second) => second.infer(&mut ResultContext::Unknown),
            ArgKind::Keyword(second) if second.key == "detault" => {
                second.infer(&mut ResultContext::Unknown)
            }
            _ => return None,
        },
        None => Inferred::new_none(),
    };

    let inferred_name = first_arg
        .clone()
        .maybe_positional_arg(i_s, &mut ResultContext::Unknown)?;
    let maybe_had_literals = inferred_name.run_on_str_literals(i_s, |key| {
        Some(Inferred::from_type({
            if let Some(member) = td.find_member(i_s.db, key) {
                if !member
                    .type_
                    .is_simple_super_type_of(i_s, &default.as_cow_type(i_s))
                    .bool()
                {
                    second_arg.as_ref().unwrap().add_issue(
                        i_s,
                        IssueKind::TypedDictSetdefaultWrongDefaultType {
                            got: default.format_short(i_s),
                            expected: member.type_.format_short(i_s.db),
                        },
                    )
                }
                member.type_.clone()
            } else {
                first_arg.add_issue(
                    i_s,
                    IssueKind::TypedDictHasNoKey {
                        typed_dict: td.format(&FormatData::new_short(i_s.db)).into(),
                        key: key.into(),
                    },
                );
                Type::ERROR
            }
        }))
    });

    if let Some(maybe_had_literals) = maybe_had_literals {
        Some(maybe_had_literals.simplified_union(i_s, default))
    } else {
        first_arg.add_issue(i_s, IssueKind::TypedDictKeysMustBeStringLiteral);
        Some(Inferred::new_any_from_error())
    }
}

pub(crate) fn typed_dict_get<'db>(
    i_s: &InferenceState<'db, '_>,
    args: &dyn Args<'db>,
    result_context: &mut ResultContext,
    on_type_error: OnTypeError,
    bound: Option<&Type>,
) -> Inferred {
    let Type::TypedDict(td) = bound.unwrap() else {
        unreachable!();
    };
    typed_dict_method_with_fallback(
        i_s,
        args,
        result_context,
        on_type_error,
        td,
        "get",
        typed_dict_get_internal,
    )
}

fn typed_dict_get_internal<'db>(
    i_s: &InferenceState<'db, '_>,
    td: &TypedDict,
    args: &dyn Args<'db>,
) -> Option<Inferred> {
    typed_dict_get_or_pop_internal(i_s, td, args, false)
}

fn typed_dict_get_or_pop_internal<'db>(
    i_s: &InferenceState<'db, '_>,
    td: &TypedDict,
    args: &dyn Args<'db>,
    is_pop: bool,
) -> Option<Inferred> {
    let mut iterator = args.iter(i_s.mode);
    let first_arg = iterator.next()?;
    let second_arg = iterator.next();
    if iterator.next().is_some() {
        return None;
    }
    let infer_default = |context: &mut _| match &second_arg {
        Some(second) => match &second.kind {
            ArgKind::Positional(second) => Some(second.infer(context)),
            ArgKind::Keyword(second) if second.key == "default" => Some(second.infer(context)),
            _ => None,
        },
        None => Some(Inferred::new_none()),
    };

    let inferred_name = first_arg
        .clone()
        .maybe_positional_arg(i_s, &mut ResultContext::Unknown)?;
    let maybe_had_literals = inferred_name.run_on_str_literals(i_s, |key| {
        Some(Inferred::from_type({
            if let Some(member) = td.find_member(i_s.db, key) {
                if is_pop && (member.required || member.read_only) {
                    first_arg.add_issue(
                        i_s,
                        IssueKind::TypedDictKeyCannotBeDeleted {
                            typed_dict: td.format(&FormatData::new_short(i_s.db)).into(),
                            key: key.into(),
                        },
                    )
                }
                member.type_.clone()
            } else if is_pop {
                first_arg.add_issue(
                    i_s,
                    IssueKind::TypedDictHasNoKey {
                        typed_dict: td.format(&FormatData::new_short(i_s.db)).into(),
                        key: key.into(),
                    },
                );
                Type::ERROR
            } else {
                i_s.db.python_state.object_type()
            }
        }))
    });

    if let Some(maybe_had_literals) = maybe_had_literals {
        let default = infer_default(&mut ResultContext::new_known(
            &maybe_had_literals.as_cow_type(i_s),
        ))?;
        if is_pop && second_arg.is_none() {
            Some(maybe_had_literals)
        } else {
            Some(maybe_had_literals.simplified_union(i_s, default))
        }
    } else {
        if is_pop {
            first_arg.add_issue(i_s, IssueKind::TypedDictKeysMustBeStringLiteral);
        }
        infer_default(&mut ResultContext::Unknown)?;
        Some(Inferred::from_type(i_s.db.python_state.object_type()))
    }
}

fn typed_dict_pop_internal<'db>(
    i_s: &InferenceState<'db, '_>,
    td: &TypedDict,
    args: &dyn Args<'db>,
) -> Option<Inferred> {
    typed_dict_get_or_pop_internal(i_s, td, args, true)
}

pub(crate) fn typed_dict_pop<'db>(
    i_s: &InferenceState<'db, '_>,
    args: &dyn Args<'db>,
    result_context: &mut ResultContext,
    on_type_error: OnTypeError,
    bound: Option<&Type>,
) -> Inferred {
    let Type::TypedDict(td) = bound.unwrap() else {
        unreachable!();
    };
    typed_dict_method_with_fallback(
        i_s,
        args,
        result_context,
        on_type_error,
        td,
        "pop",
        typed_dict_pop_internal,
    )
}

fn typed_dict_method_with_fallback<'db>(
    i_s: &InferenceState<'db, '_>,
    args: &dyn Args<'db>,
    result_context: &mut ResultContext,
    on_type_error: OnTypeError,
    td: &TypedDict,
    name: &str,
    handler: fn(
        i_s: &InferenceState<'db, '_>,
        td: &TypedDict,
        args: &dyn Args<'db>,
    ) -> Option<Inferred>,
) -> Inferred {
    method_with_fallback(
        i_s,
        args,
        result_context,
        on_type_error,
        td,
        name,
        handler,
        || Instance::new(i_s.db.python_state.typed_dict_class(), None),
    )
}

fn typed_dict_setitem<'db>(
    i_s: &InferenceState<'db, '_>,
    args: &dyn Args<'db>,
    result_context: &mut ResultContext,
    on_type_error: OnTypeError,
    bound: Option<&Type>,
) -> Inferred {
    let Type::TypedDict(td) = bound.unwrap() else {
        unreachable!();
    };
    typed_dict_method_with_fallback(
        i_s,
        args,
        result_context,
        on_type_error,
        td,
        "__setitem__",
        typed_dict_setitem_internal,
    )
}

fn typed_dict_setitem_internal<'db>(
    i_s: &InferenceState<'db, '_>,
    td: &TypedDict,
    args: &dyn Args<'db>,
) -> Option<Inferred> {
    let mut iterator = args.iter(i_s.mode);
    let first_arg = iterator.next()?;
    let second_arg = iterator.next()?;
    if iterator.next().is_some() {
        return None;
    }
    let inf_key = first_arg.maybe_positional_arg(i_s, &mut ResultContext::Unknown)?;
    let value = second_arg.maybe_positional_arg(i_s, &mut ResultContext::Unknown)?;
    if let Some(literal) = inf_key.maybe_string_literal(i_s) {
        let key = literal.as_str(i_s.db);
        if let Some(member) = td.find_member(i_s.db, key) {
            if member.read_only {
                args.add_issue(
                    i_s,
                    IssueKind::TypedDictReadOnlyKeyMutated { key: key.into() },
                );
            }
            member.type_.error_if_not_matches(
                i_s,
                &value,
                |issue| args.add_issue(i_s, issue),
                |error_types| {
                    let ErrorStrs { expected, got } = error_types.as_boxed_strs(i_s.db);
                    Some(IssueKind::TypedDictKeySetItemIncompatibleType {
                        key: key.into(),
                        got,
                        expected,
                    })
                },
            );
        } else {
            args.add_issue(
                i_s,
                IssueKind::TypedDictHasNoKey {
                    typed_dict: td.format(&FormatData::new_short(i_s.db)).into(),
                    key: key.into(),
                },
            );
        }
    } else {
        add_access_key_must_be_string_literal_issue(i_s.db, td, |issue| args.add_issue(i_s, issue))
    }
    Some(Inferred::new_none())
}

fn typed_dict_delitem<'db>(
    i_s: &InferenceState<'db, '_>,
    args: &dyn Args<'db>,
    result_context: &mut ResultContext,
    on_type_error: OnTypeError,
    bound: Option<&Type>,
) -> Inferred {
    let Type::TypedDict(td) = bound.unwrap() else {
        unreachable!();
    };
    typed_dict_method_with_fallback(
        i_s,
        args,
        result_context,
        on_type_error,
        td,
        "__delitem__",
        typed_dict_delitem_internal,
    )
}

fn typed_dict_delitem_internal<'db>(
    i_s: &InferenceState<'db, '_>,
    td: &TypedDict,
    args: &dyn Args<'db>,
) -> Option<Inferred> {
    typed_dict_get_or_pop_internal(i_s, td, args, true).map(|_| Inferred::new_none())
}

fn typed_dict_update<'db>(
    i_s: &InferenceState<'db, '_>,
    args: &dyn Args<'db>,
    result_context: &mut ResultContext,
    on_type_error: OnTypeError,
    bound: Option<&Type>,
) -> Inferred {
    let Type::TypedDict(td) = bound.unwrap() else {
        unreachable!();
    };
    typed_dict_method_with_fallback(
        i_s,
        args,
        result_context,
        on_type_error,
        td,
        "update",
        typed_dict_update_internal,
    )
}

fn typed_dict_update_internal<'db>(
    i_s: &InferenceState<'db, '_>,
    td: &TypedDict,
    args: &dyn Args<'db>,
) -> Option<Inferred> {
    let mut members: Vec<_> = td.members(i_s.db).into();
    for member in members.iter_mut() {
        member.required = false;
    }
    let expected = TypedDict::new(
        td.name,
        members.into_boxed_slice(),
        td.defined_at,
        td.generics.clone(),
    );
    args.maybe_single_positional_arg(
        i_s,
        &mut ResultContext::new_known(&Type::TypedDict(expected)),
    )?;
    Some(Inferred::new_none())
}

pub(crate) fn initialize_typed_dict<'db>(
    typed_dict: Rc<TypedDict>,
    i_s: &InferenceState<'db, '_>,
    args: &dyn Args<'db>,
) -> Inferred {
    let mut iterator = args.iter(i_s.mode);
    let mut matcher = Matcher::new_typed_dict_matcher(&typed_dict);
    if let Some(first_arg) = iterator.next().filter(|arg| !arg.is_keyword_argument()) {
        if let Some(next_arg) = iterator.next() {
            next_arg.add_issue(i_s, IssueKind::TypedDictWrongArgumentsInConstructor);
            return Inferred::new_any_from_error();
        }
        let InferredArg::Inferred(x) = first_arg.infer(&mut ResultContext::WithMatcher {
            matcher: &mut matcher,
            type_: &Type::TypedDict(typed_dict.clone()),
        }) else {
            first_arg.add_issue(i_s, IssueKind::TypedDictWrongArgumentsInConstructor);
            return Inferred::new_any_from_error();
        };
        if !matches!(x.as_cow_type(i_s).as_ref(), Type::TypedDict(td) if td.defined_at == typed_dict.defined_at)
        {
            first_arg.add_issue(i_s, IssueKind::TypedDictWrongArgumentsInConstructor);
            return Inferred::new_any_from_error();
        }
    } else {
        check_typed_dict_call(i_s, &mut matcher, typed_dict.clone(), args);
    };
    let td = if matcher.has_type_var_matcher() {
        let generics = matcher
            .into_type_arguments(i_s, typed_dict.defined_at)
            .type_arguments_into_generics(i_s.db);
        typed_dict.apply_generics(i_s.db, TypedDictGenerics::Generics(generics.unwrap()))
    } else {
        typed_dict.clone()
    };
    Inferred::from_type(Type::TypedDict(td))
}

pub(crate) fn lookup_on_typed_dict<'a>(
    typed_dict: Rc<TypedDict>,
    i_s: &'a InferenceState,
    add_issue: &dyn Fn(IssueKind),
    name: &str,
    kind: LookupKind,
) -> LookupDetails<'a> {
    let bound = || Rc::new(Type::TypedDict(typed_dict.clone()));
    let lookup = LookupResult::UnknownName(Inferred::from_type(Type::CustomBehavior(match name {
        "get" => CustomBehavior::new_method(typed_dict_get, Some(bound())),
        "setdefault" => CustomBehavior::new_method(typed_dict_setdefault, Some(bound())),
        "pop" => CustomBehavior::new_method(typed_dict_pop, Some(bound())),
        "__setitem__" => CustomBehavior::new_method(typed_dict_setitem, Some(bound())),
        "__delitem__" => CustomBehavior::new_method(typed_dict_delitem, Some(bound())),
        "update" => CustomBehavior::new_method(typed_dict_update, Some(bound())),
        _ => {
            return Instance::new(i_s.db.python_state.typed_dict_class(), None).lookup(
                i_s,
                name,
                InstanceLookupOptions::new(add_issue)
                    .with_kind(kind)
                    .with_as_self_instance(&|| Type::TypedDict(typed_dict.clone())),
            )
        }
    })));
    LookupDetails::new(
        Type::TypedDict(typed_dict),
        lookup,
        AttributeKind::DefMethod { is_final: false },
    )
}

pub(crate) fn infer_typed_dict_item(
    i_s: &InferenceState,
    typed_dict: &TypedDict,
    matcher: &mut Matcher,
    add_issue: impl Fn(IssueKind),
    key: &str,
    extra_keys: &mut Vec<String>,
    infer: impl FnOnce(&mut ResultContext) -> Inferred,
) {
    if let Some(member) = typed_dict.find_member(i_s.db, key) {
        let inferred = infer(&mut ResultContext::WithMatcher {
            type_: &member.type_,
            matcher,
        });

        member.type_.error_if_not_matches_with_matcher(
            i_s,
            matcher,
            &inferred,
            add_issue,
            |error_types, _: &MismatchReason| {
                let ErrorStrs { expected, got } = error_types.as_boxed_strs(i_s.db);
                Some(IssueKind::TypedDictIncompatibleType {
                    key: key.into(),
                    got,
                    expected,
                })
            },
        );
    } else {
        extra_keys.push(key.into())
    }
}

pub(crate) fn check_typed_dict_call<'db>(
    i_s: &InferenceState<'db, '_>,
    matcher: &mut Matcher,
    typed_dict: Rc<TypedDict>,
    args: &dyn Args<'db>,
) -> Option<Type> {
    let mut extra_keys = vec![];
    for arg in args.iter(i_s.mode) {
        if let Some(key) = arg.keyword_name(i_s.db) {
            infer_typed_dict_item(
                i_s,
                &typed_dict,
                matcher,
                |issue| arg.add_issue(i_s, issue),
                key,
                &mut extra_keys,
                |context| arg.infer_inferrable(i_s, context),
            );
        } else {
            arg.add_issue(
                i_s,
                IssueKind::ArgumentIssue(
                    format!(
                        "Unexpected argument to \"{}\"",
                        typed_dict.name_or_fallback(&FormatData::new_short(i_s.db))
                    )
                    .into(),
                ),
            );
            return None;
        }
    }
    maybe_add_extra_keys_issue(
        i_s.db,
        &typed_dict,
        |issue| args.add_issue(i_s, issue),
        extra_keys,
    );
    let mut missing_keys: Vec<Box<str>> = vec![];
    for member in typed_dict.members(i_s.db).iter() {
        if member.required {
            let expected_name = member.name.as_str(i_s.db);
            if !args
                .iter(i_s.mode)
                .any(|arg| arg.keyword_name(i_s.db) == Some(expected_name))
            {
                missing_keys.push(expected_name.into())
            }
        }
    }
    if !missing_keys.is_empty() {
        args.add_issue(
            i_s,
            IssueKind::TypedDictMissingKeys {
                typed_dict: typed_dict
                    .name_or_fallback(&FormatData::new_short(i_s.db))
                    .into(),
                keys: missing_keys.into(),
            },
        )
    }
    Some(if matches!(&typed_dict.generics, TypedDictGenerics::None) {
        Type::TypedDict(typed_dict)
    } else {
        matcher
            .replace_type_var_likes_for_unknown_type_vars(i_s.db, &Type::TypedDict(typed_dict))
            .into_owned()
    })
}

pub(crate) fn maybe_add_extra_keys_issue(
    db: &Database,
    typed_dict: &TypedDict,
    add_issue: impl Fn(IssueKind),
    mut extra_keys: Vec<String>,
) {
    add_issue(IssueKind::TypedDictExtraKey {
        key: match extra_keys.len() {
            0 => return,
            1 => format!("\"{}\"", extra_keys.remove(0)).into(),
            _ => format!(
                "({})",
                join_with_commas(extra_keys.iter().map(|key| format!("\"{key}\"")))
            )
            .into(),
        },
        typed_dict: typed_dict
            .name_or_fallback(&FormatData::new_short(db))
            .into(),
    })
}
