//! Starlark migrations: `.star` files evaluated with starlark-rust, as an
//! alternative to Dhall with Python-like syntax.
//!
//! A migration assigns its ops to a top-level `ops = [...]`, built with the
//! native functions below (`create_topic(stream = .., name = ..)` and so on),
//! which are the only way to make an op. The runner injects `env` and
//! `per_env`. Nothing can be loaded, and the standard library has no access
//! to files, environment variables, time or randomness.
//!
//! Unlike Dhall, Starlark does not guarantee termination, so evaluation runs
//! under tick, heap and wall-clock limits, on a thread with a large stack
//! (deeply nested source overflows the parser's stack otherwise) and inside
//! `catch_unwind` (a few huge allocations panic inside the interpreter).

use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::thread;
use std::time::{Duration, Instant};

use allocative::Allocative;
use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, Module};
use starlark::eval::{Evaluator, FileLoader};
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::syntax::{AstModule, Dialect, DialectTypes};
use starlark::values::dict::DictRef;
use starlark::values::list::ListRef;
use starlark::values::none::NoneOr;
use starlark::values::{ProvidesStaticType, StarlarkValue, Value, ValueLike};
use starlark_derive::{StarlarkPagablePanic, starlark_value};

use crate::dhall::Env;
use crate::migrations;
use crate::op::Op;

const MAX_SOURCE_BYTES: usize = 1 << 20;
const STACK_BYTES: usize = 256 << 20;

/// Bounds on one evaluation. Real migrations need a tiny fraction of these;
/// a release build runs 10M ticks in well under a second.
#[derive(Clone, Copy)]
struct Limits {
    ticks: u64,
    heap_bytes: usize,
    deadline: Duration,
}

const LIMITS: Limits =
    Limits { ticks: 10_000_000, heap_bytes: 64 << 20, deadline: Duration::from_secs(5) };

/// Evaluates a migration to its ops. `file` is the name shown in errors.
pub fn eval(file: &str, source: &str, env: &Env) -> Result<Vec<Op>> {
    eval_with(file, source, env, LIMITS)
}

fn eval_with(file: &str, source: &str, env: &Env, limits: Limits) -> Result<Vec<Op>> {
    if source.len() > MAX_SOURCE_BYTES {
        bail!("migration is larger than {} KiB", MAX_SOURCE_BYTES >> 10);
    }
    if !env.all.contains(&env.name) {
        bail!("unknown environment `{}`; known: {}", env.name, env.all.join(", "));
    }
    let (file, source, env) = (file.to_string(), source.to_string(), env.clone());
    let worker = thread::Builder::new()
        .name("starlark".into())
        .stack_size(STACK_BYTES)
        .spawn(move || panic::catch_unwind(AssertUnwindSafe(|| eval_here(&file, source, &env, limits))))?;
    match worker.join() {
        Ok(Ok(result)) => result,
        _ => bail!("the Starlark interpreter crashed evaluating this migration (an oversized value?)"),
    }
}

fn eval_here(file: &str, source: String, env: &Env, limits: Limits) -> Result<Vec<Op>> {
    let ast = AstModule::parse(file, source, &dialect()).map_err(|e| anyhow!("{e}"))?;
    let globals = globals(&env.name);
    let ctx = Ctx { env: env.clone() };
    Module::with_temp_heap(|module| {
        let mut eval = Evaluator::new(&module);
        eval.set_loader(&NoLoads);
        eval.extra = Some(&ctx);
        eval.enable_static_typechecking(true);
        eval.set_max_tick_count(limits.ticks).map_err(|e| anyhow!("{e}"))?;
        eval.set_max_heap_size(limits.heap_bytes).map_err(|e| anyhow!("{e}"))?;
        let deadline = Instant::now() + limits.deadline;
        eval.set_check_cancelled(Box::new(move || Instant::now() > deadline));
        eval.eval_module(ast, &globals).map_err(|e| anyhow!("{e}"))?;
        drop(eval);
        if module.get("env").is_some() {
            bail!("`env` is set by the runner; assigning it is not allowed");
        }
        let ops = module.get("ops").ok_or_else(|| anyhow!("the migration must assign `ops = [...]`"))?;
        ops_from_value(ops)
    })
}

/// The standard dialect, with type annotations (checked) and keyword-only
/// arguments. No `while`, no top-level `for`, recursion is depth-limited.
fn dialect() -> Dialect {
    Dialect { enable_types: DialectTypes::Enable, enable_keyword_only_arguments: true, ..Dialect::Standard }
}

/// The standard library with no extensions (so no `print`, `json` or
/// `debug`), plus `env` and the native functions.
fn globals(env: &str) -> Globals {
    GlobalsBuilder::standard().with(prelude).with(|b| b.set("env", env)).build()
}

struct NoLoads;

impl FileLoader for NoLoads {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        Err(starlark::Error::new_other(anyhow!(
            "cannot load `{path}`: migrations cannot load other files"
        )))
    }
}

#[derive(ProvidesStaticType)]
struct Ctx {
    env: Env,
}

fn ctx<'a>(eval: &'a Evaluator) -> &'a Ctx {
    eval.extra.and_then(|e| e.downcast_ref::<Ctx>()).expect("the runner sets the context")
}

/// An op on the Starlark heap. Opaque to migrations, so the only way to make
/// one is through the native functions, which validate it.
#[derive(Debug, ProvidesStaticType, Serialize, StarlarkPagablePanic, Allocative)]
#[serde(transparent)]
struct OpValue(#[allocative(skip)] Op);
starlark_simple_value!(OpValue);

impl fmt::Display for OpValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

#[starlark_value(type = "Op")]
impl<'v> StarlarkValue<'v> for OpValue {}

fn op(op: Op) -> Result<OpValue> {
    migrations::validate(&op)?;
    Ok(OpValue(op))
}

fn ops_from_value(value: Value) -> Result<Vec<Op>> {
    let list = ListRef::from_value(value)
        .ok_or_else(|| anyhow!("`ops` must be a list, got {}", value.get_type()))?;
    list.iter()
        .enumerate()
        .map(|(i, item)| match item.downcast_ref::<OpValue>() {
            Some(op) => Ok(op.0.clone()),
            None => bail!(
                "ops[{i}] must be an op made with create_topic(...) etc., got {} `{item}`",
                item.get_type()
            ),
        })
        .collect()
}

fn partitions(value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| anyhow!("partitions must be positive, got {value}"))
}

fn text(value: NoneOr<&str>) -> Option<String> {
    value.into_option().map(str::to_string)
}

#[starlark_module]
fn prelude(builder: &mut GlobalsBuilder) {
    fn create_stream(#[starlark(require = named)] name: &str) -> anyhow::Result<OpValue> {
        op(Op::CreateStream { name: name.into() })
    }

    fn delete_stream(#[starlark(require = named)] name: &str) -> anyhow::Result<OpValue> {
        op(Op::DeleteStream { name: name.into() })
    }

    // Keyword arguments mirror the op's fields.
    #[allow(clippy::too_many_arguments)]
    fn create_topic(
        #[starlark(require = named)] stream: &str,
        #[starlark(require = named)] name: &str,
        #[starlark(require = named, default = 1)] partitions: i64,
        #[starlark(require = named, default = NoneOr::None)] message_expiry: NoneOr<&str>,
        #[starlark(require = named, default = NoneOr::None)] max_size: NoneOr<&str>,
        #[starlark(require = named, default = NoneOr::None)] compression: NoneOr<&str>,
        #[starlark(require = named, default = NoneOr::None)] segment_size: NoneOr<&str>,
        #[starlark(require = named, default = NoneOr::None)] durability: NoneOr<&str>,
    ) -> anyhow::Result<OpValue> {
        op(Op::CreateTopic {
            stream: stream.into(),
            name: name.into(),
            partitions: self::partitions(partitions)?,
            message_expiry: text(message_expiry),
            max_size: text(max_size),
            compression: text(compression),
            segment_size: text(segment_size),
            durability: text(durability),
        })
    }

    fn update_topic(
        #[starlark(require = named)] stream: &str,
        #[starlark(require = named)] name: &str,
        #[starlark(require = named, default = NoneOr::None)] partitions: NoneOr<i64>,
        #[starlark(require = named, default = NoneOr::None)] message_expiry: NoneOr<&str>,
        #[starlark(require = named, default = NoneOr::None)] max_size: NoneOr<&str>,
        #[starlark(require = named, default = NoneOr::None)] compression: NoneOr<&str>,
    ) -> anyhow::Result<OpValue> {
        op(Op::UpdateTopic {
            stream: stream.into(),
            name: name.into(),
            partitions: partitions.into_option().map(self::partitions).transpose()?,
            message_expiry: text(message_expiry),
            max_size: text(max_size),
            compression: text(compression),
        })
    }

    fn delete_topic(
        #[starlark(require = named)] stream: &str,
        #[starlark(require = named)] name: &str,
    ) -> anyhow::Result<OpValue> {
        op(Op::DeleteTopic { stream: stream.into(), name: name.into() })
    }

    fn create_consumer_group(
        #[starlark(require = named)] stream: &str,
        #[starlark(require = named)] topic: &str,
        #[starlark(require = named)] name: &str,
    ) -> anyhow::Result<OpValue> {
        op(Op::CreateConsumerGroup { stream: stream.into(), topic: topic.into(), name: name.into() })
    }

    fn delete_consumer_group(
        #[starlark(require = named)] stream: &str,
        #[starlark(require = named)] topic: &str,
        #[starlark(require = named)] name: &str,
    ) -> anyhow::Result<OpValue> {
        op(Op::DeleteConsumerGroup { stream: stream.into(), topic: topic.into(), name: name.into() })
    }

    /// The value for the current environment. Every known environment must
    /// be given, and no others, so a missing one fails in every environment
    /// (like Dhall's `merge`), not just when that environment is migrated.
    fn per_env<'v>(
        #[starlark(kwargs)] values: DictRef<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let env = &ctx(eval).env;
        let given: Vec<&str> = values.keys().filter_map(|k| k.unpack_str()).collect();
        let missing: Vec<&str> = env.all.iter().map(String::as_str).filter(|e| !given.contains(e)).collect();
        let unknown: Vec<&str> = given.iter().copied().filter(|g| !env.all.iter().any(|e| e == g)).collect();
        let mut problems = Vec::new();
        if !missing.is_empty() {
            problems.push(format!("missing {}", missing.join(", ")));
        }
        if !unknown.is_empty() {
            problems.push(format!("unknown {}", unknown.join(", ")));
        }
        if !problems.is_empty() {
            bail!("per_env: {} (environments are {})", problems.join("; "), env.all.join(", "));
        }
        Ok(values.get_str(&env.name).expect("checked above"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dhall;

    fn env(name: &str) -> Env {
        Env { name: name.into(), all: vec!["dev".into(), "staging".into(), "prod".into()] }
    }

    fn eval_ok(source: &str, name: &str) -> Vec<Op> {
        eval("m.star", source, &env(name)).unwrap()
    }

    fn err(source: &str) -> String {
        format!("{:#}", eval("m.star", source, &env("dev")).unwrap_err())
    }

    const ORDERS: &str = r#"
ops = [create_stream(name = "orders")] + [
    create_topic(
        stream = "orders",
        name = t,
        partitions = per_env(dev = 1, staging = 3, prod = 12),
        message_expiry = "7d",
    )
    for t in ["created", "paid", "shipped"]
] + [create_consumer_group(stream = "orders", topic = "paid", name = "billing")]
"#;

    #[test]
    fn evaluates_per_env() {
        let partitions = |ops: &[Op]| match &ops[1] {
            Op::CreateTopic { partitions, .. } => *partitions,
            _ => unreachable!(),
        };
        assert_eq!(partitions(&eval_ok(ORDERS, "dev")), 1);
        assert_eq!(partitions(&eval_ok(ORDERS, "prod")), 12);
        assert_eq!(eval_ok(ORDERS, "prod").len(), 5);
    }

    /// The same ops in Dhall and Starlark have the same checksum, so a
    /// migration can be rewritten in the other format once applied.
    #[test]
    fn checksums_match_dhall() {
        let dhall_source = r#"
let topic = \(name : Text) -> Op.CreateTopic Topic::{ stream = "orders", name, partitions = merge { dev = 1, staging = 3, prod = 12 } env.kind, message_expiry = Some "7d" }
in  [ Op.CreateStream { name = "orders" } ]
  # map Text Op topic [ "created", "paid", "shipped" ]
  # [ Op.CreateConsumerGroup { stream = "orders", topic = "paid", name = "billing" } ]
"#;
        for name in ["dev", "prod"] {
            let from_dhall = dhall::eval(dhall_source, &env(name)).unwrap();
            assert_eq!(dhall::checksum(&eval_ok(ORDERS, name)), dhall::checksum(&from_dhall));
        }
    }

    #[test]
    fn nothing_can_be_loaded_or_read() {
        assert!(err(r#"load("other.star", "x")"#).contains("cannot load `other.star`"));
        for name in ["print", "open", "json", "debug", "struct", "time", "random", "getenv"] {
            assert!(err(&format!("x = {name}\nops = []")).contains("not found"), "{name} is available");
        }
    }

    #[test]
    fn evaluation_is_bounded() {
        let bounded = |source: &str, limits| format!("{:#}", eval_with("m.star", source, &env("dev"), limits).unwrap_err());
        let loop_ = "def f():\n    for i in range(1000000000):\n        pass\nf()\nops = []";
        assert!(bounded(loop_, Limits { ticks: 100_000, ..LIMITS }).contains("ticks"));
        let timed = Limits { ticks: u64::MAX, deadline: Duration::from_millis(200), ..LIMITS };
        assert!(bounded(loop_, timed).contains("cancelled"));
        let big = "x = \"x\" * 20000000\nops = []";
        assert!(bounded(big, Limits { heap_bytes: 4 << 20, ..LIMITS }).contains("memory"));
        let nested = format!("x = {}{}\nops = []", "[".repeat(5000), "]".repeat(5000));
        assert!(eval("m.star", &nested, &env("dev")).is_ok());
        assert!(err("while True:\n    pass").contains("while"));
    }

    /// Slow in debug builds (minutes): run with `cargo test --release -- --ignored`.
    #[test]
    #[ignore]
    fn interpreter_panics_are_errors() {
        assert!(err("s = [0] * 1000000000\nops = []").contains("crashed"));
    }

    #[test]
    fn mistakes_point_at_the_call() {
        let e = err(r#"ops = [create_topic(stream = "orders", name = "x", partitons = 3)]"#);
        assert!(e.contains("partitons") && e.contains("m.star:1:"), "{e}");
        assert!(err(r#"ops = [creat_topic(stream = "s", name = "t")]"#).contains("did you mean `create_topic`"));
        assert!(err(r#"ops = [create_topic(stream = "s", name = "t", partitions = "3")]"#).contains("partitions"));
        assert!(err(r#"ops = [create_topic(stream = "s", name = "t", partitions = 0)]"#).contains("between 1"));
        assert!(err(r#"ops = [create_topic(stream = "s", name = "t", message_expiry = "soon")]"#).contains("soon"));
        assert!(err(r#"ops = [delete_stream(name = "migration")]"#).contains("ledger"));
    }

    #[test]
    fn per_env_is_exhaustive() {
        assert!(err("x = per_env(dev = 1, prod = 12)\nops = []").contains("missing staging"));
        assert!(err("x = per_env(dev = 1, staging = 3, prdo = 12)\nops = []").contains("unknown prdo"));
    }

    #[test]
    fn ops_must_be_real_ops() {
        assert!(err("x = 1").contains("must assign `ops"));
        assert!(err(r#"ops = [{"CreateStream": {"name": "s"}}]"#).contains("ops[0] must be an op"));
        assert!(err("ops = ()").contains("must be a list"));
        assert!(err("env = \"prod\"\nops = []").contains("`env` is set by the runner"));
    }

    #[test]
    fn output_is_deterministic() {
        let source = r#"
names = {n: None for n in ["b", "a", "c"]}
ops = [create_stream(name = n) for n in names] + [create_stream(name = str(0.1 + 0.2))]
"#;
        let first = dhall::checksum(&eval_ok(source, "dev"));
        for _ in 0..10 {
            assert_eq!(dhall::checksum(&eval_ok(source, "dev")), first);
        }
    }
}
