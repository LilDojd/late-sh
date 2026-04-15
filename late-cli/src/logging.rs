use anyhow::{Context, Result};
use clap_verbosity_flag::Verbosity;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

pub(crate) fn init(verbosity: &Verbosity) -> Result<Option<PathBuf>> {
    if verbosity.is_silent() {
        return Ok(None);
    }

    let level = verbosity.tracing_level_filter();
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("late={level},symphonia=error,warn")));

    let path = log_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log directory {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open log file {}", path.display()))?;

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(file)
        .with_ansi(false)
        .try_init()
        .map_err(|err| anyhow::anyhow!("failed to initialize logging: {err}"))?;
    Ok(Some(path))
}

fn log_path() -> PathBuf {
    let base = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
    base.join("late").join("late.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_path_points_into_late_subdir() {
        let p = log_path();
        assert_eq!(p.file_name().and_then(|s| s.to_str()), Some("late.log"));
        assert_eq!(
            p.parent()
                .and_then(|d| d.file_name())
                .and_then(|s| s.to_str()),
            Some("late")
        );
    }
}
