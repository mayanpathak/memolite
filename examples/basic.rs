use memolite::MemoryEngine;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _engine = MemoryEngine::open("./memolite.db").await?;

    println!("ok");

    Ok(())
}
