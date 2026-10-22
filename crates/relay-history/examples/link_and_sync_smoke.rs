//! Manual check of a real link-and-sync archive, for the integrator with a real
//! phone. Unit tests only see synthetic archives; this is where a phone's own
//! export meets the importer. It talks to no server: link with the phone first
//! (relay-signal fetches the archive and hands over the key), then point this at
//! the downloaded file.
//!
//! ```text
//! SIGNAL_LINK_SYNC_KEY=<64 hex chars> cargo run -p relay-history \
//!     --example link_and_sync_smoke -- <archive file> <our ACI> [search text]
//! ```
//!
//! The key comes from the environment so it stays out of `ps` and shell history.
//! It prints what was imported, the oldest message's age (acceptance 5 wants a
//! message from 30 days before linking), and the hits for the search text.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use relay_core::ServiceId;
use relay_history::{LinkAndSyncKey, import_link_and_sync};
use relay_store::{MemoryStore, MessageStore, SearchQuery};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(archive_path), Some(aci)) = (args.first(), args.get(1)) else {
        bail!("usage: link_and_sync_smoke <archive file> <our ACI> [search text]");
    };
    let key = std::env::var("SIGNAL_LINK_SYNC_KEY").context("set SIGNAL_LINK_SYNC_KEY")?;
    let key = LinkAndSyncKey::new(parse_key(&key)?);
    let our_aci = ServiceId::parse(aci)?;
    let archive = std::fs::read(archive_path)
        .with_context(|| format!("unable to read archive \"{archive_path}\""))?;

    let store = MemoryStore::default();
    let summary = import_link_and_sync(&archive, &key, our_aci, &store, &mut |p| {
        if p.imported % 1000 == 0 {
            eprintln!("imported {}", p.imported);
        }
    })
    .await?;
    println!("{summary:?}");

    let stats = store.stats().await?;
    println!(
        "{} messages in {} conversations, {} unread",
        stats.messages, stats.conversations, stats.unread
    );
    let now_ms = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
    if let Some(oldest) = stats.oldest_ms {
        println!(
            "oldest message is {} days old",
            now_ms.saturating_sub(oldest) / DAY_MS
        );
    }

    let Some(text) = args.get(2) else {
        return Ok(());
    };
    let hits = store
        .search(&SearchQuery {
            text: text.clone(),
            limit: 10,
            ..Default::default()
        })
        .await?;
    for m in hits {
        let age_days = now_ms.saturating_sub(m.id.sent_at_ms) / DAY_MS;
        println!("{age_days:>5}d  {:?}", m.body);
    }
    Ok(())
}

fn parse_key(hex: &str) -> anyhow::Result<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        bail!(
            "SIGNAL_LINK_SYNC_KEY must be 64 hex characters, got {}",
            hex.len()
        );
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        let pair = hex.get(i * 2..i * 2 + 2).context("key is not hex")?;
        *byte = u8::from_str_radix(pair, 16).context("key is not hex")?;
    }
    Ok(key)
}
