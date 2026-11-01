mod dhall;
mod executor;
mod ledger;
mod migrations;
mod op;
mod rehearsal;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use iggy::prelude::*;

use executor::{Mode, Planned};
use ledger::{Event, Status};
use migrations::Migration;

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
    /// Show applied, pending and changed migrations.
    Status,
    /// Show what `up` would do, without changing anything.
    Plan,
    /// Apply all pending migrations, after rehearsing them on a throwaway
    /// copy of the target server in Docker.
    Up {
        /// Apply without rehearsing first.
        #[arg(long)]
        skip_rehearsal: bool,
    },
    /// Replay every migration onto an empty throwaway server in Docker, to
    /// check that the full history applies from scratch. Needs no target server.
    Verify {
        /// Verify every environment in --environments, not just --env.
        #[arg(long)]
        all_envs: bool,
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
            println!("created {}", path.display());
            return Ok(ExitCode::SUCCESS);
        }
        Command::Eval { file } => {
            let ops = dhall::eval(&std::fs::read_to_string(file)?, &env)?;
            for op in &ops {
                println!("{op:?}");
            }
            println!("checksum: {}", dhall::checksum(&ops));
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
                println!(
                    "env {name}: {} migration(s) apply from scratch ({} op(s) applied, {} skipped)",
                    s.migrations, s.applied, s.skipped
                );
            }
            return Ok(ExitCode::SUCCESS);
        }
        Command::Status | Command::Plan | Command::Up { .. } => {}
    }

    let migrations = migrations::load_dir(&cli.dir, &env)?;
    let client = IggyClient::from_connection_string(&cli.url)?;
    client.connect().await?;
    let events = ledger::read(&client).await?;
    let report = Report::new(&migrations, &events, &env)?;

    match cli.command {
        Command::Status => {
            report.print();
            Ok(if report.is_clean() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
        Command::Plan => {
            report.ensure_clean()?;
            let mut planned = Planned::default();
            for m in report.pending() {
                println!("{}", m.id);
                for (i, op) in m.ops.iter().enumerate() {
                    let action = executor::run(&client, op, Mode::DryRun(&mut planned))
                        .await
                        .with_context(|| format!("{}, op {} ({op:?})", m.id, i + 1))?;
                    println!("  {action:<34} {op:?}");
                }
            }
            if report.pending().next().is_none() {
                println!("nothing to apply");
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Up { skip_rehearsal } => {
            report.ensure_clean()?;
            if report.pending().next().is_none() {
                println!("nothing to apply");
                return Ok(ExitCode::SUCCESS);
            }
            if !skip_rehearsal {
                rehearse(&client, &report, cli.rehearsal_image.as_deref()).await?;
            }
            ledger::ensure(&client).await?;
            for m in report.pending() {
                apply(&client, m, &env).await?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::New { .. } | Command::Eval { .. } | Command::Verify { .. } => unreachable!(),
    }
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
    println!(
        "rehearsal passed on a copy of {copied} resource(s): {} migration(s), {} op(s) applied, {} skipped\n",
        s.migrations, s.applied, s.skipped
    );
    Ok(())
}

async fn apply(client: &IggyClient, m: &Migration, env: &dhall::Env) -> Result<()> {
    println!("{}", m.id);
    let started = Instant::now();
    let mut result = Ok(());
    for op in &m.ops {
        match executor::run(client, op, Mode::Apply).await {
            Ok(action) => println!("  {action:<34} {op:?}"),
            Err(e) => {
                println!("  {:<34} {op:?}", "FAILED");
                result = Err(e);
                break;
            }
        }
    }
    let event = Event {
        id: m.id.clone(),
        status: if result.is_ok() { Status::Applied } else { Status::Failed },
        checksum: m.checksum.clone(),
        env: env.name.clone(),
        ops: m.ops.clone(),
        error: result.as_ref().err().map(|e| format!("{e:#}")),
        applied_at: humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
        runner: std::env::var("IGGY_MIGRATE_RUNNER")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "unknown".into()),
        tool_version: env!("CARGO_PKG_VERSION").into(),
    };
    ledger::append(client, &event).await?;
    result.map_err(|e| e.context(format!("{} failed; fix it and run `up` again", m.id)))
}

enum State<'a> {
    Applied(&'a Event),
    Pending { out_of_order: bool },
    Changed(&'a Event),
}

struct Report<'a> {
    rows: Vec<(&'a Migration, State<'a>)>,
    /// Applied in the ledger, but no longer present as a file.
    missing: Vec<&'a Event>,
}

impl<'a> Report<'a> {
    fn new(migrations: &'a [Migration], events: &'a [Event], env: &dhall::Env) -> Result<Self> {
        let mut applied: HashMap<&str, &Event> = HashMap::new();
        for e in events.iter().filter(|e| e.status == Status::Applied) {
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
                    None => State::Pending {
                        out_of_order: latest_applied.is_some_and(|l| m.id.as_str() < l),
                    },
                };
                (m, state)
            })
            .collect();
        let mut missing: Vec<&Event> = applied
            .values()
            .filter(|e| !migrations.iter().any(|m| m.id == e.id))
            .copied()
            .collect();
        missing.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Report { rows, missing })
    }

    fn pending(&self) -> impl Iterator<Item = &'a Migration> + '_ {
        self.rows.iter().filter(|(_, s)| matches!(s, State::Pending { .. })).map(|(m, _)| *m)
    }

    fn is_clean(&self) -> bool {
        self.missing.is_empty() && !self.rows.iter().any(|(_, s)| matches!(s, State::Changed(_)))
    }

    fn ensure_clean(&self) -> Result<()> {
        if !self.is_clean() {
            self.print();
            bail!(
                "applied migrations were changed or removed; restore them and add a new \
                 migration instead"
            );
        }
        Ok(())
    }

    fn print(&self) {
        for (m, state) in &self.rows {
            match state {
                State::Applied(e) => println!("applied   {}  ({} by {})", m.id, e.applied_at, e.runner),
                State::Pending { out_of_order: false } => println!("pending   {}", m.id),
                State::Pending { out_of_order: true } => {
                    println!("pending   {}  (older than the latest applied migration)", m.id)
                }
                State::Changed(e) => println!(
                    "CHANGED   {}  (applied {} with {}, file now evaluates to {})",
                    m.id, e.applied_at, e.checksum, m.checksum
                ),
            }
        }
        for e in &self.missing {
            println!("MISSING   {}  (applied {} but the file is gone)", e.id, e.applied_at);
        }
        if self.rows.is_empty() && self.missing.is_empty() {
            println!("no migrations");
        }
    }
}
