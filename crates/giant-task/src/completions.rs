//! Shell-completion support for giant-task.
//!
//! Static scripts via `giant-task --completions <shell>` (six shells).
//! Dynamic completion of task names by reading the nearest giant.yaml
//! at TAB time - fast enough at typical config sizes.

use crate::config::TaskConfig;
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

/// Dynamic completion for the task-name positional. Scans the workspace
/// and returns bare task names plus their `//pkg:name` labels. Failure is
/// silent - no candidates is better than an error popping up at TAB time.
pub fn complete_task_names(current: &OsStr) -> Vec<CompletionCandidate> {
    let prefix = current.to_string_lossy();
    let candidates = (|| -> Option<Vec<CompletionCandidate>> {
        let cfg = TaskConfig::scan(None).ok()?;
        let mut out = Vec::new();
        let mut seen_bare = std::collections::HashSet::new();
        for (label, task) in &cfg.tasks {
            let help = || task.spec.description.clone().map(Into::into);
            // Bare name (deduped across packages) for the common case.
            if seen_bare.insert(task.name.as_str()) && task.name.starts_with(prefix.as_ref()) {
                out.push(CompletionCandidate::new(&task.name).help(help()));
            }
            // Full label, for qualifying an ambiguous bare name.
            if label.starts_with(prefix.as_ref()) {
                out.push(CompletionCandidate::new(label).help(help()));
            }
        }
        Some(out)
    })();
    candidates.unwrap_or_default()
}
