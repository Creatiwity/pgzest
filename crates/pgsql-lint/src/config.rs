use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintConfig {
    pub rules: RulesConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesConfig {
    /// Warn on SELECT * in non-CTE context (L001)
    pub no_select_star: RuleLevel,
    /// Warn if CREATE TABLE has no primary key (L002)
    pub require_primary_key: RuleLevel,
    /// Warn/error if table or column name is not snake_case (L003)
    pub naming_snake_case: RuleLevel,
    /// Warn on NOT IN (SELECT ...) — prefer NOT EXISTS (L004)
    pub no_not_in_subquery: RuleLevel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleLevel {
    Off,
    Warn,
    Error,
}

impl Default for LintConfig {
    fn default() -> Self {
        Self {
            rules: RulesConfig {
                no_select_star: RuleLevel::Warn,
                require_primary_key: RuleLevel::Warn,
                naming_snake_case: RuleLevel::Warn,
                no_not_in_subquery: RuleLevel::Warn,
            },
        }
    }
}
