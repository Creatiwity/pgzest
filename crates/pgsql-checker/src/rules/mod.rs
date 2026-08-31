mod dml_table_check;
mod function_body_check;
mod policy_check;
mod select_column_check;

pub use dml_table_check::DmlTableCheck;
pub use function_body_check::FunctionBodyCheck;
pub use policy_check::PolicyCheck;
pub use select_column_check::SelectColumnCheck;

/// Returns the full set of built-in rules, in the order they should be applied.
pub fn builtin_rules() -> Vec<Box<dyn crate::rule::CheckRule>> {
    vec![
        Box::new(SelectColumnCheck),
        Box::new(FunctionBodyCheck),
        Box::new(PolicyCheck),
        Box::new(DmlTableCheck),
    ]
}
