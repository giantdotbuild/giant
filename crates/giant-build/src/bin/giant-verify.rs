//! `giant verify` - hermeticity audit (ADR-0030 §5): `build` with the sandbox
//! and a fresh build forced on, over every target (tests included). Porcelain.

use clap::Parser;
use giant::selection::TestMode;
use giant_build::BuildArgs;

#[derive(Parser, Debug)]
#[command(
    name = "giant-verify",
    about = "Audit hermeticity: build every target sandboxed, cache bypassed"
)]
struct Cli {
    #[command(flatten)]
    args: BuildArgs,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let mut cli = Cli::parse();
    // Force sandbox enforcement and cache bypass regardless of the flags given.
    cli.args.fresh = true;
    cli.args.sandbox = true;
    match giant_build::run(cli.args, TestMode::Include).await {
        Ok(code) => std::process::ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("giant verify: {e:#}");
            std::process::ExitCode::from(1)
        }
    }
}
