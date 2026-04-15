use anyhow::Result;
use clap_verbosity_flag::{InfoLevel, Verbosity};
use tracing_subscriber::EnvFilter;

pub(crate) fn init(verbosity: &Verbosity<InfoLevel>) -> Result<()> {
    let level = verbosity.tracing_level_filter();
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("late={level},symphonia=error,warn")));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .try_init()
        .map_err(|err| anyhow::anyhow!("failed to initialize logging: {err}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Sanity: the function accepts a default Verbosity<InfoLevel> without panicking.
    // Tracing subscribers are global, so we only assert it does not panic to parse.
    #[test]
    fn init_accepts_default_verbosity() {
        let v: Verbosity<InfoLevel> = Verbosity::new(0, 0);
        // Ignore error — repeated init inside the same test process will fail.
        let _ = init(&v);
    }
}
