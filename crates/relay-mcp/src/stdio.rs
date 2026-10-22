//! The stdio shim Claude clients launch. It never parses MCP: it finds or
//! starts the agent and copies bytes, so any number of clients share the one
//! process that holds the Signal connection.

use std::{
    fs::OpenOptions,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::Context;
use relay_agent::endpoint;
use relay_core::{ProfileName, ProfilePaths};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// Serves MCP on stdin/stdout by connecting to the profile's agent (starting
/// it if needed) and copying bytes both ways.
pub async fn run_stdio(profile: &ProfileName) -> anyhow::Result<()> {
    let paths = ProfilePaths::for_profile(profile)?;
    let name = endpoint::name(profile, &paths)?;
    let agent = connect_or_spawn(&name, || spawn_agent(profile, &paths), AGENT_START_WAIT)
        .await
        .with_context(|| format!("the agent's log is \"{}\"", paths.agent_log.display()))?;
    let client = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    pipe(client, agent).await
}

/// How long a cold agent gets to open its stores and bind before the shim
/// gives up. Opening SQLCipher and presage dominates.
const AGENT_START_WAIT: Duration = Duration::from_secs(15);

/// Starts `signal-agent agent` for `profile` as a detached background process
/// that outlives this shim, logging to the profile's agent log.
// NOTE: this assumes the shim runs from the signal-agent binary itself, which
// is what Claude clients are registered to launch.
fn spawn_agent(profile: &ProfileName, paths: &ProfilePaths) -> anyhow::Result<()> {
    may_start_agent(profile, paths)?;
    let exe = std::env::current_exe().context("unable to locate the signal-agent executable")?;
    create_private_dir(&paths.root)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.agent_log)
        .with_context(|| format!("unable to open \"{}\"", paths.agent_log.display()))?;
    let mut command = Command::new(&exe);
    command
        .args(["agent", "--profile", profile])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    detach(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("unable to start \"{}\" agent", exe.display()))?;
    // Reap the agent if it exits while this shim is still running.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Refuses to start an agent for a profile without a saved access choice
/// (one never set up, or one being uninstalled, whose agent would otherwise
/// recreate its files and re-add its Claude entries), or while setup runs on
/// it and has its stores open.
fn may_start_agent(profile: &ProfileName, paths: &ProfilePaths) -> anyhow::Result<()> {
    paths.load_access().with_context(|| {
        format!("\"{profile}\" isn't set up on this computer, so its agent isn't started")
    })?;
    if relay_setup::setup_running(paths) {
        anyhow::bail!("setup is running for \"{profile}\"; its agent starts once setup finishes");
    }
    Ok(())
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // Its own process group, so signals aimed at the client's group (Claude
    // quitting) don't take the shared agent down with this shim.
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("unable to create \"{}\"", dir.display()))
}

#[cfg(windows)]
fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    // %LOCALAPPDATA% is already private to the user.
    std::fs::create_dir_all(dir).with_context(|| format!("unable to create \"{}\"", dir.display()))
}

/// Connects to the agent at `endpoint`, calling `spawn` to start it if nobody
/// is listening, then waiting up to `wait` for it to come up.
async fn connect_or_spawn(
    endpoint: &str,
    spawn: impl FnOnce() -> anyhow::Result<()>,
    wait: Duration,
) -> anyhow::Result<impl AsyncRead + AsyncWrite> {
    if let Ok(agent) = endpoint::connect(endpoint).await {
        return Ok(agent);
    }
    spawn()?;
    let deadline = Instant::now() + wait;
    loop {
        match endpoint::connect(endpoint).await {
            Ok(agent) => return Ok(agent),
            Err(e) if Instant::now() >= deadline => {
                return Err(e.context(format!("the agent did not come up within {wait:?}")));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

/// Copies bytes between a client and the agent until the agent hangs up.
// NOTE: the client closing its side only half-closes toward the agent, so
// replies already in flight still reach the client; the shim exits when the
// agent's side ends, which is also how a dead agent ends the session.
pub(crate) async fn pipe<C, A>(client: C, agent: A) -> anyhow::Result<()>
where
    C: AsyncRead + AsyncWrite,
    A: AsyncRead + AsyncWrite,
{
    let (mut client_rx, mut client_tx) = tokio::io::split(client);
    let (mut agent_rx, mut agent_tx) = tokio::io::split(agent);
    let upstream = async {
        tokio::io::copy(&mut client_rx, &mut agent_tx).await?;
        agent_tx.shutdown().await
    };
    let downstream = async {
        tokio::io::copy(&mut agent_rx, &mut client_tx).await?;
        client_tx.shutdown().await
    };
    tokio::pin!(downstream);
    tokio::select! {
        done = &mut downstream => done?,
        sent = upstream => {
            sent?;
            downstream.await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use async_trait::async_trait;
    use relay_agent::Agent;
    use relay_core::prelude::*;
    use relay_signal::{Account, FakeSignal, Outgoing};
    use relay_store::MemoryStore;
    use relay_tools::{Registry, Tool, ToolContext, ToolError};
    use rmcp::{ServiceExt, model::CallToolRequestParams};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;

    /// Sends its `body` to a fixed contact, so the test can count sends.
    struct ProbeSend;

    #[async_trait]
    impl Tool for ProbeSend {
        fn name(&self) -> &'static str {
            "probe_send"
        }
        fn description(&self) -> &'static str {
            "test-only write tool"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {"body": {"type": "string"}}})
        }
        fn writes(&self) -> bool {
            true
        }
        async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
            let Some(body) = input.get("body").and_then(Value::as_str) else {
                return Err(ToolError::InvalidInput("body is required".into()));
            };
            let to = Recipient::Contact(ServiceId::new(Uuid::from_u128(2)));
            let body = body.to_owned();
            ctx.signal
                .send(
                    to,
                    Outgoing::Text {
                        body,
                        quote: None,
                        attachments: vec![],
                    },
                )
                .await?;
            Ok(json!({}))
        }
    }

    fn test_endpoint(dir: &std::path::Path) -> String {
        if cfg!(windows) {
            format!(r"\\.\pipe\signal-agent-test-{}", Uuid::new_v4())
        } else {
            dir.join("p").join("agent.sock").display().to_string()
        }
    }

    /// One shim as Claude would run it: an MCP client on one end of a byte
    /// stream, `pipe` copying it to a fresh endpoint connection.
    async fn shim(name: &str) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        let (client_io, shim_io) = tokio::io::duplex(4096);
        let agent = endpoint::connect(name).await.expect("connect");
        tokio::spawn(pipe(shim_io, agent));
        ().serve(client_io)
            .await
            .expect("initialize through the shim")
    }

    /// With no agent running, the shim must start one and connect once it is up,
    /// since Claude may launch the shim before the login item has run.
    #[tokio::test]
    async fn a_missing_agent_is_spawned_and_waited_for() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let name = test_endpoint(tmp.path());
        let (bound_tx, bound_rx) = std::sync::mpsc::channel();
        let spawn_name = name.clone();
        let spawn = move || {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let mut listener = endpoint::Listener::bind(&spawn_name).await.expect("bind");
                bound_tx.send(()).expect("report bind");
                loop {
                    let _ = listener.accept().await;
                }
            });
            Ok(())
        };
        let connected = connect_or_spawn(&name, spawn, Duration::from_secs(5)).await;
        assert!(connected.is_ok(), "never connected: {:?}", connected.err());
        assert!(
            bound_rx.try_recv().is_ok(),
            "connected before the agent existed"
        );
    }

    /// If the agent never comes up, the shim must give up within its bound with
    /// an error naming the endpoint, not hang Claude's tool list forever.
    #[tokio::test]
    async fn an_agent_that_never_starts_fails_within_the_bound() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let name = test_endpoint(tmp.path());
        let started = std::time::Instant::now();
        let result = connect_or_spawn(&name, || Ok(()), Duration::from_millis(300)).await;
        let Err(e) = result else {
            panic!("connected to an agent that never started")
        };
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited {:?}",
            started.elapsed()
        );
        assert!(
            format!("{e:#}").contains("did not come up"),
            "unclear error: {e:#}"
        );
    }

    /// A running agent must be reused; spawning another would only fail to
    /// bind, and on a slow machine would race the live one.
    #[tokio::test]
    async fn a_running_agent_is_reused_without_spawning() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let name = test_endpoint(tmp.path());
        let mut listener = endpoint::Listener::bind(&name).await.expect("bind");
        tokio::spawn(async move {
            loop {
                let _ = listener.accept().await;
            }
        });
        let spawned = std::sync::atomic::AtomicBool::new(false);
        let result = connect_or_spawn(
            &name,
            || {
                spawned.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
            Duration::from_secs(1),
        )
        .await;
        assert!(result.is_ok(), "could not reach the running agent");
        assert!(
            !spawned.load(std::sync::atomic::Ordering::SeqCst),
            "spawned a second agent"
        );
    }

    /// PRD acceptance 8: two clients (Claude Code and Claude Desktop) driving
    /// one agent at once must not error or corrupt each other, and every send
    /// must reach Signal exactly once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_shims_interleave_100_operations_without_loss_or_duplication() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let name = test_endpoint(tmp.path());
        let signal = Arc::new(FakeSignal::new(Account {
            service_id: ServiceId::new(Uuid::from_u128(1)),
            phone_number: None,
            kind: ProfileKind::Linked,
            device_name: "test".into(),
        }));
        let agent = Agent::new(
            signal.clone(),
            Arc::new(MemoryStore::default()),
            Registry::default().with(ProbeSend),
            AccessMode::Full,
        );
        let listener = endpoint::Listener::bind(&name).await.expect("bind");
        let served = tokio::spawn(agent.serve(listener, std::future::pending()));

        let code = shim(&name).await;
        let desktop = shim(&name).await;
        let calls = (0..100).map(|i| {
            let client = if i % 2 == 0 { &code } else { &desktop };
            let Value::Object(args) = json!({"body": format!("op-{i}")}) else {
                unreachable!()
            };
            let params = CallToolRequestParams::new("probe_send").with_arguments(args);
            async move { (i, client.peer().call_tool(params).await) }
        });
        let results = futures::future::join_all(calls).await;
        served.abort();

        let failures: Vec<_> = results
            .iter()
            .filter(|(_, r)| r.as_ref().map_or(true, |r| r.is_error == Some(true)))
            .collect();
        assert!(failures.is_empty(), "operations failed: {failures:?}");
        let mut counts: HashMap<String, usize> = HashMap::new();
        for (_, sent) in signal.sent() {
            let Outgoing::Text { body, .. } = sent else {
                panic!("unexpected send {sent:?}")
            };
            *counts.entry(body).or_default() += 1;
        }
        let wrong: Vec<_> = (0..100)
            .map(|i| format!("op-{i}"))
            .filter(|b| counts.get(b) != Some(&1))
            .collect();
        assert!(
            wrong.is_empty(),
            "not sent exactly once: {wrong:?} (all: {counts:?})"
        );
        assert_eq!(counts.len(), 100, "unexpected extra sends: {counts:?}");
    }

    /// A Claude client's health check launches this shim during uninstall;
    /// it must not start an agent for a profile whose setup is gone, and
    /// must still start one for a set-up profile.
    #[test]
    fn no_agent_is_started_for_a_profile_that_isnt_set_up() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let profile = ProfileName::default_profile();
        let paths = ProfilePaths::under(tmp.path().to_owned(), &profile);
        assert!(
            may_start_agent(&profile, &paths).is_err(),
            "started without setup"
        );
        assert!(!paths.root.exists(), "created the profile folder");
        paths
            .save_access(relay_core::AccessMode::ReadOnly)
            .expect("save");
        assert!(may_start_agent(&profile, &paths).is_ok());
    }

    /// A Claude client launching the shim while setup re-links a profile
    /// must not start that profile's agent onto the store setup has open.
    #[test]
    fn no_agent_is_started_while_setup_runs_on_the_profile() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let profile = ProfileName::default_profile();
        let paths = ProfilePaths::under(tmp.path().to_owned(), &profile);
        paths
            .save_access(relay_core::AccessMode::Full)
            .expect("save");
        let setup = relay_setup::SetupLock::try_acquire(&paths)
            .expect("lock")
            .expect("free");
        let Err(e) = may_start_agent(&profile, &paths) else {
            panic!("started an agent while setup runs");
        };
        assert!(format!("{e:#}").contains("setup"), "{e:#}");
        drop(setup);
        assert!(may_start_agent(&profile, &paths).is_ok());
    }
}
