use color_eyre::eyre::Result;

pub mod commands;
pub mod config;
pub mod metrics;
pub mod probe;
pub mod rpc_tip;
pub mod server;
pub mod tip_filter;

#[cfg(test)]
mod tests;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    color_eyre::install()?;
    commands::SeederApp::run().await
}
