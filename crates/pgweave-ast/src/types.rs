use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DataType {
    // Numeric
    SmallInt,
    Integer,
    BigInt,
    Numeric {
        precision: Option<u64>,
        scale: Option<u64>,
    },
    Real,
    DoublePrecision,
    // Text
    Text,
    Varchar(Option<u64>),
    Char(Option<u64>),
    // Temporal
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
    Interval,
    // Other
    Boolean,
    Bytea,
    Json,
    Jsonb,
    Uuid,
    // Array of another type
    Array(Box<DataType>),
    // Custom type (e.g. enums, domains, extension types)
    Custom(String),
}
