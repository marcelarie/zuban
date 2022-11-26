use parsa_python_ast::*;

use super::type_computation::{cache_name_on_class, SpecialType, TypeNameLookup};
use crate::database::{
    Locality, Point, PointType, TypeVarIndex, TypeVarLike, TypeVarLikes, TypeVarManager,
};
use crate::diagnostics::IssueType;
use crate::file::{Inference, PythonFile};
use crate::file_state::File;
use crate::getitem::{SliceOrSimple, SliceType};
use crate::inferred::Inferred;
use crate::node_ref::NodeRef;
use crate::value::Class;

#[derive(Debug, Clone)]
enum BaseLookup<'file> {
    Module(&'file PythonFile),
    Class(Inferred),
    Protocol,
    Callable,
    Generic,
    Other,
}

pub struct ClassTypeVarFinder<'db, 'file, 'i_s, 'b, 'c> {
    inference: &'c mut Inference<'db, 'file, 'i_s, 'b>,
    class: &'c Class<'c>,
    type_var_manager: TypeVarManager,
    generic_or_protocol_slice: Option<SliceType<'file>>,
    current_generic_or_protocol_index: Option<TypeVarIndex>,
    had_generic_or_protocol_issue: bool,
}

impl<'db, 'file, 'i_s, 'b, 'c> ClassTypeVarFinder<'db, 'file, 'i_s, 'b, 'c> {
    pub fn find(
        inference: &'c mut Inference<'db, 'file, 'i_s, 'b>,
        class: &'c Class<'file>,
    ) -> TypeVarLikes {
        let mut finder = Self {
            inference,
            class,
            type_var_manager: TypeVarManager::default(),
            generic_or_protocol_slice: None,
            current_generic_or_protocol_index: None,
            had_generic_or_protocol_issue: false,
        };

        if let Some(arguments) = class.node().arguments() {
            for argument in arguments.iter() {
                match argument {
                    Argument::Positional(n) => {
                        finder.find_in_expr(n.expression());
                    }
                    Argument::Keyword(_, _) => (), // Ignore for now -> part of meta class
                    Argument::Starred(_) | Argument::DoubleStarred(_) => (), // Nobody probably cares about this
                }
            }
        }
        if let Some(slice_type) = finder.generic_or_protocol_slice {
            if !finder.had_generic_or_protocol_issue {
                finder.check_generic_or_protocol_length(slice_type)
            }
        }
        finder.type_var_manager.into_type_vars()
    }

    fn find_in_expr(&mut self, expr: Expression<'file>) {
        match expr.unpack() {
            ExpressionContent::ExpressionPart(n) => {
                self.find_in_expression_part(n);
            }
            ExpressionContent::Lambda(_) => todo!(),
            ExpressionContent::Ternary(t) => todo!(),
        };
    }

    fn find_in_expression_part(&mut self, node: ExpressionPart<'file>) -> BaseLookup<'file> {
        match node {
            ExpressionPart::Atom(atom) => self.find_in_atom(atom),
            ExpressionPart::Primary(primary) => self.find_in_primary(primary),
            ExpressionPart::BitwiseOr(bitwise_or) => {
                let (a, b) = bitwise_or.unpack();
                self.find_in_expression_part(a);
                self.find_in_expression_part(b);
                BaseLookup::Other
            }
            _ => BaseLookup::Other,
        }
    }

    fn find_in_primary(&mut self, primary: Primary<'file>) -> BaseLookup<'db> {
        let base = self.find_in_primary_or_atom(primary.first());
        match primary.second() {
            PrimaryContent::Attribute(name) => {
                match base {
                    BaseLookup::Module(f) => {
                        // TODO this is a bit weird. shouldn't this just do a goto?
                        if let Some(index) = f.symbol_table.lookup_symbol(name.as_str()) {
                            self.inference.file.points.set(
                                name.index(),
                                Point::new_redirect(f.file_index(), index, Locality::Todo),
                            );
                            self.find_in_name(name)
                        } else {
                            BaseLookup::Other
                        }
                    }
                    BaseLookup::Class(i) => {
                        let cls = i.maybe_class(self.inference.i_s).unwrap();
                        let point_type = cache_name_on_class(cls, self.inference.file, name);
                        if point_type == PointType::Redirect {
                            self.find_in_name(name)
                        } else {
                            BaseLookup::Other
                        }
                    }
                    _ => BaseLookup::Other,
                }
            }
            PrimaryContent::Execution(details) => BaseLookup::Other,
            PrimaryContent::GetItem(slice_type) => {
                let s = SliceType::new(self.inference.file, primary.index(), slice_type);
                match base {
                    BaseLookup::Protocol | BaseLookup::Generic => {
                        if self.generic_or_protocol_slice.is_some() {
                            self.had_generic_or_protocol_issue = true;
                            NodeRef::new(self.inference.file, primary.index()).add_typing_issue(
                                self.inference.i_s.db,
                                IssueType::EnsureSingleGenericOrProtocol,
                            );
                        }
                        self.generic_or_protocol_slice = Some(SliceType::new(
                            self.inference.file,
                            primary.index(),
                            slice_type,
                        ));
                        for (i, slice_or_simple) in s.iter().enumerate() {
                            self.current_generic_or_protocol_index = Some(i.into());
                            if let SliceOrSimple::Simple(s) = slice_or_simple {
                                self.find_in_expr(s.named_expr.expression())
                            }
                        }
                        self.current_generic_or_protocol_index = None;
                    }
                    BaseLookup::Callable => self.find_in_callable(s),
                    _ => {
                        for slice_or_simple in s.iter() {
                            if let SliceOrSimple::Simple(s) = slice_or_simple {
                                self.find_in_expr(s.named_expr.expression())
                            }
                        }
                    }
                }
                BaseLookup::Other
            }
        }
    }

    fn find_in_atom(&mut self, atom: Atom) -> BaseLookup<'db> {
        match atom.unpack() {
            AtomContent::Name(n) => {
                self.inference.infer_name_reference(n);
                self.find_in_name(n)
            }
            AtomContent::Strings(s_o_b) => match s_o_b.as_python_string() {
                Some(PythonString::Ref(start, s)) => {
                    todo!()
                    //self.compute_forward_reference(start, s.to_owned())
                }
                Some(PythonString::String(start, s)) => todo!(),
                Some(PythonString::FString) => todo!(),
                None => todo!(),
            },
            _ => BaseLookup::Other,
        }
    }

    fn find_in_name(&mut self, name: Name) -> BaseLookup<'db> {
        match self.inference.lookup_type_name(name) {
            TypeNameLookup::Module(f) => BaseLookup::Module(f),
            TypeNameLookup::Class(i) => BaseLookup::Class(i),
            TypeNameLookup::TypeVarLike(type_var_like) => {
                if self
                    .class
                    .maybe_type_var_like_in_parent(self.inference.i_s, &type_var_like)
                    .is_none()
                {
                    if let TypeVarLike::TypeVarTuple(t) = &type_var_like {
                        if self.type_var_manager.has_type_var_tuples() {
                            NodeRef::new(self.inference.file, name.index()).add_typing_issue(
                                self.inference.i_s.db,
                                IssueType::MultipleTypeVarTuplesInClassDef,
                            )
                            // TODO this type var tuple should probably not be added
                        }
                    }
                    let old_index = self.type_var_manager.add(type_var_like, None);
                    if let Some(force_index) = self.current_generic_or_protocol_index {
                        if old_index < force_index {
                            NodeRef::new(self.inference.file, name.index()).add_typing_issue(
                                self.inference.i_s.db,
                                IssueType::DuplicateTypeVar,
                            )
                        } else if old_index != force_index {
                            self.type_var_manager.move_index(old_index, force_index);
                        }
                    }
                }
                BaseLookup::Other
            }
            TypeNameLookup::SpecialType(SpecialType::Generic) => BaseLookup::Generic,
            TypeNameLookup::SpecialType(SpecialType::Protocol) => BaseLookup::Protocol,
            TypeNameLookup::SpecialType(SpecialType::Callable) => BaseLookup::Callable,
            _ => BaseLookup::Other,
        }
    }

    fn find_in_primary_or_atom(&mut self, p: PrimaryOrAtom<'file>) -> BaseLookup<'db> {
        match p {
            PrimaryOrAtom::Primary(primary) => self.find_in_primary(primary),
            PrimaryOrAtom::Atom(atom) => self.find_in_atom(atom),
        }
    }

    fn find_in_callable(&mut self, slice_type: SliceType<'file>) {
        if slice_type.iter().count() == 2 {
            let mut iterator = slice_type.iter();
            if let SliceOrSimple::Simple(n) = iterator.next().unwrap() {
                if let ExpressionContent::ExpressionPart(ExpressionPart::Atom(atom)) =
                    n.named_expr.expression().unpack()
                {
                    if let AtomContent::List(list) = atom.unpack() {
                        if let Some(iterator) = list.unpack() {
                            for i in iterator {
                                if let StarLikeExpression::NamedExpression(n) = i {
                                    self.find_in_expr(n.expression());
                                }
                            }
                        }
                    }
                }
            }
            let slice_or_simple = iterator.next().unwrap();
            if let SliceOrSimple::Simple(s) = slice_or_simple {
                self.find_in_expr(s.named_expr.expression())
            }
        }
    }

    fn check_generic_or_protocol_length(&self, slice_type: SliceType) {
        // Reorder slices
        if slice_type.iter().count() < self.type_var_manager.len() {
            slice_type.as_node_ref().add_typing_issue(
                self.inference.i_s.db,
                IssueType::IncompleteGenericOrProtocolTypeVars,
            )
        }
    }
}
