pub mod banner;
pub mod io;

use crate::cli::{AddressFamily, Config};
use anyhow::{Context, Result};
use russh::Disconnect;
use russh::client::{self, Handle};
use russh::keys::ssh_key::PublicKey;
use russh::keys::{self, HashAlg, PrivateKeyWithHashAlg};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::lookup_host;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

pub const CLI_MODE_ENV: &str = "LATE_CLI_MODE";
const DEFAULT_SSH_PORT: u16 = 22;
const CMD_CHANNEL_BUFFER: usize = 64;

/// Commands routed to the task that owns the russh channel.
#[derive(Debug)]
pub enum ChannelCmd {
    Stdin(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Eof,
    Close,
}

/// Handle to an established russh client session. Owns the long-running tasks
/// driving stdio forwarding, SIGWINCH handling, and the underlying channel.
pub struct SshSession {
    pub handle: Handle<Client>,
    pub output_task: JoinHandle<Result<()>>,
    pub resize_task: JoinHandle<()>,
    pub stdin_task: Option<JoinHandle<Result<()>>>,
    pub exit_rx: oneshot::Receiver<Option<u32>>,
    pub cmd_tx: mpsc::Sender<ChannelCmd>,
}

impl SshSession {
    /// Spawn the stdin forwarder. Caller invokes this after the session token
    /// has been observed so that pre-handshake keystrokes are discarded (they
    /// stay in the kernel line buffer / never entered the mpsc).
    pub fn spawn_stdin(&mut self) {
        if self.stdin_task.is_some() {
            return;
        }
        let tx = self.cmd_tx.clone();
        self.stdin_task = Some(tokio::spawn(io::forward_stdin(tx)));
    }
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

/// Connect to the ssh target and bring the session up through `request_shell`.
/// The returned [`SshSession`] has the output task and resize task already
/// running; spawn stdin via [`SshSession::spawn_stdin`] once the session token
/// banner has been observed.
pub async fn connect(cfg: &Config, token_tx: oneshot::Sender<String>) -> Result<SshSession> {
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

    authenticate(&mut handle, &whoami_or_fallback()).await?;

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

    let (cmd_tx, cmd_rx) = mpsc::channel::<ChannelCmd>(CMD_CHANNEL_BUFFER);
    let (exit_tx, exit_rx) = oneshot::channel::<Option<u32>>();

    let output_task = tokio::spawn(io::forward_output(channel, token_tx, cmd_rx, exit_tx));

    let resize_task = tokio::spawn(forward_resize_events(cmd_tx.clone()));

    Ok(SshSession {
        handle,
        output_task,
        resize_task,
        stdin_task: None,
        exit_rx,
        cmd_tx,
    })
}

/// Authenticate the handle, preferring ssh-agent identities over the dedicated
/// `~/.ssh/id_late_sh_ed25519` key (which is generated on demand via
/// [`crate::identity::ensure`]). Mirrors OpenSSH behaviour so that users who
/// already have an agent-loaded key don't end up with a second identity on the
/// server.
async fn authenticate(handle: &mut Handle<Client>, user: &str) -> Result<()> {
    let hash = handle
        .best_supported_rsa_hash()
        .await
        .context("failed to negotiate rsa hash algorithm")?
        .flatten();

    let (agent_tried, agent_key_count) = match try_agent_auth(handle, user, hash).await {
        Ok(AgentAuthOutcome::Success) => return Ok(()),
        Ok(AgentAuthOutcome::Rejected { tried }) => (true, tried),
        Err(err) => {
            tracing::debug!(error = ?err, "ssh-agent unavailable; falling back to dedicated key");
            (false, 0)
        }
    };

    // Fall back to the dedicated key (prompts to generate on first run).
    let identity_path = crate::identity::ensure()?;
    let identity_owned: PathBuf = identity_path.clone();
    let key_pair =
        tokio::task::spawn_blocking(move || keys::load_secret_key(&identity_owned, None))
            .await
            .context("key load task panicked")?
            .with_context(|| format!("failed to load SSH key {}", identity_path.display()))?;

    let auth = handle
        .authenticate_publickey(
            user.to_string(),
            PrivateKeyWithHashAlg::new(Arc::new(key_pair), hash),
        )
        .await
        .context("public key authentication failed")?;

    if auth.success() {
        tracing::info!(path = %identity_path.display(), "authenticated via dedicated key");
        Ok(())
    } else if agent_tried {
        anyhow::bail!(
            "authentication failed (tried {agent_key_count} agent key(s), then dedicated key)"
        )
    } else {
        anyhow::bail!("authentication failed via dedicated key (ssh-agent not available)")
    }
}

enum AgentAuthOutcome {
    Success,
    Rejected { tried: usize },
}

async fn try_agent_auth(
    handle: &mut Handle<Client>,
    user: &str,
    hash: Option<HashAlg>,
) -> Result<AgentAuthOutcome> {
    let mut agent = keys::agent::client::AgentClient::connect_env()
        .await
        .context("no ssh-agent available (SSH_AUTH_SOCK unset or socket unreachable)")?;

    let identities = agent
        .request_identities()
        .await
        .context("ssh-agent request_identities failed")?;

    if identities.is_empty() {
        tracing::debug!("ssh-agent reachable but no identities loaded");
        return Ok(AgentAuthOutcome::Rejected { tried: 0 });
    }
    tracing::debug!(count = identities.len(), "ssh-agent offered identities");

    let total = identities.len();
    for pubkey in identities {
        let fingerprint = pubkey.fingerprint(Default::default()).to_string();
        match handle
            .authenticate_publickey_with(user.to_string(), pubkey, hash, &mut agent)
            .await
        {
            Ok(r) if r.success() => {
                tracing::info!(%fingerprint, "authenticated via ssh-agent");
                return Ok(AgentAuthOutcome::Success);
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
    Ok(AgentAuthOutcome::Rejected { tried: total })
}

/// Listen for SIGWINCH and forward window size changes via the command channel.
async fn forward_resize_events(cmd_tx: mpsc::Sender<ChannelCmd>) {
    let Ok(mut sigwinch) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
    else {
        return;
    };

    while sigwinch.recv().await.is_some() {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        if cmd_tx
            .send(ChannelCmd::Resize { cols, rows })
            .await
            .is_err()
        {
            break;
        }
        tracing::debug!(cols, rows, "forwarded SIGWINCH to ssh channel");
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

    #[test]
    fn smoke_agent_client_connect_env_reads_env_var() {
        // Just ensures the ssh-agent symbol is importable/linkable.
        // Actual agent connection is env-dependent and not tested here.
        let _ = russh::keys::agent::client::AgentClient::connect_env;
    }
}
