pub mod banner;
pub mod io;

use crate::cli::{AddressFamily, Config};
use anyhow::{Context, Result};
use russh::Disconnect;
use russh::client::{self, Handle};
use russh::keys::ssh_key::PublicKey;
use russh::keys::{self, PrivateKeyWithHashAlg};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::lookup_host;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub const CLI_MODE_ENV: &str = "LATE_CLI_MODE";
const DEFAULT_SSH_PORT: u16 = 22;

/// Handle to an established russh client session. Owns the unified IO task
/// driving stdio forwarding, SIGWINCH handling, and the underlying channel.
pub struct SshSession {
    pub handle: Handle<Client>,
    pub io_task: JoinHandle<Result<()>>,
    pub exit_rx: oneshot::Receiver<Option<u32>>,
}

/// russh client handler. Implements accept-new host-key policy via
/// `known_hosts`: unknown hosts are recorded silently; mismatches are rejected.
pub struct Client {
    host: String,
    port: u16,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKey) -> Result<bool, Self::Error> {
        let host = self.host.clone();
        let port = self.port;
        let key = key.clone();

        let join = tokio::task::spawn_blocking(move || -> Result<bool, keys::Error> {
            match keys::known_hosts::check_known_hosts(&host, port, &key) {
                Ok(true) => Ok(true),
                Ok(false) => {
                    // Host not present in known_hosts — learn it.
                    keys::known_hosts::learn_known_hosts(&host, port, &key)?;
                    tracing::debug!(host, port, "learned new ssh host key");
                    Ok(true)
                }
                Err(keys::Error::KeyChanged { line }) => {
                    eprintln!(
                        "error: host key for {host}:{port} does not match known_hosts line {line} — refusing to connect"
                    );
                    Ok(false)
                }
                Err(err) => Err(err),
            }
        })
        .await;

        match join {
            Ok(Ok(accepted)) => Ok(accepted),
            Ok(Err(keys_err)) => Err(russh::Error::from(keys_err)),
            Err(join_err) => {
                tracing::warn!(error = ?join_err, "known_hosts check task panicked");
                Err(russh::Error::Disconnect)
            }
        }
    }
}

fn parse_target(target: &str) -> (String, u16) {
    // `target` is "host" or "host:port". We do *not* parse bracketed IPv6 here:
    // users providing raw IPv6 literals should use `--ssh-target <addr>` without
    // a port (port defaults to 22). A v6 literal contains colons, so we detect
    // that and avoid treating the last `:` as a port separator.
    if let Some((host, port_str)) = target.rsplit_once(':')
        && !host.contains(':')
        && let Ok(port) = port_str.parse::<u16>()
    {
        return (host.to_string(), port);
    }
    (target.to_string(), DEFAULT_SSH_PORT)
}

/// Filter resolved socket addresses by the requested address family.
pub fn filter_addrs(addrs: Vec<SocketAddr>, family: AddressFamily) -> Vec<SocketAddr> {
    match family {
        AddressFamily::Auto => addrs,
        AddressFamily::V4 => addrs.into_iter().filter(|a| a.is_ipv4()).collect(),
        AddressFamily::V6 => addrs.into_iter().filter(|a| a.is_ipv6()).collect(),
    }
}

async fn resolve_addrs(target: &str, family: AddressFamily) -> Result<Vec<SocketAddr>> {
    let (host, port) = parse_target(target);
    let all: Vec<SocketAddr> = lookup_host((host.as_str(), port))
        .await
        .with_context(|| format!("failed to resolve {host}:{port}"))?
        .collect();
    let filtered = filter_addrs(all, family);
    if filtered.is_empty() {
        anyhow::bail!("no {:?} addresses found for {host}:{port}", family);
    }
    Ok(filtered)
}

/// Probe ssh-agent before raw mode engages. Returns `None` when the agent is
/// reachable and has at least one identity (caller defers to agent auth).
/// Otherwise resolves the dedicated key path, prompting the user to generate
/// one if necessary — this MUST run before raw mode is enabled because
/// `identity::ensure` reads a y/N prompt with `stdin().read_line`, which
/// blocks forever in raw mode (enter sends CR, not LF).
pub async fn prepare_identity() -> Result<Option<PathBuf>> {
    match keys::agent::client::AgentClient::connect_env().await {
        Ok(mut agent) => match agent.request_identities().await {
            Ok(ids) if !ids.is_empty() => {
                tracing::debug!(
                    count = ids.len(),
                    "ssh-agent has identities; will use agent"
                );
                return Ok(None);
            }
            Ok(_) => {
                tracing::debug!("ssh-agent has no identities; falling back to dedicated key");
            }
            Err(err) => {
                tracing::debug!(error = ?err, "ssh-agent request_identities failed; falling back");
            }
        },
        Err(err) => {
            tracing::debug!(error = ?err, "ssh-agent unavailable; falling back to dedicated key");
        }
    }
    Ok(Some(crate::identity::ensure()?))
}

/// Connect to the ssh target and bring the session up through `request_shell`.
/// The returned [`SshSession`] has the unified IO task already running.
///
/// `identity` selects the auth path: `None` means "agent-only" (the caller has
/// probed ssh-agent via [`prepare_identity`]); `Some(path)` means use the
/// dedicated key at that path.
pub async fn connect(
    cfg: &Config,
    identity: Option<&Path>,
    token_tx: oneshot::Sender<String>,
) -> Result<SshSession> {
    let (host, port) = parse_target(&cfg.ssh_target);
    let addrs = resolve_addrs(&cfg.ssh_target, cfg.address_family).await?;
    tracing::debug!(?addrs, "resolved ssh target");

    let config = Arc::new(client::Config::default());
    let client_handler = Client {
        host: host.clone(),
        port,
    };

    let mut handle = client::connect(config, &addrs[..], client_handler)
        .await
        .with_context(|| format!("failed to ssh to {host}:{port}"))?;

    authenticate(&mut handle, &whoami_or_fallback(), identity).await?;

    let channel = handle
        .channel_open_session()
        .await
        .context("failed to open ssh session channel")?;

    channel
        .set_env(false, CLI_MODE_ENV, "1")
        .await
        .context("failed to set LATE_CLI_MODE env")?;

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into());
    channel
        .request_pty(false, &term, cols as u32, rows as u32, 0, 0, &[])
        .await
        .context("failed to request pty")?;
    channel
        .request_shell(true)
        .await
        .context("failed to request shell")?;

    let (exit_tx, exit_rx) = oneshot::channel::<Option<u32>>();
    let io_task = tokio::spawn(io::run_io_loop(channel, token_tx, exit_tx));

    Ok(SshSession {
        handle,
        io_task,
        exit_rx,
    })
}

/// Authenticate the handle. When `identity` is `None` the caller has already
/// verified ssh-agent has identities loaded, so an agent failure here is
/// fatal. When `identity` is `Some(path)`, use that dedicated key.
async fn authenticate(
    handle: &mut Handle<Client>,
    user: &str,
    identity: Option<&Path>,
) -> Result<()> {
    let hash = handle
        .best_supported_rsa_hash()
        .await
        .context("failed to negotiate rsa hash algorithm")?
        .flatten();

    match identity {
        None => {
            let mut agent = keys::agent::client::AgentClient::connect_env()
                .await
                .context("ssh-agent disappeared between probe and connect")?;
            let identities = agent
                .request_identities()
                .await
                .context("ssh-agent request_identities failed after probe")?;
            for pubkey in identities {
                let fingerprint = pubkey.fingerprint(Default::default()).to_string();
                match handle
                    .authenticate_publickey_with(user.to_string(), pubkey, hash, &mut agent)
                    .await
                {
                    Ok(r) if r.success() => {
                        tracing::info!(%fingerprint, "authenticated via ssh-agent");
                        return Ok(());
                    }
                    Ok(_) => {
                        tracing::debug!(%fingerprint, "agent key rejected by server, trying next");
                    }
                    Err(err) => {
                        tracing::debug!(
                            %fingerprint,
                            error = ?err,
                            "agent auth attempt failed, trying next"
                        );
                    }
                }
            }
            anyhow::bail!("ssh-agent has identities but none were accepted by the server")
        }
        Some(path) => {
            let owned: PathBuf = path.to_path_buf();
            let path_str = path.display().to_string();
            let key_pair = tokio::task::spawn_blocking(move || keys::load_secret_key(&owned, None))
                .await
                .context("key load task panicked")?
                .with_context(|| format!("failed to load SSH key {path_str}"))?;

            let auth = handle
                .authenticate_publickey(
                    user.to_string(),
                    PrivateKeyWithHashAlg::new(Arc::new(key_pair), hash),
                )
                .await
                .context("public key authentication failed")?;

            if auth.success() {
                tracing::info!(path = %path_str, "authenticated via dedicated key");
                Ok(())
            } else {
                anyhow::bail!("dedicated key authentication failed")
            }
        }
    }
}

/// Best-effort disconnect. Errors are logged, not returned.
pub async fn disconnect(handle: &Handle<Client>) {
    if let Err(err) = handle.disconnect(Disconnect::ByApplication, "", "en").await {
        tracing::debug!(error = ?err, "ssh disconnect returned an error");
    }
}

/// Username forwarded to the server. `late-ssh` stores this on first connect
/// and ignores it otherwise, but an empty string would trip some servers so we
/// fall back to a stable value.
fn whoami_or_fallback() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| "late".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

    fn v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(a, b, c, d), port))
    }

    fn v6(port: u16) -> SocketAddr {
        SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 0, 0))
    }

    #[test]
    fn parse_target_defaults_port_22() {
        let (host, port) = parse_target("late.sh");
        assert_eq!(host, "late.sh");
        assert_eq!(port, 22);
    }

    #[test]
    fn parse_target_accepts_host_port() {
        let (host, port) = parse_target("example.com:2222");
        assert_eq!(host, "example.com");
        assert_eq!(port, 2222);
    }

    #[test]
    fn parse_target_leaves_ipv6_literal_alone() {
        let (host, port) = parse_target("2001:db8::1");
        assert_eq!(host, "2001:db8::1");
        assert_eq!(port, DEFAULT_SSH_PORT);
    }

    #[test]
    fn resolve_filters_by_family_v4() {
        let mixed = vec![v4(1, 2, 3, 4, 22), v6(22), v4(5, 6, 7, 8, 22)];
        let only_v4 = filter_addrs(mixed, AddressFamily::V4);
        assert_eq!(only_v4.len(), 2);
        assert!(only_v4.iter().all(|a| a.is_ipv4()));
    }

    #[test]
    fn resolve_filters_by_family_v6() {
        let mixed = vec![v4(1, 2, 3, 4, 22), v6(22)];
        let only_v6 = filter_addrs(mixed, AddressFamily::V6);
        assert_eq!(only_v6.len(), 1);
        assert!(only_v6[0].is_ipv6());
    }

    #[test]
    fn resolve_filters_by_family_auto_keeps_all() {
        let mixed = vec![v4(1, 2, 3, 4, 22), v6(22)];
        let all = filter_addrs(mixed.clone(), AddressFamily::Auto);
        assert_eq!(all.len(), mixed.len());
    }
}
