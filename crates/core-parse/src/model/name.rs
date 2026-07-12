//! Identifiers and source spans shared across the model.

use crate::parser::{ast, Location};

/// A source span, reusing the parser's 1-based [`Location`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Span {
    pub start: Location,
    pub end: Location,
}

impl Span {
    pub fn new(start: Location, end: Location) -> Self {
        Span { start, end }
    }
}

/// A SQL identifier (table, column, alias, …).
///
/// Postgres folds unquoted identifiers to lowercase and preserves quoted ones, so
/// equality is defined on the *normalized* form while the original text is kept for
/// diagnostics.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Name {
    /// The identifier as written, original case preserved.
    pub text: String,
    /// Whether it was quoted in the source (`"User"` vs `user`).
    pub quoted: bool,
}

impl Name {
    pub fn new(text: impl Into<String>, quoted: bool) -> Self {
        Name {
            text: text.into(),
            quoted,
        }
    }

    /// The form used for resolution comparisons: lowercased unless quoted.
    pub fn normalized(&self) -> String {
        if self.quoted {
            self.text.clone()
        } else {
            self.text.to_lowercase()
        }
    }

    /// Whether two identifiers refer to the same name under Postgres folding rules.
    pub fn matches(&self, other: &Name) -> bool {
        self.normalized() == other.normalized()
    }

    /// Build from a sqlparser identifier.
    pub fn from_ident(ident: &ast::Ident) -> Name {
        Name::new(ident.value.clone(), ident.quote_style.is_some())
    }
}

/// The last identifier of an object name as a [`Name`] (e.g. a column, index, or
/// function name), falling back to the rendered text for non-identifier parts.
pub fn object_name_last(name: &ast::ObjectName) -> Name {
    name.0
        .iter()
        .filter_map(|p| p.as_ident())
        .next_back()
        .map(Name::from_ident)
        .unwrap_or_else(|| Name::new(name.to_string(), false))
}

/// A possibly schema-qualified table name (`public.users` or `users`).
#[derive(Debug, Clone)]
pub struct TableName {
    pub schema: Option<Name>,
    pub name: Name,
}

impl TableName {
    /// Build from a sqlparser object name, collapsing any deeper qualification to a
    /// `(schema, name)` pair.
    pub fn from_object_name(name: &ast::ObjectName) -> TableName {
        let idents: Vec<&ast::Ident> = name.0.iter().filter_map(|p| p.as_ident()).collect();
        match idents.len() {
            0 => TableName {
                schema: None,
                name: Name::new(name.to_string(), false),
            },
            1 => TableName {
                schema: None,
                name: Name::from_ident(idents[0]),
            },
            n => TableName {
                schema: Some(Name::from_ident(idents[n - 2])),
                name: Name::from_ident(idents[n - 1]),
            },
        }
    }
}
