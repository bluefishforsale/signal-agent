//! `signal-agent http`: the human's controls for the HTTP endpoint. These are
//! deliberately not MCP tools, so no client can open the endpoint wider or
//! mint itself a token.

use std::{io::Write, path::PathBuf};

use clap::{Subcommand, ValueEnum};
use relay_core::prelude::*;
use relay_mcp::{HttpSettings, TlsFiles, Tokens};

#[derive(Subcommand)]
pub enum HttpCmd {
    /// Show whether the endpoint is on, its URL, certificate and tokens.
    Status,
    /// Turn the endpoint on, from the next time the agent starts.
    Enable {
        /// Listen on every network interface instead of this machine only.
        /// Always served over TLS.
        #[arg(long)]
        lan: bool,
        /// The port to listen on; 8765 unless set before.
        #[arg(long)]
        port: Option<u16>,
        /// Your own PEM certificate chain, instead of a self-signed one.
        #[arg(long, requires = "key")]
        cert: Option<PathBuf>,
        /// The PEM private key for --cert.
        #[arg(long, requires = "cert")]
        key: Option<PathBuf>,
        /// A host name or address LAN clients dial, for the self-signed
        /// certificate. Repeat for several.
        #[arg(long = "tls-name")]
        tls_names: Vec<String>,
    },
    /// Turn the endpoint off, from the next time the agent starts.
    Disable,
    /// Manage the tokens remote clients use.
    #[command(subcommand)]
    Token(TokenCmd),
}

#[derive(Subcommand)]
pub enum TokenCmd {
    /// Make a token for one client. It is shown once and never again.
    Create {
        /// What to call the client, for the log and for revoking it.
        name: String,
        /// What the client may do.
        #[arg(long, value_enum)]
        scope: Scope,
    },
    /// List every client with a live token.
    List,
    /// Lock a client out from its next request on.
    Revoke { name: String },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Scope {
    /// Read and search only; every tool that changes the account is refused.
    ReadOnly,
    /// Everything the profile allows, including sending.
    Full,
}

pub fn run(cmd: HttpCmd, paths: &ProfilePaths, out: &mut impl Write) -> anyhow::Result<()> {
    match cmd {
        HttpCmd::Status => status(paths, out),
        HttpCmd::Enable {
            lan,
            port,
            cert,
            key,
            tls_names,
        } => {
            let mut settings = HttpSettings::load(paths)?;
            settings.enabled = true;
            settings.lan = lan;
            settings.port = port.unwrap_or(settings.port);
            settings.tls = cert.zip(key).map(|(cert, key)| TlsFiles { cert, key });
            settings.tls_names = tls_names;
            settings.listen_on(paths)?;
            settings.save(paths)?;
            writeln!(out, "The HTTP endpoint is on from the next agent start.")?;
            status(paths, out)
        }
        HttpCmd::Disable => {
            let mut settings = HttpSettings::load(paths)?;
            settings.enabled = false;
            settings.save(paths)?;
            writeln!(out, "The HTTP endpoint is off from the next agent start.")?;
            Ok(())
        }
        HttpCmd::Token(cmd) => token(cmd, paths, out),
    }
}

fn status(paths: &ProfilePaths, out: &mut impl Write) -> anyhow::Result<()> {
    let settings = HttpSettings::load(paths)?;
    if !settings.enabled {
        writeln!(out, "The HTTP endpoint is off.")?;
    } else {
        let (addr, tls) = settings.listen_on(paths)?;
        let scheme = if tls.is_some() { "https" } else { "http" };
        writeln!(out, "Serving at {scheme}://{addr}/mcp")?;
        if let Some(tls) = tls {
            writeln!(out, "Certificate SHA-256: {}", tls.fingerprint)?;
        }
    }
    let tokens = Tokens::of(paths).list()?;
    if tokens.is_empty() {
        writeln!(out, "No tokens.")?;
    }
    for grant in tokens {
        writeln!(out, "Token \"{}\": {}", grant.name, scope_name(grant.scope))?;
    }
    Ok(())
}

fn token(cmd: TokenCmd, paths: &ProfilePaths, out: &mut impl Write) -> anyhow::Result<()> {
    let tokens = Tokens::of(paths);
    match cmd {
        TokenCmd::Create { name, scope } => {
            let scope = match scope {
                Scope::ReadOnly => AccessMode::ReadOnly,
                Scope::Full => AccessMode::Full,
            };
            let token = tokens.create(&name, scope)?;
            writeln!(
                out,
                "Token for \"{name}\" ({}):\n\n  {token}\n",
                scope_name(scope)
            )?;
            writeln!(
                out,
                "This is the only time it is shown. Give the client this header:\n\n  \
                 Authorization: Bearer {token}\n\nFor Claude Code:\n\n  \
                 claude mcp add --transport http signal <URL> --header \"Authorization: Bearer {token}\""
            )?;
        }
        TokenCmd::List => {
            for grant in tokens.list()? {
                writeln!(out, "{}\t{}", grant.name, scope_name(grant.scope))?;
            }
        }
        TokenCmd::Revoke { name } => {
            tokens.revoke(&name)?;
            writeln!(out, "Revoked \"{name}\"; its next request is refused.")?;
        }
    }
    Ok(())
}

fn scope_name(scope: AccessMode) -> &'static str {
    match scope {
        AccessMode::Full => "full",
        AccessMode::ReadOnly => "read-only",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(dir: &tempfile::TempDir) -> ProfilePaths {
        ProfilePaths::under(dir.path().to_path_buf(), &ProfileName::default_profile())
    }

    /// A token is shown once, at creation, and listing clients must never
    /// print one again.
    #[test]
    fn a_created_token_is_printed_once_and_never_listed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(&dir);
        let mut created = vec![];
        let create = TokenCmd::Create {
            name: "laptop".into(),
            scope: Scope::ReadOnly,
        };
        run(HttpCmd::Token(create), &paths, &mut created).expect("create");
        let created = String::from_utf8(created).expect("utf8");
        let token = created
            .lines()
            .map(str::trim)
            .find(|l| l.len() == 64)
            .expect("token printed")
            .to_owned();

        for cmd in [HttpCmd::Token(TokenCmd::List), HttpCmd::Status] {
            let mut listed = vec![];
            run(cmd, &paths, &mut listed).expect("list");
            let listed = String::from_utf8(listed).expect("utf8");
            assert!(listed.contains("laptop"), "client missing: {listed}");
            assert!(listed.contains("read-only"), "scope missing: {listed}");
            assert!(!listed.contains(&token), "token printed again: {listed}");
        }
    }
}
