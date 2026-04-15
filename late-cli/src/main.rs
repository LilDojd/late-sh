mod cli;
mod legacy;
mod logging;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    legacy::run().await
}
