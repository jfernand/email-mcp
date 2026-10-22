mod describe;
mod server;

use std::sync::Arc;

use email_engine::{Engine, tls};
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: email-mcp               serve MCP over stdio
       email-mcp --describe    print the server instructions and every tool, for review
       email-mcp --describe --json   the same, as JSON";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .collect();
    let args: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .collect();
    match args.as_slice() {
        [] => {}
        ["--describe"] | ["--describe", "--json"] | ["--json", "--describe"] => {
            // No config or network needed: the tool list is static.
            let server = server::EmailServer::new(Arc::new(Engine::default()));
            if args.contains(&"--json") {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&describe::json(&server))?
                );
            } else {
                print!("{}", describe::text(&server));
            }
            return Ok(());
        }
        ["-h"] | ["--help"] => {
            println!("{USAGE}");
            return Ok(());
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }

    // stdout carries the MCP protocol, so logs must go to stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();
    tls::init();

    let engine = Engine::load()?;
    let service = server::EmailServer::new(engine)
        .serve(rmcp::transport::stdio_pipes())
        .await?;
    service
        .waiting()
        .await?;
    Ok(())
}
