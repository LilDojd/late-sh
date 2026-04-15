use anyhow::{Context, Result};
use clap::Parser;
use clap_verbosity_flag::Verbosity;
use shlex::Shlex;
use url::Url;

const DEFAULT_SSH_TARGET: &str = "late.sh";
const DEFAULT_AUDIO_BASE_URL: &str = "http://audio.late.sh";
const DEFAULT_API_BASE_URL: &str = "https://api.late.sh";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressFamily {
    Auto,
    V4,
    V6,
}

#[derive(Debug, clap::Args)]
#[group(multiple = false)]
struct AddressFamilyFlags {
    /// Force IPv4 for the SSH connection (passes -4)
    #[arg(short = '4', long)]
    ipv4: bool,

    /// Force IPv6 for the SSH connection (passes -6)
    #[arg(short = '6', long)]
    ipv6: bool,
}

impl From<&AddressFamilyFlags> for AddressFamily {
    fn from(f: &AddressFamilyFlags) -> Self {
        match (f.ipv4, f.ipv6) {
            (true, _) => Self::V4,
            (_, true) => Self::V6,
            _ => Self::Auto,
        }
    }
}

#[derive(Debug, Parser)]
#[command(version, about = "late.sh companion CLI", long_about = None)]
pub struct Args {
    #[arg(long, env = "LATE_SSH_TARGET", default_value = DEFAULT_SSH_TARGET)]
    ssh_target: String,

    #[arg(long, env = "LATE_SSH_BIN", default_value = "ssh")]
    ssh_bin: String,

    #[arg(long, env = "LATE_AUDIO_BASE_URL", default_value = DEFAULT_AUDIO_BASE_URL)]
    audio_base_url: String,

    #[arg(long, env = "LATE_API_BASE_URL", default_value = DEFAULT_API_BASE_URL)]
    api_base_url: String,

    #[command(flatten)]
    address_family: AddressFamilyFlags,

    #[command(flatten)]
    verbose: Verbosity,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub ssh_target: String,
    pub ssh_bin: Vec<String>,
    pub audio_base_url: Url,
    pub api_base_url: Url,
    pub address_family: AddressFamily,
}

impl Args {
    pub fn resolve(self) -> Result<(Config, Verbosity)> {
        let ssh_bin = parse_ssh_bin_spec(&self.ssh_bin)?;
        let audio_base_url = Url::parse(&self.audio_base_url)
            .with_context(|| format!("invalid --audio-base-url {:?}", self.audio_base_url))?;
        let api_base_url = Url::parse(&self.api_base_url)
            .with_context(|| format!("invalid --api-base-url {:?}", self.api_base_url))?;
        let address_family = AddressFamily::from(&self.address_family);
        Ok((
            Config {
                ssh_target: self.ssh_target,
                ssh_bin,
                audio_base_url,
                api_base_url,
                address_family,
            },
            self.verbose,
        ))
    }
}

fn parse_ssh_bin_spec(spec: &str) -> Result<Vec<String>> {
    let parts: Vec<String> = Shlex::new(spec).collect();
    if parts.is_empty() {
        anyhow::bail!("ssh client command cannot be empty");
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn parse_ssh_bin_spec_splits_command_and_args() {
        assert_eq!(
            parse_ssh_bin_spec("ssh -p 2222").unwrap(),
            vec!["ssh".to_string(), "-p".to_string(), "2222".to_string()],
        );
    }

    #[test]
    fn parse_ssh_bin_spec_rejects_empty() {
        assert!(parse_ssh_bin_spec("").is_err());
    }

    #[test]
    fn resolve_parses_urls_and_defaults_to_auto_family() {
        let args = Args::parse_from(["late"]);
        let (config, _) = args.resolve().unwrap();
        assert_eq!(config.ssh_target, "late.sh");
        assert_eq!(config.audio_base_url.scheme(), "http");
        assert_eq!(config.api_base_url.scheme(), "https");
        assert_eq!(config.address_family, AddressFamily::Auto);
    }

    #[test]
    fn resolve_picks_ipv4_when_flag_set() {
        let args = Args::parse_from(["late", "-4"]);
        let (config, _) = args.resolve().unwrap();
        assert_eq!(config.address_family, AddressFamily::V4);
    }

    #[test]
    fn resolve_picks_ipv6_when_flag_set() {
        let args = Args::parse_from(["late", "-6"]);
        let (config, _) = args.resolve().unwrap();
        assert_eq!(config.address_family, AddressFamily::V6);
    }

    #[test]
    fn resolve_rejects_bad_audio_url() {
        let args = Args::parse_from(["late", "--audio-base-url", "not a url"]);
        assert!(args.resolve().is_err());
    }

    #[test]
    fn v4_and_v6_are_mutually_exclusive() {
        let result = Args::try_parse_from(["late", "-4", "-6"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_debug_asserts() {
        Args::command().debug_assert();
    }
}
