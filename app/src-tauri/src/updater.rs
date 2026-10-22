//! Updates with consent (PRD goal 10, anti-goal 6): looking for a release only
//! reads its manifest and tells the user; downloading and installing happen
//! only when they click Install, and the Tauri plugin refuses any payload not
//! signed by the key in tauri.conf.json.

use std::{
    path::Path,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::UpdaterExt;

/// The manifest the release workflow attaches to every release.
pub const FEED: &str =
    "https://github.com/bluefishforsale/signal-agent/releases/latest/download/latest.json";

/// The running app's version.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A release newer than the running app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Offer {
    pub version: String,
}

#[derive(Deserialize)]
struct Manifest {
    version: String,
}

/// Reads `feed` and returns its release if it is newer than `current`.
/// Nothing but the manifest is fetched.
pub async fn newer_release(feed: &str, current: &str) -> anyhow::Result<Option<Offer>> {
    let current = parse_version(current)?;
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // Losing this race to another installer is fine; either provider works.
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    let client = reqwest::Client::builder()
        .user_agent(concat!("signal-agent-app/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .context("unable to build an HTTP client")?;
    let manifest: Manifest = client
        .get(feed)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("unable to fetch {feed}"))?
        .json()
        .await
        .with_context(|| format!("{feed} is not a release manifest"))?;
    let offered = parse_version(&manifest.version)?;
    Ok((offered > current).then(|| Offer {
        version: offered.to_string(),
    }))
}

/// The newest release on offer while the window is open.
#[derive(Default)]
pub struct Offered(Mutex<Option<Offer>>);

impl Offered {
    pub fn get(&self) -> Option<Offer> {
        self.0.lock().expect("offer lock").clone()
    }
}

/// `--check-for-update`: what the agent runs daily while the window is
/// closed. Looks if due and, if a release is newer, says so in a native
/// notification that opens the app; it never downloads anything.
pub fn check_in_background() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("no async runtime")?;
    let log = update_log()?;
    let offer = runtime.block_on(check_if_due(FEED, VERSION, &log, SystemTime::now()))?;
    match offer {
        Some(offer) => notify(&offer),
        None => Ok(()),
    }
}

/// Looks for a release now and then daily while the window is open, showing
/// any newer one in the window.
pub async fn watch(app: AppHandle) {
    loop {
        match newer_release(FEED, VERSION).await {
            Ok(offer) => {
                if let Err(e) = update_log().and_then(|log| record(&log, SystemTime::now())) {
                    eprintln!("unable to remember the update check: {e:#}");
                }
                if let Some(offer) = offer {
                    *app.state::<Offered>().0.lock().expect("offer lock") = Some(offer.clone());
                    if let Err(e) = app.emit("update", &offer) {
                        eprintln!("unable to show the update: {e}");
                    }
                }
            }
            Err(e) => eprintln!("unable to look for an update: {e:#}"),
        }
        tokio::time::sleep(EVERY).await;
    }
}

/// Downloads, verifies and installs the release the user clicked Install
/// on, then restarts the agents and the app from the new version.
pub async fn install(app: &AppHandle) -> anyhow::Result<()> {
    let Some(update) = app.updater()?.check().await? else {
        bail!("there is no newer release to install any more");
    };
    // NOTE: the plugin checks the payload against the pubkey in
    // tauri.conf.json before installing; a payload not signed by the
    // release key is refused here, whatever the feed says.
    update.download_and_install(|_, _| {}, || {}).await?;
    restart_agents();
    app.restart()
}

/// Restarts the login item so the agents run the new version's binary
/// instead of the replaced one. Windows needs nothing: its installer stops
/// the agents before replacing files and starts them after.
fn restart_agents() {
    #[cfg(target_os = "macos")]
    {
        let restarted = std::process::Command::new("id")
            .arg("-u")
            .output()
            .context("unable to run id")
            .and_then(|out| {
                let uid = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                let target = format!("gui/{uid}/{}", relay_setup::LAUNCHD_LABEL);
                let status = std::process::Command::new("launchctl")
                    .args(["kickstart", "-k", &target])
                    .status()
                    .context("unable to run launchctl")?;
                if !status.success() {
                    bail!("launchctl kickstart {target} failed ({status})");
                }
                Ok(())
            });
        if let Err(e) = restarted {
            eprintln!("the agents keep running the old version until next login: {e:#}");
        }
    }
}

fn notify(offer: &Offer) -> anyhow::Result<()> {
    // NOTE: without this the notification is filed under Terminal, and
    // clicking it would open Terminal instead of this app.
    #[cfg(target_os = "macos")]
    let _ = notify_rust::set_application(relay_setup::APP_ID);
    let mut notification = notify_rust::Notification::new();
    notification
        .summary("An update is ready")
        .body(&format!(
            "MCP Relay for Signal {} is available. Open the app to install it.",
            offer.version
        ))
        .appname("MCP Relay for Signal");
    #[cfg(windows)]
    notification.app_id(relay_setup::APP_ID);
    notification
        .show()
        .context("unable to show the update notification")?;
    Ok(())
}

fn update_log() -> anyhow::Result<std::path::PathBuf> {
    let footprint = relay_setup::Footprint::new(
        relay_setup::claude::Platform::current(),
        relay_setup::Roots::system()?,
    );
    Ok(footprint.update_check_file())
}

/// How often the app looks for a release.
const EVERY: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Serialize, Deserialize)]
struct CheckLog {
    last_check_unix: u64,
}

/// Looks for a release if a day has passed since the last successful look
/// recorded in `log`, recording this one when it succeeds; a failed look is
/// retried next time instead of waiting a day.
pub async fn check_if_due(
    feed: &str,
    current: &str,
    log: &Path,
    now: SystemTime,
) -> anyhow::Result<Option<Offer>> {
    if !due(log, now) {
        return Ok(None);
    }
    let offer = newer_release(feed, current).await?;
    record(log, now)?;
    Ok(offer)
}

fn due(log: &Path, now: SystemTime) -> bool {
    let last = std::fs::read_to_string(log)
        .ok()
        .and_then(|text| serde_json::from_str::<CheckLog>(&text).ok())
        .map(|log| UNIX_EPOCH + Duration::from_secs(log.last_check_unix));
    // A last check in the future means the clock moved back; look now rather
    // than stay silent until the clock catches up.
    last.and_then(|last| now.duration_since(last).ok())
        .is_none_or(|since| since >= EVERY)
}

/// Remembers `now` as the last look, in `log`.
pub fn record(log: &Path, now: SystemTime) -> anyhow::Result<()> {
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let text = serde_json::to_string(&CheckLog {
        last_check_unix: secs,
    })?;
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("unable to create \"{}\"", dir.display()))?;
    }
    std::fs::write(log, text).with_context(|| format!("unable to write \"{}\"", log.display()))
}

fn parse_version(text: &str) -> anyhow::Result<semver::Version> {
    let trimmed = text.trim().trim_start_matches('v');
    semver::Version::parse(trimmed).with_context(|| format!("\"{text}\" is not a version"))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    /// A release feed on loopback that serves `manifest` at /latest.json and
    /// records every path asked for.
    struct Feed {
        url: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl Feed {
        async fn serving(version: &str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            let manifest = serde_json::json!({
                "version": version,
                "platforms": {
                    "darwin-aarch64": {"url": format!("http://{addr}/payload.app.tar.gz"), "signature": "sig"},
                    "windows-x86_64": {"url": format!("http://{addr}/payload-setup.exe"), "signature": "sig"},
                },
            })
            .to_string();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = requests.clone();
            tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let mut buf = vec![0; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let path = request.split_whitespace().nth(1).unwrap_or("").to_owned();
                    seen.lock().expect("requests").push(path.clone());
                    let (status, body) = if path == "/latest.json" {
                        ("200 OK", manifest.clone())
                    } else {
                        ("404 Not Found", String::new())
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                }
            });
            Self {
                url: format!("http://{addr}/latest.json"),
                requests,
            }
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().expect("requests").clone()
        }
    }

    /// Looking for an update must offer only a strictly newer release and
    /// never fetch a payload: downloading is the install, and that waits for
    /// the user's click.
    #[tokio::test]
    async fn a_check_offers_only_newer_releases_and_fetches_only_the_manifest() {
        for (offered, current, expected) in [
            ("0.2.0", "0.1.0", Some("0.2.0")),
            ("v0.2.0", "0.1.0", Some("0.2.0")),
            ("0.1.0", "0.1.0", None),
            ("0.0.9", "0.1.0", None),
        ] {
            let feed = Feed::serving(offered).await;
            let offer = newer_release(&feed.url, current).await.expect("check");
            assert_eq!(
                offer.map(|o| o.version).as_deref(),
                expected,
                "{offered} offered to {current}"
            );
            assert_eq!(feed.requests(), ["/latest.json"], "{offered}");
        }
    }

    /// However often the agent asks, the app looks at GitHub at most once a
    /// day, and a look that failed (offline, say) is retried next time.
    #[tokio::test]
    async fn the_daily_check_looks_at_most_once_a_day_and_retries_failures() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("update-check.json");
        let feed = Feed::serving("0.2.0").await;
        let start = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let hours = |h: u64| start + Duration::from_secs(h * 3600);

        let dead = "http://127.0.0.1:9/latest.json";
        assert!(check_if_due(dead, "0.1.0", &log, hours(0)).await.is_err());
        let first = check_if_due(&feed.url, "0.1.0", &log, hours(1))
            .await
            .expect("check");
        assert_eq!(first.map(|o| o.version).as_deref(), Some("0.2.0"));
        let soon = check_if_due(&feed.url, "0.1.0", &log, hours(12))
            .await
            .expect("check");
        assert_eq!(soon, None, "looked again within a day");
        let tomorrow = check_if_due(&feed.url, "0.1.0", &log, hours(25))
            .await
            .expect("check");
        assert!(tomorrow.is_some(), "didn't look again after a day");
        assert_eq!(feed.requests().len(), 2, "{:?}", feed.requests());
    }

    /// The installer plugin and the daily check must read the same feed, or
    /// the notification could offer a release Install can't find.
    #[test]
    fn the_installer_reads_the_feed_the_daily_check_reads() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("tauri.conf.json");
        assert_eq!(
            config["plugins"]["updater"]["endpoints"],
            serde_json::json!([FEED])
        );
    }
}
