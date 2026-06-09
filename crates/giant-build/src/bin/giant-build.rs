//! `giant build` - build targets (non-test by default). Porcelain.

use clap::Parser;
use giant::selection::TestMode;
use giant_build::BuildArgs;

#[derive(Parser, Debug)]
#[command(name = "giant-build", about = "Build targets")]
struct Cli {
    #[command(flatten)]
    args: BuildArgs,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match giant_build::run(cli.args, TestMode::Exclude).await {
        Ok(code) => std::process::ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("giant build: {e:#}");
            std::process::ExitCode::from(1)
        }
    }
}
