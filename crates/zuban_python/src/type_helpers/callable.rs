use super::Class;
use crate::{
    arguments::Args,
    database::Database,
    diagnostics::IssueKind,
    file::FLOW_ANALYSIS,
    inference_state::InferenceState,
    inferred::Inferred,
    matching::{calculate_callable_type_vars_and_return2, OnTypeError, ResultContext},
    type_::{CallableContent, NeverCause, ReplaceSelf, Type},
};

#[derive(Debug, Copy, Clone)]
pub(crate) struct Callable<'a> {
    pub content: &'a CallableContent,
    pub defined_in: Option<Class<'a>>,
}

impl<'a> Callable<'a> {
    pub fn new(content: &'a CallableContent, defined_in: Option<Class<'a>>) -> Self {
        Self {
            content,
            defined_in,
        }
    }

    pub fn diagnostic_string(&self, db: &Database) -> Option<String> {
        self.content.name.as_ref().map(|n| {
            let name = n.as_str(db);
            match self.content.class_name {
                Some(c) => format!("\"{}\" of \"{}\"", name, c.as_str(db)),
                None => format!("\"{name}\""),
            }
        })
    }

    pub(crate) fn execute<'db>(
        &self,
        i_s: &InferenceState<'db, '_>,
        args: &dyn Args<'db>,
        on_type_error: OnTypeError,
        result_context: &mut ResultContext,
    ) -> Inferred {
        let result = self.execute_internal(i_s, args, false, on_type_error, result_context, None);
        if matches!(self.content.return_type, Type::Never(NeverCause::Explicit)) {
            FLOW_ANALYSIS.with(|fa| fa.mark_current_frame_unreachable())
        }
        result
    }

    pub(crate) fn execute_internal<'db>(
        &self,
        i_s: &InferenceState<'db, '_>,
        args: &dyn Args<'db>,
        skip_first_argument: bool,
        on_type_error: OnTypeError,
        result_context: &mut ResultContext,
        as_self_type: Option<ReplaceSelf>,
    ) -> Inferred {
        let return_type = &self.content.return_type;
        if result_context.expect_not_none() && matches!(&return_type, Type::None) {
            args.add_issue(
                i_s,
                IssueKind::DoesNotReturnAValue(
                    self.diagnostic_string(i_s.db)
                        .unwrap_or_else(|| "Function".into())
                        .into(),
                ),
            );
            return Inferred::new_any_from_error();
        }
        self.execute_for_custom_return_type(
            i_s,
            args,
            skip_first_argument,
            return_type,
            on_type_error,
            result_context,
            as_self_type,
        )
    }

    pub(crate) fn execute_for_custom_return_type<'db>(
        &self,
        i_s: &InferenceState<'db, '_>,
        args: &dyn Args<'db>,
        skip_first_argument: bool,
        return_type: &Type,
        on_type_error: OnTypeError,
        result_context: &mut ResultContext,
        as_self_type: Option<ReplaceSelf>,
    ) -> Inferred {
        let calculated_type_vars = calculate_callable_type_vars_and_return2(
            i_s,
            *self,
            args.iter(i_s.mode),
            |issue| args.add_issue(i_s, issue),
            skip_first_argument,
            result_context,
            as_self_type,
            Some(on_type_error),
        );
        calculated_type_vars.into_return_type(
            i_s,
            return_type,
            self.defined_in.as_ref(),
            as_self_type.unwrap_or(&|| self.defined_in.map(|c| c.as_type(i_s.db))),
        )
    }
}
