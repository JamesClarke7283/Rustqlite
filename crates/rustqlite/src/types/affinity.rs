//! Column type affinity, faithful to SQLite's rules
//! (<https://www.sqlite.org/datatype3.html#determination_of_column_affinity>).

/// The five SQLite column affinities. (SQLite calls BLOB affinity "NONE" internally; we use
/// the documented user-facing name.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Affinity {
    Integer,
    Text,
    Blob,
    Real,
    Numeric,
}

/// Derive a column's affinity from its declared type, exactly as SQLite does. The rules are
/// applied in order against a case-insensitive scan of the declared type string:
///
/// 1. contains `"INT"`                       → INTEGER
/// 2. contains `"CHAR"`/`"CLOB"`/`"TEXT"`     → TEXT
/// 3. contains `"BLOB"`, or no type given     → BLOB (a.k.a. NONE)
/// 4. contains `"REAL"`/`"FLOA"`/`"DOUB"`     → REAL
/// 5. otherwise                               → NUMERIC
pub fn affinity_of(declared_type: Option<&str>) -> Affinity {
    let Some(decl) = declared_type else {
        return Affinity::Blob;
    };
    // A typeless column is stored in sqlite_schema as an empty type string (not NULL); SQLite
    // gives it BLOB affinity, the same as a wholly absent type (datatype3 rule 3).
    if decl.trim().is_empty() {
        return Affinity::Blob;
    }
    let t = decl.to_ascii_uppercase();
    if t.contains("INT") {
        Affinity::Integer
    } else if t.contains("CHAR") || t.contains("CLOB") || t.contains("TEXT") {
        Affinity::Text
    } else if t.contains("BLOB") {
        Affinity::Blob
    } else if t.contains("REAL") || t.contains("FLOA") || t.contains("DOUB") {
        Affinity::Real
    } else {
        Affinity::Numeric
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_affinity_examples() {
        // The canonical examples from datatype3.html.
        assert_eq!(affinity_of(Some("INT")), Affinity::Integer);
        assert_eq!(affinity_of(Some("INTEGER")), Affinity::Integer);
        assert_eq!(affinity_of(Some("TINYINT")), Affinity::Integer);
        assert_eq!(affinity_of(Some("BIGINT")), Affinity::Integer);
        assert_eq!(affinity_of(Some("CHARACTER(20)")), Affinity::Text);
        assert_eq!(affinity_of(Some("VARCHAR(255)")), Affinity::Text);
        assert_eq!(affinity_of(Some("NVARCHAR(100)")), Affinity::Text);
        assert_eq!(affinity_of(Some("TEXT")), Affinity::Text);
        assert_eq!(affinity_of(Some("CLOB")), Affinity::Text);
        assert_eq!(affinity_of(Some("BLOB")), Affinity::Blob);
        assert_eq!(affinity_of(None), Affinity::Blob);
        // A typeless column (empty declared type) is BLOB affinity, like a missing type.
        assert_eq!(affinity_of(Some("")), Affinity::Blob);
        assert_eq!(affinity_of(Some("   ")), Affinity::Blob);
        assert_eq!(affinity_of(Some("REAL")), Affinity::Real);
        assert_eq!(affinity_of(Some("DOUBLE")), Affinity::Real);
        assert_eq!(affinity_of(Some("DOUBLE PRECISION")), Affinity::Real);
        assert_eq!(affinity_of(Some("FLOAT")), Affinity::Real);
        assert_eq!(affinity_of(Some("NUMERIC")), Affinity::Numeric);
        assert_eq!(affinity_of(Some("DECIMAL(10,5)")), Affinity::Numeric);
        assert_eq!(affinity_of(Some("BOOLEAN")), Affinity::Numeric);
        assert_eq!(affinity_of(Some("DATE")), Affinity::Numeric);
        assert_eq!(affinity_of(Some("DATETIME")), Affinity::Numeric);
        // "INT" precedence beats "CHAR" — "POINT" contains neither; "INTCHAR" would be INTEGER.
        assert_eq!(affinity_of(Some("INTCHAR")), Affinity::Integer);
    }
}
