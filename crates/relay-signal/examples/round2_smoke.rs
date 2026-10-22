//! Manual smoke test of the round 2 additions against a real phone: link as a
//! new device, then exercise uploads, pins, polls and the directory lookups,
//! sending only to the account's own Note to Self. CI never runs this; it
//! needs a person to scan the QR code and to look at the phone.
//!
//!     cargo run -p relay-signal --example round2_smoke -- <file-to-send> [username] [--staging]
//!
//! Scan the printed `sgnl://linkdevice` URI as in link_smoke. Every request
//! here reaches Signal, so run it once and unlink the device from the phone
//! when done; a rate-limit error means stop, not retry.

use std::{num::NonZeroU32, path::PathBuf, time::Duration};

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
    let mut positional = args.iter().filter(|a| !a.starts_with("--"));
    let file = PathBuf::from(positional.next().context("name a file to send")?);
    let username = positional.next().cloned();

    let dir = std::env::temp_dir().join(format!("signal-agent-round2-{}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("unable to create \"{}\"", dir.display()))?;
    let path = dir.join("signal.db");
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?;

    let provisioner = PresageProvisioner::open(&path, &key, servers).await?;
    let mut link = provisioner.link("signal-agent round 2 smoke".into());
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
    drop(link);
    drop(provisioner);
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
    let me = Recipient::Contact(account.service_id);

    println!("devices: {:#?}", signal.devices().await?);
    println!("identities: {:#?}", signal.identities().await?);
    let avatar = signal.avatar(me).await?;
    println!("own avatar: {:?} bytes", avatar.map(|a| a.len()));
    if let Some(number) = &account.phone_number {
        println!(
            "discover own number: {:?}",
            signal.discover(std::slice::from_ref(number)).await?
        );
    }
    if let Some(username) = username {
        println!(
            "username {username}: {:?}",
            signal.lookup_username(&username).await?
        );
    }

    let parent = file
        .parent()
        .context("the file has no folder")?
        .to_path_buf();
    let sendable = SendRoots::new([parent])?.admit(&file)?;
    let uploaded = signal.upload(&sendable).await?;
    let text = signal
        .send(
            me,
            Outgoing::Text {
                body: "round 2 smoke: attachment".into(),
                quote: None,
                attachments: vec![uploaded],
            },
        )
        .await?;
    println!("sent attachment: {text:?}; check the phone");

    signal
        .send(
            me,
            Outgoing::Pin {
                target: text,
                duration_secs: NonZeroU32::new(3600),
            },
        )
        .await?;
    println!("pinned it for an hour; check the phone");
    tokio::time::sleep(Duration::from_secs(10)).await;
    signal.send(me, Outgoing::Unpin { target: text }).await?;
    println!("unpinned it");

    let poll = signal
        .send(
            me,
            Outgoing::PollCreate {
                question: "round 2 smoke: does this poll show?".into(),
                options: vec!["yes".into(), "no".into()],
                multi_select: false,
            },
        )
        .await?;
    signal
        .send(
            me,
            Outgoing::PollVote {
                poll,
                option_indexes: vec![0],
                vote_count: 1,
            },
        )
        .await?;
    signal.send(me, Outgoing::PollTerminate { poll }).await?;
    println!("created, voted on and ended a poll; check the phone");

    tokio::time::sleep(Duration::from_secs(10)).await;
    printer.abort();
    println!("done; unlink \"signal-agent round 2 smoke\" from the phone");
    Ok(())
}
