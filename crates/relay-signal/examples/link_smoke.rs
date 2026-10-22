//! Manual smoke test against a real phone: link as a new device, then send a
//! note to self. CI never runs this; it needs a person to scan the QR code.
//!
//!     cargo run -p relay-signal --example link_smoke -- [store-dir] [--staging]
//!
//! Turn the printed `sgnl://linkdevice` URI into a QR code (for example
//! `qrencode -t ansiutf8 '<uri>'`) and scan it from Signal on the phone under
//! Settings, Linked devices. The store and its throwaway key live in
//! `store-dir` (a fresh temp dir by default); unlink the device from the phone
//! when done.

use std::time::Duration;

use anyhow::Context;
use futures::StreamExt;
use relay_core::prelude::*;
use relay_signal::prelude::*;
use relay_signal::{PresageProvisioner, PresageSignal, Servers};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let servers = if args.iter().any(|a| a == "--staging") {
        Servers::Staging
    } else {
        Servers::Production
    };
    let dir = match args.iter().find(|a| !a.starts_with("--")) {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::env::temp_dir().join(format!("signal-agent-smoke-{}", std::process::id())),
    };
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("unable to create \"{}\"", dir.display()))?;
    let path = dir.join("signal.db");
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?;
    println!("store: {}", path.display());

    let provisioner = PresageProvisioner::open(&path, &key, servers).await?;
    let mut link = provisioner.link("signal-agent smoke test".into());
    let account = loop {
        match link
            .next()
            .await
            .context("linking ended without an account")??
        {
            LinkEvent::Uri { uri } => println!("\nscan this as a QR code:\n{uri}\n"),
            LinkEvent::Linked { account } => break account,
        }
    };
    println!("linked: {account:?}");

    let signal = PresageSignal::open(&path, &key).await?;
    let mut incoming = signal.incoming();
    let printer = tokio::spawn(async move {
        while let Some(event) = incoming.next().await {
            match event {
                Ok(event) => println!("incoming: {event:?}"),
                Err(e) => println!("incoming error: {e:#}"),
            }
        }
    });
    // Let the first sync drain before sending, as presage recommends.
    tokio::time::sleep(Duration::from_secs(10)).await;
    let sent = signal
        .send(
            Recipient::Contact(account.service_id),
            Outgoing::Text {
                body: "signal-agent smoke test: note to self".into(),
                quote: None,
            },
        )
        .await?;
    println!("sent note to self: {sent:?}; check the phone");
    tokio::time::sleep(Duration::from_secs(10)).await;
    printer.abort();
    Ok(())
}
