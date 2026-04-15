use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

pub fn ensure() -> Result<PathBuf> {
    let dedicated_key = dedicated_path()?;
    if dedicated_key.exists() {
        return Ok(dedicated_key);
    }

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "no SSH identity found; generate {} manually or rerun in an interactive terminal",
            dedicated_key.display()
        );
    }

    prompt_generate(&dedicated_key)?;
    Ok(dedicated_key)
}

fn ssh_dir() -> Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".ssh"))
}

fn dedicated_path() -> Result<PathBuf> {
    Ok(ssh_dir()?.join("id_late_sh_ed25519"))
}

fn prompt_generate(path: &Path) -> Result<()> {
    print!(
        "No SSH key found for late.sh.\n\
         Generate a dedicated Ed25519 key at {}? [y/N]: ",
        path.display()
    );
    std::io::stdout()
        .flush()
        .context("failed to flush prompt")?;

    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("failed to read prompt response")?;

    if !is_affirmative(input.trim()) {
        anyhow::bail!("SSH key generation declined");
    }

    generate(path)
}

pub fn is_affirmative(input: &str) -> bool {
    matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    )
}

fn generate(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("generated identity path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }

    let status = std::process::Command::new("ssh-keygen")
        .arg("-t")
        .arg("ed25519")
        .arg("-f")
        .arg(path)
        .arg("-N")
        .arg("")
        .arg("-C")
        .arg("late.sh cli")
        .status()
        .context("failed to run ssh-keygen")?;

    if !status.success() {
        anyhow::bail!("ssh-keygen exited with status {status}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affirmative_accepts_lower_upper_mixed_case() {
        for good in ["y", "Y", "yes", "YES", "Yes", "yES", "yeS"] {
            assert!(is_affirmative(good), "{good} should be affirmative");
        }
    }

    #[test]
    fn affirmative_rejects_other_input() {
        for bad in ["n", "N", "no", "NO", "", "sure", "ok"] {
            assert!(!is_affirmative(bad), "{bad} should not be affirmative");
        }
    }

    #[test]
    fn affirmative_trims_whitespace() {
        assert!(is_affirmative("  y\n"));
        assert!(is_affirmative(" yes "));
    }
}
