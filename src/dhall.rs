//! Evaluates Dhall migrations into `Vec<Op>`.
//!
//! Migrations are evaluated with imports disabled, so they cannot read files,
//! URLs or environment variables. The schema (`Op`, `Topic`) and a few helpers
//! (`map`, `concat`) come from an embedded prelude, and the runner injects an
//! `env` record with the values it chooses to expose.
//!
//! Dhall cannot compare `Text`, so the environment is exposed as a union
//! (`Env`) that migrations branch on with `merge`.

use std::fmt;

use crate::op::Op;

const PRELUDE: &str = include_str!("dhall/prelude.dhall");

/// Values the runner exposes to migrations as `env`.
#[derive(Debug, Clone)]
pub struct Env {
    /// The environment being migrated; must be one of `all`.
    pub name: String,
    /// Every known environment, so migrations can `merge` exhaustively.
    pub all: Vec<String>,
}

#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub fn eval(source: &str, env: &Env) -> Result<Vec<Op>, Error> {
    if !env.all.contains(&env.name) {
        return Err(Error(format!("unknown environment `{}`", env.name)));
    }
    let labels: Vec<String> = env.all.iter().map(|e| label(e)).collect();
    let header = format!(
        "{PRELUDE} let Env = < {} > let env = {{ name = {}, kind = Env.{} }} in ",
        labels.join(" | "),
        text_literal(&env.name),
        label(&env.name),
    );
    // The header takes exactly one line, so error locations are the
    // migration's own location plus one line.
    let header = header.split_whitespace().collect::<Vec<_>>().join(" ");
    let program = format!("{header}\n{source}");
    serde_dhall::from_str(&program)
        .imports(false)
        // Typechecks the result against the Rust `Op` type, so the prelude
        // cannot drift from `op.rs` without every evaluation failing.
        .static_type_annotation()
        .parse::<Vec<Op>>()
        .map_err(|e| Error(describe(&e.to_string(), source)))
}

/// Rewrites a serde_dhall error to point into the migration file rather than
/// the generated program, with a snippet drawn from the migration itself.
fn describe(raw: &str, source: &str) -> String {
    if raw.starts_with("UnexpectedImport") {
        return "imports are not allowed in migrations (files, URLs and env: are all disabled)"
            .into();
    }
    let location = raw.lines().find_map(|l| {
        let loc = l.trim_start().strip_prefix("--> ")?;
        let mut parts = loc.rsplit(':');
        let col: usize = parts.next()?.parse().ok()?;
        let line: usize = parts.next()?.parse().ok()?;
        Some((line.checked_sub(1).filter(|l| *l > 0)?, col))
    });
    let Some((line, col)) = location else {
        return raw.into();
    };
    let summary = match raw.lines().next() {
        Some(first) if !first.trim_start().starts_with("-->") => {
            first.trim_start_matches("Type error: ").trim_start_matches("error: ").to_string()
        }
        _ => match raw.lines().find_map(|l| l.trim_start().strip_prefix("= ")) {
            Some(expected) => format!("syntax error: {expected}"),
            None => "syntax error".into(),
        },
    };
    let text = source.lines().nth(line - 1).unwrap_or_default();
    let gutter = " ".repeat(line.to_string().len());
    format!(
        "line {line}, column {col}: {summary}\n {gutter} |\n {line} | {text}\n {gutter} | {}^",
        " ".repeat(col.saturating_sub(1))
    )
}

/// Hash of the evaluated ops, used as the migration checksum. Formatting or
/// refactoring a file leaves it unchanged; changing what it does changes it.
pub fn checksum(ops: &[Op]) -> String {
    use sha2::{Digest, Sha256};
    let json = serde_json::to_vec(ops).expect("ops serialize to JSON");
    let digest = Sha256::digest(&json);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

fn label(s: &str) -> String {
    format!("`{}`", s.replace('`', ""))
}

fn text_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '$' => out.push_str("\\u0024"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_named(name: &str) -> Env {
        Env {
            name: name.into(),
            all: vec!["dev".into(), "staging".into(), "prod".into()],
        }
    }

    fn env() -> Env {
        env_named("dev")
    }

    fn err(source: &str) -> String {
        eval(source, &env()).unwrap_err().to_string()
    }

    #[test]
    fn evaluates_plain_ops_with_defaults() {
        let ops = eval(
            r#"[ Op.CreateStream { name = "orders" }
               , Op.CreateTopic Topic::{ stream = "orders", name = "events" }
               ]"#,
            &env(),
        )
        .unwrap();
        assert_eq!(
            ops,
            vec![
                Op::CreateStream { name: "orders".into() },
                Op::CreateTopic {
                    stream: "orders".into(),
                    name: "events".into(),
                    partitions: 1,
                    message_expiry: None,
                    max_size: None,
                },
            ]
        );
    }

    #[test]
    fn helpers_allow_loops() {
        let ops = eval(
            r#"let topic = \(name : Text) -> Op.CreateTopic Topic::{ stream = "orders", name, partitions = 3 }
               in  [ Op.CreateStream { name = "orders" } ]
                 # map Text Op topic [ "created", "paid", "shipped" ]"#,
            &env(),
        )
        .unwrap();
        assert_eq!(ops.len(), 4);
        assert!(matches!(&ops[3], Op::CreateTopic { name, partitions: 3, .. } if name == "shipped"));
    }

    #[test]
    fn env_is_injected() {
        let ops = eval(
            r#"[ Op.CreateStream { name = "orders-${env.name}" } ]"#,
            &env_named("prod"),
        )
        .unwrap();
        assert_eq!(ops, vec![Op::CreateStream { name: "orders-prod".into() }]);
    }

    #[test]
    fn env_kind_supports_exhaustive_merge() {
        let src = r#"[ Op.CreateTopic Topic::{
                       , stream = "s"
                       , name = "t"
                       , partitions = merge { dev = 1, staging = 3, prod = 12 } env.kind
                       } ]"#;
        let partitions = |e| match &eval(src, &env_named(e)).unwrap()[0] {
            Op::CreateTopic { partitions, .. } => *partitions,
            op => panic!("unexpected {op:?}"),
        };
        assert_eq!((partitions("dev"), partitions("prod")), (1, 12));
        // Forgetting an environment is a type error, not a silent default.
        assert!(!err(r#"[ Op.CreateStream { name = merge { dev = "a", prod = "b" } env.kind } ]"#).is_empty());
    }

    #[test]
    fn unknown_env_is_rejected() {
        assert!(eval("[] : List Op", &env_named("qa")).is_err());
    }

    #[test]
    fn first_line_errors_point_into_migration() {
        let e = err(r#"[ Op.CreateStream { name = 1 } ]"#);
        assert!(e.starts_with("line 1, column "), "{e}");
        assert!(!e.contains("let Topic"), "{e}");
    }

    #[test]
    fn syntax_errors_point_into_migration() {
        let e = err("[ Op.CreateStream { name = \"a\" }\n,, ]");
        assert!(e.starts_with("line 2, column 2: syntax error"), "{e}");
    }

    #[test]
    fn import_errors_are_explained() {
        assert!(err("env:HOME as Text").contains("imports are not allowed"));
    }

    #[test]
    fn error_lines_match_migration_file() {
        let e = err("[ Op.CreateStream { name = \"a\" }\n, Op.CreateStream { name = 1 }\n]");
        assert!(e.starts_with("line 2, "), "{e}");
    }

    #[test]
    fn env_values_cannot_inject_code() {
        let ops = eval(
            r#"[ Op.CreateStream { name = env.name } ]"#,
            &Env {
                name: r#"a" } ] # [ Op.DeleteStream { name = "x"#.into(),
                all: vec![r#"a" } ] # [ Op.DeleteStream { name = "x"#.into()],
            },
        )
        .unwrap();
        assert_eq!(ops.len(), 1);
    }

    #[test]
    fn local_imports_are_rejected() {
        assert!(!err("./other.dhall").is_empty());
    }

    #[test]
    fn env_imports_are_rejected() {
        assert!(!err("env:HOME as Text").is_empty());
    }

    #[test]
    fn remote_imports_are_rejected() {
        assert!(!err("https://prelude.dhall-lang.org/package.dhall").is_empty());
    }

    #[test]
    fn misspelled_field_is_a_type_error() {
        assert!(!err(r#"[ Op.CreateTopic Topic::{ stream = "s", name = "t", partitons = 3 } ]"#).is_empty());
    }

    #[test]
    fn unknown_op_is_a_type_error() {
        assert!(!err(r#"[ Op.PurgeEverything { name = "s" } ]"#).is_empty());
    }

    #[test]
    fn checksum_ignores_formatting_but_not_meaning() {
        let a = eval(r#"[ Op.CreateStream { name = "orders" } ]"#, &env()).unwrap();
        let b = eval(
            r#"let n = "ord" ++ "ers"
               in  [ Op.CreateStream { name = n } ]"#,
            &env(),
        )
        .unwrap();
        let c = eval(r#"[ Op.CreateStream { name = "order" } ]"#, &env()).unwrap();
        assert_eq!(checksum(&a), checksum(&b));
        assert_ne!(checksum(&a), checksum(&c));
    }
}
