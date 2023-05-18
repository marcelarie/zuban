use std::borrow::Cow;

use parsa_python_ast::{Expression, SliceContent, SliceIterator, Slices};

use super::{FormatData, Generic, Match, Matcher, Type};
use crate::database::{
    ClassGenerics, Database, DbType, GenericItem, GenericsList, ParamSpecArgument, ParamSpecUsage,
    PointLink, TypeVarLike, TypeVarLikeUsage, TypeVarLikes, Variance,
};
use crate::debug;
use crate::file::{use_cached_simple_generic_type, File, PythonFile};
use crate::inference_state::InferenceState;
use crate::node_ref::NodeRef;

macro_rules! replace_class_vars {
    ($db:expr, $g:ident, $type_var_generics:ident) => {
        match $type_var_generics {
            None | Some(Generics::None | Generics::NotDefinedYet) => Generic::new($g),
            Some(type_var_generics) => Generic::owned($g.replace_type_var_likes(
                $db,
                &mut |t| type_var_generics.nth_usage($db, &t).into_generic_item($db),
                &mut || DbType::Self_,
            )),
        }
    };
}

#[derive(Debug, Clone, Copy)]
pub enum Generics<'a> {
    ExpressionWithClassType(&'a PythonFile, Expression<'a>),
    SlicesWithClassTypes(&'a PythonFile, Slices<'a>),
    List(&'a GenericsList, Option<&'a Generics<'a>>),
    Self_ {
        class_definition: PointLink,
        type_var_likes: Option<&'a TypeVarLikes>,
    },
    None,
    NotDefinedYet,
}

impl<'a> Generics<'a> {
    pub fn new_list(list: &'a GenericsList) -> Self {
        Self::List(list, None)
    }

    pub fn from_class_generics(db: &'a Database, g: &'a ClassGenerics) -> Self {
        match g {
            ClassGenerics::List(l) => Self::List(l, None),
            ClassGenerics::None => Generics::None,
            ClassGenerics::ExpressionWithClassType(link) => {
                let node_ref = NodeRef::from_link(db, *link);
                Self::ExpressionWithClassType(node_ref.file, node_ref.as_expression())
            }
            ClassGenerics::SlicesWithClassTypes(link) => {
                let node_ref = NodeRef::from_link(db, *link);
                Self::SlicesWithClassTypes(node_ref.file, node_ref.as_slices())
            }
        }
    }

    pub fn nth_usage<'db: 'a>(&self, db: &'db Database, usage: &TypeVarLikeUsage) -> Generic<'a> {
        self.nth(db, &usage.as_type_var_like(), usage.index().as_usize())
    }

    pub fn nth_param_spec_usage<'db: 'a>(
        &self,
        db: &'db Database,
        usage: &ParamSpecUsage,
    ) -> Cow<'a, ParamSpecArgument> {
        let generic = self.nth_usage(db, &TypeVarLikeUsage::ParamSpec(Cow::Borrowed(usage)));
        if let Generic::ParamSpecArgument(p) = generic {
            p
        } else {
            unreachable!()
        }
    }

    pub fn nth_type_argument<'db: 'a>(
        &self,
        db: &'db Database,
        type_var_like: &TypeVarLike,
        n: usize,
    ) -> Type<'a> {
        let generic = self.nth(db, type_var_like, n);
        if let Generic::TypeArgument(p) = generic {
            p
        } else {
            unreachable!()
        }
    }

    pub fn nth<'db: 'a>(
        &self,
        db: &'db Database,
        type_var_like: &TypeVarLike,
        n: usize,
    ) -> Generic<'a> {
        match self {
            Self::ExpressionWithClassType(file, expr) => {
                if n == 0 {
                    Generic::TypeArgument(use_cached_simple_generic_type(db, file, *expr))
                } else {
                    debug!(
                        "Generic expr {:?} has one item, but {:?} was requested",
                        expr.short_debug(),
                        n,
                    );
                    todo!()
                }
            }
            Self::SlicesWithClassTypes(file, slices) => Generic::TypeArgument(
                slices
                    .iter()
                    .nth(n)
                    .map(|slice_content| match slice_content {
                        SliceContent::NamedExpression(n) => {
                            use_cached_simple_generic_type(db, file, n.expression())
                        }
                        SliceContent::Slice(s) => todo!(),
                    })
                    .unwrap_or_else(|| todo!()),
            ),
            Self::List(list, type_var_generics) => {
                if let Some(g) = list.nth(n.into()) {
                    replace_class_vars!(db, g, type_var_generics)
                } else {
                    debug!(
                        "Generic list {} given, but item {:?} was requested",
                        self.format(&FormatData::new_short(db), None),
                        n,
                    );
                    todo!()
                }
            }
            Self::NotDefinedYet => Generic::owned(type_var_like.as_any_generic_item()),
            Self::Self_ {
                class_definition, ..
            } => Generic::owned(
                type_var_like
                    .as_type_var_like_usage(n.into(), *class_definition)
                    .into_generic_item(),
            ),
            Self::None => unreachable!("No generics given, but {:?} was requested", n),
        }
    }

    pub fn iter<'x>(&'x self, db: &'x Database) -> GenericsIterator<'x> {
        let item = match self {
            Self::ExpressionWithClassType(file, expr) => {
                GenericsIteratorItem::SimpleGenericExpression(file, *expr)
            }
            Self::SlicesWithClassTypes(file, slices) => {
                GenericsIteratorItem::SimpleGenericSliceIterator(file, slices.iter())
            }
            Self::List(l, t) => GenericsIteratorItem::GenericsList(l.iter(), *t),
            Self::Self_ {
                class_definition,
                type_var_likes,
            } => GenericsIteratorItem::TypeVarLikeIterator {
                iterator: type_var_likes.unwrap().iter().enumerate(),
                definition: *class_definition,
            },
            Self::None | Self::NotDefinedYet => GenericsIteratorItem::None,
        };
        GenericsIterator::new(db, item)
    }

    pub fn as_generics_list(
        &self,
        db: &Database,
        type_vars: Option<&TypeVarLikes>,
    ) -> ClassGenerics {
        match type_vars {
            Some(type_vars) => match self {
                Self::NotDefinedYet => ClassGenerics::List(GenericsList::new_generics(
                    type_vars.iter().map(|t| t.as_any_generic_item()).collect(),
                )),
                Self::ExpressionWithClassType(file, expr) => {
                    ClassGenerics::ExpressionWithClassType(PointLink::new(
                        file.file_index(),
                        expr.index(),
                    ))
                }
                Self::SlicesWithClassTypes(file, slices) => ClassGenerics::SlicesWithClassTypes(
                    PointLink::new(file.file_index(), slices.index()),
                ),
                _ => ClassGenerics::List(GenericsList::new_generics(
                    self.iter(db).map(|g| g.into_generic_item(db)).collect(),
                )),
            },
            None => ClassGenerics::None,
        }
    }

    pub fn format(&self, format_data: &FormatData, expected: Option<usize>) -> String {
        // Returns something like [str] or [List[int], Set[Any]]
        let mut strings: Vec<_> = self
            .iter(format_data.db)
            .map(|g| g.format(format_data))
            .collect();
        if let Some(expected) = expected {
            if strings.len() != expected {
                // TODO this should probably never happen and we should assert this
            }
            strings.resize_with(expected, || Box::from("Any"))
        }
        format!("[{}]", strings.join(", "))
    }

    pub fn matches(
        &self,
        i_s: &InferenceState,
        matcher: &mut Matcher,
        value_generics: Self,
        type_vars: &TypeVarLikes,
    ) -> Match {
        let value_generics = value_generics.iter(i_s.db);
        let mut matches = Match::new_true();
        for ((t1, t2), tv) in self.iter(i_s.db).zip(value_generics).zip(type_vars.iter()) {
            let v = match tv {
                TypeVarLike::TypeVar(t) => t.variance,
                TypeVarLike::TypeVarTuple(_) => Variance::Invariant,
                TypeVarLike::ParamSpec(_) => Variance::Invariant,
            };
            matches &= t1.matches(i_s, matcher, &t2, v);
        }
        matches
    }

    pub fn overlaps(
        self,
        i_s: &InferenceState,
        other_generics: Self,
        type_vars: Option<&TypeVarLikes>,
    ) -> bool {
        let other_generics = other_generics.iter(i_s.db);
        let mut matches = true;
        let mut type_var_iterator = type_vars.map(|t| t.iter());
        for (t1, t2) in self.iter(i_s.db).zip(other_generics) {
            if let Some(t) = type_var_iterator.as_mut().and_then(|t| t.next()) {
                // TODO ?
            } else {
            };
            matches &= t1.overlaps(i_s, t2);
        }
        matches
    }
}

pub struct GenericsIterator<'a> {
    db: &'a Database,
    ended: bool,
    item: GenericsIteratorItem<'a>,
}

impl<'a> GenericsIterator<'a> {
    fn new(db: &'a Database, item: GenericsIteratorItem<'a>) -> Self {
        Self {
            db,
            ended: false,
            item,
        }
    }
}

enum GenericsIteratorItem<'a> {
    SimpleGenericSliceIterator(&'a PythonFile, SliceIterator<'a>),
    GenericsList(std::slice::Iter<'a, GenericItem>, Option<&'a Generics<'a>>),
    SimpleGenericExpression(&'a PythonFile, Expression<'a>),
    TypeVarLikeIterator {
        iterator: std::iter::Enumerate<std::slice::Iter<'a, TypeVarLike>>,
        definition: PointLink,
    },
    None,
}

impl<'a> Iterator for GenericsIterator<'a> {
    type Item = Generic<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.item {
            GenericsIteratorItem::SimpleGenericExpression(file, expr) => {
                if self.ended {
                    return None;
                }
                self.ended = true;
                Some(Generic::TypeArgument(use_cached_simple_generic_type(
                    self.db, file, *expr,
                )))
            }
            GenericsIteratorItem::SimpleGenericSliceIterator(file, iter) => {
                if let Some(SliceContent::NamedExpression(s)) = iter.next() {
                    Some(Generic::TypeArgument(use_cached_simple_generic_type(
                        self.db,
                        file,
                        s.expression(),
                    )))
                } else {
                    None
                }
            }
            GenericsIteratorItem::GenericsList(iterator, type_var_generics) => iterator
                .next()
                .map(|g| replace_class_vars!(self.db, g, type_var_generics)),
            GenericsIteratorItem::TypeVarLikeIterator {
                iterator,
                definition,
            } => iterator.next().map(|(i, t)| {
                Generic::owned(
                    t.as_type_var_like_usage(i.into(), *definition)
                        .into_generic_item(),
                )
            }),
            GenericsIteratorItem::None => None,
        }
    }
}
