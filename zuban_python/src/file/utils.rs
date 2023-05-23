use std::rc::Rc;

use parsa_python_ast::{
    Dict, DictElement, Int, List, StarLikeExpression, StarLikeExpressionIterator,
};

use crate::arguments::Argument;
use crate::database::{
    ClassGenerics, DbType, GenericItem, GenericsList, Literal, LiteralKind, LiteralValue,
};
use crate::diagnostics::IssueType;
use crate::file::{Inference, PythonFile};
use crate::getitem::Simple;
use crate::inference_state::InferenceState;
use crate::inferred::UnionValue;
use crate::matching::{Matcher, MismatchReason, ResultContext, Type};
use crate::node_ref::NodeRef;
use crate::type_helpers::Class;
use crate::{debug, Inferred};

impl<'db> Inference<'db, '_, '_> {
    pub fn create_list_or_set_generics(
        &mut self,
        elements: StarLikeExpressionIterator,
    ) -> GenericItem {
        let mut result = DbType::Never;
        for child in elements {
            let mut t = match child {
                StarLikeExpression::NamedExpression(named_expr) => self
                    .infer_named_expression(named_expr)
                    .class_as_db_type(self.i_s),
                StarLikeExpression::StarNamedExpression(e) => self
                    .infer_expression_part(e.expression_part(), &mut ResultContext::Unknown)
                    .save_and_iter(self.i_s, NodeRef::new(self.file, e.index()))
                    .infer_all(self.i_s)
                    .class_as_db_type(self.i_s),
                StarLikeExpression::Expression(_) | StarLikeExpression::StarExpression(_) => {
                    unreachable!()
                }
            };
            // Just because we defined a literal somewhere, we should probably not infer that.
            if let DbType::Literal(l) = t {
                t = self.i_s.db.python_state.literal_db_type(l.kind);
            }
            result.union_in_place(t);
        }
        GenericItem::TypeArgument(result)
    }

    pub fn infer_list_literal_from_context(
        &mut self,
        list: List,
        result_context: &mut ResultContext,
    ) -> Option<Inferred> {
        let file = self.file;
        result_context
            .with_type_if_exists(self.i_s, |i_s: &InferenceState<'db, '_>, type_, matcher| {
                let mut found = None;
                let mut fallback = None;
                type_.on_any_class(i_s, matcher, &mut |i_s, matcher, list_cls| {
                    if list_cls.node_ref == i_s.db.python_state.list_node_ref() {
                        let type_vars = list_cls.type_vars(i_s).unwrap();
                        let generic_t = list_cls
                            .generics()
                            .nth(i_s.db, &type_vars[0], 0)
                            .expect_type_argument();
                        found = check_list_with_context(i_s, matcher, generic_t, file, list);
                        if found.is_none() {
                            // As a fallback if there were only errors or no items at all, just use
                            // the given and expected result context as a type.
                            fallback = Some(
                                Type::owned(list_cls.as_db_type(i_s.db))
                                    .replace_type_var_likes(self.i_s.db, &mut |tv| {
                                        tv.as_type_var_like().as_any_generic_item()
                                    }),
                            );
                            // TODO we need something like this for testUnpackingUnionOfListsInFunction
                            //self.file.reset_non_name_cache_between(list.node_index_range());
                            false
                        } else {
                            true
                        }
                    } else {
                        false
                    }
                });
                // `found` might still be empty, because we matched Any.
                found.or(fallback).map(|found| Inferred::from_type(found))
            })
            .flatten()
    }

    pub fn create_dict_generics(
        &mut self,
        dict: Dict,
        result_context: &mut ResultContext,
    ) -> GenericsList {
        let mut keys: Option<DbType> = None;
        let mut values: Option<DbType> = None;
        for child in dict.iter_elements() {
            match child {
                DictElement::KeyValue(key_value) => {
                    let key_t = self
                        .infer_expression(key_value.key())
                        .class_as_db_type(self.i_s);
                    match keys.as_mut() {
                        Some(keys) => keys.union_in_place(key_t),
                        None => keys = Some(key_t),
                    };
                    let value_t = self
                        .infer_expression(key_value.value())
                        .class_as_db_type(self.i_s);
                    match values.as_mut() {
                        Some(values) => values.union_in_place(value_t),
                        None => values = Some(value_t),
                    };
                }
                DictElement::DictStarred(_) => {
                    todo!()
                }
            }
        }
        let keys = keys.unwrap_or(DbType::Any);
        let values = values.unwrap_or(DbType::Any);
        debug!(
            "Calculated generics for {}: dict[{}, {}]",
            dict.short_debug(),
            keys.format_short(self.i_s.db),
            values.format_short(self.i_s.db),
        );
        GenericsList::new_generics(Rc::new([
            GenericItem::TypeArgument(keys),
            GenericItem::TypeArgument(values),
        ]))
    }

    pub fn parse_int(&mut self, int: Int, result_context: &mut ResultContext) -> Option<i64> {
        if result_context.is_literal_context(self.i_s) {
            let result = int.parse();
            if result.is_none() {
                todo!("Add diagnostic");
            }
            result
        } else {
            None
        }
    }
}

fn check_list_with_context<'db>(
    i_s: &InferenceState<'db, '_>,
    matcher: &mut Matcher,
    generic_t: Type,
    file: &PythonFile,
    list: List,
) -> Option<DbType> {
    let mut new_result_context = ResultContext::Known(&generic_t);

    // Since it's a list, now check all the entries if they match the given
    // result generic;
    let mut found: Option<DbType> = None;
    for (item, element) in list.unpack().enumerate() {
        let mut check_item = |i_s: &InferenceState<'db, '_>, inferred: Inferred, index| {
            let m = generic_t.error_if_not_matches_with_matcher(
                i_s,
                matcher,
                &inferred,
                Some(
                    |i_s: &InferenceState<'db, '_>, got, expected, _: &MismatchReason| {
                        let node_ref = NodeRef::new(file, index).to_db_lifetime(i_s.db);
                        node_ref.add_typing_issue(
                            i_s,
                            IssueType::ListItemMismatch {
                                item,
                                got,
                                expected,
                            },
                        );
                        node_ref
                    },
                ),
            );
            if m.bool() {
                let resembling = inferred
                    .as_type(i_s)
                    .try_to_resemble_context(i_s, matcher, &generic_t);
                if let Some(found) = &mut found {
                    found.union_in_place(resembling)
                } else {
                    found = Some(resembling);
                }
            } else if i_s.is_checking_overload() {
                let t = inferred.as_type(i_s).into_db_type();
                if let Some(found) = &mut found {
                    found.union_in_place(t)
                } else {
                    found = Some(t);
                }
            }
        };
        let mut inference = file.inference(i_s);
        match element {
            StarLikeExpression::NamedExpression(e) => {
                let inferred =
                    inference.infer_named_expression_with_context(e, &mut new_result_context);
                check_item(i_s, inferred, e.index())
            }
            StarLikeExpression::StarNamedExpression(e) => {
                todo!("{e:?}")
            }
            StarLikeExpression::Expression(e) => unreachable!(),
            StarLikeExpression::StarExpression(e) => unreachable!(),
        };
    }
    found.map(|inner| {
        DbType::Class(
            i_s.db.python_state.list_node_ref().as_link(),
            ClassGenerics::List(GenericsList::new_generics(Rc::new([
                GenericItem::TypeArgument(inner),
            ]))),
        )
    })
}

pub fn on_argument_type_error(
    i_s: &InferenceState,
    class: Option<&Class>,
    error_text: &dyn Fn(&str) -> Option<Box<str>>,
    arg: &Argument,
    t1: Box<str>,
    t2: Box<str>,
) {
    arg.as_node_ref().add_typing_issue(
        i_s,
        IssueType::ArgumentIssue(
            format!(
                "Argument {}{} has incompatible type \"{t1}\"; expected \"{t2}\"",
                arg.human_readable_index(),
                error_text(" to ").as_deref().unwrap_or(""),
            )
            .into(),
        ),
    )
}

pub fn infer_index(
    i_s: &InferenceState,
    simple: Simple,
    callable: impl Fn(usize) -> Option<Inferred>,
) -> Inferred {
    let infer = |i_s: &InferenceState, literal: Literal| {
        if !matches!(literal.kind, LiteralKind::Int(_)) {
            return None;
        }
        let LiteralValue::Int(i) = literal.value(i_s.db) else {
            unreachable!();
        };
        let index = usize::try_from(i).ok().unwrap_or_else(|| todo!());
        callable(index)
    };
    match simple
        .infer(i_s, &mut ResultContext::ExpectLiteral)
        .maybe_literal(i_s.db)
    {
        UnionValue::Single(literal) => infer(i_s, literal),
        UnionValue::Multiple(mut literals) => {
            literals
                .next()
                .and_then(|l| infer(i_s, l))
                .and_then(|mut inferred| {
                    for literal in literals {
                        if let Some(new_inf) = infer(i_s, literal) {
                            inferred = inferred.union(i_s, new_inf);
                        } else {
                            return None;
                        }
                    }
                    Some(inferred)
                })
        }
        UnionValue::Any => todo!(),
    }
    .unwrap_or_else(Inferred::new_unknown)
}
