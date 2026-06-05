//! Host-owned I/O: the impure operations the `ws` methods delegate to. Keeping
//! every side effect here (filesystem, process) is what lets the host normalize
//! for determinism in one place (TDD-0024 §F).

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Workspace-relative paths under `root` matching `pattern`, sorted and
/// gitignore-aware (so the set mirrors what the engine will scan).
pub(crate) fn glob(root: &Path, pattern: &str) -> Result<Vec<String>> {
    let pat = glob::Pattern::new(pattern).with_context(|| format!("invalid glob '{pattern}'"))?;
    let mut out = Vec::new();
    for entry in ignore::Walk::new(root).flatten() {
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if pat.matches(&rel) {
            out.push(rel);
        }
    }
    out.sort();
    Ok(out)
}

/// The captured result of a subprocess run.
pub(crate) struct ExecOut {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) code: i32,
}

/// Run `args` from `root` (or `root/cwd` when given), capturing stdout/stderr.
/// With `check`, a nonzero exit is an error carrying stderr.
pub(crate) fn exec(
    root: &Path,
    args: &[String],
    cwd: Option<&str>,
    check: bool,
) -> Result<ExecOut> {
    let Some((cmd, rest)) = args.split_first() else {
        bail!("ws.exec needs a non-empty command");
    };
    let dir = match cwd {
        Some(c) => root.join(c),
        None => root.to_path_buf(),
    };
    let output = Command::new(cmd)
        .args(rest)
        .current_dir(&dir)
        .output()
        .with_context(|| format!("running {cmd}"))?;
    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if check && code != 0 {
        bail!("`{cmd}` exited with {code}:\n{stderr}");
    }
    Ok(ExecOut {
        stdout,
        stderr,
        code,
    })
}

/// Relativize a `//`-rooted or absolute path against the workspace root.
/// A plain relative path is returned unchanged.
pub(crate) fn rel(root: &Path, path: &str) -> String {
    if let Some(stripped) = path.strip_prefix("//") {
        return stripped.to_string();
    }
    let p = Path::new(path);
    if p.is_absolute()
        && let Ok(stripped) = p.strip_prefix(root)
    {
        return stripped.to_string_lossy().replace('\\', "/");
    }
    path.to_string()
}
