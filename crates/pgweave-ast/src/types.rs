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
    /// Précision des secondes fractionnaires (`time(3)`), absente = 6.
    Time(Option<u64>),
    TimeTz(Option<u64>),
    Timestamp(Option<u64>),
    TimestampTz(Option<u64>),
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
    /// Type nommé avec modificateurs, tels qu'écrits : `geometry(Point,
    /// 4326)`, `vector(1536)`. Sans modificateurs, c'est `Custom`.
    Modified {
        name: String,
        modifiers: Vec<String>,
    },
}
