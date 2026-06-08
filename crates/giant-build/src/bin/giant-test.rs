//! `giant test` - same machinery as `giant build`, restricted to `test: true`
//! targets. Porcelain (ADR-0034).

use clap::Parser;
use giant::selection::TestMode;
use giant_build::BuildArgs;

#[derive(Parser, Debug)]
#[command(name = "giant-test", about = "Run test targets")]
struct Cli {
    #[command(flatten)]
    args: BuildArgs,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match giant_build::run(cli.args, TestMode::Only).await {
        Ok(code) => std::process::ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("giant test: {e:#}");
            std::process::ExitCode::from(1)
        }
    }
}
