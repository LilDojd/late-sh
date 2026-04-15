mod audio;
mod cli;
mod identity;
mod legacy;
mod logging;
mod pair;
mod ssh;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    legacy::run().await
}
