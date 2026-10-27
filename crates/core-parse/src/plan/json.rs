//! The one way plan JSON is read. serde_json refuses anything deeper than 128 levels, because
//! parsing recurses once per level, and DuckDB 2.0 nests real plans past that (TPC-DS query 64 is
//! 160). So the depth is measured first with a flat scan, refused past [`MAX_DEPTH`], and only then
//! parsed with serde_json's own limit off. The cap's stack cost per front door is measured in
//! `docs/phase-dk3-plan-depth.md`.

use serde::de::DeserializeOwned;

use super::PlanError;

/// About 256 operators: 3.2 times the deepest real plan seen, a ninth of MCP's 2 MiB stack.
pub(crate) const MAX_DEPTH: usize = 512;

/// The server reads the client's serialized plan, not the user's `EXPLAIN`, and the two nest
/// differently per format (SQL Server's XML is not capped here at all). Double the client's cap,
/// so a plan any client produced always gets through; 512 operators cost about 1.2 MB of the
/// server's 64 MiB engine thread.
pub(crate) const WIRE_MAX_DEPTH: usize = 2 * MAX_DEPTH;

pub(crate) fn read<T: DeserializeOwned>(json: &str, cap: usize) -> Result<T, PlanError> {
    let depth = depth(json);
    if depth > cap {
        return Err(PlanError::TooDeep { depth, cap });
    }
    let not_json = |e: serde_json::Error| PlanError::NotJson(e.to_string());
    let mut de = serde_json::Deserializer::from_str(json);
    de.disable_recursion_limit();
    let value = T::deserialize(&mut de).map_err(not_json)?;
    de.end().map_err(not_json)?;
    Ok(value)
}

/// The deepest `[`/`{` nesting, not counting brackets inside strings. Never recurses, so a hostile
/// document costs one pass and no stack.
fn depth(json: &str) -> usize {
    let (mut depth, mut max, mut in_string, mut escaped) = (0usize, 0usize, false, false);
    for b in json.bytes() {
        if in_string {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth += 1;
                max = max.max(depth);
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn nested(levels: usize) -> String {
        format!("{}{}", "[".repeat(levels), "]".repeat(levels))
    }

    #[test]
    fn brackets_inside_strings_do_not_count() {
        assert_eq!(depth(r#"[{"a": "[[[{{{", "b": "\"]]]"}]"#), 2);
    }

    #[test]
    fn reads_past_serde_jsons_own_limit_up_to_the_cap() {
        assert!(
            serde_json::from_str::<Value>(&nested(129)).is_err(),
            "serde_json's default"
        );
        assert!(read::<Value>(&nested(MAX_DEPTH), MAX_DEPTH).is_ok());
    }

    #[test]
    fn refuses_past_the_cap_before_parsing() {
        let deep = nested(100_000);
        assert!(matches!(
            read::<Value>(&deep, MAX_DEPTH),
            Err(PlanError::TooDeep { depth: 100_000, .. })
        ));
    }

    #[test]
    fn trailing_text_is_still_an_error() {
        assert!(matches!(
            read::<Value>("[] x", MAX_DEPTH),
            Err(PlanError::NotJson(_))
        ));
    }
}
