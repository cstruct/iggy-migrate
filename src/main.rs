mod config;
mod drift;
mod executor;
mod ledger;
mod lsp;
mod migrations;
mod op;
mod rehearsal;
mod squash;
mod star;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
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
use ledger::{Event, Ledger, Status};
use migrations::{Migration, Replaced};
pub(crate) use out;

#[derive(Parser)]
#[command(version, about = "Forward-only migrations for Iggy streams, topics and consumer groups")]
struct Cli {
    /// Config file (default: iggy-migrate.styx, if it exists).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Secrets file, the only place for passwords and tokens (default: the
    /// config file's name with `.secrets.styx`, if it exists).
    #[arg(long, global = true)]
    secrets: Option<PathBuf>,
    /// Iggy connection string without the password (`iggy.url`).
    #[arg(long, global = true)]
    url: Option<String>,
    /// Directory containing migration files (`dir`).
    #[arg(long, global = true)]
    dir: Option<String>,
    /// Environment being migrated, exposed to migrations as `env` (`env`).
    #[arg(long, global = true)]
    env: Option<String>,
    /// Every known environment, comma separated (`environments`).
    #[arg(long, global = true)]
    environments: Option<String>,
    /// Docker image for rehearsal servers (`rehearsal.image`).
    #[arg(long, global = true)]
    rehearsal_image: Option<String>,
    #[command(subcommand)]
    command: Command,
}

impl Cli {
    fn flags(&self) -> config::Flags {
        let mut values = Vec::new();
        let mut set = |path, value: &Option<String>| {
            if let Some(v) = value {
                values.push((path, v.clone()));
            }
        };
        set("iggy.url", &self.url);
        set("dir", &self.dir);
        set("env", &self.env);
        set("environments", &self.environments);
        set("rehearsal.image", &self.rehearsal_image);
        if let Command::Up { allow_deletes: true, .. } = self.command {
            values.push(("allow_deletes", "true".into()));
        }
        config::Flags { config: self.config.clone(), secrets: self.secrets.clone(), values }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Show applied, pending and changed migrations. Exits 2 if any are
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
        /// groups, and the messages and offsets in them (`allow_deletes`).
        #[arg(long)]
        allow_deletes: bool,
    },
    /// Compare the server with what the applied migrations created, to find
    /// resources created, changed or deleted by hand. Exits 2 on drift.
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
    /// Run a language server for migration files on stdin/stdout, for editors.
    Lsp,
    /// Evaluate one migration file and print its ops, without connecting.
    Eval { file: PathBuf },
    /// Manage the ledger.
    #[command(subcommand)]
    Ledger(LedgerCommand),
}

#[derive(Subcommand)]
enum LedgerCommand {
    /// Copy the events of this server's Iggy ledger into the configured
    /// Postgres ledger, once, when switching to Postgres.
    Import,
    /// List every event in the ledger, oldest first, failed runs and repairs
    /// included.
    History {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Show every event recorded for one migration, with its ops and errors.
    Show {
        id: String,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Record a migration as applied with its file's current checksum,
    /// without running it: for a CHANGED migration whose edit is intended, or
    /// a pending one whose changes were already made by hand.
    Accept {
        id: String,
        /// Why, recorded in the ledger.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Forget that a migration was applied: it becomes pending again or, if
    /// its file is gone, is no longer MISSING. Changes nothing on the server.
    Forget {
        id: String,
        /// Why, recorded in the ledger.
        #[arg(long)]
        reason: Option<String>,
    },
}

/// What `status`, `plan --detailed-exitcode` and `drift` exit with when they
/// find something. Errors, including invalid arguments, exit 1, so CI can tell
/// the two apart.
fn findings() -> ExitCode {
    ExitCode::from(2)
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            // clap exits 2 on invalid arguments, which would read as findings.
            return if e.use_stderr() { ExitCode::FAILURE } else { ExitCode::SUCCESS };
        }
    };
    // On Ctrl-C the `run` future is dropped, which removes any rehearsal
    // container it started.
    let result = tokio::select! {
        result = run(cli) => result,
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
    let config = config::load(cli.flags())?;
    let environments = config.environments();
    let dir = config.dir();
    let env = migrations::Env { name: config.env.clone(), all: environments.clone() };
    match &cli.command {
        Command::New { name } => {
            let path = migrations::create(&dir, name)?;
            out!("created {}", path.display());
            return Ok(ExitCode::SUCCESS);
        }
        Command::Eval { file } => {
            let ops = migrations::eval_file(file, &env)?;
            for op in &ops {
                out!("{op:?}");
            }
            out!("checksum: {}", migrations::checksum(&ops));
            return Ok(ExitCode::SUCCESS);
        }
        Command::Lsp => {
            tokio::task::spawn_blocking(move || lsp::run(environments)).await??;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Verify { all_envs } => {
            let envs = if *all_envs { environments.clone() } else { vec![config.env.clone()] };
            let image = config.rehearsal.image.as_deref().unwrap_or(rehearsal::DEFAULT_IMAGE);
            let server = rehearsal::Server::start(image).await?;
            for name in envs {
                let env = migrations::Env { name: name.clone(), all: environments.clone() };
                let migrations = migrations::load_dir(&dir, &env)?;
                server.reset().await?;
                let all: Vec<&Migration> = migrations.iter().collect();
                let s = rehearsal::apply(&server.client, &all)
                    .await
                    .with_context(|| format!("env {name}: history does not apply from scratch"))?;
                out!(
                    "env {name}: {} migration(s) apply from scratch ({} op(s) applied, {} skipped)",
                    s.migrations,
                    s.applied,
                    s.skipped
                );
            }
            return Ok(ExitCode::SUCCESS);
        }
        Command::Squash { through } => {
            let image = config.rehearsal.image.as_deref().unwrap_or(rehearsal::DEFAULT_IMAGE);
            squash::run(&dir, &environments, through.as_deref(), image).await?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Status { .. }
        | Command::Plan { .. }
        | Command::Up { .. }
        | Command::Drift { .. }
        | Command::Ledger(_) => {}
    }

    // The SDK retries an unreachable server forever, so bound it.
    let connect_timeout = config::parse_duration("iggy.connect_timeout", &config.iggy.connect_timeout)?;
    let client = IggyClient::from_connection_string(&config.iggy_connection_string()?)?;
    tokio::time::timeout(connect_timeout, client.connect())
        .await
        .map_err(|_| {
            anyhow!(
                "could not reach Iggy at {} within {} (iggy.connect_timeout)",
                config.iggy.url,
                humantime::format_duration(connect_timeout)
            )
        })?
        .context("connecting to Iggy")?;
    let ledger = open_ledger(&config, &client).await?;
    // The ledger alone, so a broken migration file doesn't hide its history.
    match &cli.command {
        Command::Ledger(LedgerCommand::Import) => return import(&client, &ledger).await,
        Command::Ledger(LedgerCommand::History { json }) => {
            print_history(&read_events(&client, &ledger).await?, *json)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Ledger(LedgerCommand::Show { id, json }) => {
            print_show(&read_events(&client, &ledger).await?, id, *json)?;
            return Ok(ExitCode::SUCCESS);
        }
        _ => {}
    }
    let migrations = migrations::load_dir(&dir, &env)?;
    if let Command::Up { .. } | Command::Ledger(LedgerCommand::Accept { .. } | LedgerCommand::Forget { .. }) =
        cli.command
    {
        // Before reading, so a runner that waited sees what the other applied.
        ledger.lock().await?;
    }
    let events = read_events(&client, &ledger).await?;
    let report = Report::new(&migrations, &events, &env)?;
    let runner = config.runner();

    match cli.command {
        Command::Status { json } => {
            if json {
                out!("{}", serde_json::to_string_pretty(&report.to_json(&env))?);
            } else {
                report.print();
            }
            Ok(if report.is_clean() { ExitCode::SUCCESS } else { findings() })
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
                    out!(
                        "{}\n  mark applied (all {} replaced migrations are applied)",
                        m.id,
                        m.direct_replaces().count()
                    );
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
            Ok(if detailed_exitcode && !report.is_up_to_date() { findings() } else { ExitCode::SUCCESS })
        }
        Command::Up { skip_rehearsal, .. } => {
            report.ensure_clean()?;
            if report.is_up_to_date() {
                out!("nothing to apply");
                return Ok(ExitCode::SUCCESS);
            }
            if !config.allow_deletes {
                ensure_no_deletes(&report)?;
            }
            if !skip_rehearsal && report.pending().next().is_some() {
                rehearse(&client, &report, config.rehearsal.image.as_deref()).await?;
            }
            ledger.ensure().await?;
            for (m, state) in &report.rows {
                match state {
                    State::Pending { .. } => apply(&client, &ledger, m, &env, &runner).await?,
                    State::Mark => mark(&ledger, m, &env, &runner).await?,
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
            Ok(if items.is_empty() { ExitCode::SUCCESS } else { findings() })
        }
        Command::Ledger(LedgerCommand::Accept { id, reason }) => {
            accept(&ledger, &report, &id, reason, &env, &runner).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Ledger(LedgerCommand::Forget { id, reason }) => {
            forget(&ledger, &report, &id, reason, &env, &runner).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::New { .. }
        | Command::Eval { .. }
        | Command::Lsp
        | Command::Verify { .. }
        | Command::Squash { .. }
        | Command::Ledger(LedgerCommand::Import | LedgerCommand::History { .. } | LedgerCommand::Show { .. }) => {
            unreachable!()
        }
    }
}

async fn open_ledger<'a>(config: &config::Config, client: &'a IggyClient) -> Result<Ledger<'a>> {
    Ok(match &config.ledger {
        config::Ledger::Iggy => Ledger::Iggy(client),
        config::Ledger::Postgres { url, password, namespace, table, lock_timeout } => {
            let runner = config.runner();
            Ledger::Postgres(Box::new(
                ledger::postgres::Ledger::connect(ledger::postgres::Options {
                    url,
                    password: password.as_deref(),
                    namespace: namespace.as_deref().unwrap_or(&config.env),
                    table,
                    lock_timeout: config::parse_duration("ledger.lock_timeout", lock_timeout)?,
                    runner: &runner,
                })
                .await?,
            ))
        }
    })
}

/// Every event, refusing a Postgres ledger that is empty only because the
/// server's Iggy ledger has not been imported yet.
async fn read_events(client: &IggyClient, ledger: &Ledger<'_>) -> Result<Vec<Event>> {
    let events = ledger.read().await?;
    if events.is_empty() && matches!(ledger, Ledger::Postgres(_)) {
        ensure_nothing_to_import(client).await?;
    }
    Ok(events)
}

/// With a Postgres ledger that has nothing for this namespace yet, refuses a
/// server whose Iggy ledger has history, which would otherwise look new.
async fn ensure_nothing_to_import(client: &IggyClient) -> Result<()> {
    let events = ledger::iggy::read(client).await?;
    if !events.is_empty() {
        bail!(
            "the Postgres ledger has no events for this server, but its Iggy ledger has {}; \
             copy them over with `iggy-migrate ledger import` first",
            events.len()
        );
    }
    Ok(())
}

/// Copies the Iggy ledger into an empty Postgres namespace, under the lock.
async fn import(client: &IggyClient, ledger: &Ledger<'_>) -> Result<ExitCode> {
    let Ledger::Postgres(postgres) = ledger else {
        bail!("`ledger import` copies into a Postgres ledger; set `ledger {{ type postgres ... }}` first");
    };
    ledger.lock().await?;
    if !ledger.read().await?.is_empty() {
        bail!("ledger namespace `{}` already has events; refusing to import into it", postgres.namespace());
    }
    let events = ledger::iggy::read(client).await?;
    if events.is_empty() {
        bail!("this server has no Iggy ledger events to import");
    }
    ledger.ensure().await?;
    for event in &events {
        ledger.append(event).await?;
    }
    out!(
        "imported {} event(s) into ledger namespace `{}`. The Iggy ledger (stream `{}`) is left as it \
         was; delete it once you have checked `status`.",
        events.len(),
        postgres.namespace(),
        ledger::STREAM
    );
    Ok(ExitCode::SUCCESS)
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
             rerun with --allow-deletes (or IGGY_MIGRATE__ALLOW_DELETES=true)"
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
        eprintln!("warning: rehearsal server runs {} but the target runs {version}", server.version);
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
        s.migrations,
        s.applied,
        s.skipped
    );
    Ok(())
}

async fn apply(
    client: &IggyClient,
    ledger: &Ledger<'_>,
    m: &Migration,
    env: &migrations::Env,
    runner: &str,
) -> Result<()> {
    out!("{}", m.id);
    let started = Instant::now();
    let mut result = Ok(());
    for op in &m.ops {
        match executor::run(client, op, Mode::Apply).await {
            Ok(action) => out!("  {:<34} {op:?}", action.to_string()),
            Err(e) => {
                out!("  {:<34} {op:?}", "FAILED");
                result = Err(e);
                break;
            }
        }
    }
    let status = if result.is_ok() { Status::Applied } else { Status::Failed };
    let mut event = event(m, env, runner, status, m.ops.clone(), started);
    event.error = result.as_ref().err().map(|e| format!("{e:#}"));
    ledger.append(&event).await?;
    result.map_err(|e| e.context(format!("{} failed; fix it and run `up` again", m.id)))
}

/// Records a baseline as applied without running it.
async fn mark(ledger: &Ledger<'_>, m: &Migration, env: &migrations::Env, runner: &str) -> Result<()> {
    out!("{}\n  marked applied (all {} replaced migrations are applied)", m.id, m.direct_replaces().count());
    ledger.append(&event(m, env, runner, Status::Baselined, Vec::new(), Instant::now())).await
}

fn event(
    m: &Migration,
    env: &migrations::Env,
    runner: &str,
    status: Status,
    ops: Vec<op::Op>,
    started: Instant,
) -> Event {
    Event {
        id: m.id.clone(),
        status,
        checksum: m.checksum.clone(),
        env: env.name.clone(),
        ops,
        error: None,
        reason: None,
        replaces: m.replaces.iter().map(|r| r.id.clone()).collect(),
        applied_at: humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
        runner: runner.into(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
    }
}

/// Records a migration as applied with its file's current checksum, without
/// running its ops.
async fn accept(
    ledger: &Ledger<'_>,
    report: &Report<'_>,
    id: &str,
    reason: Option<String>,
    env: &migrations::Env,
    runner: &str,
) -> Result<()> {
    let Some((m, state)) = report.rows.iter().find(|(m, _)| m.id == id) else {
        bail!("there is no migration {id} in the migrations directory");
    };
    let was = match state {
        State::Applied(_) => bail!("{id} is already applied with {}; there is nothing to accept", m.checksum),
        State::Changed(e) => format!("applied with {}", e.checksum),
        State::Pending { .. } | State::Mark => "pending".into(),
        State::Partial { .. } => "PARTIAL".into(),
    };
    ledger.ensure().await?;
    let mut e = event(m, env, runner, Status::Accepted, m.ops.clone(), Instant::now());
    e.reason = reason;
    ledger.append(&e).await?;
    out!("accepted {id} (was {was}): now applied with {}, without running its ops", m.checksum);
    Ok(())
}

/// Records that a migration's earlier applied events no longer count.
async fn forget(
    ledger: &Ledger<'_>,
    report: &Report<'_>,
    id: &str,
    reason: Option<String>,
    env: &migrations::Env,
    runner: &str,
) -> Result<()> {
    let Some(applied) = report.applied.get(id) else {
        bail!("{id} is not applied on this server; there is nothing to forget");
    };
    let event = Event {
        id: id.into(),
        status: Status::Forgotten,
        checksum: applied.checksum.clone(),
        env: env.name.clone(),
        ops: Vec::new(),
        error: None,
        reason,
        replaces: Vec::new(),
        applied_at: humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
        duration_ms: 0,
        runner: runner.into(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
    };
    ledger.append(&event).await?;
    if report.rows.iter().any(|(m, _)| m.id == id) {
        out!("forgot {id}: it is pending again, so `up` will run it; nothing on the server was changed");
    } else {
        out!("forgot {id}: its file is gone and it is no longer MISSING; nothing on the server was changed");
    }
    Ok(())
}

fn print_history(events: &[Event], json: bool) -> Result<()> {
    if json {
        out!("{}", serde_json::to_string_pretty(&json!({ "events": events }))?);
        return Ok(());
    }
    for e in events {
        out!("{}  {:<9}  {}  by {}", e.applied_at, e.status.as_str(), e.id, e.runner);
        if let Some(reason) = &e.reason {
            out!("{:22}{:<11}reason: {reason}", "", "");
        }
        if let Some(error) = &e.error {
            out!("{:22}{:<11}error: {error}", "", "");
        }
    }
    if events.is_empty() {
        out!("the ledger is empty");
    }
    Ok(())
}

fn print_show(events: &[Event], id: &str, json: bool) -> Result<()> {
    let events: Vec<&Event> = events.iter().filter(|e| e.id == id).collect();
    if events.is_empty() {
        bail!("the ledger has no events for {id}");
    }
    if json {
        out!("{}", serde_json::to_string_pretty(&json!({ "id": id, "events": events }))?);
        return Ok(());
    }
    for (i, e) in events.iter().enumerate() {
        if i > 0 {
            out!();
        }
        out!("{}  {}", e.applied_at, e.status.as_str());
        out!("  env       {}", e.env);
        out!("  runner    {}", e.runner);
        out!("  checksum  {}", e.checksum);
        out!("  duration  {} ms", e.duration_ms);
        out!("  version   iggy-migrate {}", e.tool_version);
        if !e.replaces.is_empty() {
            out!("  replaces  {}", e.replaces.join(", "));
        }
        if let Some(reason) = &e.reason {
            out!("  reason    {reason}");
        }
        if let Some(error) = &e.error {
            out!("  error     {error}");
        }
        for op in &e.ops {
            out!("  op        {op:?}");
        }
    }
    Ok(())
}

enum State<'a> {
    Applied(&'a Event),
    Pending {
        out_of_order: bool,
    },
    Changed(&'a Event),
    /// A baseline whose replaced migrations are all applied: mark it applied.
    Mark,
    /// A baseline whose replaced migrations are only partly applied, or were
    /// applied with different checksums.
    Partial {
        not_applied: Vec<String>,
        mismatched: Vec<String>,
    },
}

struct Report<'a> {
    rows: Vec<(&'a Migration, State<'a>)>,
    /// Applied in the ledger, but no longer present as a file nor replaced by
    /// a baseline.
    missing: Vec<&'a Event>,
    /// The event each applied migration's state comes from, by id.
    applied: HashMap<&'a str, &'a Event>,
}

impl<'a> Report<'a> {
    fn new(migrations: &'a [Migration], events: &'a [Event], env: &migrations::Env) -> Result<Self> {
        let mut applied: HashMap<&str, &Event> = HashMap::new();
        for e in events.iter().filter(|e| e.status != Status::Failed) {
            if e.env != env.name {
                bail!(
                    "this server's ledger was migrated as env `{}`, but --env is `{}`; \
                     check that you are pointing at the right server",
                    e.env,
                    env.name
                );
            }
            match e.status {
                // The first applied event wins, so when concurrent runners both
                // apply a migration, the checksum that ran first is kept.
                Status::Applied | Status::Baselined => {
                    applied.entry(&e.id).or_insert(e);
                }
                // Repairs are deliberate, so they win over what came before.
                Status::Accepted => {
                    applied.insert(&e.id, e);
                }
                Status::Forgotten => {
                    applied.remove(e.id.as_str());
                }
                Status::Failed => {}
            }
        }
        let latest_applied = applied.keys().max().copied();
        let rows = migrations
            .iter()
            .map(|m| {
                let state = match applied.get(m.id.as_str()) {
                    Some(e) if e.checksum == m.checksum => State::Applied(e),
                    Some(e) => State::Changed(e),
                    None if m.is_baseline() => baseline_state(m, &applied),
                    None => State::Pending { out_of_order: latest_applied.is_some_and(|l| m.id.as_str() < l) },
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
        Ok(Report { rows, missing, applied })
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
        self.rows.iter().filter(|(_, s)| matches!(s, State::Applied(_) | State::Mark)).map(|(m, _)| *m).collect()
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
                    m.id,
                    e.applied_at,
                    e.checksum,
                    m.checksum
                ),
                State::Mark => push!(
                    "pending   {}  (baseline; will be marked applied, all {} replaced migrations are applied)",
                    m.id,
                    m.direct_replaces().count()
                ),
                State::Partial { not_applied, mismatched } => {
                    push!(
                        "PARTIAL   {}  (baseline, but this server is partway through the migrations it replaces)",
                        m.id
                    );
                    if !not_applied.is_empty() {
                        push!("          not applied here: {}", not_applied.join(", "));
                    }
                    if !mismatched.is_empty() {
                        push!("          applied with a different checksum: {}", mismatched.join(", "));
                    }
                    push!("          apply them from the commit before the squash, then run `up` again");
                }
            }
        }
        for e in &self.missing {
            push!("MISSING   {}  (applied {} but the file is gone); restore it", e.id, e.applied_at);
        }
        if self.rows.is_empty() && self.missing.is_empty() {
            push!("no migrations");
        }
        lines
    }
}

impl Report<'_> {
    fn to_json(&self, env: &migrations::Env) -> Value {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> migrations::Env {
        migrations::Env { name: "dev".into(), all: vec!["dev".into()] }
    }

    fn migration(id: &str, checksum: &str) -> Migration {
        Migration {
            id: id.into(),
            path: PathBuf::new(),
            ops: Vec::new(),
            checksum: checksum.into(),
            replaces: Vec::new(),
        }
    }

    fn event(id: &str, status: Status, checksum: &str) -> Event {
        Event {
            id: id.into(),
            status,
            checksum: checksum.into(),
            env: "dev".into(),
            ops: Vec::new(),
            error: None,
            reason: None,
            replaces: Vec::new(),
            applied_at: "2026-11-03T00:00:00Z".into(),
            duration_ms: 0,
            runner: "test".into(),
            tool_version: "0".into(),
        }
    }

    /// Each migration's state, as `status` prints its first word.
    fn states(report: &Report<'_>) -> Vec<String> {
        let mut states: Vec<String> = report
            .rows
            .iter()
            .map(|(m, s)| {
                let state = match s {
                    State::Applied(_) => "applied",
                    State::Pending { .. } => "pending",
                    State::Changed(_) => "changed",
                    State::Mark => "mark",
                    State::Partial { .. } => "partial",
                };
                format!("{} {state}", m.id)
            })
            .collect();
        states.extend(report.missing.iter().map(|e| format!("{} missing", e.id)));
        states
    }

    const A: &str = "20260101000000_a";
    const B: &str = "20260102000000_b";

    #[test]
    fn the_first_applied_checksum_wins() {
        let migrations = [migration(A, "sha256:2")];
        let events = [event(A, Status::Applied, "sha256:1"), event(A, Status::Applied, "sha256:2")];
        let report = Report::new(&migrations, &events, &env()).unwrap();
        assert_eq!(states(&report), [format!("{A} changed")]);
    }

    #[test]
    fn failed_events_are_ignored() {
        let migrations = [migration(A, "sha256:1")];
        let events = [event(A, Status::Failed, "sha256:1")];
        let report = Report::new(&migrations, &events, &env()).unwrap();
        assert_eq!(states(&report), [format!("{A} pending")]);
    }

    #[test]
    fn accept_replaces_the_applied_checksum() {
        let migrations = [migration(A, "sha256:2"), migration(B, "sha256:3")];
        let events = [
            event(A, Status::Applied, "sha256:1"),
            event(A, Status::Accepted, "sha256:2"),
            event(B, Status::Accepted, "sha256:3"),
        ];
        let report = Report::new(&migrations, &events, &env()).unwrap();
        assert_eq!(states(&report), [format!("{A} applied"), format!("{B} applied")]);
        assert!(report.is_clean() && report.is_up_to_date());
    }

    #[test]
    fn forget_makes_a_migration_pending_or_not_missing() {
        let migrations = [migration(A, "sha256:1")];
        let events = [
            event(A, Status::Applied, "sha256:1"),
            event(B, Status::Applied, "sha256:2"),
            event(A, Status::Forgotten, "sha256:1"),
            event(B, Status::Forgotten, "sha256:2"),
        ];
        let report = Report::new(&migrations, &events, &env()).unwrap();
        assert_eq!(states(&report), [format!("{A} pending")]);
        assert!(report.is_clean());
    }

    #[test]
    fn a_forgotten_migration_can_be_applied_again() {
        let migrations = [migration(A, "sha256:2")];
        let events = [
            event(A, Status::Applied, "sha256:1"),
            event(A, Status::Forgotten, "sha256:1"),
            event(A, Status::Applied, "sha256:2"),
        ];
        let report = Report::new(&migrations, &events, &env()).unwrap();
        assert_eq!(states(&report), [format!("{A} applied")]);
    }

    #[test]
    fn repairs_in_another_env_are_refused() {
        let migrations = [migration(A, "sha256:1")];
        let mut accepted = event(A, Status::Accepted, "sha256:1");
        accepted.env = "prod".into();
        assert!(Report::new(&migrations, &[accepted], &env()).is_err());
    }

    #[test]
    fn events_without_a_reason_still_parse() {
        let json = r#"{"id":"x","status":"applied","checksum":"c","env":"dev","ops":[],
            "applied_at":"t","duration_ms":1,"runner":"r","tool_version":"0.1.1"}"#;
        let e: Event = serde_json::from_str(json).unwrap();
        assert_eq!(e.reason, None);
        assert!(!serde_json::to_string(&e).unwrap().contains("reason"));
    }
}
