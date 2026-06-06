#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Keyword {
    // Blocs
    Begin, End, Declare,
    // Contrôle
    If, Then, Elsif, Else, EndIf,
    Loop, EndLoop, While,
    For, In, Reverse, By,
    Exit, Continue, When,
    // Retour
    Return, ReturnNext, ReturnQuery,
    // SQL
    Select, Insert, Update, Delete, Perform, Execute, Call,
    Into, Strict, Using,
    // Diagnostics
    Raise, Exception, Debug, Log, Info, Notice, Warning,
    Message, Detail, Hint, Errcode, Column, Constraint, Datatype, Table, Schema,
    Get, Diagnostics,
    // Autres
    Null, Not, And, Or, Found, New, Old,
    Sqlstate, Others,
    No, // pour NO SCROLL
    // Pas un keyword
    Other,
}

pub fn keyword_from_str(s: &str) -> Keyword {
    match s.to_uppercase().as_str() {
        "BEGIN" => Keyword::Begin,
        "END" => Keyword::End,
        "DECLARE" => Keyword::Declare,
        "IF" => Keyword::If,
        "THEN" => Keyword::Then,
        "ELSIF" | "ELSEIF" => Keyword::Elsif,
        "ELSE" => Keyword::Else,
        "LOOP" => Keyword::Loop,
        "WHILE" => Keyword::While,
        "FOR" => Keyword::For,
        "IN" => Keyword::In,
        "REVERSE" => Keyword::Reverse,
        "BY" => Keyword::By,
        "EXIT" => Keyword::Exit,
        "CONTINUE" => Keyword::Continue,
        "WHEN" => Keyword::When,
        "RETURN" => Keyword::Return,
        "NEXT" => Keyword::ReturnNext,   // géré en contexte
        "QUERY" => Keyword::ReturnQuery, // géré en contexte
        "SELECT" => Keyword::Select,
        "INSERT" => Keyword::Insert,
        "UPDATE" => Keyword::Update,
        "DELETE" => Keyword::Delete,
        "PERFORM" => Keyword::Perform,
        "EXECUTE" => Keyword::Execute,
        "CALL" => Keyword::Call,
        "INTO" => Keyword::Into,
        "STRICT" => Keyword::Strict,
        "USING" => Keyword::Using,
        "RAISE" => Keyword::Raise,
        "EXCEPTION" => Keyword::Exception,
        "DEBUG" => Keyword::Debug,
        "LOG" => Keyword::Log,
        "INFO" => Keyword::Info,
        "NOTICE" => Keyword::Notice,
        "WARNING" => Keyword::Warning,
        "MESSAGE" => Keyword::Message,
        "DETAIL" => Keyword::Detail,
        "HINT" => Keyword::Hint,
        "ERRCODE" => Keyword::Errcode,
        "COLUMN" => Keyword::Column,
        "CONSTRAINT" => Keyword::Constraint,
        "DATATYPE" => Keyword::Datatype,
        "TABLE" => Keyword::Table,
        "SCHEMA" => Keyword::Schema,
        "GET" => Keyword::Get,
        "DIAGNOSTICS" => Keyword::Diagnostics,
        "NULL" => Keyword::Null,
        "NOT" => Keyword::Not,
        "AND" => Keyword::And,
        "OR" => Keyword::Or,
        "FOUND" => Keyword::Found,
        "NEW" => Keyword::New,
        "OLD" => Keyword::Old,
        "SQLSTATE" => Keyword::Sqlstate,
        "OTHERS" => Keyword::Others,
        "NO" => Keyword::No,
        _ => Keyword::Other,
    }
}
