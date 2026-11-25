//! Runs the mock API until killed. Prints the gateway URL to use in config.toml.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mock = inxt_mock::Mock::start().await?;
    println!("{}", mock.gateway_url());
    println!("token: {}", mock.token());
    std::future::pending::<()>().await;
    Ok(())
}
