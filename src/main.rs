mod app;
mod compose;
mod config;
mod imap;
mod oauth;
mod secrets;
mod server;
mod smtp;
mod tls;

use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stdout carries the MCP protocol, so logs must go to stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();
    tls::init();

    let app = app::App::load()?;
    let service = server::EmailServer::new(app)
        .serve(rmcp::transport::stdio_pipes())
        .await?;
    service.waiting().await?;
    Ok(())
}
