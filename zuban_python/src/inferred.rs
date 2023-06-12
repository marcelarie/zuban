use parsa_python_ast::{NodeIndex, PrimaryContent, PythonString, SliceType as ASTSliceType};
use std::rc::Rc;

use crate::arguments::{Arguments, CombinedArguments, KnownArguments};
use crate::database::{
    CallableContent, ClassGenerics, ComplexPoint, Database, DbType, FileIndex, GenericItem,
    GenericsList, Literal as DbLiteral, LiteralKind, Locality, MroIndex, NewType, Point, PointLink,
    PointType, Specific, TypeVarLike,
};
use crate::debug;
use crate::diagnostics::IssueType;
use crate::file::{on_argument_type_error, use_cached_annotation_type, File, PythonFile};
use crate::getitem::{SliceType, SliceTypeContent};
use crate::inference_state::InferenceState;
use crate::matching::{
    create_signature_without_self, replace_class_type_vars, FormatData, Generics, IteratorContent,
    LookupResult, Matcher, OnLookupError, OnTypeError, ResultContext, Type,
};
use crate::node_ref::NodeRef;
use crate::type_helpers::{
    BoundMethod, BoundMethodFunction, Callable, Class, FirstParamProperties, Function, Instance,
    NewTypeClass, OverloadedFunction, ParamSpecClass, RevealTypeFunction, TypeOrClass,
    TypeVarClass, TypeVarTupleClass, TypingCast, TypingClass,
};

#[derive(Debug)]
pub enum FunctionOrOverload<'a> {
    Function(Function<'a, 'a>),
    Callable(Rc<CallableContent>),
    Overload(OverloadedFunction<'a>),
}

#[derive(Debug, Clone, PartialEq)]
enum InferredState {
    Saved(PointLink),
    UnsavedFileReference(FileIndex),
    UnsavedComplex(ComplexPoint),
    UnsavedSpecific(Specific),
    BoundMethod {
        instance: BoundMethodInstance,
        mro_index: MroIndex,
        func_link: PointLink,
    },
    Unknown,
}

#[derive(Clone, Debug)]
pub struct Inferred {
    state: InferredState,
}

impl<'db: 'slf, 'slf> Inferred {
    pub fn new_and_save(file: &'db PythonFile, node_index: NodeIndex, point: Point) -> Self {
        file.points.set(node_index, point);
        Self::new_saved(file, node_index, point)
    }

    pub fn from_saved_node_ref(node_ref: NodeRef) -> Self {
        Self {
            state: InferredState::Saved(node_ref.as_link()),
        }
    }

    pub fn new_saved(file: &'db PythonFile, node_index: NodeIndex, point: Point) -> Self {
        Self {
            state: InferredState::Saved(PointLink::new(file.file_index(), node_index)),
        }
    }

    pub fn new_unsaved_complex(complex: ComplexPoint) -> Self {
        Self {
            state: InferredState::UnsavedComplex(complex),
        }
    }

    fn new_bound_method(
        instance: BoundMethodInstance,
        mro_index: MroIndex,
        func_link: PointLink,
    ) -> Self {
        Self {
            state: InferredState::BoundMethod {
                instance,
                mro_index,
                func_link,
            },
        }
    }

    pub fn new_cycle() -> Self {
        Self {
            state: InferredState::UnsavedSpecific(Specific::Cycle),
        }
    }

    pub fn new_unknown() -> Self {
        Self {
            state: InferredState::Unknown,
        }
    }

    pub fn new_none() -> Self {
        Self {
            state: InferredState::UnsavedSpecific(Specific::None),
        }
    }

    pub fn new_any() -> Self {
        Self {
            state: InferredState::UnsavedSpecific(Specific::Any),
        }
    }

    pub fn new_file_reference(index: FileIndex) -> Self {
        // TODO maybe remove this and UnsavedFileReference??? unused??
        Self {
            state: InferredState::UnsavedFileReference(index),
        }
    }

    pub fn execute_db_type_allocation_todo(i_s: &InferenceState<'db, '_>, t: &Type) -> Self {
        // Everything that calls this should probably not allocate.
        let t = t.as_db_type();
        Self::from_type(t)
    }

    pub fn from_type(t: DbType) -> Self {
        Self {
            state: InferredState::UnsavedComplex(ComplexPoint::TypeInstance(t)),
        }
    }

    pub fn execute_db_type(i_s: &InferenceState<'db, '_>, generic: DbType) -> Self {
        let state = match generic {
            DbType::Type(ref c) if matches!(c.as_ref(), DbType::Class(_, ClassGenerics::None)) => {
                match c.as_ref() {
                    DbType::Class(link, ClassGenerics::None) => {
                        let node_ref = NodeRef::from_link(i_s.db, *link);
                        InferredState::Saved(*link)
                    }
                    _ => unreachable!(),
                }
            }
            DbType::None => return Inferred::new_none(),
            DbType::Any => return Inferred::new_any(),
            _ => InferredState::UnsavedComplex(ComplexPoint::TypeInstance(generic)),
        };
        Self { state }
    }

    pub fn create_instance(class: PointLink, generics: Option<Rc<[GenericItem]>>) -> Self {
        Self::new_unsaved_complex(ComplexPoint::TypeInstance(DbType::Class(
            class,
            match generics {
                Some(generics) => ClassGenerics::List(GenericsList::new_generics(generics)),
                None => ClassGenerics::None,
            },
        )))
    }

    pub fn saved_as_type(&self, i_s: &InferenceState<'db, '_>) -> Option<Type<'db>> {
        match &self.state {
            InferredState::Saved(definition) => Some(saved_as_type(i_s, *definition)),
            _ => None,
        }
    }

    pub fn as_type(&'slf self, i_s: &InferenceState<'db, '_>) -> Type<'slf> {
        match &self.state {
            InferredState::Saved(definition) => saved_as_type(i_s, *definition),
            InferredState::UnsavedComplex(complex) => type_of_complex(i_s, complex, None),
            InferredState::UnsavedSpecific(specific) => match specific {
                Specific::None => Type::new(&DbType::None),
                Specific::Any | Specific::Cycle => Type::new(&DbType::Any),
                _ => unreachable!("{specific:?}"),
            },
            InferredState::UnsavedFileReference(file_index) => {
                Type::owned(DbType::Module(*file_index))
            }
            InferredState::BoundMethod {
                instance,
                mro_index,
                func_link,
            } => {
                // TODO this is potentially not needed, a class could lazily be fetched with a
                // closure
                let (instance, class) = Self::load_bound_method_instance(i_s, instance, *mro_index);
                load_bound_method(i_s, &instance, class, *mro_index, *func_link).as_type(i_s)
            }
            InferredState::Unknown => Type::new(&DbType::Any),
        }
    }

    pub fn class_as_db_type(&self, i_s: &InferenceState<'db, '_>) -> DbType {
        self.as_type(i_s).into_db_type()
    }

    pub fn format(&self, i_s: &InferenceState<'db, '_>, format_data: &FormatData) -> Box<str> {
        self.as_type(i_s).format(format_data)
    }

    pub fn format_short(&self, i_s: &InferenceState<'db, '_>) -> Box<str> {
        self.format(i_s, &FormatData::new_short(i_s.db))
    }

    fn load_bound_method_instance(
        i_s: &InferenceState<'db, '_>,
        instance: &'slf BoundMethodInstance,
        mro_index: MroIndex,
    ) -> (Instance<'slf>, Class<'slf>) {
        let instance = Instance::new(
            match instance {
                BoundMethodInstance::Reference(def) => NodeRef::from_link(i_s.db, *def)
                    .into_saved_inferred()
                    .saved_as_type(i_s)
                    .unwrap()
                    .expect_borrowed_class(i_s.db),
                BoundMethodInstance::Complex(ComplexPoint::TypeInstance(DbType::Class(
                    link,
                    generics,
                ))) => Class::from_db_type(i_s.db, *link, generics),
                _ => unreachable!(),
            },
            None,
        );

        let class_t = instance
            .class
            .mro(i_s.db)
            .nth(mro_index.0 as usize)
            .unwrap()
            .1;
        // Mro classes are never owned, because they are saved on classes.
        let TypeOrClass::Class(class) = class_t else {
            unreachable!()
        };
        (instance, class)
    }
    pub fn maybe_type_var_like(&self, i_s: &InferenceState<'db, '_>) -> Option<TypeVarLike> {
        if let InferredState::Saved(definition) = self.state {
            let node_ref = NodeRef::from_link(i_s.db, definition);
            if let Some(ComplexPoint::TypeVarLike(t)) = node_ref.complex() {
                return Some(t.clone());
            }
        }
        None
    }

    pub fn maybe_new_type(&self, i_s: &InferenceState) -> Option<Rc<NewType>> {
        if let InferredState::Saved(definition) = self.state {
            let node_ref = NodeRef::from_link(i_s.db, definition);
            if let Some(ComplexPoint::NewTypeDefinition(n)) = node_ref.complex() {
                return Some(n.clone());
            }
        }
        None
    }

    pub fn maybe_named_tuple_definition(&self, i_s: &InferenceState) -> Option<DbType> {
        if let InferredState::Saved(definition) = self.state {
            let node_ref = NodeRef::from_link(i_s.db, definition);
            if let Some(ComplexPoint::NamedTupleDefinition(n)) = node_ref.complex() {
                return Some(n.as_ref().clone());
            }
        }
        None
    }

    pub fn resolve_untyped_function_return(self, i_s: &InferenceState<'db, '_>) -> Self {
        if let InferredState::Saved(definition) = self.state {
            let definition = NodeRef::from_link(i_s.db, definition);
            let point = definition.point();
            if point.type_() == PointType::Specific && point.specific() == Specific::Closure {
                todo!()
            }
        }
        if let Some(class) = i_s.current_class() {
            self.resolve_class_type_vars(i_s, class)
        } else {
            self
        }
    }

    pub fn resolve_class_type_vars(self, i_s: &InferenceState<'db, '_>, class: &Class) -> Self {
        if matches!(class.generics, Generics::Self_ { .. }) {
            return self;
        }
        if let InferredState::Saved(link) = self.state {
            let definition = NodeRef::from_link(i_s.db, link);
            if let Some(specific) = definition.point().maybe_specific() {
                match specific {
                    Specific::Closure => {
                        todo!()
                    }
                    Specific::Param | Specific::SelfParam => {
                        todo!("might not even happen - remove")
                        //return i_s.infer_param(&definition);
                    }
                    Specific::AnnotationOrTypeCommentWithTypeVars => {
                        let file = i_s.db.loaded_python_file(link.file);
                        let t = file
                            .inference(i_s)
                            .use_db_type_of_annotation_or_type_comment(definition.node_index);
                        let d = replace_class_type_vars(i_s, t, class);
                        return Inferred::from_type(d);
                    }
                    _ => (),
                }
            }
        }
        self
    }

    pub fn expect_class_or_simple_generic(&self, i_s: &InferenceState<'db, '_>) -> Type<'db> {
        let mut generics = ClassGenerics::None;
        if let InferredState::Saved(link) = &self.state {
            let definition = NodeRef::from_link(i_s.db, *link);
            let point = definition.point();
            if point.type_() == PointType::Specific {
                generics = Self::expect_class_generics(definition, point);
            }
        }
        let link = self.get_class_link(i_s);
        Type::owned(DbType::Class(link, generics))
    }

    fn get_class_link(&self, i_s: &InferenceState) -> PointLink {
        let InferredState::Saved(link) = &self.state else {
            unreachable!()
        };
        let node_ref = NodeRef::from_link(i_s.db, *link);
        let point = node_ref.point();
        match point.type_() {
            PointType::Complex => {
                let complex = node_ref.file.complex_points.get(point.complex_index());
                let ComplexPoint::Class(c) = complex else {
                    unreachable!();
                };
                *link
            }
            PointType::Specific => match point.specific() {
                Specific::SimpleGeneric => {
                    let inferred = node_ref
                        .file
                        .inference(i_s)
                        .infer_primary_or_atom(node_ref.as_primary().first());
                    inferred.get_class_link(i_s)
                }
                _ => unreachable!(),
            },
            _ => unreachable!(),
        }
    }

    fn maybe_complex_point(&'slf self, db: &'db Database) -> Option<&'slf ComplexPoint> {
        match &self.state {
            InferredState::Saved(definition) => {
                let definition = NodeRef::from_link(db, *definition);
                Some(
                    definition
                        .file
                        .complex_points
                        .get(definition.point().complex_index()),
                )
            }
            InferredState::UnsavedComplex(t) => Some(t),
            _ => None,
        }
    }

    pub fn maybe_literal(
        &'slf self,
        db: &'db Database,
    ) -> UnionValue<DbLiteral, impl Iterator<Item = DbLiteral> + 'slf> {
        if let InferredState::Saved(link) = self.state {
            let node_ref = NodeRef::from_link(db, link);
            if let Some(Specific::IntLiteral) = node_ref.point().maybe_specific() {
                return UnionValue::Single(DbLiteral {
                    kind: LiteralKind::Int(node_ref.expect_int().parse().unwrap()),
                    implicit: true,
                });
            }
        }
        if let Some(ComplexPoint::TypeInstance(t)) = self.maybe_complex_point(db) {
            match t {
                DbType::Union(t) => match t.iter().next() {
                    Some(DbType::Literal(_)) => {
                        UnionValue::Multiple(t.iter().map_while(|t| match t {
                            DbType::Literal(literal) => Some(*literal),
                            _ => None,
                        }))
                    }
                    _ => UnionValue::Any,
                },
                DbType::Literal(literal) => UnionValue::Single(*literal),
                _ => UnionValue::Any,
            }
        } else {
            UnionValue::Any
        }
    }

    pub fn maybe_str(&self, db: &'db Database) -> Option<PythonString<'db>> {
        if let InferredState::Saved(link) = self.state {
            let node_ref = NodeRef::from_link(db, link);
            if let Some(Specific::String) = node_ref.point().maybe_specific() {
                return node_ref.infer_str();
            }
        }
        None
    }

    pub fn save_redirect(self, i_s: &InferenceState, file: &PythonFile, index: NodeIndex) -> Self {
        self.maybe_save_redirect(i_s, file, index, false)
    }

    pub fn maybe_save_redirect(
        self,
        i_s: &InferenceState,
        file: &PythonFile,
        index: NodeIndex,
        ignore_if_already_saved: bool,
    ) -> Self {
        // TODO this locality should be calculated in a more correct way
        let p = file.points.get(index);
        if p.calculated() && p.maybe_specific() == Some(Specific::Cycle) {
            return Self::new_saved(file, index, file.points.get(index));
        }
        let point = match self.state {
            InferredState::Saved(definition) => {
                // Overwriting strings needs to be possible, because of string annotations
                if p.calculated()
                    && !matches!(
                        p.maybe_specific(),
                        Some(Specific::String | Specific::Cycle | Specific::LazyInferredFunction)
                    )
                {
                    if ignore_if_already_saved {
                        return self;
                    }
                    let node_ref = NodeRef::new(file, index);
                    todo!(
                        "{self:?} >>>> {p:?} {index:?}, {}, {:?}",
                        file.tree.short_debug_of_index(index),
                        node_ref.complex()
                    );
                }
                file.points.set(
                    index,
                    Point::new_redirect(definition.file, definition.node_index, Locality::Todo),
                );
                return self;
            }
            InferredState::UnsavedComplex(complex) => {
                file.complex_points
                    .insert(&file.points, index, complex.clone(), Locality::Todo);
                return Self::new_saved(file, index, file.points.get(index));
            }
            InferredState::UnsavedSpecific(mut specific) => {
                if specific == Specific::Cycle {
                    let r = NodeRef::new(file, index);
                    r.add_typing_issue(
                        i_s,
                        IssueType::CyclicDefinition {
                            name: Box::from(r.as_code()),
                        },
                    );
                    specific = Specific::Any;
                }
                Point::new_simple_specific(specific, Locality::Todo)
            }
            InferredState::UnsavedFileReference(file_index) => {
                Point::new_file_reference(file_index, Locality::Todo)
            }
            InferredState::BoundMethod {
                instance,
                mro_index,
                func_link,
            } => {
                let (instance, class) = Self::load_bound_method_instance(i_s, &instance, mro_index);
                let bound_method = load_bound_method(i_s, &instance, class, mro_index, func_link);
                file.complex_points.insert(
                    &file.points,
                    index,
                    ComplexPoint::TypeInstance(bound_method.as_type(i_s).into_db_type()),
                    Locality::Todo,
                );
                return Self::new_saved(file, index, file.points.get(index));
            }
            InferredState::Unknown => Point::new_unknown(file.file_index(), Locality::Todo),
        };
        file.points.set(index, point);
        Self::new_saved(file, index, point)
    }

    pub fn save_if_unsaved(
        self,
        i_s: &InferenceState,
        file: &'db PythonFile,
        index: NodeIndex,
    ) -> Self {
        if matches!(self.state, InferredState::Saved(_)) {
            self
        } else {
            self.save_redirect(i_s, file, index)
        }
    }

    pub fn init_as_function<'a>(
        &self,
        db: &'db Database,
        class: Option<Class<'a>>,
    ) -> Option<FunctionOrOverload<'a>>
    where
        'db: 'a,
    {
        match &self.state {
            InferredState::Saved(link) => {
                let definition = NodeRef::from_link(db, *link);
                if let Some(Specific::Function) = definition.point().maybe_specific() {
                    return Some(FunctionOrOverload::Function(Function::new(
                        definition, class,
                    )));
                }
                match definition.complex() {
                    Some(ComplexPoint::FunctionOverload(overload)) => {
                        return Some(FunctionOrOverload::Overload(OverloadedFunction::new(
                            definition, overload, class,
                        )));
                    }
                    Some(ComplexPoint::TypeInstance(t)) => {
                        if let DbType::Callable(c) = t {
                            return Some(FunctionOrOverload::Callable(c.clone()));
                        }
                    }
                    _ => (),
                }
            }
            InferredState::UnsavedComplex(ComplexPoint::TypeInstance(t)) => {
                if let DbType::Callable(c) = t {
                    return Some(FunctionOrOverload::Callable(c.clone()));
                }
            }
            _ => (),
        }
        None
    }

    #[inline]
    pub fn gather_types_union(
        callable: impl FnOnce(&mut dyn FnMut(&InferenceState<'db, '_>, Self)),
    ) -> Self {
        let mut result: Option<Self> = None;
        let r = &mut result;
        callable(&mut |i_s, inferred| {
            *r = Some(match r.take() {
                Some(i) => i.types_union(i_s, inferred, &mut ResultContext::Unknown),
                None => inferred,
            });
        });
        result.unwrap_or_else(|| todo!())
    }

    pub fn types_union(
        self,
        i_s: &InferenceState<'db, '_>,
        other: Self,
        result_context: &mut ResultContext,
    ) -> Self {
        if result_context.expects_union(i_s) || self.is_union(i_s.db) || other.is_union(i_s.db) {
            self.union(i_s, other)
        } else {
            let second = other.as_type(i_s);
            let t = self.as_type(i_s).common_base_type(i_s, &second);
            Inferred::from_type(t)
        }
    }

    pub fn union(self, i_s: &InferenceState, other: Self) -> Self {
        if self.state == other.state {
            self
        } else {
            Inferred::from_type(
                self.as_type(i_s)
                    .union(i_s.db, other.as_type(i_s))
                    .into_db_type(),
            )
        }
    }

    #[inline]
    pub fn gather_union(i_s: &InferenceState, callable: impl FnOnce(&mut dyn FnMut(Self))) -> Self {
        let mut result: Option<Self> = None;
        let r = &mut result;
        callable(&mut |inferred| {
            *r = Some(match r.take() {
                Some(i) => i.union(i_s, inferred),
                None => inferred,
            });
        });
        result.unwrap_or_else(|| todo!())
    }

    pub fn bind_instance_descriptors(
        self,
        i_s: &InferenceState<'db, '_>,
        instance: &Instance,
        func_class: Class,
        get_inferred: impl Fn(&InferenceState<'db, '_>) -> Inferred,
        from: Option<NodeRef>,
        mro_index: MroIndex,
    ) -> Option<Self> {
        match &self.state {
            InferredState::Saved(definition) => {
                let node_ref = NodeRef::from_link(i_s.db, *definition);
                let point = node_ref.point();
                match point.type_() {
                    PointType::Specific => match point.specific() {
                        Specific::Function => {
                            let func = prepare_func(i_s, *definition, func_class);
                            return if let Some(first_type) = func.first_param_annotation_type(i_s) {
                                if let Some(t) =
                                    create_signature_without_self(i_s, func, instance, &first_type)
                                {
                                    Some(Self::new_unsaved_complex(ComplexPoint::TypeInstance(t)))
                                } else {
                                    if let Some(from) = from {
                                        let t = IssueType::InvalidSelfArgument {
                                            argument_type: instance.class.format_short(i_s.db),
                                            function_name: Box::from(func.name()),
                                            callable: func.as_type(i_s).format_short(i_s.db),
                                        };
                                        from.add_typing_issue(i_s, t);
                                        Some(Self::new_any())
                                    } else {
                                        // In case there is no node ref, we do not want to just
                                        // ignore the type error and we basically say that the
                                        // attribute does not even exist.
                                        None
                                    }
                                }
                            } else {
                                Some(Self::new_bound_method(
                                    get_inferred(i_s).as_bound_method_instance(i_s),
                                    mro_index,
                                    *definition,
                                ))
                            };
                        }
                        Specific::ClassMethod => {
                            let result =
                                infer_class_method(i_s, instance.class, func_class, *definition);
                            if result.is_none() {
                                if let Some(from) = from {
                                    let func = prepare_func(i_s, *definition, func_class);
                                    let t = IssueType::InvalidClassMethodFirstArgument {
                                        argument_type: instance
                                            .class
                                            .as_type(i_s)
                                            .format_short(i_s.db),
                                        function_name: Box::from(func.name()),
                                        callable: func.as_type(i_s).format_short(i_s.db),
                                    };
                                    from.add_typing_issue(i_s, t);
                                    return Some(Self::new_any());
                                } else {
                                    todo!()
                                }
                            }
                            return result;
                        }
                        Specific::Property => todo!(),
                        _ => (),
                    },
                    PointType::Complex => {
                        let file = i_s.db.loaded_python_file(definition.file);
                        match file.complex_points.get(point.complex_index()) {
                            ComplexPoint::FunctionOverload(o) => {
                                let has_self_arguments = o.functions.iter().any(|func_link| {
                                    let func = prepare_func(i_s, *func_link, func_class);
                                    func.first_param_annotation_type(i_s).is_some()
                                });
                                if has_self_arguments {
                                    let results: Vec<_> = o
                                        .functions
                                        .iter()
                                        .filter_map(|func_link| {
                                            let node_ref = NodeRef::from_link(i_s.db, *func_link);
                                            let func = Function::new(node_ref, Some(func_class));
                                            if let Some(t) = func.first_param_annotation_type(i_s) {
                                                create_signature_without_self(
                                                    i_s, func, instance, &t,
                                                )
                                            } else {
                                                Some(
                                                    func.as_db_type(
                                                        i_s,
                                                        FirstParamProperties::Skip,
                                                    ),
                                                )
                                            }
                                        })
                                        .collect();
                                    if results.len() == 1 {
                                        return Some(Inferred::from_type(
                                            results.into_iter().next().unwrap(),
                                        ));
                                    } else if results.len() != o.functions.len() {
                                        todo!()
                                    }
                                }
                                return Some(Self::new_bound_method(
                                    get_inferred(i_s).as_bound_method_instance(i_s),
                                    mro_index,
                                    *definition,
                                ));
                            }
                            ComplexPoint::TypeInstance(t) => match t {
                                DbType::Callable(c) => {
                                    // TODO should use create_signature_without_self!
                                    return Some(Self::new_bound_method(
                                        get_inferred(i_s).as_bound_method_instance(i_s),
                                        mro_index,
                                        *definition,
                                    ));
                                }
                                DbType::Class(link, generics) => {
                                    let inst = use_instance_with_ref(
                                        NodeRef::from_link(i_s.db, *link),
                                        Generics::from_class_generics(i_s.db, generics),
                                        Some(&self),
                                    );
                                    if let Some(inf) =
                                        inst.lookup(i_s, from, "__get__").into_maybe_inferred()
                                    {
                                        let from = from.unwrap_or_else(|| todo!());
                                        let class_as_inferred = instance.class.as_inferred(i_s);
                                        let instance = get_inferred(i_s);
                                        return Some(inf.execute(
                                            i_s,
                                            &CombinedArguments::new(
                                                &KnownArguments::new(&instance, from),
                                                &KnownArguments::new(&class_as_inferred, from),
                                            ),
                                        ));
                                    }
                                }
                                _ => (),
                            },
                            ComplexPoint::Class(cls_storage) => {
                                let class = Class::new(
                                    node_ref,
                                    cls_storage,
                                    Generics::NotDefinedYet,
                                    None,
                                );
                                debug!("TODO class descriptors");
                            }
                            _ => (),
                        }
                    }
                    _ => (),
                }
            }
            InferredState::UnsavedComplex(complex) => {
                if let ComplexPoint::TypeInstance(t) = complex {
                    debug!("TODO type instances");
                }
            }
            InferredState::UnsavedSpecific(specific) => todo!(),
            InferredState::UnsavedFileReference(file_index) => todo!(),
            InferredState::BoundMethod { .. } => todo!(),
            InferredState::Unknown => (),
        }
        Some(self)
    }

    pub fn bind_class_descriptors(
        self,
        i_s: &InferenceState<'db, '_>,
        class: &Class,
        attribute_class: Class, // The (sub-)class in which an attribute is defined
        from: Option<NodeRef>,
        apply_descriptor: bool,
    ) -> Option<Self> {
        match &self.state {
            InferredState::Saved(definition) => {
                let node_ref = NodeRef::from_link(i_s.db, *definition);
                let point = node_ref.point();
                match point.type_() {
                    PointType::Specific => match point.specific() {
                        Specific::Function => {
                            let func = Function::new(node_ref, Some(attribute_class));
                            let t =
                                func.as_db_type(i_s, FirstParamProperties::MethodAccessedOnClass);
                            return Some(Inferred::from_type(t));
                        }
                        Specific::ClassMethod => {
                            let result =
                                infer_class_method(i_s, *class, attribute_class, *definition);
                            if result.is_none() {
                                if let Some(from) = from {
                                    let func = prepare_func(i_s, *definition, attribute_class);
                                    let t = IssueType::InvalidSelfArgument {
                                        argument_type: class.as_type(i_s).format_short(i_s.db),
                                        function_name: Box::from(func.name()),
                                        callable: func.as_type(i_s).format_short(i_s.db),
                                    };
                                    from.add_typing_issue(i_s, t);
                                    return Some(Self::new_any());
                                } else {
                                    todo!()
                                }
                            }
                            return result;
                        }
                        Specific::Property => todo!(),
                        Specific::AnnotationOrTypeCommentWithTypeVars => {
                            if let Some(from) = from {
                                from.add_typing_issue(i_s, IssueType::AmbigousClassVariableAccess);
                            } else {
                                return None;
                            }
                            let file = i_s.db.loaded_python_file(definition.file);
                            let t = file
                                .inference(i_s)
                                .use_db_type_of_annotation_or_type_comment(definition.node_index);
                            let t = replace_class_type_vars(i_s, t, class);
                            return Some(Inferred::from_type(t));
                        }
                        _ => (),
                    },
                    PointType::Complex => {
                        let file = i_s.db.loaded_python_file(definition.file);
                        match file.complex_points.get(point.complex_index()) {
                            ComplexPoint::FunctionOverload(o) => {
                                debug!("TODO function overload class descriptor");
                            }
                            ComplexPoint::TypeInstance(t) => match t {
                                DbType::Callable(c) => {
                                    todo!()
                                }
                                DbType::Class(link, generics) if apply_descriptor => {
                                    let inst = use_instance_with_ref(
                                        NodeRef::from_link(i_s.db, *link),
                                        Generics::from_class_generics(i_s.db, generics),
                                        Some(&self),
                                    );
                                    if let Some(inf) =
                                        inst.lookup(i_s, from, "__get__").into_maybe_inferred()
                                    {
                                        let from = from.unwrap_or_else(|| todo!());
                                        let class_as_inferred = class.as_inferred(i_s);
                                        return Some(inf.execute(
                                            i_s,
                                            &CombinedArguments::new(
                                                &KnownArguments::new(&Inferred::new_none(), from),
                                                &KnownArguments::new(&class_as_inferred, from),
                                            ),
                                        ));
                                    }
                                }
                                _ => (),
                            },
                            _ => (),
                        }
                    }
                    _ => (),
                }
            }
            InferredState::UnsavedComplex(complex) => (),
            InferredState::UnsavedSpecific(specific) => todo!(),
            InferredState::UnsavedFileReference(file_index) => todo!(),
            InferredState::BoundMethod { .. } => todo!(),
            InferredState::Unknown => (),
        }
        Some(self)
    }

    pub fn debug_info(&self, i_s: &InferenceState<'db, '_>) -> String {
        let details = match &self.state {
            InferredState::Saved(definition) => {
                let definition = NodeRef::from_link(i_s.db, *definition);
                format!(
                    "{} (complex?: {:?})",
                    definition.file.file_path(i_s.db),
                    definition.complex(),
                )
            }
            _ => "".to_owned(),
        };
        format!(
            "description = {}\ndebug = {self:?}\ndetails = {details}",
            self.format_short(i_s),
        )
    }

    fn as_bound_method_instance(&self, i_s: &InferenceState<'db, '_>) -> BoundMethodInstance {
        match &self.state {
            InferredState::Saved(definition) => BoundMethodInstance::Reference(*definition),
            InferredState::UnsavedComplex(complex) => BoundMethodInstance::Complex(complex.clone()),
            InferredState::UnsavedSpecific(specific) => todo!(),
            InferredState::UnsavedFileReference(file_index) => unreachable!(),
            InferredState::BoundMethod { .. } => unreachable!(),
            InferredState::Unknown => unreachable!(),
        }
    }

    fn expect_class_generics(definition: NodeRef<'db>, point: Point) -> ClassGenerics {
        debug_assert_eq!(point.type_(), PointType::Specific);
        debug_assert_eq!(point.specific(), Specific::SimpleGeneric);
        let PrimaryContent::GetItem(slice_type) = definition.as_primary().second() else {
            unreachable!();
        };
        match slice_type {
            ASTSliceType::NamedExpression(named) => ClassGenerics::ExpressionWithClassType(
                PointLink::new(definition.file_index(), named.expression().index()),
            ),
            ASTSliceType::Slice(_) => unreachable!(),
            ASTSliceType::Slices(slices) => ClassGenerics::SlicesWithClassTypes(PointLink::new(
                definition.file_index(),
                slices.index(),
            )),
        }
    }

    pub fn is_union(&self, db: &'db Database) -> bool {
        let check_complex_point = |c: &_| match c {
            ComplexPoint::TypeInstance(t) => Type::new(t).is_union(),
            _ => false,
        };
        match &self.state {
            InferredState::Saved(reference) => {
                let node_ref = NodeRef::from_link(db, *reference);
                let point = node_ref.point();
                if point.type_() == PointType::Specific {
                    if point.specific() == Specific::AnnotationOrTypeCommentWithTypeVars {
                        // TODO the node_ref may not be an annotation.
                        use_cached_annotation_type(db, node_ref.file, node_ref.as_annotation())
                            .is_union()
                    } else {
                        false
                    }
                } else {
                    node_ref.complex().is_some_and(|c| check_complex_point(c))
                }
            }
            InferredState::UnsavedComplex(c) => check_complex_point(c),
            _ => false,
        }
    }

    #[inline]
    pub fn run_after_lookup_on_each_union_member(
        &self,
        i_s: &InferenceState<'db, '_>,
        from: Option<NodeRef>,
        name: &str,
        callable: &mut impl FnMut(&Type, LookupResult),
    ) {
        self.as_type(i_s).run_after_lookup_on_each_union_member(
            i_s,
            Some(self),
            from,
            name,
            callable,
        )
    }

    pub fn lookup_with_error(
        &self,
        i_s: &InferenceState<'db, '_>,
        node_ref: NodeRef,
        name: &str,
    ) -> LookupResult {
        let base_type = self.as_type(i_s);
        base_type.lookup_with_error(i_s, node_ref, name, &|t| {
            add_attribute_error(i_s, node_ref, &base_type, t, name)
        })
    }

    pub fn lookup_and_execute(
        &self,
        i_s: &InferenceState<'db, '_>,
        from: NodeRef,
        name: &str,
        args: &dyn Arguments<'db>,
        on_lookup_error: OnLookupError,
    ) -> Self {
        self.lookup_and_execute_with_details(
            i_s,
            from,
            name,
            args,
            on_lookup_error,
            OnTypeError::new(&on_argument_type_error),
        )
    }

    pub fn lookup_and_execute_with_details(
        &self,
        i_s: &InferenceState<'db, '_>,
        from: NodeRef,
        name: &str,
        args: &dyn Arguments<'db>,
        on_lookup_error: OnLookupError,
        on_type_error: OnTypeError<'db, '_>,
    ) -> Self {
        let mut result: Option<Inferred> = None;
        self.run_after_lookup_on_each_union_member(
            i_s,
            Some(from),
            name,
            &mut |_, lookup_result| {
                if matches!(lookup_result, LookupResult::None) {
                    on_lookup_error(&self.as_type(i_s));
                }
                let inf = lookup_result.into_inferred().execute_with_details(
                    i_s,
                    args,
                    &mut ResultContext::Unknown,
                    on_type_error,
                );
                result = if let Some(r) = result.take() {
                    Some(r.union(i_s, inf))
                } else {
                    Some(inf)
                }
            },
        );
        result.unwrap_or_else(|| todo!())
    }

    pub fn execute(&self, i_s: &InferenceState<'db, '_>, args: &dyn Arguments<'db>) -> Self {
        self.execute_with_details(
            i_s,
            args,
            &mut ResultContext::Unknown,
            OnTypeError::new(&on_argument_type_error),
        )
    }

    pub fn execute_with_details(
        &self,
        i_s: &InferenceState<'db, '_>,
        args: &dyn Arguments<'db>,
        result_context: &mut ResultContext,
        on_type_error: OnTypeError<'db, '_>,
    ) -> Self {
        match &self.state {
            InferredState::Saved(link) => {
                let node_ref = NodeRef::from_link(i_s.db, *link);
                let point = node_ref.point();
                match point.type_() {
                    PointType::Specific => {
                        let specific = point.specific();
                        match specific {
                            Specific::Function => {
                                return Function::new(node_ref, None).execute(
                                    i_s,
                                    args,
                                    result_context,
                                    on_type_error,
                                )
                            }
                            Specific::TypingTypeVarClass => {
                                return TypeVarClass().execute(
                                    i_s,
                                    args,
                                    result_context,
                                    on_type_error,
                                )
                            }
                            Specific::TypingTypeVarTupleClass => {
                                return TypeVarTupleClass().execute(
                                    i_s,
                                    args,
                                    result_context,
                                    on_type_error,
                                )
                            }
                            Specific::TypingParamSpecClass => {
                                return ParamSpecClass().execute(
                                    i_s,
                                    args,
                                    result_context,
                                    on_type_error,
                                )
                            }
                            Specific::TypingProtocol
                            | Specific::TypingGeneric
                            | Specific::TypingTuple
                            | Specific::TypingUnion
                            | Specific::TypingOptional
                            | Specific::TypingType
                            | Specific::TypingLiteral
                            | Specific::TypingAnnotated
                            | Specific::TypingNamedTuple
                            | Specific::CollectionsNamedTuple
                            | Specific::TypingCallable => {
                                return TypingClass::new(specific).execute(
                                    i_s,
                                    args,
                                    result_context,
                                    on_type_error,
                                )
                            }
                            Specific::TypingCast => {
                                return TypingCast().execute(
                                    i_s,
                                    args,
                                    result_context,
                                    on_type_error,
                                )
                            }
                            Specific::RevealTypeFunction => {
                                return RevealTypeFunction().execute(
                                    i_s,
                                    args,
                                    result_context,
                                    on_type_error,
                                )
                            }
                            Specific::TypingNewType => {
                                return NewTypeClass().execute(
                                    i_s,
                                    args,
                                    result_context,
                                    on_type_error,
                                )
                            }
                            Specific::TypingAny => {
                                args.as_node_ref()
                                    .add_typing_issue(i_s, IssueType::AnyNotCallable);
                                args.iter().calculate_diagnostics(i_s);
                                return Inferred::new_any();
                            }
                            Specific::MypyExtensionsArg
                            | Specific::MypyExtensionsDefaultArg
                            | Specific::MypyExtensionsNamedArg
                            | Specific::MypyExtensionsDefaultNamedArg
                            | Specific::MypyExtensionsVarArg
                            | Specific::MypyExtensionsKwArg => {
                                let func = i_s.db.python_state.mypy_extensions_arg_func(specific);
                                return func.execute(i_s, args, result_context, on_type_error);
                            }
                            _ => (),
                        }
                    }
                    PointType::Complex => {
                        match node_ref.file.complex_points.get(point.complex_index()) {
                            ComplexPoint::FunctionOverload(overload) => {
                                return OverloadedFunction::new(node_ref, &overload, None).execute(
                                    i_s,
                                    args,
                                    result_context,
                                    on_type_error,
                                );
                            }
                            ComplexPoint::Class(cls) => {
                                return Class::new(node_ref, cls, Generics::NotDefinedYet, None)
                                    .execute(i_s, args, result_context, on_type_error)
                            }
                            ComplexPoint::TypeAlias(alias) => {
                                if alias.is_class() {
                                    return Inferred::from_type(
                                        alias.as_db_type_and_set_type_vars_any(i_s.db),
                                    );
                                }
                                args.as_node_ref().add_typing_issue(
                                    i_s,
                                    IssueType::NotCallable {
                                        type_: Box::from("\"<typing special form>\""),
                                    },
                                );
                                return Inferred::new_any();
                            }
                            ComplexPoint::NewTypeDefinition(new_type) => {
                                let mut iterator = args.iter();
                                if let Some(first_arg) = iterator.next() {
                                    let t = Type::new(new_type.type_(i_s));
                                    let inf = first_arg.infer(i_s, &mut ResultContext::Known(&t));
                                    t.error_if_not_matches(
                                        i_s,
                                        &inf,
                                        |i_s: &InferenceState<'db, '_>, t1, t2| {
                                            (on_type_error.callback)(
                                                i_s,
                                                None,
                                                &|_| {
                                                    Some(
                                                        format!(
                                                            " to \"{}\"",
                                                            new_type.name(i_s.db)
                                                        )
                                                        .into(),
                                                    )
                                                },
                                                &first_arg,
                                                t1,
                                                t2,
                                            );
                                            args.as_node_ref().to_db_lifetime(i_s.db)
                                        },
                                    );
                                } else {
                                    args.as_node_ref().add_typing_issue(
                                        i_s,
                                        IssueType::TooFewArguments(
                                            format!(" for \"{}\"", new_type.name(i_s.db)).into(),
                                        ),
                                    );
                                }
                                if iterator.next().is_some() {
                                    args.as_node_ref().add_typing_issue(
                                        i_s,
                                        IssueType::TooManyArguments(
                                            format!(" for \"{}\"", new_type.name(i_s.db)).into(),
                                        ),
                                    );
                                }
                                return Self::from_type(DbType::NewType(new_type.clone()));
                            }
                            _ => (),
                        }
                    }
                    PointType::FileReference => {
                        args.as_node_ref().add_typing_issue(
                            i_s,
                            IssueType::NotCallable {
                                type_: Box::from("Module"),
                            },
                        );
                        return Inferred::new_unknown();
                    }
                    _ => (),
                }
            }
            InferredState::BoundMethod {
                instance,
                mro_index,
                func_link,
            } => {
                let (instance, class) = Self::load_bound_method_instance(i_s, instance, *mro_index);
                return load_bound_method(i_s, &instance, class, *mro_index, *func_link).execute(
                    i_s,
                    args,
                    result_context,
                    on_type_error,
                );
            }
            _ => (),
        }
        self.as_type(i_s)
            .execute(i_s, Some(self), args, result_context, on_type_error)
    }

    pub fn get_item(
        &self,
        i_s: &InferenceState,
        slice_type: &SliceType,
        result_context: &mut ResultContext,
    ) -> Inferred {
        if let InferredState::Saved(link) = self.state {
            let node_ref = NodeRef::from_link(i_s.db, link);
            match node_ref.point().maybe_specific() {
                Some(
                    specific @ (Specific::TypingProtocol
                    | Specific::TypingGeneric
                    | Specific::TypingTuple
                    | Specific::TypingUnion
                    | Specific::TypingOptional
                    | Specific::TypingType
                    | Specific::TypingLiteral
                    | Specific::TypingAnnotated
                    | Specific::TypingNamedTuple
                    | Specific::CollectionsNamedTuple
                    | Specific::TypingCallable),
                ) => {
                    return slice_type
                        .file
                        .inference(i_s)
                        .compute_type_application_on_typing_class(
                            specific,
                            *slice_type,
                            matches!(result_context, ResultContext::AssignmentNewDefinition),
                        )
                }
                Some(Specific::TypingClassVar) => {
                    return match slice_type.unpack() {
                        SliceTypeContent::Simple(simple) => {
                            // TODO if it is a (), it's am empty tuple
                            simple.infer(i_s, &mut ResultContext::Unknown)
                        }
                        SliceTypeContent::Slice(x) => {
                            todo!()
                        }
                        SliceTypeContent::Slices(x) => {
                            todo!()
                        }
                    };
                }
                _ => {
                    let node_ref = NodeRef::from_link(i_s.db, link);
                    match node_ref.complex() {
                        Some(ComplexPoint::TypeAlias(ta)) => {
                            return slice_type
                                .file
                                .inference(i_s)
                                .compute_type_application_on_alias(
                                    ta,
                                    *slice_type,
                                    matches!(
                                        result_context,
                                        ResultContext::AssignmentNewDefinition
                                    ),
                                )
                        }
                        Some(ComplexPoint::Class(c)) => {
                            let class = Class::new(node_ref, c, Generics::NotDefinedYet, None);
                            return class.get_item(i_s, slice_type, result_context);
                        }
                        _ => (),
                    }
                }
            }
        }
        self.as_type(i_s)
            .get_item(i_s, Some(self), slice_type, result_context)
    }

    pub fn save_and_iter(
        self,
        i_s: &InferenceState<'db, '_>,
        from: NodeRef,
    ) -> IteratorContent<'db> {
        let inferred = self.save_if_unsaved(i_s, from.file, from.node_index);
        let t = inferred.saved_as_type(i_s).unwrap();
        t.iter_on_borrowed(i_s, from)
    }
}

fn load_bound_method<'db: 'a, 'a, 'b>(
    i_s: &InferenceState<'db, '_>,
    instance: &'b Instance<'a>,
    class: Class<'a>,
    mro_index: MroIndex,
    func_link: PointLink,
) -> BoundMethod<'a, 'b> {
    let reference = NodeRef::from_link(i_s.db, func_link);
    match reference.complex() {
        Some(ComplexPoint::FunctionOverload(overload)) => {
            let func = OverloadedFunction::new(reference, overload, Some(class));
            BoundMethod::new(instance, mro_index, BoundMethodFunction::Overload(func))
        }
        Some(ComplexPoint::TypeInstance(t)) => match t {
            DbType::Callable(c) => BoundMethod::new(
                instance,
                mro_index,
                BoundMethodFunction::Callable(Callable::new(t, c)),
            ),
            _ => unreachable!("{t:?}"),
        },
        None => {
            let func = Function::new(reference, Some(class));
            BoundMethod::new(instance, mro_index, BoundMethodFunction::Function(func))
        }
        _ => unreachable!(),
    }
}

fn resolve_specific(i_s: &InferenceState, specific: Specific) -> Type<'static> {
    // TODO this should be using python_state.str_node_ref etc.
    load_builtin_type_from_str(
        i_s,
        match specific {
            Specific::String | Specific::StringLiteral => "str",
            Specific::IntLiteral | Specific::Int => "int",
            Specific::Float => "float",
            Specific::BoolLiteral | Specific::Bool => "bool",
            Specific::BytesLiteral | Specific::Bytes => "bytes",
            Specific::Complex => "complex",
            Specific::Ellipsis => "ellipsis", // TODO this should not even be public
            actual => unreachable!("{actual:?}"),
        },
    )
}

fn load_builtin_type_from_str(i_s: &InferenceState, name: &'static str) -> Type<'static> {
    let builtins = i_s.db.python_state.builtins();
    let node_index = builtins.lookup_global(name).unwrap().node_index - 1;
    // TODO this is slow and ugly, please make sure to do this different
    builtins
        .inference(i_s)
        .infer_name_definition(NodeRef::new(builtins, node_index).as_name_def());

    let v = builtins.points.get(node_index);
    debug_assert_eq!(v.type_(), PointType::Redirect);
    debug_assert_eq!(v.file_index(), builtins.file_index());
    Type::owned(DbType::Class(
        PointLink::new(builtins.file_index(), v.node_index()),
        ClassGenerics::None,
    ))
}

fn use_instance_with_ref<'a>(
    class_reference: NodeRef<'a>,
    generics: Generics<'a>,
    instance_reference: Option<&'a Inferred>,
) -> Instance<'a> {
    let class = Class::from_position(class_reference, generics, None);
    Instance::new(class, instance_reference)
}

fn prepare_func<'db, 'class>(
    i_s: &InferenceState<'db, '_>,
    definition: PointLink,
    func_class: Class<'class>,
) -> Function<'db, 'class> {
    let node_ref = NodeRef::from_link(i_s.db, definition);
    let func = Function::new(node_ref, Some(func_class));
    func.type_vars(i_s); // Cache annotations
    func
}

pub enum UnionValue<T, I: Iterator<Item = T>> {
    Single(T),
    Multiple(I),
    Any,
}

fn infer_class_method(
    i_s: &InferenceState,
    mut class: Class,
    func_class: Class,
    definition: PointLink,
) -> Option<Inferred> {
    let mut func_class = func_class;
    let class_generics_not_defined_yet =
        matches!(class.generics, Generics::NotDefinedYet) && class.type_vars(i_s).is_some();
    if class_generics_not_defined_yet {
        // Check why this is necessary by following class_generics_not_defined_yet.
        let self_generics = Generics::Self_ {
            type_var_likes: class.type_vars(i_s),
            class_definition: class.node_ref.as_link(),
        };
        class.generics = self_generics;
        func_class.generics = self_generics;
    }
    let func = prepare_func(i_s, definition, func_class);
    if let Some(first_type) = func.first_param_annotation_type(i_s) {
        let type_vars = func.type_vars(i_s);
        let mut matcher = Matcher::new_function_matcher(Some(&class), func, type_vars);
        let instance_t = class.as_type(i_s);
        // TODO It is questionable that we do not match Self here
        if !first_type
            .is_super_type_of(i_s, &mut matcher, &instance_t)
            .bool()
        {
            return None;
        }
    }
    let t = func.classmethod_as_db_type(i_s, &class, class_generics_not_defined_yet);
    Some(Inferred::from_type(t))
}

fn type_of_complex<'db: 'x, 'x>(
    i_s: &InferenceState<'db, '_>,
    complex: &'x ComplexPoint,
    definition: Option<NodeRef<'x>>,
) -> Type<'x> {
    match complex {
        ComplexPoint::Class(cls_storage) => {
            // This can only ever happen for saved definitions, therefore we can unwrap.
            Type::owned(DbType::Type(Rc::new(DbType::Class(
                definition.unwrap().as_link(),
                ClassGenerics::NotDefinedYet,
            ))))
        }
        ComplexPoint::FunctionOverload(overload) => {
            let overload = OverloadedFunction::new(definition.unwrap(), overload, None);
            overload.as_type(i_s)
        }
        ComplexPoint::TypeInstance(t) => Type::new(t),
        // TODO is this type correct with the Any?
        ComplexPoint::TypeAlias(alias) => Type::owned(DbType::Type(Rc::new(
            alias.as_db_type_and_set_type_vars_any(i_s.db),
        ))),
        ComplexPoint::TypeVarLike(t) => match t {
            TypeVarLike::TypeVar(_) => i_s.db.python_state.type_var_type(),
            TypeVarLike::TypeVarTuple(_) => todo!(),
            TypeVarLike::ParamSpec(_) => todo!(),
        },
        ComplexPoint::NewTypeDefinition(n) => {
            Type::owned(DbType::Type(Rc::new(DbType::NewType(n.clone()))))
        }
        ComplexPoint::NamedTupleDefinition(n) => Type::owned(DbType::Type(n.clone())),
        _ => {
            unreachable!("Classes are handled earlier {complex:?}")
        }
    }
}

fn saved_as_type<'db>(i_s: &InferenceState<'db, '_>, definition: PointLink) -> Type<'db> {
    let definition = NodeRef::from_link(i_s.db, definition);
    let point = definition.point();
    match point.type_() {
        PointType::Specific => {
            let specific = point.specific();
            match specific {
                Specific::Any | Specific::Cycle => Type::new(&DbType::Any),
                Specific::IntLiteral => Type::owned(DbType::Literal(DbLiteral {
                    kind: LiteralKind::Int(definition.expect_int().parse().unwrap()),
                    implicit: true,
                })),
                Specific::StringLiteral => Type::owned(DbType::Literal(DbLiteral {
                    kind: LiteralKind::String(definition.as_link()),
                    implicit: true,
                })),
                Specific::BoolLiteral => Type::owned(DbType::Literal(DbLiteral {
                    kind: LiteralKind::Bool(definition.as_code() == "True"),
                    implicit: true,
                })),
                Specific::BytesLiteral => Type::owned(DbType::Literal(DbLiteral {
                    kind: LiteralKind::Bytes(definition.as_link()),
                    implicit: true,
                })),
                Specific::Function => Function::new(definition, None).as_type(i_s),
                Specific::ClassMethod => todo!(),
                Specific::Property => todo!(),
                Specific::AnnotationOrTypeCommentClassInstance
                | Specific::AnnotationOrTypeCommentWithTypeVars => definition
                    .file
                    .inference(i_s)
                    .use_cached_annotation_or_type_comment_type_internal(
                        definition.node_index,
                        definition.add_to_node_index(2).as_expression(),
                    ),
                Specific::SelfParam => Type::new(&DbType::Self_),
                Specific::TypingTypeVarClass => todo!(),
                Specific::TypingTypeVarTupleClass => todo!(),
                Specific::TypingParamSpecClass => todo!(),
                Specific::TypingType => Type::new(&i_s.db.python_state.type_of_any),
                Specific::TypingTuple => Type::new(&i_s.db.python_state.type_of_arbitrary_tuple),
                Specific::CollectionsNamedTuple => todo!(),
                Specific::TypingProtocol
                | Specific::TypingGeneric
                | Specific::TypingUnion
                | Specific::TypingOptional
                | Specific::TypingLiteral
                | Specific::TypingAnnotated
                | Specific::TypingNamedTuple
                | Specific::TypingCallable => todo!(),
                Specific::TypingCast => todo!(),
                Specific::TypingClassVar => todo!(),
                Specific::RevealTypeFunction => todo!(),
                Specific::None => Type::new(&DbType::None),
                Specific::TypingNewType => todo!(),
                Specific::TypingAny => todo!(),
                Specific::MypyExtensionsArg
                | Specific::MypyExtensionsDefaultArg
                | Specific::MypyExtensionsNamedArg
                | Specific::MypyExtensionsDefaultNamedArg
                | Specific::MypyExtensionsVarArg
                | Specific::MypyExtensionsKwArg => {
                    let func = i_s.db.python_state.mypy_extensions_arg_func(specific);
                    todo!()
                }
                _ => resolve_specific(i_s, specific),
            }
        }
        PointType::Complex => {
            let complex = definition.file.complex_points.get(point.complex_index());
            type_of_complex(i_s, complex, Some(definition))
        }
        PointType::Unknown => Type::new(&DbType::Any),
        PointType::FileReference => Type::owned(DbType::Module(point.file_index())),
        x => unreachable!("{x:?}"),
    }
}

#[derive(Debug, Clone, PartialEq)]
enum BoundMethodInstance {
    Reference(PointLink),
    Complex(ComplexPoint),
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_sizes() {
        use super::*;
        use std::mem::size_of;
        assert_eq!(size_of::<Inferred>(), 48);
    }
}

pub fn add_attribute_error<'db>(
    i_s: &InferenceState<'db, '_>,
    node_ref: NodeRef,
    full_type: &Type,
    t: &Type,
    name: &str,
) {
    let object = if matches!(t.as_ref(), DbType::Module(_)) {
        Box::from("Module")
    } else {
        format!("{:?}", t.format_short(i_s.db)).into()
    };
    let name = Box::from(name);
    if let DbType::TypeVar(usage) = full_type.as_ref() {
        if let Some(bound) = &usage.type_var.bound {
            if Type::new(bound).is_union() {
                let bound = bound.format_short(i_s.db);
                let type_var_name = usage.type_var.name(i_s.db);
                node_ref.add_typing_issue(
                    i_s,
                    IssueType::UnionAttributeErrorOfUpperBound(format!(
                        r#"Item {object} of the upper bound "{bound}" of type variable "{type_var_name}" has no attribute "{name}""#
                    ).into())
                );
                return;
            }
        }
    }
    node_ref.add_typing_issue(
        i_s,
        match full_type.is_union() {
            false => IssueType::AttributeError { object, name },
            true => IssueType::UnionAttributeError {
                object,
                union: full_type.format_short(i_s.db),
                name,
            },
        },
    );
}
