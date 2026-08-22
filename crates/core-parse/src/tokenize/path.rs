//! What a file path is allowed to say about itself.
//!
//! DuckDB reads files directly (`FROM 'sales/2024/region=west/data.parquet'`), so a path reaches the
//! tokenizer in the two positions this module names. Scrambling it whole would make every
//! file-shaped rule impossible; sending it would hand over a bucket, a directory tree and a date.
//! What travels instead is three bits — see [`PathShape`].

/// Stem for a synthetic path sentinel — distinct from `PREFIX` so a path never looks like a table.
const PATH_STEM: &str = "vqp";

/// DuckDB's file-reading table functions. Their *names* carry no user data — they are engine
/// vocabulary, like the `BUILTINS` above — and the analysis cannot tell a Parquet read from a CSV
/// read without them. Kept verbatim, and used to scope [`PathShape`] to the one position where a
/// string is known to be a file path (DD5b decision 1).
pub(super) const PATH_READERS: &[&str] = &[
    "read_parquet",
    "read_csv",
    "read_csv_auto",
    "read_json",
    "read_json_auto",
    "read_ndjson",
    "glob",
];

/// Option names accepted by those readers. Kept verbatim **only inside a reader's argument list**
/// — putting them in `BUILTINS` would keep a *column* named `header` verbatim too, which is exactly
/// the leak this module exists to prevent.
pub(super) const READER_ARGS: &[&str] = &[
    "header",
    "columns",
    "types",
    "filename",
    "hive_partitioning",
    "union_by_name",
];

/// The three facts a file path may reveal, and the only three: which kind of file it is, whether it
/// globs over many, and whether it is laid out in `key=value` directories. **Never any path text**
/// — no bucket, no directory, no date, no identifier, not the stem.
///
/// Each earns its place by a rule that cannot exist without it: the extension separates a Parquet
/// read from a CSV read, the glob is what makes a scan wide, and the `k=v` segments are what
/// partition pruning acts on. Precedent for the trade is [`LitShape::DateStr`] and
/// [`LitShape::Str::has_wild`], which already give up one shape fact so a rule survives.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct PathShape {
    ext: Ext,
    glob: bool,
    kv: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ext {
    Parquet,
    Csv,
    Json,
    Other,
}

impl Ext {
    fn suffix(self) -> &'static str {
        match self {
            Ext::Parquet => "parquet",
            Ext::Csv => "csv",
            Ext::Json => "json",
            Ext::Other => "dat",
        }
    }
}

impl PathShape {
    /// Classify a string as a path, or `None` if it does not look like one.
    pub(super) fn of(s: &str) -> Option<PathShape> {
        let lower = s.to_ascii_lowercase();
        let stem = lower.rsplit(['/', '\\']).next().unwrap_or(&lower);
        let ext = match stem.rsplit_once('.').map(|(_, e)| e) {
            Some("parquet") => Ext::Parquet,
            Some("csv" | "tsv") => Ext::Csv,
            Some("json" | "ndjson") => Ext::Json,
            _ if lower.contains('/') || lower.contains('\\') => Ext::Other,
            _ => return None,
        };
        Some(PathShape {
            ext,
            glob: s.contains(['*', '?']) || s.contains('['),
            kv: s
                .split(['/', '\\'])
                .any(|seg| seg.split_once('=').is_some_and(|(k, _)| !k.is_empty())),
        })
    }

    pub(super) fn tag(self) -> String {
        format!(
            "p{}{}{}",
            self.ext.suffix(),
            u8::from(self.glob),
            u8::from(self.kv)
        )
    }

    /// A synthetic path carrying the three facts and nothing else. Rendered as a real path so the
    /// rules that read it work on the payload exactly as they would on the original.
    pub(super) fn render(self, n: usize) -> String {
        let partition = if self.kv {
            format!("k{n}=v{n}/")
        } else {
            String::new()
        };
        let name = if self.glob {
            "*".to_string()
        } else {
            format!("f{n}")
        };
        format!("{PATH_STEM}{n}/{partition}{name}.{}", self.ext.suffix())
    }
}
