use std::{
    cell::{Cell, OnceCell},
    hash::{Hash, Hasher},
    ops::AddAssign,
    rc::Rc,
};

use parsa_python_cst::NodeIndex;

use super::{
    AnyCause, CallableContent, CallableParams, FormatStyle, GenericItem, GenericsList, NeverCause,
    TupleArgs, TupleUnpack, Type, TypeArgs, WithUnpack,
};
use crate::{
    database::{ComplexPoint, Database, ParentScope, PointLink},
    diagnostics::IssueKind,
    format_data::{FormatData, ParamsStyle},
    inference_state::InferenceState,
    matching::Matcher,
    node_ref::NodeRef,
    utils::join_with_commas,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Hash)]
pub struct TypeVarIndex(pub(super) u32);

impl TypeVarIndex {
    pub fn as_usize(&self) -> usize {
        self.0 as usize
    }
}

impl AddAssign<i32> for TypeVarIndex {
    fn add_assign(&mut self, other: i32) {
        self.0 = (self.0 as i32 + other) as u32;
    }
}

impl From<usize> for TypeVarIndex {
    fn from(item: usize) -> Self {
        Self(item as u32)
    }
}

#[derive(Debug)]
pub struct CallableWithParent<T> {
    pub defined_at: T,
    pub parent_callable: Option<T>,
}

struct CallableAncestors<'a, T> {
    callables: &'a [CallableWithParent<T>],
    next: Option<&'a T>,
}

impl<'a, T: CallableId> Iterator for CallableAncestors<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        // This algorithm seems a bit weird in terms of Big O, but it shouldn't matter at all,
        // because this will have at most 3-5 callables (more typical is 0-1).
        if let Some(next) = self.next {
            let result = next;
            for callable_with_parent in self.callables {
                if callable_with_parent.defined_at.is_same(next) {
                    self.next = callable_with_parent.parent_callable.as_ref();
                    return Some(result);
                }
            }
            self.next = None;
            Some(result)
        } else {
            None
        }
    }
}

#[derive(Debug)]
struct UnresolvedTypeVarLike<T> {
    pub type_var_like: TypeVarLike,
    pub most_outer_callable: Option<T>,
}

#[derive(Debug)]
pub struct TypeVarManager<T> {
    type_vars: Vec<UnresolvedTypeVarLike<T>>,
    callables: Vec<CallableWithParent<T>>,
}

impl<T: CallableId> TypeVarManager<T> {
    pub fn position(&self, type_var: &TypeVarLike) -> Option<usize> {
        self.type_vars
            .iter()
            .position(|t| &t.type_var_like == type_var)
    }

    pub fn add(&mut self, type_var_like: TypeVarLike, in_callable: Option<T>) -> TypeVarIndex {
        if let Some(index) = self.position(&type_var_like) {
            self.type_vars[index].most_outer_callable = self.calculate_most_outer_callable(
                self.type_vars[index].most_outer_callable.as_ref(),
                in_callable,
            );
            index.into()
        } else {
            self.type_vars.push(UnresolvedTypeVarLike {
                type_var_like,
                most_outer_callable: in_callable,
            });
            (self.type_vars.len() - 1).into()
        }
    }

    pub fn register_callable(&mut self, c: CallableWithParent<T>) {
        self.callables.push(c)
    }

    pub fn is_callable_known(&self, callable: &Rc<CallableContent>) -> bool {
        self.callables
            .iter()
            .any(|c| c.defined_at.matches_callable(callable))
    }

    pub fn move_index(&mut self, old_index: TypeVarIndex, force_index: TypeVarIndex) {
        let removed = self.type_vars.remove(old_index.as_usize());
        self.type_vars.insert(force_index.as_usize(), removed);
    }

    pub fn has_late_bound_type_vars(&self) -> bool {
        self.type_vars
            .iter()
            .any(|t| t.most_outer_callable.is_some())
    }

    pub fn has_type_vars(&self) -> bool {
        !self.type_vars.is_empty()
    }

    pub fn has_type_var_tuples(&self) -> bool {
        self.type_vars
            .iter()
            .any(|t| matches!(t.type_var_like, TypeVarLike::TypeVarTuple(_)))
    }

    pub fn into_type_vars(self) -> TypeVarLikes {
        TypeVarLikes(
            self.type_vars
                .into_iter()
                .filter_map(|t| t.most_outer_callable.is_none().then_some(t.type_var_like))
                .collect(),
        )
    }

    pub fn iter(&self) -> impl Iterator<Item = &TypeVarLike> {
        self.type_vars.iter().map(|u| &u.type_var_like)
    }

    pub fn last(&self) -> Option<&TypeVarLike> {
        self.type_vars.last().map(|u| &u.type_var_like)
    }

    pub fn type_vars_for_callable(&self, callable: &Rc<CallableContent>) -> TypeVarLikes {
        TypeVarLikes::new(
            self.type_vars
                .iter()
                .filter(|&t| {
                    t.most_outer_callable
                        .as_ref()
                        .is_some_and(|m| m.matches_callable(callable))
                })
                .map(|t| t.type_var_like.clone())
                .collect(),
        )
    }

    pub fn len(&self) -> usize {
        self.type_vars.len()
    }

    fn calculate_most_outer_callable(&self, first: Option<&T>, second: Option<T>) -> Option<T> {
        for ancestor1 in (CallableAncestors {
            callables: &self.callables,
            next: first,
        }) {
            for ancestor2 in (CallableAncestors {
                callables: &self.callables,
                next: second.as_ref(),
            }) {
                if ancestor1.is_same(ancestor2) {
                    return Some(ancestor1.clone());
                }
            }
        }
        None
    }

    fn remap_internal(
        &self,
        usage: &TypeVarLikeUsage,
    ) -> Option<(TypeVarIndex, Option<PointLink>)> {
        let mut index = 0;
        let mut in_definition: Option<Option<&T>> = None;
        for t in self.type_vars.iter().rev() {
            let matched = match &t.type_var_like {
                TypeVarLike::TypeVar(type_var) => match usage {
                    TypeVarLikeUsage::TypeVar(u) => type_var.as_ref() == u.type_var.as_ref(),
                    _ => false,
                },
                TypeVarLike::TypeVarTuple(t) => match usage {
                    TypeVarLikeUsage::TypeVarTuple(u) => t.as_ref() == u.type_var_tuple.as_ref(),
                    _ => false,
                },
                TypeVarLike::ParamSpec(p) => match usage {
                    TypeVarLikeUsage::ParamSpec(u) => p.as_ref() == u.param_spec.as_ref(),
                    _ => false,
                },
            };
            if let Some(in_def) = in_definition {
                if in_def.is_none() && t.most_outer_callable.is_none()
                    || in_def
                        .zip(t.most_outer_callable.as_ref())
                        .is_some_and(|(in_def, m)| in_def.is_same(m))
                {
                    index += 1;
                }
            } else if matched {
                in_definition = Some(t.most_outer_callable.as_ref());
                index = 0;
            }
        }
        in_definition.map(|d| (index.into(), d.map(|d| d.as_in_definition())))
    }

    pub fn remap_type_var(&self, usage: &TypeVarUsage) -> TypeVarUsage {
        if let Some((index, in_definition)) =
            self.remap_internal(&TypeVarLikeUsage::TypeVar(usage.clone()))
        {
            TypeVarUsage::new(
                usage.type_var.clone(),
                in_definition.unwrap_or(usage.in_definition),
                index,
            )
        } else {
            usage.clone()
        }
    }

    pub fn remap_type_var_tuple(&self, usage: &TypeVarTupleUsage) -> TypeVarTupleUsage {
        if let Some((index, in_definition)) =
            self.remap_internal(&TypeVarLikeUsage::TypeVarTuple(usage.clone()))
        {
            TypeVarTupleUsage::new(
                usage.type_var_tuple.clone(),
                in_definition.unwrap_or(usage.in_definition),
                index,
            )
        } else {
            usage.clone()
        }
    }

    pub fn remap_param_spec(&self, usage: &ParamSpecUsage) -> ParamSpecUsage {
        if let Some((index, in_definition)) =
            self.remap_internal(&TypeVarLikeUsage::ParamSpec(usage.clone()))
        {
            ParamSpecUsage::new(
                usage.param_spec.clone(),
                in_definition.unwrap_or(usage.in_definition),
                index,
            )
        } else {
            usage.clone()
        }
    }
}

impl Default for TypeVarManager<PointLink> {
    fn default() -> Self {
        Self {
            type_vars: vec![],
            callables: vec![],
        }
    }
}

impl Default for TypeVarManager<Rc<CallableContent>> {
    fn default() -> Self {
        Self {
            type_vars: vec![],
            callables: vec![],
        }
    }
}

pub trait CallableId: Clone {
    fn is_same(&self, other: &Self) -> bool;
    fn as_in_definition(&self) -> PointLink;
    fn matches_callable(&self, callable: &Rc<CallableContent>) -> bool;
}

impl CallableId for PointLink {
    fn is_same(&self, other: &Self) -> bool {
        self == other
    }

    fn as_in_definition(&self) -> PointLink {
        *self
    }

    fn matches_callable(&self, callable: &Rc<CallableContent>) -> bool {
        *self == callable.defined_at
    }
}

impl CallableId for Rc<CallableContent> {
    fn is_same(&self, other: &Self) -> bool {
        Rc::ptr_eq(self, other)
    }

    fn as_in_definition(&self) -> PointLink {
        self.defined_at
    }

    fn matches_callable(&self, callable: &Self) -> bool {
        self.is_same(callable)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Variance {
    Invariant = 0,
    Covariant,
    Contravariant,
}

impl Variance {
    pub fn name(self) -> &'static str {
        match self {
            Self::Invariant => "Invariant",
            Self::Covariant => "Covariant",
            Self::Contravariant => "Contravariant",
        }
    }

    pub fn invert(self) -> Self {
        match self {
            Variance::Covariant => Variance::Contravariant,
            Variance::Contravariant => Variance::Covariant,
            Variance::Invariant => Variance::Invariant,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeVarLikes(Rc<[TypeVarLike]>);

impl TypeVarLikes {
    pub fn new(rc: Rc<[TypeVarLike]>) -> Self {
        Self(rc)
    }

    pub fn from_vec(vec: Vec<TypeVarLike>) -> Self {
        Self(Rc::from(vec))
    }

    pub fn as_vec(&self) -> Vec<TypeVarLike> {
        Vec::from(self.0.as_ref())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains_non_default(&self) -> bool {
        self.iter().any(|tv| !tv.has_default())
    }

    pub fn has_constraints(&self, db: &Database) -> bool {
        self.iter().any(|tv| {
            matches!(tv, TypeVarLike::TypeVar(tv)
                          if matches!(&tv.kind(db), TypeVarKind::Constraints(_)))
        })
    }

    pub fn find(
        &self,
        type_var_like: TypeVarLike,
        in_definition: PointLink,
    ) -> Option<TypeVarLikeUsage> {
        self.0
            .iter()
            .position(|t| t == &type_var_like)
            .map(|index| match type_var_like {
                TypeVarLike::TypeVar(type_var) => TypeVarLikeUsage::TypeVar(TypeVarUsage::new(
                    type_var,
                    in_definition,
                    index.into(),
                )),
                TypeVarLike::TypeVarTuple(type_var_tuple) => TypeVarLikeUsage::TypeVarTuple(
                    TypeVarTupleUsage::new(type_var_tuple, in_definition, index.into()),
                ),
                TypeVarLike::ParamSpec(param_spec) => TypeVarLikeUsage::ParamSpec(
                    ParamSpecUsage::new(param_spec, in_definition, index.into()),
                ),
            })
    }

    pub fn as_any_generic_list(&self, db: &Database) -> GenericsList {
        GenericsList::new_generics(self.iter().map(|tv| tv.as_any_generic_item(db)).collect())
    }

    pub fn iter(&self) -> std::slice::Iter<TypeVarLike> {
        self.0.iter()
    }

    pub fn format(&self, format_data: &FormatData) -> String {
        debug_assert!(!self.is_empty());
        format!(
            "[{}] ",
            join_with_commas(self.iter().map(|t| match t {
                TypeVarLike::TypeVar(t) => t.format(format_data),
                TypeVarLike::TypeVarTuple(tvt) => tvt.format(format_data),
                TypeVarLike::ParamSpec(s) => s.format(format_data),
            }))
        )
    }

    pub fn load_saved_type_vars<'a>(db: &'a Database, node_ref: NodeRef<'a>) -> &'a TypeVarLikes {
        debug_assert!(node_ref.point().calculated());
        match node_ref.complex() {
            Some(ComplexPoint::TypeVarLikes(type_vars)) => type_vars,
            None => &db.python_state.empty_type_var_likes,
            _ => unreachable!(),
        }
    }
}

impl std::ops::Index<usize> for TypeVarLikes {
    type Output = TypeVarLike;

    fn index(&self, index: usize) -> &TypeVarLike {
        &self.0[index]
    }
}

#[derive(Debug, Clone, Eq)]
pub enum TypeVarLike {
    TypeVar(Rc<TypeVar>),
    TypeVarTuple(Rc<TypeVarTuple>),
    ParamSpec(Rc<ParamSpec>),
}

impl TypeVarLike {
    pub fn name<'db>(&self, db: &'db Database) -> &'db str {
        match self {
            Self::TypeVar(t) => t.name(db),
            Self::TypeVarTuple(t) => t.name(db),
            Self::ParamSpec(s) => s.name(db),
        }
    }

    pub fn has_default(&self) -> bool {
        match self {
            TypeVarLike::TypeVar(tv) => tv.default.is_some(),
            TypeVarLike::TypeVarTuple(tvt) => tvt.default.is_some(),
            TypeVarLike::ParamSpec(param_spec) => param_spec.default.is_some(),
        }
    }

    pub fn as_type_var_like_usage(
        &self,
        index: TypeVarIndex,
        in_definition: PointLink,
    ) -> TypeVarLikeUsage {
        match self {
            Self::TypeVar(type_var) => {
                TypeVarLikeUsage::TypeVar(TypeVarUsage::new(type_var.clone(), in_definition, index))
            }
            Self::TypeVarTuple(t) => TypeVarLikeUsage::TypeVarTuple(TypeVarTupleUsage::new(
                t.clone(),
                in_definition,
                index,
            )),
            Self::ParamSpec(p) => {
                TypeVarLikeUsage::ParamSpec(ParamSpecUsage::new(p.clone(), in_definition, index))
            }
        }
    }

    pub fn as_any_generic_item(&self, db: &Database) -> GenericItem {
        match self {
            TypeVarLike::TypeVar(tv) => match tv.default(db) {
                Some(default) => GenericItem::TypeArg(default.clone()),
                None => GenericItem::TypeArg(Type::Any(AnyCause::Todo)),
            },
            TypeVarLike::TypeVarTuple(tvt) => match &tvt.default {
                Some(default) => GenericItem::TypeArgs(default.clone()),
                None => {
                    GenericItem::TypeArgs(TypeArgs::new_arbitrary_length(Type::Any(AnyCause::Todo)))
                }
            },
            TypeVarLike::ParamSpec(param_spec) => match &param_spec.default {
                Some(default) => {
                    GenericItem::ParamSpecArg(ParamSpecArg::new(default.clone(), None))
                }
                None => GenericItem::ParamSpecArg(ParamSpecArg::new_any(AnyCause::Todo)),
            },
        }
    }

    pub fn as_never_generic_item(&self, db: &Database, cause: NeverCause) -> GenericItem {
        match self {
            TypeVarLike::TypeVar(tv) => match tv.default(db) {
                Some(default) => GenericItem::TypeArg(default.clone()),
                None => GenericItem::TypeArg(Type::Never(cause)),
            },
            TypeVarLike::TypeVarTuple(tvt) => match &tvt.default {
                Some(default) => GenericItem::TypeArgs(default.clone()),
                None => GenericItem::TypeArgs(TypeArgs::new_arbitrary_length(Type::Never(cause))),
            },
            TypeVarLike::ParamSpec(param_spec) => match &param_spec.default {
                Some(default) => {
                    GenericItem::ParamSpecArg(ParamSpecArg::new(default.clone(), None))
                }
                // TODO ParamSpec: this feels wrong, should maybe be never?
                None => GenericItem::ParamSpecArg(ParamSpecArg::new_never(cause)),
            },
        }
    }

    pub fn ensure_calculated_types(&self, db: &Database) {
        match self {
            Self::TypeVar(tv) => {
                if let TypeVarKind::Constraints(constraints) = tv.kind(db) {
                    // Consume the iterator for constraints to ensure it is calculated
                    constraints.for_each(|_| ())
                }
                tv.default(db);
            }
            _ => (),
        }
    }
}

impl std::cmp::PartialEq for TypeVarLike {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::TypeVar(t1), Self::TypeVar(t2)) => Rc::ptr_eq(t1, t2),
            (Self::TypeVarTuple(t1), Self::TypeVarTuple(t2)) => Rc::ptr_eq(t1, t2),
            (Self::ParamSpec(p1), Self::ParamSpec(p2)) => Rc::ptr_eq(p1, p2),
            _ => false,
        }
    }
}

impl Hash for TypeVarLike {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            TypeVarLike::TypeVar(tv) => Rc::as_ptr(tv).hash(state),
            TypeVarLike::TypeVarTuple(tvt) => Rc::as_ptr(tvt).hash(state),
            TypeVarLike::ParamSpec(p) => Rc::as_ptr(p).hash(state),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeVarName {
    PointLink(PointLink),
    Self_,
}

#[derive(Debug, Clone)]
pub struct TypeInTypeVar {
    node: Option<NodeIndex>,
    calculating: Cell<bool>,
    pub t: OnceCell<Type>,
}

impl TypeInTypeVar {
    pub fn new_lazy(node: NodeIndex) -> Self {
        Self {
            node: Some(node),
            calculating: Cell::new(false),
            t: OnceCell::new(),
        }
    }

    pub fn new_known(t: Type) -> Self {
        Self {
            node: None,
            calculating: Cell::new(false),
            t: OnceCell::from(t),
        }
    }

    #[inline]
    fn get_type(
        &self,
        db: &Database,
        name_string: &TypeVarName,
        scope: ParentScope,
        calculate_type: impl FnOnce(&InferenceState, NodeRef) -> Type,
    ) -> &Type {
        if self.calculating.get() {
            // TODO we should add an error here.
            return &Type::ERROR;
        }
        self.t.get_or_init(|| {
            self.calculating.set(true);
            let node = self.node.unwrap();
            let TypeVarName::PointLink(link) = name_string else {
                unreachable!()
            };
            let node_ref = NodeRef::from_link(db, PointLink::new(link.file, node));
            InferenceState::run_with_parent_scope(db, link.file, scope, |i_s| {
                let t = calculate_type(&i_s, node_ref);
                self.calculating.set(false);
                t
            })
        })
    }
}

#[derive(Debug, Clone)]
pub enum TypeVarKindInfos {
    Unrestricted,
    Bound(TypeInTypeVar),
    Constraints(Box<[TypeInTypeVar]>),
}

pub enum TypeVarKind<'a, I: Iterator<Item = &'a Type> + Clone> {
    Unrestricted,
    Bound(&'a Type),
    Constraints(I),
}

#[derive(Debug, Clone)]
pub struct TypeVar {
    pub name_string: TypeVarName,
    scope: ParentScope,
    kind: TypeVarKindInfos,
    default: Option<TypeInTypeVar>,
    pub variance: Variance,
}

impl PartialEq for TypeVar {
    fn eq(&self, other: &Self) -> bool {
        self.name_string == other.name_string
    }
}

impl Eq for TypeVar {}

impl TypeVar {
    pub fn new(
        name_link: PointLink,
        scope: ParentScope,
        kind: TypeVarKindInfos,
        default: Option<NodeIndex>,
        variance: Variance,
    ) -> Self {
        Self {
            name_string: TypeVarName::PointLink(name_link),
            scope,
            kind,
            default: default.map(TypeInTypeVar::new_lazy),
            variance,
        }
    }

    pub fn new_self(kind: TypeVarKindInfos) -> Self {
        Self {
            name_string: TypeVarName::Self_,
            scope: ParentScope::Module,
            kind,
            default: None,
            variance: Variance::Invariant,
        }
    }

    pub fn name<'db>(&self, db: &'db Database) -> &'db str {
        match self.name_string {
            TypeVarName::PointLink(link) => {
                NodeRef::from_link(db, link).maybe_str().unwrap().content()
            }
            TypeVarName::Self_ => "Self",
        }
    }

    pub fn kind<'a>(
        &'a self,
        db: &'a Database,
    ) -> TypeVarKind<'a, impl Iterator<Item = &'a Type> + Clone + 'a> {
        match &self.kind {
            TypeVarKindInfos::Unrestricted => TypeVarKind::Unrestricted,
            TypeVarKindInfos::Bound(bound) => TypeVarKind::Bound(bound.get_type(
                db,
                &self.name_string,
                self.scope,
                |i_s, node_ref| {
                    node_ref
                        .file
                        .inference(i_s)
                        .compute_type_var_bound(node_ref.as_expression())
                },
            )),
            TypeVarKindInfos::Constraints(constraints) => {
                TypeVarKind::Constraints(constraints.iter().map(|c| {
                    c.get_type(db, &self.name_string, self.scope, |i_s, node_ref| {
                        node_ref
                            .file
                            .inference(i_s)
                            .compute_type_var_value(node_ref.as_expression())
                            .unwrap_or(Type::ERROR)
                    })
                }))
            }
        }
    }

    pub fn default(&self, db: &Database) -> Option<&Type> {
        let default = self.default.as_ref()?;
        Some(
            default.get_type(db, &self.name_string, self.scope, |i_s, node_ref| {
                let default = if let Some(t) = node_ref
                    .file
                    .inference(i_s)
                    .compute_type_var_default(node_ref.as_expression())
                {
                    t
                } else {
                    node_ref.add_issue(i_s, IssueKind::TypeVarInvalidDefault);
                    Type::ERROR
                };
                match self.kind(db) {
                    TypeVarKind::Unrestricted => (),
                    TypeVarKind::Bound(bound) => {
                        if !default.is_simple_sub_type_of(i_s, bound).bool() {
                            node_ref.add_issue(i_s, IssueKind::TypeVarDefaultMustBeASubtypeOfBound);
                        }
                    }
                    TypeVarKind::Constraints(mut constraints) => {
                        if !constraints.any(|constraint| {
                            default
                                .is_sub_type_of(
                                    i_s,
                                    &mut Matcher::with_ignored_promotions(),
                                    constraint,
                                )
                                .bool()
                        }) {
                            node_ref.add_issue(
                                i_s,
                                IssueKind::TypeVarDefaultMustBeASubtypeOfConstraints,
                            );
                        }
                    }
                };
                default
            }),
        )
    }

    pub fn is_unrestricted(&self) -> bool {
        matches!(self.kind, TypeVarKindInfos::Unrestricted)
    }

    pub fn qualified_name(&self, db: &Database) -> Box<str> {
        match self.name_string {
            TypeVarName::PointLink(link) => {
                let node_ref = NodeRef::from_link(db, link);
                format!(
                    "{}.{}",
                    node_ref.file.qualified_name(db),
                    node_ref.maybe_str().unwrap().content()
                )
                .into()
            }
            TypeVarName::Self_ => Box::from("Self"),
        }
    }

    pub fn format(&self, format_data: &FormatData) -> String {
        let mut s = self.name(format_data.db).to_owned();
        match self.kind(format_data.db) {
            TypeVarKind::Unrestricted => (),
            TypeVarKind::Bound(bound) => {
                if format_data.style == FormatStyle::MypyRevealType {
                    s += " <: ";
                } else {
                    s += ": ";
                }
                s += &bound.format(format_data);
            }
            TypeVarKind::Constraints(constraints) => {
                if format_data.style == FormatStyle::MypyRevealType {
                    s += " in ";
                } else {
                    s += ": ";
                }
                s += &format!(
                    "({})",
                    join_with_commas(constraints.map(|t| t.format(format_data).into()))
                );
            }
        }
        if let Some(default) = self.default(format_data.db) {
            s += " = ";
            s += &default.format(format_data);
        }
        s
    }
}

#[derive(Debug, Clone, Eq)]
pub struct TypeVarTuple {
    pub name_string: PointLink,
    // TODO calculated these lazily
    pub default: Option<TypeArgs>,
}

impl TypeVarTuple {
    pub fn name<'db>(&self, db: &'db Database) -> &'db str {
        NodeRef::from_link(db, self.name_string)
            .maybe_str()
            .unwrap()
            .content()
    }

    pub fn format(&self, format_data: &FormatData) -> String {
        if let Some(default) = &self.default {
            format!(
                "{} = Unpack[tuple[{}]]",
                self.name(format_data.db),
                default
                    .format(format_data)
                    .unwrap_or_else(|| "TODO format empty tuple".into())
            )
        } else {
            self.name(format_data.db).into()
        }
    }
}

impl PartialEq for TypeVarTuple {
    fn eq(&self, other: &Self) -> bool {
        self.name_string == other.name_string
    }
}

#[derive(Debug, Clone, Eq)]
pub struct ParamSpec {
    pub name_string: PointLink,
    // TODO calculated these lazily
    pub default: Option<CallableParams>,
}

impl ParamSpec {
    pub fn name<'db>(&self, db: &'db Database) -> &'db str {
        NodeRef::from_link(db, self.name_string)
            .maybe_str()
            .unwrap()
            .content()
    }

    fn format(&self, format_data: &FormatData) -> String {
        if let Some(default) = &self.default {
            format!(
                "{} = [{}]",
                self.name(format_data.db),
                default.format(format_data, ParamsStyle::Unreachable)
            )
        } else {
            self.name(format_data.db).into()
        }
    }
}

impl PartialEq for ParamSpec {
    fn eq(&self, other: &Self) -> bool {
        self.name_string == other.name_string
    }
}

#[derive(Debug, Eq, Clone)]
pub struct TypeVarUsage {
    pub type_var: Rc<TypeVar>,
    pub in_definition: PointLink,
    pub index: TypeVarIndex,
    // This should only ever be used for type matching. This is also only used for stuff like
    // foo(foo) where the callable is used twice with type vars and polymorphic matching is needed
    // to negotiate the type vars. This is reset after type matching and should always be 0.
    pub temporary_matcher_id: u32,
}

impl TypeVarUsage {
    pub fn new(type_var: Rc<TypeVar>, in_definition: PointLink, index: TypeVarIndex) -> Self {
        Self {
            type_var,
            in_definition,
            index,
            temporary_matcher_id: 0,
        }
    }
}

impl std::cmp::PartialEq for TypeVarUsage {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.type_var, &other.type_var)
            && self.in_definition == other.in_definition
            && self.index == other.index
            && self.temporary_matcher_id == other.temporary_matcher_id
    }
}

impl Hash for TypeVarUsage {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Rc::as_ptr(&self.type_var).hash(state);
        self.in_definition.hash(state);
        self.index.hash(state);
        self.temporary_matcher_id.hash(state);
    }
}

#[derive(Debug, PartialEq, Clone, Eq)]
pub struct TypeVarTupleUsage {
    pub type_var_tuple: Rc<TypeVarTuple>,
    pub in_definition: PointLink,
    pub index: TypeVarIndex,
    pub temporary_matcher_id: u32,
}

impl TypeVarTupleUsage {
    pub fn new(
        type_var_tuple: Rc<TypeVarTuple>,
        in_definition: PointLink,
        index: TypeVarIndex,
    ) -> Self {
        Self {
            type_var_tuple,
            in_definition,
            index,
            temporary_matcher_id: 0,
        }
    }
}

impl Hash for TypeVarTupleUsage {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Rc::as_ptr(&self.type_var_tuple).hash(state);
        self.in_definition.hash(state);
        self.index.hash(state);
        self.temporary_matcher_id.hash(state);
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ParamSpecUsage {
    pub param_spec: Rc<ParamSpec>,
    pub in_definition: PointLink,
    pub index: TypeVarIndex,
    pub temporary_matcher_id: u32,
}

impl ParamSpecUsage {
    pub fn new(param_spec: Rc<ParamSpec>, in_definition: PointLink, index: TypeVarIndex) -> Self {
        Self {
            param_spec,
            in_definition,
            index,
            temporary_matcher_id: 0,
        }
    }

    pub fn into_generic_item(self) -> GenericItem {
        GenericItem::ParamSpecArg(ParamSpecArg::new(
            CallableParams::new_param_spec(self),
            None,
        ))
    }
}

impl Hash for ParamSpecUsage {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Rc::as_ptr(&self.param_spec).hash(state);
        self.in_definition.hash(state);
        self.index.hash(state);
        self.temporary_matcher_id.hash(state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParamSpecTypeVars {
    pub type_vars: TypeVarLikes,
    pub in_definition: PointLink,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParamSpecArg {
    pub params: CallableParams,
    pub type_vars: Option<ParamSpecTypeVars>,
}

impl ParamSpecArg {
    pub fn new(params: CallableParams, type_vars: Option<ParamSpecTypeVars>) -> Self {
        Self { params, type_vars }
    }

    pub fn new_any(cause: AnyCause) -> Self {
        Self {
            params: CallableParams::Any(cause),
            type_vars: None,
        }
    }

    pub fn new_never(cause: NeverCause) -> Self {
        Self {
            params: CallableParams::Never(cause),
            type_vars: None,
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum TypeVarLikeUsage {
    TypeVar(TypeVarUsage),
    TypeVarTuple(TypeVarTupleUsage),
    ParamSpec(ParamSpecUsage),
}

impl TypeVarLikeUsage {
    pub fn in_definition(&self) -> PointLink {
        match self {
            Self::TypeVar(t) => t.in_definition,
            Self::TypeVarTuple(t) => t.in_definition,
            Self::ParamSpec(p) => p.in_definition,
        }
    }

    pub fn name_definition(&self) -> Option<PointLink> {
        match self {
            Self::TypeVar(t) => match t.type_var.name_string {
                TypeVarName::PointLink(link) => Some(link),
                TypeVarName::Self_ => None,
            },
            Self::TypeVarTuple(t) => Some(t.type_var_tuple.name_string),
            Self::ParamSpec(p) => Some(p.param_spec.name_string),
        }
    }

    pub fn add_to_index(&mut self, amount: i32) {
        match self {
            Self::TypeVar(t) => t.index += amount,
            Self::TypeVarTuple(t) => t.index += amount,
            Self::ParamSpec(p) => p.index += amount,
        }
    }

    pub fn index(&self) -> TypeVarIndex {
        match self {
            Self::TypeVar(t) => t.index,
            Self::TypeVarTuple(t) => t.index,
            Self::ParamSpec(p) => p.index,
        }
    }

    pub fn temporary_matcher_id(&self) -> u32 {
        match self {
            Self::TypeVar(t) => t.temporary_matcher_id,
            Self::TypeVarTuple(t) => t.temporary_matcher_id,
            Self::ParamSpec(p) => p.temporary_matcher_id,
        }
    }

    pub fn as_type_var_like(&self) -> TypeVarLike {
        match self {
            Self::TypeVar(t) => TypeVarLike::TypeVar(t.type_var.clone()),
            Self::TypeVarTuple(t) => TypeVarLike::TypeVarTuple(t.type_var_tuple.clone()),
            Self::ParamSpec(p) => TypeVarLike::ParamSpec(p.param_spec.clone()),
        }
    }

    pub fn as_any_generic_item(&self, db: &Database) -> GenericItem {
        self.as_type_var_like().as_any_generic_item(db)
    }

    pub fn into_generic_item(self) -> GenericItem {
        match self {
            TypeVarLikeUsage::TypeVar(usage) => GenericItem::TypeArg(Type::TypeVar(usage)),
            TypeVarLikeUsage::TypeVarTuple(usage) => GenericItem::TypeArgs(TypeArgs {
                args: TupleArgs::WithUnpack(WithUnpack {
                    before: Rc::from([]),
                    unpack: TupleUnpack::TypeVarTuple(usage),
                    after: Rc::from([]),
                }),
            }),
            TypeVarLikeUsage::ParamSpec(usage) => usage.into_generic_item(),
        }
    }

    pub fn into_generic_item_with_new_index(self, index: TypeVarIndex) -> GenericItem {
        match self {
            TypeVarLikeUsage::TypeVar(mut usage) => {
                usage.index = index;
                GenericItem::TypeArg(Type::TypeVar(usage))
            }
            TypeVarLikeUsage::TypeVarTuple(mut usage) => {
                usage.index = index;
                GenericItem::TypeArgs(TypeArgs {
                    args: TupleArgs::WithUnpack(WithUnpack {
                        before: Rc::from([]),
                        unpack: TupleUnpack::TypeVarTuple(usage),
                        after: Rc::from([]),
                    }),
                })
            }
            TypeVarLikeUsage::ParamSpec(mut usage) => {
                usage.index = index;
                GenericItem::ParamSpecArg(ParamSpecArg::new(
                    CallableParams::new_param_spec(usage),
                    None,
                ))
            }
        }
    }

    pub fn update_in_definition_and_index(
        &mut self,
        in_definition: PointLink,
        index: TypeVarIndex,
    ) {
        match self {
            Self::TypeVar(t) => {
                t.index = index;
                t.in_definition = in_definition;
            }
            Self::TypeVarTuple(t) => {
                t.index = index;
                t.in_definition = in_definition;
            }
            Self::ParamSpec(p) => {
                p.index = index;
                p.in_definition = in_definition;
            }
        }
    }

    pub fn update_temporary_matcher_index(&mut self, index: u32) {
        match self {
            Self::TypeVar(t) => {
                t.temporary_matcher_id = index;
            }
            Self::TypeVarTuple(t) => {
                t.temporary_matcher_id = index;
            }
            Self::ParamSpec(p) => {
                p.temporary_matcher_id = index;
            }
        }
    }

    pub fn format_without_matcher(&self, db: &Database, params_style: ParamsStyle) -> String {
        match self {
            Self::TypeVar(usage) => {
                let mut s = usage.type_var.name(db).to_owned();
                if let Some(default) = usage.type_var.default(db) {
                    s += " = ";
                    s += &default.format_short(db);
                }
                s
            }
            Self::TypeVarTuple(t) => format!(
                "Unpack[{}]",
                t.type_var_tuple.format(&FormatData::new_short(db))
            ),
            Self::ParamSpec(p) => match params_style {
                ParamsStyle::CallableParams => p.param_spec.format(&FormatData::new_short(db)),
                ParamsStyle::CallableParamsInner => format!("**{}", p.param_spec.name(db)),
                ParamsStyle::Unreachable => unreachable!(),
            },
        }
    }
}
