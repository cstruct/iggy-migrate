//! Rehearsals: applying migrations to a throwaway Iggy server in Docker
//! before (or instead of) applying them for real. The server catches what
//! `plan` cannot: its own validation rules, version-specific behaviour, and
//! bugs in this tool's model of Iggy.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use iggy::prelude::*;

use crate::executor::{self, Action, Mode};
use crate::ledger;
use crate::migrations::Migration;

/// The server image used when there is no live server to match, e.g. for
/// `verify`. Matches the SDK version this tool is built against.
pub const DEFAULT_IMAGE: &str = "apache/iggy:0.9.0";

const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// A throwaway Iggy server container, removed when dropped.
pub struct Server {
    container: String,
    pub client: IggyClient,
    pub version: String,
}

impl Server {
    pub async fn start(image: &str) -> Result<Server> {
        if docker(&["version", "--format", "{{.Server.Version}}"]).is_err() {
            bail!("rehearsals need a running Docker daemon (use --skip-rehearsal to apply without one)");
        }

        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let container = format!("iggy-migrate-rehearsal-{}-{nanos}", std::process::id());
        eprintln!("starting rehearsal server ({image})");
        let run = Command::new("docker")
            .args(["run", "-d", "--name", &container, "--label", "iggy-migrate=rehearsal"])
            // The server needs io_uring, which Docker's default seccomp
            // profile blocks, and cannot pin cores under Docker Desktop.
            .args(["--security-opt", "seccomp=unconfined", "-p", "127.0.0.1::8090"])
            .args(["-e", "IGGY_TCP_ADDRESS=0.0.0.0:8090", "-e", "IGGY_NODE_ADVERTISED_ADDRESS=127.0.0.1"])
            .args(["-e", "IGGY_SHARDING_CPU_ALLOCATION=2", "-e", "IGGY_SHARDING_PIN_CORES=false"])
            .args(["-e", "IGGY_ROOT_USERNAME=iggy", "-e", "IGGY_ROOT_PASSWORD=iggy", image])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .context("running docker")?;
        // From here on the container exists, so let `Drop` clean it up.
        let guard = Guard(container.clone());
        if !run.success() {
            bail!("could not start rehearsal container from {image}");
        }

        let port = docker(&["port", &container, "8090/tcp"])?;
        let address = port.lines().next().context("rehearsal container exposes no port")?;
        let url = format!("iggy://iggy:iggy@{address}");

        let started = Instant::now();
        loop {
            match connect(&url).await {
                Ok((client, version)) => {
                    std::mem::forget(guard);
                    return Ok(Server { container, client, version });
                }
                Err(e) if started.elapsed() > STARTUP_TIMEOUT => {
                    let logs = docker(&["logs", "--tail", "20", &container]).unwrap_or_default();
                    bail!("rehearsal server did not become ready: {e:#}\n{logs}");
                }
                Err(_) => {
                    if docker(&["inspect", "-f", "{{.State.Running}}", &container])?.trim() != "true" {
                        let logs = docker(&["logs", "--tail", "20", &container]).unwrap_or_default();
                        bail!("rehearsal server exited during startup:\n{logs}");
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
    }

    /// Deletes every stream, returning the server to empty.
    pub async fn reset(&self) -> Result<()> {
        for stream in self.client.get_streams().await? {
            self.client.delete_stream(&Identifier::named(&stream.name)?).await?;
        }
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        remove(&self.container);
    }
}

struct Guard(String);

impl Drop for Guard {
    fn drop(&mut self) {
        remove(&self.0);
    }
}

fn remove(container: &str) {
    let _ = Command::new("docker").args(["rm", "-f", container]).output();
}

fn docker(args: &[&str]) -> Result<String> {
    let out = Command::new("docker").args(args).output().context("running docker")?;
    if !out.status.success() {
        bail!("docker {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8(out.stdout)?)
}

async fn connect(url: &str) -> Result<(IggyClient, String)> {
    let attempt = async {
        let client = IggyClient::from_connection_string(url)?;
        client.connect().await?;
        let version = client.get_stats().await?.iggy_server_version;
        anyhow::Ok((client, version))
    };
    tokio::time::timeout(Duration::from_secs(5), attempt).await.context("timed out connecting")?
}

pub async fn server_version(client: &IggyClient) -> Result<String> {
    Ok(client.get_stats().await?.iggy_server_version)
}

/// Recreates every stream, topic and consumer group from `from` on `to`,
/// except the ledger. Metadata only: no messages are copied.
pub async fn copy_state(from: &IggyClient, to: &IggyClient) -> Result<usize> {
    let mut count = 0;
    for stream in from.get_streams().await? {
        if stream.name == ledger::STREAM {
            continue;
        }
        let stream_id = Identifier::named(&stream.name)?;
        to.create_stream(&stream.name).await?;
        count += 1;
        for topic in from.get_topics(&stream_id).await? {
            let topic_id = Identifier::named(&topic.name)?;
            let options = TopicCreateOptions {
                partitions_count: Some(topic.partitions_count),
                compression_algorithm: Some(topic.compression_algorithm),
                message_expiry: Some(topic.message_expiry),
                max_topic_size: Some(topic.max_topic_size),
                ..Default::default()
            };
            to.create_topic(&stream_id, &topic.name, &options)
                .await
                .with_context(|| format!("copying topic {}/{}", stream.name, topic.name))?;
            count += 1;
            for group in from.get_consumer_groups(&stream_id, &topic_id).await? {
                to.create_consumer_group(&stream_id, &topic_id, &group.name).await?;
                count += 1;
            }
        }
    }
    Ok(count)
}

#[derive(Default)]
pub struct Summary {
    pub migrations: usize,
    pub applied: usize,
    pub skipped: usize,
}

/// Applies `migrations` in order, stopping at the first failure.
pub async fn apply(client: &IggyClient, migrations: &[&Migration]) -> Result<Summary> {
    let mut summary = Summary::default();
    for m in migrations {
        for (i, op) in m.ops.iter().enumerate() {
            match executor::run(client, op, Mode::Apply).await {
                Ok(Action::Apply) => summary.applied += 1,
                Ok(Action::Skip(_)) => summary.skipped += 1,
                Err(e) => {
                    return Err(e.context(format!("{}, op {} ({op:?})", m.id, i + 1)));
                }
            }
        }
        summary.migrations += 1;
    }
    Ok(summary)
}
