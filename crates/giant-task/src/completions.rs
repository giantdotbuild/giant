//! Shell-completion support for giant-task.
//!
//! Static scripts via `giant-task --completions <shell>` (six shells).
//! Dynamic completion of task names by reading the nearest giant.yaml
//! at TAB time - fast enough at typical config sizes.

use crate::config::TaskConfig;
use crate::workspace;
use clap::CommandFactory;
use clap_complete::{CompletionCandidate, Shell};
use std::ffi::OsStr;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ShellChoice {
    Bash,
    Zsh,
    Fish,
    Powershell,
    Elvish,
    Nushell,
}

pub fn emit(shell: ShellChoice) {
    let mut cmd = crate::Cli::command();
    let name = cmd.get_name().to_string();
    let mut out = std::io::stdout().lock();
    match shell {
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
}

/// Dynamic completion for the task-name positional. Reads the nearest
/// giant.yaml and returns task names. Failure is silent - no
/// candidates is better than an error popping up at TAB time.
pub fn complete_task_names(current: &OsStr) -> Vec<CompletionCandidate> {
    let prefix = current.to_string_lossy();
    let candidates = (|| -> Option<Vec<CompletionCandidate>> {
        let cwd = std::env::current_dir().ok()?;
        let cfg_path = workspace::find_config(&cwd).ok()?;
        let cfg = TaskConfig::load(&cfg_path).ok()?;
        let out: Vec<CompletionCandidate> = cfg
            .tasks
            .iter()
            .filter(|(n, _)| n.starts_with(prefix.as_ref()))
            .map(|(n, spec)| {
                let mut cand = CompletionCandidate::new(n);
                if let Some(desc) = spec.description.clone() {
                    cand = cand.help(Some(desc.into()));
                }
                cand
            })
            .collect();
        Some(out)
    })();
    candidates.unwrap_or_default()
}
