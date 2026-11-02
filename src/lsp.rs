//! `iggy-migrate lsp`: a language server for migrations, built on
//! starlark-rust's. Generic Starlark servers don't know the functions the
//! runner injects, so they flag `create_topic` and `env` as undefined; this
//! one has the real globals, and reports exactly what `up` would refuse.

use std::path::Path;

use anyhow::{Result, anyhow};
use starlark::docs::DocModule;
use starlark::errors::EvalMessage;
use starlark::syntax::AstModule;
use starlark_lsp::error::eval_message_to_lsp_diagnostic;
use starlark_lsp::server::{LspContext, LspEvalResult, LspUri, StringLiteralResult};

use crate::migrations::Env;
use crate::star;

/// Serves LSP over stdin and stdout until the editor disconnects.
pub fn run(environments: Vec<String>) -> Result<()> {
    starlark_lsp::server::stdio_server(Context { environments }).map_err(|e| anyhow!("{e:?}"))
}

struct Context {
    environments: Vec<String>,
}

impl LspContext for Context {
    /// Evaluates the file in every environment, as `up` would before applying
    /// anything. A problem that only some environments hit says which.
    fn parse_file_with_contents(&self, uri: &LspUri, content: String) -> LspEvalResult {
        let LspUri::File(path) = uri else {
            return LspEvalResult::default();
        };
        let file = path.to_string_lossy();
        let ast = AstModule::parse(&file, content.clone(), &star::dialect()).ok();
        let mut found: Vec<(EvalMessage, Vec<&str>)> = Vec::new();
        for name in &self.environments {
            let env = Env { name: name.clone(), all: self.environments.clone() };
            let Some(message) = star::diagnose(&file, &content, &env) else {
                continue;
            };
            match found.iter_mut().find(|(m, _)| m.span == message.span && m.description == message.description) {
                Some((_, envs)) => envs.push(name),
                None => found.push((message, vec![name])),
            }
        }
        let diagnostics = found
            .into_iter()
            .map(|(mut message, envs)| {
                if envs.len() < self.environments.len() {
                    message.description = format!("in {}: {}", envs.join(", "), message.description);
                }
                eval_message_to_lsp_diagnostic(message)
            })
            .collect();
        LspEvalResult { diagnostics, ast }
    }

    fn resolve_load(&self, path: &str, _: &LspUri, _: Option<&Path>) -> Result<LspUri, String> {
        Err(format!("cannot load `{path}`: migrations cannot load other files"))
    }

    fn render_as_load(&self, _: &LspUri, _: &LspUri, _: Option<&Path>) -> Result<String, String> {
        Err("migrations cannot load other files".into())
    }

    fn resolve_string_literal(
        &self,
        _: &str,
        _: &LspUri,
        _: Option<&Path>,
    ) -> Result<Option<StringLiteralResult>, String> {
        Ok(None)
    }

    fn get_load_contents(&self, uri: &LspUri) -> Result<Option<String>, String> {
        match uri {
            LspUri::File(path) => match std::fs::read_to_string(path) {
                Ok(contents) => Ok(Some(contents)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.to_string()),
            },
            _ => Ok(None),
        }
    }

    /// The globals every migration sees, for completion and hover.
    fn get_environment(&self, _: &LspUri) -> DocModule {
        let env = self.environments.first().map_or("dev", String::as_str);
        star::globals(env).documentation()
    }

    fn get_uri_for_global_symbol(&self, _: &LspUri, _: &str) -> Result<Option<LspUri>, String> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostics(source: &str) -> Vec<(u32, String)> {
        let context = Context { environments: vec!["dev".into(), "staging".into(), "prod".into()] };
        let uri = LspUri::File("/m/20261101000000_m.star".into());
        let result = context.parse_file_with_contents(&uri, source.into());
        result.diagnostics.into_iter().map(|d| (d.range.start.line, d.message)).collect()
    }

    #[test]
    fn reports_what_up_would_refuse_with_its_line() {
        assert_eq!(diagnostics("ops = [create_stream(name = \"orders\")]\n"), vec![]);
        let found = diagnostics("ops = [\n    create_topic(stream = \"s\", name = \"t\", partitons = 3),\n]\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, 1);
        assert!(found[0].1.contains("partitons") && !found[0].1.starts_with("in "), "{found:?}");
    }

    #[test]
    fn says_which_environments_hit_a_problem() {
        let found = diagnostics("p = {\"dev\": 1, \"staging\": 3}[env]\nops = []\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].1.starts_with("in prod: "), "{found:?}");
    }

    #[test]
    fn globals_are_known() {
        let context = Context { environments: vec!["dev".into()] };
        let names: Vec<String> = context.get_environment(&LspUri::File("/m.star".into())).members.into_keys().collect();
        for name in ["create_topic", "update_topic", "per_env", "env"] {
            assert!(names.iter().any(|n| n == name), "{name} missing from {names:?}");
        }
    }
}
