use std::cell::Cell;

use vfs::FileIndex;

use crate::{
    database::{Database, ParentScope, PointLink},
    file::TypeVarCallbackReturn,
    node_ref::NodeRef,
    type_::{CallableContent, TypeVarLike},
    type_helpers::{Class, ClassNodeRef, Function},
    TypeCheckerFlags,
};

#[derive(Debug, Copy, Clone)]
enum Context<'a> {
    None,
    DiagnosticClass(&'a Class<'a>),
    Class(&'a Class<'a>),
    DiagnosticExecution(&'a Function<'a, 'a>),
    Execution(&'a Function<'a, 'a>),
    LambdaCallable {
        callable: &'a CallableContent,
        parent_context: &'a Context<'a>,
    },
}

impl<'a> Context<'a> {
    fn current_class(&self, db: &'a Database) -> Option<Class<'a>> {
        match self {
            Context::DiagnosticClass(c) | Context::Class(c) => Some(**c),
            Context::DiagnosticExecution(func) | Context::Execution(func) => func.parent_class(db),
            Context::LambdaCallable { parent_context, .. } => parent_context.current_class(db),
            Context::None => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Mode<'a> {
    Normal,
    EnumMemberCalculation,
    AvoidErrors { had_error: &'a Cell<bool> },
}

#[derive(Clone, Copy, Debug)]
pub struct InferenceState<'db, 'a> {
    pub db: &'db Database,
    context: Context<'a>,
    pub mode: Mode<'a>,
}

impl<'db, 'a> InferenceState<'db, 'a> {
    pub fn new(db: &'db Database) -> Self {
        Self {
            db,
            context: Context::None,
            mode: Mode::Normal,
        }
    }

    pub fn run_with_parent_scope<T>(
        db: &'db Database,
        file_index: FileIndex,
        parent_scope: ParentScope,
        callback: impl FnOnce(InferenceState) -> T,
    ) -> T {
        let class;
        let func;
        let context = match parent_scope {
            ParentScope::Module => Context::None,
            ParentScope::Function(func_index) => {
                func = Function::new_with_unknown_parent(
                    db,
                    NodeRef::from_link(db, PointLink::new(file_index, func_index)),
                );
                Context::DiagnosticExecution(&func)
            }
            ParentScope::Class(class_index) => {
                class = Class::with_undefined_generics(ClassNodeRef::from_link(
                    db,
                    PointLink::new(file_index, class_index),
                ));
                Context::DiagnosticClass(&class)
            }
        };
        callback(InferenceState {
            db,
            context,
            mode: Mode::Normal,
        })
    }

    pub(crate) fn with_func_and_args(&self, func: &'a Function<'a, 'a>) -> Self {
        Self {
            db: self.db,
            context: Context::Execution(func),
            mode: self.mode,
        }
    }

    pub(crate) fn with_diagnostic_func_and_args(&self, func: &'a Function<'a, 'a>) -> Self {
        Self {
            db: self.db,
            context: Context::DiagnosticExecution(func),
            mode: self.mode,
        }
    }

    pub fn with_simplified_annotation_instance(&self) -> Self {
        Self {
            db: self.db,
            context: Context::None,
            mode: self.mode,
        }
    }

    pub fn without_context(&self) -> Self {
        self.with_simplified_annotation_instance()
    }

    pub fn with_class_context(&self, current_class: &'a Class<'a>) -> Self {
        Self {
            db: self.db,
            context: Context::Class(current_class),
            mode: self.mode,
        }
    }

    pub fn with_diagnostic_class_context(&self, current_class: &'a Class<'a>) -> Self {
        Self {
            db: self.db,
            context: Context::DiagnosticClass(current_class),
            mode: self.mode,
        }
    }

    pub fn with_lambda_callable<'x: 'a>(
        &'x self,
        callable: &'x CallableContent,
    ) -> InferenceState<'db, 'x> {
        Self {
            db: self.db,
            context: Context::LambdaCallable {
                callable,
                parent_context: &self.context,
            },
            mode: self.mode,
        }
    }

    pub fn with_mode<'b: 'a>(&self, mode: Mode<'b>) -> InferenceState<'db, 'a> {
        let mut new = *self;
        new.mode = mode;
        new
    }

    pub fn with_enum_calculation_mode(&self) -> Self {
        let mut new = *self;
        new.mode = Mode::EnumMemberCalculation;
        new
    }

    pub fn avoid_errors_within<T>(
        &self,
        mut callable: impl FnMut(&InferenceState<'db, '_>) -> T,
    ) -> (T, bool) {
        let had_error = &Cell::new(false);
        let i_s = &InferenceState {
            db: self.db,
            context: self.context,
            mode: Mode::AvoidErrors { had_error },
        };
        let result = callable(i_s);
        (result, had_error.get())
    }

    pub fn is_calculating_enum_members(&self) -> bool {
        matches!(self.mode, Mode::EnumMemberCalculation)
    }

    pub fn current_function(&self) -> Option<&'a Function<'a, 'a>> {
        match &self.context {
            Context::DiagnosticExecution(func) | Context::Execution(func) => Some(func),
            _ => None,
        }
    }

    pub fn current_class(&self) -> Option<Class<'a>>
    where
        'db: 'a,
    {
        self.context.current_class(self.db)
    }

    pub fn current_lambda_callable(&self) -> Option<&'a CallableContent> {
        match &self.context {
            Context::LambdaCallable { callable, .. } => Some(callable),
            _ => None,
        }
    }

    pub fn in_class_scope(&self) -> Option<&'a Class<'a>> {
        match self.context {
            Context::DiagnosticClass(c) | Context::Class(c) => Some(c),
            _ => None,
        }
    }

    pub fn in_module_context(&self) -> bool {
        matches!(self.context, Context::None)
    }

    pub(crate) fn find_parent_type_var(
        &self,
        searched: &TypeVarLike,
    ) -> Option<TypeVarCallbackReturn> {
        if let Some(func) = self.current_function() {
            if let Some(usage) =
                func.find_type_var_like_including_ancestors(self.db, searched, false)
            {
                return Some(usage);
            }
        }
        if let Some(class) = self.in_class_scope() {
            if let Some(usage) =
                class.find_type_var_like_including_ancestors(self.db, searched, false)
            {
                return Some(usage);
            }
        }
        None
    }

    pub fn as_parent_scope(&self) -> ParentScope {
        if let Some(func) = self.current_function() {
            ParentScope::Function(func.node_ref.node_index)
        } else if let Some(class) = self.current_class() {
            ParentScope::Class(class.node_ref.node_index)
        } else {
            ParentScope::Module
        }
    }

    pub fn is_diagnostic(&self) -> bool {
        matches!(
            self.context,
            Context::DiagnosticClass(_) | Context::DiagnosticExecution(..)
        )
    }

    pub fn should_add_issue(&self) -> bool {
        match self.mode {
            Mode::AvoidErrors { had_error } => {
                had_error.set(true);
                false
            }
            _ => true,
        }
    }

    pub fn flags(&self) -> &TypeCheckerFlags {
        // TODO this is not implemented properly with context, yet.
        &self.db.project.flags
    }
}
