mod legacy;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    legacy::run().await
}
