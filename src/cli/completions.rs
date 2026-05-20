//! `giant completions <shell>` - emit a static completion script for
//! the given shell. Six shells supported (bash, zsh, fish, PowerShell,
//! elvish via clap_complete; nushell via clap_complete_nushell).
//!
//! Dynamic completion of target IDs and task names lives in the
//! `dynamic.rs` module - it runs at TAB-time, not when the script is
//! generated.

use clap::{Args, CommandFactory};
use clap_complete::Shell;

#[derive(Args, Debug)]
pub struct CompletionsArgs {
    /// Shell to generate completions for.
    pub shell: ShellChoice,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ShellChoice {
    Bash,
    Zsh,
    Fish,
    Powershell,
    Elvish,
    Nushell,
}

pub fn execute(args: CompletionsArgs) -> anyhow::Result<()> {
    let mut cmd = super::Cli::command();
    let name = cmd.get_name().to_string();
    let mut out = std::io::stdout().lock();
    match args.shell {
        ShellChoice::Bash => clap_complete::generate(Shell::Bash, &mut cmd, &name, &mut out),
        ShellChoice::Zsh => clap_complete::generate(Shell::Zsh, &mut cmd, &name, &mut out),
        ShellChoice::Fish => clap_complete::generate(Shell::Fish, &mut cmd, &name, &mut out),
        ShellChoice::Powershell => {
            clap_complete::generate(Shell::PowerShell, &mut cmd, &name, &mut out)
        }
        ShellChoice::Elvish => clap_complete::generate(Shell::Elvish, &mut cmd, &name, &mut out),
        ShellChoice::Nushell => {
            clap_complete::generate(clap_complete_nushell::Nushell, &mut cmd, &name, &mut out)
        }
    }
    Ok(())
}
