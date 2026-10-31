use crate::rule::{CheckRule, RuleCtx};
use pgweave_ast::ddl::CreatePolicy;

pub struct PolicyCheck;

impl CheckRule for PolicyCheck {
    fn name(&self) -> &'static str {
        "policy-check"
    }

    fn check_policy(&self, policy: &CreatePolicy, ctx: &mut RuleCtx) {
        crate::check::check_create_policy_rule(policy, ctx.schema, ctx.diags);
    }
}
