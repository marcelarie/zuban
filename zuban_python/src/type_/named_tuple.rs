use std::{cell::OnceCell, rc::Rc};

use parsa_python_cst::{AtomContent, CodeIndex, StarLikeExpression};

use super::{
    tuple::lookup_tuple_magic_methods, AnyCause, CallableContent, CallableParam, CallableParams,
    DbString, FormatStyle, FunctionKind, Literal, LiteralKind, ParamType, StringSlice, Tuple, Type,
};
use crate::{
    arguments::{ArgIterator, ArgKind, Args, KeywordArg},
    database::{ComplexPoint, Database, FileIndex, PointLink},
    diagnostics::IssueKind,
    file::{File, TypeComputation, TypeComputationOrigin, TypeVarCallbackReturn},
    getitem::SliceType,
    inference_state::InferenceState,
    inferred::{AttributeKind, Inferred},
    matching::{
        AvoidRecursionFor, FormatData, Generics, IteratorContent, LookupKind, LookupResult,
        OnTypeError, ResultContext,
    },
    new_class,
    node_ref::NodeRef,
    type_helpers::{start_namedtuple_params, LookupDetails, Module},
    utils::join_with_commas,
};

#[derive(Debug, PartialEq, Clone)]
pub struct NamedTuple {
    pub name: StringSlice,
    pub __new__: Rc<CallableContent>,
    tuple: OnceCell<Rc<Tuple>>,
}

impl NamedTuple {
    pub fn new(name: StringSlice, __new__: CallableContent) -> Self {
        Self {
            name,
            __new__: Rc::new(__new__),
            tuple: OnceCell::new(),
        }
    }

    pub fn clone_with_new_init_class(&self, name: StringSlice) -> Rc<NamedTuple> {
        let mut nt = self.clone();
        let mut callable = nt.__new__.as_ref().clone();
        callable.name = Some(DbString::StringSlice(name));
        nt.__new__ = Rc::new(callable);
        Rc::new(nt)
    }

    pub fn params(&self) -> &[CallableParam] {
        // Namedtuple callables contain a first param `Type[Self]` that we should skip.
        &self.__new__.expect_simple_params()[1..]
    }

    pub fn search_param(&self, db: &Database, search_name: &str) -> Option<&CallableParam> {
        self.params()
            .iter()
            .find(|p| p.name.as_ref().unwrap().as_str(db) == search_name)
    }

    pub fn name<'a>(&self, db: &'a Database) -> &'a str {
        self.name.as_str(db)
    }

    pub fn qualified_name(&self, db: &Database) -> String {
        let module = Module::from_file_index(db, self.name.file_index).qualified_name(db);
        format!("{module}.{}", self.name(db))
    }

    pub fn as_tuple(&self) -> Rc<Tuple> {
        self.tuple
            .get_or_init(|| {
                Tuple::new_fixed_length(
                    self.params()
                        .iter()
                        .map(|t| t.type_.expect_positional_type_as_ref().clone())
                        .collect(),
                )
            })
            .clone()
    }

    pub fn as_tuple_ref(&self) -> &Tuple {
        self.as_tuple();
        self.tuple.get().unwrap()
    }

    pub fn format_with_name(
        &self,
        format_data: &FormatData,
        name: &str,
        generics: Generics,
    ) -> Box<str> {
        if format_data.style != FormatStyle::MypyRevealType {
            return Box::from(name);
        }
        let params = self.params();
        // We need to check recursions here, because for class definitions of named tuples can
        // recurse with their attributes.
        let avoid = AvoidRecursionFor::NamedTuple(self.__new__.defined_at);
        if format_data.has_already_seen_recursive_type(avoid) {
            return Box::from("...");
        }
        let format_data = &format_data.with_seen_recursive_type(avoid);
        let types = match params.is_empty() {
            true => "()".into(),
            false => join_with_commas(params.iter().map(|p| {
                let t = p.type_.expect_positional_type_as_ref();
                match generics {
                    Generics::NotDefinedYet | Generics::None => t.format(format_data),
                    _ => t
                        .replace_type_var_likes_and_self(
                            format_data.db,
                            &mut |usage| {
                                generics
                                    .nth_usage(format_data.db, &usage)
                                    .into_generic_item(format_data.db)
                            },
                            &|| todo!(),
                        )
                        .format(format_data),
                }
                .into()
            })),
        };
        format!("tuple[{types}, fallback={name}]",).into()
    }

    pub fn iter(&self, i_s: &InferenceState, from: NodeRef) -> IteratorContent {
        self.as_tuple().iter(i_s, from)
    }

    pub fn get_item(
        &self,
        i_s: &InferenceState,
        slice_type: &SliceType,
        result_context: &mut ResultContext,
    ) -> Inferred {
        self.as_tuple().get_item(i_s, slice_type, result_context)
    }

    fn lookup_internal(
        &self,
        i_s: &InferenceState,
        name: &str,
        from_type: bool,
        as_self: Option<&dyn Fn() -> Type>,
    ) -> LookupDetails<'static> {
        let mut attr_kind = AttributeKind::Attribute;
        let type_ = match name {
            "__new__" => Type::Callable(self.__new__.clone()),
            "_replace" => Type::Callable({
                attr_kind = AttributeKind::DefMethod;
                let mut params = vec![];
                if from_type {
                    params.push(CallableParam::new_anonymous(ParamType::PositionalOnly(
                        as_self.map(|as_self| as_self()).unwrap_or(Type::Self_),
                    )));
                }
                for param in self.params() {
                    let mut new_param = param.clone();
                    new_param.has_default = true;
                    new_param.type_ =
                        ParamType::KeywordOnly(new_param.type_.expect_positional_type().clone());
                    params.push(new_param);
                }
                Rc::new(CallableContent {
                    name: Some(DbString::Static("_replace")),
                    class_name: Some(self.name),
                    defined_at: PointLink::new(FileIndex(0), 0),
                    kind: FunctionKind::Function {
                        had_first_self_or_class_annotation: true,
                    },
                    type_vars: i_s.db.python_state.empty_type_var_likes.clone(),
                    guard: None,
                    is_abstract: false,
                    params: CallableParams::Simple(params.into()),
                    return_type: as_self.map(|as_self| as_self()).unwrap_or(Type::Self_),
                })
            }),
            "_asdict" => Type::Callable({
                attr_kind = AttributeKind::DefMethod;
                let mut params = vec![];
                if from_type {
                    params.push(CallableParam::new_anonymous(ParamType::PositionalOnly(
                        as_self.map(|as_self| as_self()).unwrap_or(Type::Self_),
                    )));
                }
                Rc::new(CallableContent {
                    name: Some(DbString::Static("_as_dict")),
                    class_name: Some(self.name),
                    defined_at: PointLink::new(FileIndex(0), 0),
                    kind: FunctionKind::Function {
                        had_first_self_or_class_annotation: true,
                    },
                    type_vars: i_s.db.python_state.empty_type_var_likes.clone(),
                    guard: None,
                    is_abstract: false,
                    params: CallableParams::Simple(params.into()),
                    return_type: new_class!(
                        i_s.db.python_state.dict_node_ref().as_link(),
                        i_s.db.python_state.str_type(),
                        Type::Any(AnyCause::Explicit),
                    ),
                })
            }),
            "_make" => Type::Callable({
                attr_kind = AttributeKind::Classmethod;
                let mut params = vec![];
                if as_self.is_none() {
                    params.push(CallableParam::new_anonymous(ParamType::PositionalOnly(
                        i_s.db.python_state.type_of_self.clone(),
                    )));
                }
                params.push(CallableParam {
                    type_: ParamType::PositionalOrKeyword(new_class!(
                        i_s.db.python_state.iterable_link(),
                        Type::Any(AnyCause::Explicit),
                    )),
                    name: Some(DbString::Static("iterable")),
                    has_default: false,
                });
                Rc::new(CallableContent {
                    name: Some(DbString::Static("_make")),
                    class_name: Some(self.name),
                    defined_at: PointLink::new(FileIndex(0), 0),
                    kind: FunctionKind::Classmethod {
                        had_first_self_or_class_annotation: true,
                    },
                    type_vars: i_s.db.python_state.empty_type_var_likes.clone(),
                    guard: None,
                    is_abstract: false,
                    params: CallableParams::Simple(params.into()),
                    return_type: as_self.map(|as_self| as_self()).unwrap_or(Type::Self_),
                })
            }),
            "_fields" => Type::Tuple(Tuple::new_fixed_length(
                std::iter::repeat(i_s.db.python_state.str_type())
                    .take(self.params().len())
                    .collect(),
            )),
            "_field_defaults" => new_class!(
                i_s.db.python_state.dict_node_ref().as_link(),
                i_s.db.python_state.str_type(),
                Type::Any(AnyCause::Explicit),
            ),
            "_field_types" => new_class!(
                i_s.db.python_state.dict_node_ref().as_link(),
                i_s.db.python_state.str_type(),
                Type::Any(AnyCause::Explicit),
            ),
            "_source" => i_s.db.python_state.str_type(),
            "__mul__" | "__rmul__" | "__add__" => {
                return lookup_tuple_magic_methods(self.as_tuple(), name)
            }
            "__match_args__" if i_s.flags().python_version.at_least_3_dot(10) => {
                Type::Tuple(Tuple::new_fixed_length(
                    self.params()
                        .iter()
                        .map(|p| {
                            Type::Literal(Literal::new(LiteralKind::String(
                                p.name.as_ref().unwrap().clone(),
                            )))
                        })
                        .collect(),
                ))
            }
            _ => {
                if let Some(param) = self.search_param(i_s.db, name) {
                    param.type_.expect_positional_type_as_ref().clone()
                } else {
                    return LookupDetails::none();
                }
            }
        };
        LookupDetails::new(
            Type::Any(AnyCause::Internal), // TODO is this Any ok?
            LookupResult::UnknownName(Inferred::from_type(type_)),
            attr_kind,
        )
    }

    pub fn type_lookup(
        &self,
        i_s: &InferenceState,
        name: &str,
        as_self: Option<&dyn Fn() -> Type>,
    ) -> LookupDetails {
        // TODO use or_else like in lookups
        self.lookup_internal(i_s, name, true, as_self)
    }

    pub(crate) fn lookup<'a>(
        &self,
        i_s: &'a InferenceState,
        add_issue: &dyn Fn(IssueKind),
        name: &str,
        as_self: Option<&dyn Fn() -> Type>,
    ) -> LookupDetails<'a> {
        self.lookup_internal(i_s, name, false, as_self)
            .or_else(move || {
                i_s.db
                    .python_state
                    .typing_named_tuple_class()
                    .instance()
                    .lookup_with_details(i_s, add_issue, name, LookupKind::Normal)
            })
    }
}

pub(crate) fn execute_typing_named_tuple(i_s: &InferenceState, args: &dyn Args) -> Inferred {
    match new_typing_named_tuple(i_s, args, false) {
        Some(rc) => Inferred::new_unsaved_complex(ComplexPoint::NamedTupleDefinition(Rc::new(
            Type::NamedTuple(rc),
        ))),
        None => Inferred::new_any_from_error(),
    }
}

pub(crate) fn execute_collections_named_tuple<'db>(
    i_s: &InferenceState<'db, '_>,
    args: &dyn Args<'db>,
    result_context: &mut ResultContext,
    on_type_error: OnTypeError,
) -> Inferred {
    let func = i_s.db.python_state.collections_namedtuple_function();
    func.ensure_cached_func(i_s);
    func.execute(i_s, args, result_context, on_type_error);
    match new_collections_named_tuple(i_s, args) {
        Some(rc) => Inferred::new_unsaved_complex(ComplexPoint::NamedTupleDefinition(Rc::new(
            Type::NamedTuple(rc),
        ))),
        None => Inferred::new_any_from_error(),
    }
}

fn check_named_tuple_name<'x, 'y>(
    i_s: &InferenceState,
    executable_name: &'static str,
    args: &'y dyn Args<'x>,
) -> Option<(
    StringSlice,
    NodeRef<'y>,
    AtomContent<'y>,
    ArgIterator<'x, 'y>,
)> {
    let mut iterator = args.iter();
    let Some(first_arg) = iterator.next() else {
        todo!()
    };
    let ArgKind::Positional(pos) = first_arg.kind else {
        first_arg.add_issue(i_s, IssueKind::UnexpectedArgumentsTo { name: "namedtuple" });
        return None
    };
    let expr = pos.node_ref.as_named_expression().expression();
    let first = expr
        .maybe_single_string_literal()
        .map(|py_string| (pos.node_ref, py_string));
    let Some(mut string_slice) = StringSlice::from_string_in_expression(pos.node_ref.file_index(), expr) else {
        pos.node_ref.add_issue(i_s, IssueKind::NamedTupleExpectsStringLiteralAsFirstArg { name: executable_name });
        return None
    };
    let py_string = expr.maybe_single_string_literal()?;
    if let Some(name) = py_string.in_simple_assignment() {
        let should = name.as_code();
        let is = py_string.content();
        if should != is {
            pos.node_ref.add_issue(
                i_s,
                IssueKind::NamedTupleFirstArgumentMismatch {
                    should: should.into(),
                    is: is.into(),
                },
            );
            string_slice = StringSlice::from_name(pos.node_ref.file_index(), name.name());
        }
    }
    let Some(second_arg) = iterator.next() else {
        if executable_name != "namedtuple" {
            // For namedtuple this is already handled by type checking.
            args.add_issue(i_s, IssueKind::TooFewArguments(r#" for "NamedTuple()""#.into()));
        }
        return None
    };
    let ArgKind::Positional(second) = second_arg.kind else {
        todo!()
    };
    let Some(atom_content) = second.node_ref.as_named_expression().expression().maybe_unpacked_atom() else {
        todo!()
    };
    Some((string_slice, second.node_ref, atom_content, iterator))
}

pub(crate) fn new_typing_named_tuple(
    i_s: &InferenceState,
    args: &dyn Args,
    in_type_definition: bool,
) -> Option<Rc<NamedTuple>> {
    let Some((name, second_node_ref, atom_content, mut iterator)) = check_named_tuple_name(i_s, "NamedTuple", args) else {
        return None
    };
    if iterator.next().is_some() {
        args.add_issue(
            i_s,
            IssueKind::TooManyArguments(" for \"NamedTuple()\"".into()),
        );
        return None;
    }
    let list_iterator = match atom_content {
        AtomContent::List(list) => list.unpack(),
        AtomContent::Tuple(tup) => tup.iter(),
        _ => {
            second_node_ref.add_issue(
                i_s,
                IssueKind::InvalidSecondArgumentToNamedTuple { name: "NamedTuple" },
            );
            return None;
        }
    };
    let on_type_var = &mut |i_s: &InferenceState, _: &_, type_var_like, _| {
        i_s.find_parent_type_var(&type_var_like)
            .unwrap_or(TypeVarCallbackReturn::NotFound)
    };
    let mut inference = second_node_ref.file.inference(i_s);
    let mut comp = TypeComputation::new(
        &mut inference,
        second_node_ref.as_link(),
        on_type_var,
        TypeComputationOrigin::NamedTupleMember,
    );
    if let Some(params) = comp.compute_named_tuple_initializer(second_node_ref, list_iterator) {
        check_named_tuple_has_no_fields_with_underscore(i_s, "NamedTuple", args, &params);
        let type_var_likes = comp.into_type_vars(|_, _| ());
        if in_type_definition && !type_var_likes.is_empty() {
            args.add_issue(i_s, IssueKind::NamedTupleGenericInClassDefinition);
            return None;
        }
        let callable = CallableContent {
            name: Some(DbString::StringSlice(name)),
            class_name: None,
            defined_at: second_node_ref.as_link(),
            kind: FunctionKind::Function {
                had_first_self_or_class_annotation: true,
            },
            type_vars: type_var_likes,
            guard: None,
            is_abstract: false,
            params: CallableParams::Simple(Rc::from(params)),
            return_type: Type::Self_,
        };
        Some(Rc::new(NamedTuple::new(name, callable)))
    } else {
        None
    }
}

pub(crate) fn new_collections_named_tuple(
    i_s: &InferenceState,
    args: &dyn Args,
) -> Option<Rc<NamedTuple>> {
    let Some((name, second_node_ref, atom_content, _)) = check_named_tuple_name(i_s, "namedtuple", args) else {
        return None
    };
    let mut params = start_namedtuple_params(i_s.db);

    let mut add_param = |name| {
        params.push(CallableParam {
            type_: ParamType::PositionalOrKeyword(Type::Any(AnyCause::Todo)),
            name: Some(name),
            has_default: false,
        })
    };

    let mut add_from_iterator = |iterator| {
        for element in iterator {
            let StarLikeExpression::NamedExpression(ne) = element else {
            todo!()
        };
            let Some(string_slice) = StringSlice::from_string_in_expression(second_node_ref.file.file_index(), ne.expression()) else {
                NodeRef::new(second_node_ref.file, ne.index())
                    .add_issue(i_s, IssueKind::StringLiteralExpectedAsNamedTupleItem);
                continue
            };
            add_param(string_slice.into())
        }
    };
    match atom_content {
        AtomContent::List(list) => add_from_iterator(list.unpack()),
        AtomContent::Tuple(tup) => add_from_iterator(tup.iter()),
        AtomContent::Strings(s) => match s.maybe_single_string_literal() {
            Some(s) => {
                let (mut start, _) = s.content_start_and_end_in_literal();
                start += s.start();
                for part in s.content().split(&[',', ' ']) {
                    add_param(
                        StringSlice::new(
                            second_node_ref.file_index(),
                            start,
                            start + part.len() as CodeIndex,
                        )
                        .into(),
                    );
                    start += part.len() as CodeIndex + 1;
                }
            }
            _ => todo!(),
        },
        _ => {
            second_node_ref.add_issue(
                i_s,
                IssueKind::InvalidSecondArgumentToNamedTuple { name: "namedtuple" },
            );
            return None;
        }
    };
    check_named_tuple_has_no_fields_with_underscore(i_s, "namedtuple", args, &params);

    for arg in args.iter() {
        if let ArgKind::Keyword(KeywordArg {
            key: "defaults",
            expression,
            ..
        }) = arg.kind
        {
            let defaults_iterator = match expression.maybe_unpacked_atom() {
                Some(AtomContent::List(list)) => list.unpack(),
                Some(AtomContent::Tuple(tuple)) => tuple.iter(),
                _ => {
                    arg.add_issue(i_s, IssueKind::NamedTupleDefaultsShouldBeListOrTuple);
                    return None;
                }
            };
            let member_count = params.len() - 1;
            let defaults_count = defaults_iterator.count();
            let skip = if defaults_count > member_count {
                arg.add_issue(i_s, IssueKind::NamedTupleToManyDefaults);
                0
            } else {
                member_count - defaults_count
            };
            for param in params.iter_mut().skip(skip + 1) {
                param.has_default = true;
            }
            break;
        }
    }

    let callable = CallableContent {
        name: Some(DbString::StringSlice(name)),
        class_name: None,
        defined_at: second_node_ref.as_link(),
        kind: FunctionKind::Function {
            had_first_self_or_class_annotation: true,
        },
        type_vars: i_s.db.python_state.empty_type_var_likes.clone(),
        guard: None,
        is_abstract: false,
        params: CallableParams::Simple(Rc::from(params)),
        return_type: Type::Self_,
    };
    Some(Rc::new(NamedTuple::new(name, callable)))
}

fn check_named_tuple_has_no_fields_with_underscore(
    i_s: &InferenceState,
    name: &'static str,
    args: &dyn Args,
    params: &[CallableParam],
) {
    let field_names_with_underscore: Vec<_> = params
        .iter()
        .filter_map(|p| {
            p.name.as_ref().and_then(|name| {
                let name_str = name.as_str(i_s.db);
                name_str.starts_with('_').then_some(name_str)
            })
        })
        .collect();
    if !field_names_with_underscore.is_empty() {
        args.add_issue(
            i_s,
            IssueKind::NamedTupleNamesCannotStartWithUnderscore {
                name,
                field_names: field_names_with_underscore.join(", ").into(),
            },
        );
    }
}
