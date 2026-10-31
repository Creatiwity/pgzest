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
    /// Warn when an FK column lacks a covering index (L005)
    pub missing_fk_index: RuleLevel,
    /// Warn on LIKE/ILIKE with leading wildcard (L006)
    pub no_leading_wildcard: RuleLevel,
    /// Warn on aggregate mixed with bare col without GROUP BY (L007)
    pub aggregate_without_group_by: RuleLevel,
    /// Warn on ADD COLUMN NOT NULL without DEFAULT (L008)
    pub add_column_not_null_no_default: RuleLevel,
    /// Warn on JOIN whose table is never referenced (L009)
    pub unused_join: RuleLevel,
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
                missing_fk_index: RuleLevel::Warn,
                no_leading_wildcard: RuleLevel::Warn,
                aggregate_without_group_by: RuleLevel::Warn,
                add_column_not_null_no_default: RuleLevel::Warn,
                unused_join: RuleLevel::Warn,
            },
        }
    }
}
