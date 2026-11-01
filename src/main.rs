mod dhall;
mod drift;
mod executor;
mod ledger;
mod migrations;
mod op;
mod rehearsal;
mod squash;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use iggy::prelude::*;

use executor::{Action, Mode, Planned};
use serde_json::{Value, json};

/// `println!` that ignores write errors, so a closed pipe (`up | head`)
/// cannot abort a run partway through applying a migration.
macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}
pub(crate) use out;
use ledger::{Event, Status};
use migrations::{Migration, Replaced};

#[derive(Parser)]
#[command(version, about = "Forward-only migrations for Iggy streams, topics and consumer groups")]
struct Cli {
    /// Iggy connection string.
    #[arg(long, env = "IGGY_MIGRATE_URL", default_value = "iggy://iggy:iggy@localhost:8090", global = true, hide_env_values = true)]
    url: String,
    /// Directory containing migration files.
    #[arg(long, env = "IGGY_MIGRATE_DIR", default_value = "migrations", global = true)]
    dir: PathBuf,
    /// Environment being migrated, exposed to migrations as `env`.
    #[arg(long, env = "IGGY_MIGRATE_ENV", default_value = "dev", global = true)]
    env: String,
    /// Every known environment, so migrations can `merge` over all of them.
    #[arg(long, env = "IGGY_MIGRATE_ENVIRONMENTS", default_value = "dev,staging,prod", value_delimiter = ',', global = true)]
    environments: Vec<String>,
    /// Docker image for rehearsal servers. Defaults to the target server's
    /// version for `up`, and to the version this tool is built for otherwise.
    #[arg(long, env = "IGGY_MIGRATE_REHEARSAL_IMAGE", global = true)]
    rehearsal_image: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show applied, pending and changed migrations. Exits 1 if any are
    /// CHANGED, PARTIAL or MISSING.
    Status {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Show what `up` would do, without changing anything.
    Plan {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
        /// Exit 2 instead of 0 when there is something to apply.
        #[arg(long)]
        detailed_exitcode: bool,
    },
    /// Apply all pending migrations, after rehearsing them on a throwaway
    /// copy of the target server in Docker.
    Up {
        /// Apply without rehearsing first.
        #[arg(long)]
        skip_rehearsal: bool,
        /// Allow pending migrations to delete streams, topics or consumer
        /// groups, and the messages and offsets in them.
        #[arg(long, env = "IGGY_MIGRATE_ALLOW_DELETES")]
        allow_deletes: bool,
    },
    /// Compare the server with what the applied migrations created, to find
    /// resources created, changed or deleted by hand. Exits 1 on drift.
    Drift {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Replay every migration onto an empty throwaway server in Docker, to
    /// check that the full history applies from scratch. Needs no target server.
    Verify {
        /// Verify every environment in --environments, not just --env.
        #[arg(long)]
        all_envs: bool,
    },
    /// Replace migrations with one baseline that creates their final state.
    /// Servers that applied all of them mark it applied without running it.
    Squash {
        /// Squash migrations up to and including this id (default: all).
        #[arg(long)]
        through: Option<String>,
    },
    /// Create a new, empty migration file.
    New { name: String },
    /// Evaluate one migration file and print its ops, without connecting.
    Eval { file: PathBuf },
}

#[tokio::main]
async fn main() -> ExitCode {
    // On Ctrl-C the `run` future is dropped, which removes any rehearsal
    // container it started.
    let result = tokio::select! {
        result = run(Cli::parse()) => result,
        _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("interrupted")),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let env = dhall::Env { name: cli.env.clone(), all: cli.environments.clone() };
    match &cli.command {
        Command::New { name } => {
            let path = migrations::create(&cli.dir, name)?;
            out!("created {}", path.display());
            return Ok(ExitCode::SUCCESS);
        }
        Command::Eval { file } => {
            let ops = dhall::eval(&std::fs::read_to_string(file)?, &env)?;
            for op in &ops {
                out!("{op:?}");
            }
            out!("checksum: {}", dhall::checksum(&ops));
            return Ok(ExitCode::SUCCESS);
        }
        Command::Verify { all_envs } => {
            let envs = if *all_envs { cli.environments.clone() } else { vec![cli.env.clone()] };
            let image = cli.rehearsal_image.as_deref().unwrap_or(rehearsal::DEFAULT_IMAGE);
            let server = rehearsal::Server::start(image).await?;
            for name in envs {
                let env = dhall::Env { name: name.clone(), all: cli.environments.clone() };
                let migrations = migrations::load_dir(&cli.dir, &env)?;
                server.reset().await?;
                let all: Vec<&Migration> = migrations.iter().collect();
                let s = rehearsal::apply(&server.client, &all)
                    .await
                    .with_context(|| format!("env {name}: history does not apply from scratch"))?;
                out!(
                    "env {name}: {} migration(s) apply from scratch ({} op(s) applied, {} skipped)",
                    s.migrations, s.applied, s.skipped
                );
            }
            return Ok(ExitCode::SUCCESS);
        }
        Command::Squash { through } => {
            let image = cli.rehearsal_image.as_deref().unwrap_or(rehearsal::DEFAULT_IMAGE);
            squash::run(&cli.dir, &cli.environments, through.as_deref(), image).await?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Status { .. } | Command::Plan { .. } | Command::Up { .. } | Command::Drift { .. } => {}
    }

    let migrations = migrations::load_dir(&cli.dir, &env)?;
    let client = IggyClient::from_connection_string(&cli.url)?;
    client.connect().await?;
    let events = ledger::read(&client).await?;
    let report = Report::new(&migrations, &events, &env)?;

    match cli.command {
        Command::Status { json } => {
            if json {
                out!("{}", serde_json::to_string_pretty(&report.to_json(&env))?);
            } else {
                report.print();
            }
            Ok(if report.is_clean() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
        Command::Plan { json, detailed_exitcode } => {
            report.ensure_clean()?;
            let plan = plan(&client, &report).await?;
            if json {
                let mark: Vec<&str> = report.to_mark().map(|m| m.id.as_str()).collect();
                let doc = json!({ "env": env.name, "mark": mark, "migrations": plan_json(&plan) });
                out!("{}", serde_json::to_string_pretty(&doc)?);
            } else {
                for m in report.to_mark() {
                    out!("{}\n  mark applied (all {} replaced migrations are applied)", m.id, m.direct_replaces().count());
                }
                for (m, actions) in &plan {
                    out!("{}", m.id);
                    for (op, action) in m.ops.iter().zip(actions) {
                        out!("  {:<34} {op:?}", action_label(op, action));
                    }
                }
                if report.is_up_to_date() {
                    out!("nothing to apply");
                }
            }
            Ok(if detailed_exitcode && !report.is_up_to_date() { ExitCode::from(2) } else { ExitCode::SUCCESS })
        }
        Command::Up { skip_rehearsal, allow_deletes } => {
            report.ensure_clean()?;
            if report.is_up_to_date() {
                out!("nothing to apply");
                return Ok(ExitCode::SUCCESS);
            }
            if !allow_deletes {
                ensure_no_deletes(&report)?;
            }
            if !skip_rehearsal && report.pending().next().is_some() {
                rehearse(&client, &report, cli.rehearsal_image.as_deref()).await?;
            }
            ledger::ensure(&client).await?;
            for (m, state) in &report.rows {
                match state {
                    State::Pending { .. } => apply(&client, m, &env).await?,
                    State::Mark => mark(&client, m, &env).await?,
                    _ => {}
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Drift { json } => {
            report.ensure_clean()?;
            let applied = report.applied();
            let items = drift::detect(&client, &applied).await?;
            if json {
                let doc = json!({ "env": env.name, "drift": !items.is_empty(), "items": items });
                out!("{}", serde_json::to_string_pretty(&doc)?);
            } else {
                for i in &items {
                    let kind = serde_json::to_value(i.kind)?;
                    let kind = kind.as_str().unwrap_or_default();
                    match &i.details {
                        Some(d) => out!("{kind:<10} {} {}: {d}", i.resource, i.key),
                        None => out!("{kind:<10} {} {}", i.resource, i.key),
                    }
                }
                if items.is_empty() {
                    out!("no drift: the server matches the {} applied migration(s)", applied.len());
                }
            }
            Ok(if items.is_empty() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
        Command::New { .. } | Command::Eval { .. } | Command::Verify { .. } | Command::Squash { .. } => {
            unreachable!()
        }
    }
}

type Plan<'a> = Vec<(&'a Migration, Vec<Action>)>;

/// Dry-runs the pending migrations against the server, in order.
async fn plan<'a>(client: &IggyClient, report: &Report<'a>) -> Result<Plan<'a>> {
    let mut planned = Planned::default();
    let mut plan = Vec::new();
    for m in report.pending() {
        let mut actions = Vec::new();
        for (i, op) in m.ops.iter().enumerate() {
            let action = executor::run(client, op, Mode::DryRun(&mut planned))
                .await
                .with_context(|| format!("{}, op {} ({op:?})", m.id, i + 1))?;
            actions.push(action);
        }
        plan.push((m, actions));
    }
    Ok(plan)
}

fn plan_json(plan: &Plan<'_>) -> Value {
    plan.iter()
        .map(|(m, actions)| {
            let ops: Vec<Value> = m
                .ops
                .iter()
                .zip(actions)
                .map(|(op, action)| match action {
                    Action::Apply => json!({ "op": op, "action": "apply", "deletes": op.deletes() }),
                    Action::Skip(reason) => json!({ "op": op, "action": "skip", "reason": reason }),
                })
                .collect();
            json!({ "id": m.id, "ops": ops })
        })
        .collect()
}

fn action_label(op: &op::Op, action: &Action) -> String {
    match action {
        Action::Apply if op.deletes() => "DELETE".into(),
        _ => action.to_string(),
    }
}

/// Refuses to apply deletes unless asked to, since they drop messages and
/// consumer offsets that no migration can bring back.
fn ensure_no_deletes(report: &Report<'_>) -> Result<()> {
    let deletes: Vec<String> = report
        .pending()
        .flat_map(|m| m.ops.iter().filter(|op| op.deletes()).map(move |op| format!("  {}: {op:?}", m.id)))
        .collect();
    if !deletes.is_empty() {
        eprintln!("pending migrations delete resources:\n{}", deletes.join("\n"));
        bail!(
            "deleting drops their messages and consumer offsets for good; review with `plan`, then \
             rerun with --allow-deletes (or IGGY_MIGRATE_ALLOW_DELETES=true)"
        );
    }
    Ok(())
}

/// Applies the pending migrations to a throwaway copy of the target server.
async fn rehearse(client: &IggyClient, report: &Report<'_>, image: Option<&str>) -> Result<()> {
    let version = rehearsal::server_version(client).await?;
    let image = image.map_or_else(|| format!("apache/iggy:{version}"), str::to_string);
    let server = rehearsal::Server::start(&image).await?;
    if server.version != version {
        eprintln!(
            "warning: rehearsal server runs {} but the target runs {version}",
            server.version
        );
    }
    let copied = rehearsal::copy_state(client, &server.client)
        .await
        .context("copying the target's streams, topics and groups to the rehearsal server")?;
    let pending: Vec<&Migration> = report.pending().collect();
    let s = rehearsal::apply(&server.client, &pending)
        .await
        .context("rehearsal failed, nothing was applied to the target")?;
    out!(
        "rehearsal passed on a copy of {copied} resource(s): {} migration(s), {} op(s) applied, {} skipped\n",
        s.migrations, s.applied, s.skipped
    );
    Ok(())
}

async fn apply(client: &IggyClient, m: &Migration, env: &dhall::Env) -> Result<()> {
    out!("{}", m.id);
    let started = Instant::now();
    let mut result = Ok(());
    for op in &m.ops {
        match executor::run(client, op, Mode::Apply).await {
            Ok(action) => out!("  {action:<34} {op:?}"),
            Err(e) => {
                out!("  {:<34} {op:?}", "FAILED");
                result = Err(e);
                break;
            }
        }
    }
    let status = if result.is_ok() { Status::Applied } else { Status::Failed };
    let mut event = event(m, env, status, m.ops.clone(), started);
    event.error = result.as_ref().err().map(|e| format!("{e:#}"));
    ledger::append(client, &event).await?;
    result.map_err(|e| e.context(format!("{} failed; fix it and run `up` again", m.id)))
}

/// Records a baseline as applied without running it.
async fn mark(client: &IggyClient, m: &Migration, env: &dhall::Env) -> Result<()> {
    out!("{}\n  marked applied (all {} replaced migrations are applied)", m.id, m.direct_replaces().count());
    ledger::append(client, &event(m, env, Status::Baselined, Vec::new(), Instant::now())).await
}

fn event(m: &Migration, env: &dhall::Env, status: Status, ops: Vec<op::Op>, started: Instant) -> Event {
    Event {
        id: m.id.clone(),
        status,
        checksum: m.checksum.clone(),
        env: env.name.clone(),
        ops,
        error: None,
        replaces: m.replaces.iter().map(|r| r.id.clone()).collect(),
        applied_at: humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
        runner: std::env::var("IGGY_MIGRATE_RUNNER")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "unknown".into()),
        tool_version: env!("CARGO_PKG_VERSION").into(),
    }
}

enum State<'a> {
    Applied(&'a Event),
    Pending { out_of_order: bool },
    Changed(&'a Event),
    /// A baseline whose replaced migrations are all applied: mark it applied.
    Mark,
    /// A baseline whose replaced migrations are only partly applied, or were
    /// applied with different checksums.
    Partial { not_applied: Vec<String>, mismatched: Vec<String> },
}

struct Report<'a> {
    rows: Vec<(&'a Migration, State<'a>)>,
    /// Applied in the ledger, but no longer present as a file nor replaced by
    /// a baseline.
    missing: Vec<&'a Event>,
}

impl<'a> Report<'a> {
    fn new(migrations: &'a [Migration], events: &'a [Event], env: &dhall::Env) -> Result<Self> {
        let mut applied: HashMap<&str, &Event> = HashMap::new();
        for e in events.iter().filter(|e| matches!(e.status, Status::Applied | Status::Baselined)) {
            if e.env != env.name {
                bail!(
                    "this server's ledger was migrated as env `{}`, but --env is `{}`; \
                     check that you are pointing at the right server",
                    e.env,
                    env.name
                );
            }
            applied.entry(&e.id).or_insert(e);
        }
        let latest_applied = applied.keys().max().copied();
        let rows = migrations
            .iter()
            .map(|m| {
                let state = match applied.get(m.id.as_str()) {
                    Some(e) if e.checksum == m.checksum => State::Applied(e),
                    Some(e) => State::Changed(e),
                    None if m.is_baseline() => baseline_state(m, &applied),
                    None => State::Pending {
                        out_of_order: latest_applied.is_some_and(|l| m.id.as_str() < l),
                    },
                };
                (m, state)
            })
            .collect();
        // Ids replaced by a baseline, either one still on disk or one the
        // ledger recorded (so squashing a baseline again keeps covering the
        // migrations it replaced).
        let replaced: HashSet<&str> = migrations
            .iter()
            .flat_map(|m| m.replaces.iter().map(|r| r.id.as_str()))
            .chain(applied.values().flat_map(|e| e.replaces.iter().map(String::as_str)))
            .collect();
        let mut missing: Vec<&Event> = applied
            .values()
            .filter(|e| !migrations.iter().any(|m| m.id == e.id) && !replaced.contains(e.id.as_str()))
            .copied()
            .collect();
        missing.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Report { rows, missing })
    }

    fn pending(&self) -> impl Iterator<Item = &'a Migration> + '_ {
        self.rows.iter().filter(|(_, s)| matches!(s, State::Pending { .. })).map(|(m, _)| *m)
    }

    fn to_mark(&self) -> impl Iterator<Item = &'a Migration> + '_ {
        self.rows.iter().filter(|(_, s)| matches!(s, State::Mark)).map(|(m, _)| *m)
    }

    /// Migrations whose ops this server's state reflects: applied ones, and
    /// baselines about to be marked because what they replace is applied.
    fn applied(&self) -> Vec<&'a Migration> {
        self.rows
            .iter()
            .filter(|(_, s)| matches!(s, State::Applied(_) | State::Mark))
            .map(|(m, _)| *m)
            .collect()
    }

    fn is_up_to_date(&self) -> bool {
        self.pending().next().is_none() && self.to_mark().next().is_none()
    }

    fn is_clean(&self) -> bool {
        self.missing.is_empty()
            && !self.rows.iter().any(|(_, s)| matches!(s, State::Changed(_) | State::Partial { .. }))
    }

    fn ensure_clean(&self) -> Result<()> {
        if !self.is_clean() {
            for line in self.lines() {
                eprintln!("{line}");
            }
            bail!("refusing to continue until the problems above are fixed");
        }
        Ok(())
    }

    fn print(&self) {
        for line in self.lines() {
            out!("{line}");
        }
    }

    fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        macro_rules! push {
            ($($arg:tt)*) => { lines.push(format!($($arg)*)) };
        }
        for (m, state) in &self.rows {
            match state {
                State::Applied(e) => push!("applied   {}  ({} by {})", m.id, e.applied_at, e.runner),
                State::Pending { out_of_order: false } => push!("pending   {}", m.id),
                State::Pending { out_of_order: true } => {
                    push!("pending   {}  (older than the latest applied migration)", m.id)
                }
                State::Changed(e) => push!(
                    "CHANGED   {}  (applied {} with {}, file now evaluates to {}); restore it and \
                     add a new migration instead",
                    m.id, e.applied_at, e.checksum, m.checksum
                ),
                State::Mark => push!(
                    "pending   {}  (baseline; will be marked applied, all {} replaced migrations are applied)",
                    m.id,
                    m.direct_replaces().count()
                ),
                State::Partial { not_applied, mismatched } => {
                    push!("PARTIAL   {}  (baseline, but this server is partway through the migrations it replaces)", m.id);
                    if !not_applied.is_empty() {
                        push!("          not applied here: {}", not_applied.join(", "));
                    }
                    if !mismatched.is_empty() {
                        push!("          applied with a different checksum: {}", mismatched.join(", "));
                    }
                    push!(
                        "          apply them from the commit before the squash, then run `up` again"
                    );
                }
            }
        }
        for e in &self.missing {
            push!(
                "MISSING   {}  (applied {} but the file is gone); restore it",
                e.id, e.applied_at
            );
        }
        if self.rows.is_empty() && self.missing.is_empty() {
            push!("no migrations");
        }
        lines
    }
}

impl Report<'_> {
    fn to_json(&self, env: &dhall::Env) -> Value {
        let migrations: Vec<Value> = self
            .rows
            .iter()
            .map(|(m, state)| {
                let mut row = json!({ "id": m.id, "checksum": m.checksum });
                let fields = match state {
                    State::Applied(e) => json!({ "state": "applied", "applied_at": e.applied_at, "runner": e.runner }),
                    State::Pending { out_of_order } => json!({ "state": "pending", "out_of_order": out_of_order }),
                    State::Changed(e) => json!({
                        "state": "changed", "applied_at": e.applied_at, "applied_checksum": e.checksum,
                    }),
                    State::Mark => json!({ "state": "mark" }),
                    State::Partial { not_applied, mismatched } => {
                        json!({ "state": "partial", "not_applied": not_applied, "mismatched": mismatched })
                    }
                };
                if let (Value::Object(row), Value::Object(fields)) = (&mut row, fields) {
                    row.extend(fields);
                }
                row
            })
            .collect();
        let missing: Vec<Value> = self
            .missing
            .iter()
            .map(|e| json!({ "id": e.id, "checksum": e.checksum, "applied_at": e.applied_at }))
            .collect();
        json!({
            "env": env.name,
            "clean": self.is_clean(),
            "up_to_date": self.is_up_to_date(),
            "migrations": migrations,
            "missing": missing,
        })
    }
}

/// How a baseline that is not in the ledger applies to this server: run it
/// on a server that applied none of the migrations it replaces, mark it on
/// one that applied all of them, and refuse anything in between. A replaced
/// baseline counts as applied if everything it replaced was.
fn baseline_state<'a>(m: &Migration, applied: &HashMap<&str, &'a Event>) -> State<'a> {
    let progress: Vec<Progress> = m.direct_replaces().map(|r| progress(m, r, applied)).collect();
    if progress.iter().all(|p| p.done) {
        return State::Mark;
    }
    if !progress.iter().any(|p| p.touched) {
        return State::Pending { out_of_order: false };
    }
    let mut not_applied = Vec::new();
    let mut mismatched = Vec::new();
    for p in progress {
        not_applied.extend(p.not_applied);
        mismatched.extend(p.mismatched);
    }
    State::Partial { not_applied, mismatched }
}

#[derive(Default)]
struct Progress {
    /// Applied with a matching checksum, directly or through what it replaced.
    done: bool,
    /// Anything of it is applied.
    touched: bool,
    not_applied: Vec<String>,
    mismatched: Vec<String>,
}

fn progress(m: &Migration, r: &Replaced, applied: &HashMap<&str, &Event>) -> Progress {
    match applied.get(r.id.as_str()) {
        Some(e) if e.checksum == r.checksum => return Progress { done: true, touched: true, ..Default::default() },
        Some(_) => {
            return Progress { touched: true, mismatched: vec![r.id.clone()], ..Default::default() };
        }
        None => {}
    }
    let children: Vec<Progress> = m
        .replaces
        .iter()
        .filter(|c| c.parent.as_deref() == Some(r.id.as_str()))
        .map(|c| progress(m, c, applied))
        .collect();
    if children.is_empty() {
        return Progress { not_applied: vec![r.id.clone()], ..Default::default() };
    }
    let mut p = Progress {
        done: children.iter().all(|c| c.done),
        touched: children.iter().any(|c| c.touched),
        ..Default::default()
    };
    for c in children {
        p.not_applied.extend(c.not_applied);
        p.mismatched.extend(c.mismatched);
    }
    p
}
