//! A DNS seeder for the Zcash network.
//!
//! Crawls the network with `zebra-network` and serves the addresses of
//! recently-live, version-current peers over DNS (via `hickory-dns`) so new
//! nodes can bootstrap.

use color_eyre::eyre::Result;

mod commands;
mod config;
mod crawl;
mod dns;
mod metrics;
mod seeder;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    color_eyre::install()?;
    commands::SeederApp::run().await
}
