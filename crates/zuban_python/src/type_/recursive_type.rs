use std::{
    cell::OnceCell,
    hash::{Hash, Hasher},
};

use super::{GenericsList, Type};
use crate::{
    database::{Database, PointLink, TypeAlias},
    file::ClassNodeRef,
    matching::Generics,
    node_ref::NodeRef,
    type_helpers::Class,
};

#[derive(Clone, Eq)]
pub(crate) struct RecursiveType {
    pub link: PointLink,
    pub generics: Option<GenericsList>,
    calculated_type: OnceCell<Type>,
}

impl RecursiveType {
    pub fn new(link: PointLink, generics: Option<GenericsList>) -> Self {
        Self {
            link,
            generics,
            calculated_type: OnceCell::new(),
        }
    }

    pub(super) fn name<'x>(&'x self, db: &'x Database) -> &'x str {
        match self.origin(db) {
            RecursiveTypeOrigin::TypeAlias(alias) => alias.name(db),
            RecursiveTypeOrigin::Class(class) => class.name(),
        }
    }

    pub fn origin<'x>(&'x self, db: &'x Database) -> RecursiveTypeOrigin<'x> {
        let from = NodeRef::from_link(db, self.link);
        match from.maybe_alias() {
            Some(alias) => RecursiveTypeOrigin::TypeAlias(alias),
            None => RecursiveTypeOrigin::Class(Class::from_position(
                ClassNodeRef::from_node_ref(from),
                match &self.generics {
                    Some(list) => Generics::List(list, None),
                    None => Generics::None,
                },
                None,
            )),
        }
    }

    pub fn has_alias_origin(&self, db: &Database) -> bool {
        NodeRef::from_link(db, self.link).maybe_alias().is_some()
    }

    pub fn calculating(&self, db: &Database) -> bool {
        match self.origin(db) {
            RecursiveTypeOrigin::TypeAlias(alias) => alias.calculating(),
            RecursiveTypeOrigin::Class(class) => class.is_calculating_class_infos(),
        }
    }

    pub fn calculated_type<'db: 'slf, 'slf>(&'slf self, db: &'db Database) -> &'slf Type {
        match self.origin(db) {
            RecursiveTypeOrigin::TypeAlias(alias) => {
                if self.generics.is_none() {
                    alias.type_if_valid()
                } else {
                    self.calculated_type.get_or_init(|| {
                        alias
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
            RecursiveTypeOrigin::Class(class) => {
                self.calculated_type.get_or_init(|| class.as_type(db))
            }
        }
    }
}

impl std::cmp::PartialEq for RecursiveType {
    fn eq(&self, other: &Self) -> bool {
        self.link == other.link && self.generics == other.generics
    }
}

impl Hash for RecursiveType {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.link.hash(state);
        self.generics.hash(state);
    }
}

impl std::fmt::Debug for RecursiveType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct(stringify!(RecursiveType))
            .field("link", &self.link)
            .field("generics", &self.generics)
            .finish()
    }
}

pub(crate) enum RecursiveTypeOrigin<'x> {
    TypeAlias(&'x TypeAlias),
    Class(Class<'x>),
}
