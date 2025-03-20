mod diagnostics;
mod file_state;
mod flow_analysis;
mod inference;
mod name_binder;
mod name_resolution;
mod python_file;
mod type_computation;
mod type_var_finder;
mod utils;

pub use diagnostics::{check_multiple_inheritance, OVERLAPPING_REVERSE_TO_NORMAL_METHODS};
pub use file_state::{File, Leaf};
pub use flow_analysis::{process_unfinished_partials, FLOW_ANALYSIS};
use inference::Inference;
pub use inference::{first_defined_name, first_defined_name_of_multi_def};
pub use name_binder::{
    func_parent_scope, FuncParentScope, FUNC_TO_RETURN_OR_YIELD_DIFF, FUNC_TO_TYPE_VAR_DIFF,
};
pub use python_file::{dotted_path_from_dir, ComplexValues, OtherDefinitionIterator, PythonFile};
pub(crate) use type_computation::{
    linearize_mro_and_return_linearizable, maybe_saved_annotation,
    use_cached_annotation_or_type_comment, use_cached_annotation_type,
    use_cached_param_annotation_type, use_cached_simple_generic_type, CalculatedBaseClass,
    ClassInitializer, ClassNodeRef, FuncNodeRef, FuncParent, GenericCounts, TypeComputation,
    TypeComputationOrigin, TypeVarCallbackReturn, ANNOTATION_TO_EXPR_DIFFERENCE,
    CLASS_TO_CLASS_INFO_DIFFERENCE, ORDERING_METHODS,
};
pub use type_var_finder::TypeVarFinder;
pub use utils::{infer_index, infer_string_index, on_argument_type_error};
