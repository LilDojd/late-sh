mod audio;
mod cli;
mod identity;
mod legacy;
mod logging;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    legacy::run().await
}
