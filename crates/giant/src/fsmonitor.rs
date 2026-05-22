//! Git fsmonitor hook protocol v2 client (TDD-0016).
//!
//! When `core.fsmonitor` is set, we delegate "what changed since last
//! run?" to the configured monitor and use the result to narrow the
//! recorded-reads verifier. The protocol is opaque: we hand it a
//! token, it returns a new token + workspace-relative changed paths,
//! or a single `/` meaning "fresh instance - assume everything moved."
//!
//! Token persistence is deferred until the verifier and build are
//! done - committing a new token earlier would lose change reports on
//! crash.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

/// Past this the monitor is treated as wedged; we degrade to full
/// verification (fresh-instance result).
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Token filename inside the configured state directory.
const TOKEN_FILE: &str = "fsmonitor-token";

/// Sentinel path the fsmonitor protocol uses to mean "fresh instance -
/// treat every recorded path as potentially changed."
const FRESH_INSTANCE_MARKER: &[u8] = b"/";

#[derive(Debug, Clone)]
pub enum ChangeSet {
    /// Every recorded path is potentially changed; the verifier must
    /// do a full check.
    FreshInstance,
    /// Only these workspace-relative paths have changed since the
    /// prior token. Empty set means "nothing changed."
    Delta(HashSet<PathBuf>),
}

impl ChangeSet {
    /// Does this changeset rule out a file entry? Used to short-circuit
    /// per-entry verify. `path` is workspace-relative.
    pub fn file_might_have_changed(&self, path: &Path) -> bool {
        match self {
            ChangeSet::FreshInstance => true,
            ChangeSet::Delta(set) => set.contains(path),
        }
    }

    /// Same idea, for a directory entry: any change under the dir
    /// counts. Path comparison is by prefix.
    pub fn dir_might_have_changed(&self, path: &Path) -> bool {
        match self {
            ChangeSet::FreshInstance => true,
            ChangeSet::Delta(set) => set.iter().any(|p| p.starts_with(path) || p == path),
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, ChangeSet::Delta(s) if s.is_empty())
    }
}

#[derive(Debug)]
enum Backend {
    /// `git fsmonitor--daemon query <token>`.
    BuiltinDaemon,
    /// External hook script, invoked as `<script> 2 <token>` (protocol
    /// v2). Path is whatever `core.fsmonitor` is set to.
    HookScript(PathBuf),
}

pub struct FsmonitorClient {
    workspace_root: PathBuf,
    state_dir: PathBuf,
    backend: Backend,
    last_token: Option<String>,
    new_token: Option<String>,
}

impl FsmonitorClient {
    /// `Ok(None)` if fsmonitor isn't configured (or the workspace isn't
    /// in a git worktree); `Ok(Some(_))` when a backend is selected.
    /// `state_dir` is where the token is persisted (typically
    /// `<workspace_root>/.giant`, but `state.dir` in giant.yaml lets
    /// users override it).
    pub async fn open(workspace_root: &Path, state_dir: &Path) -> io::Result<Option<Self>> {
        let Ok(repo) = crate::git::open(workspace_root) else {
            return Ok(None);
        };
        let cfg = repo.config_snapshot();
        let raw = cfg.string("core.fsmonitor").map(|v| v.to_string());
        drop(cfg);
        let Some(raw) = raw else {
            return Ok(None);
        };

        let backend = match raw.as_str() {
            "" | "false" | "0" => return Ok(None),
            "true" | "1" => Backend::BuiltinDaemon,
            other => Backend::HookScript(PathBuf::from(other)),
        };

        let last_token = read_token(state_dir).ok();
        Ok(Some(Self {
            workspace_root: workspace_root.to_path_buf(),
            state_dir: state_dir.to_path_buf(),
            backend,
            last_token,
            new_token: None,
        }))
    }

    /// Ask the monitor what changed since the last token. Caches the
    /// new token internally for [`persist_token`] to write atomically
    /// at the end of the build.
    pub async fn query(&mut self) -> io::Result<ChangeSet> {
        let token = self.last_token.clone().unwrap_or_default();
        let mut cmd = match &self.backend {
            Backend::BuiltinDaemon => {
                let mut c = Command::new("git");
                c.args(["fsmonitor--daemon", "query", &token]);
                c
            }
            Backend::HookScript(path) => {
                let mut c = Command::new(path);
                c.arg("2").arg(&token);
                c
            }
        };
        cmd.current_dir(&self.workspace_root);

        // Any failure (timeout, spawn error, non-zero exit) degrades
        // to fresh instance so the verifier does its own full check.
        let out = match timeout(QUERY_TIMEOUT, cmd.output()).await {
            Ok(Ok(o)) if o.status.success() => o,
            _ => return Ok(ChangeSet::FreshInstance),
        };

        Ok(parse_response(&out.stdout, &mut self.new_token))
    }

    /// Persist the token. Caller must invoke at the **end** of the
    /// build - committing earlier loses change reports on crash.
    pub async fn persist_token(&self) -> io::Result<()> {
        let Some(tok) = self.new_token.as_ref() else {
            return Ok(());
        };
        crate::cache::atomic_write_output(
            self.state_dir.join(TOKEN_FILE),
            tok.clone().into_bytes(),
            false,
        )
        .await
    }
}

/// Parse the NUL-delimited response: first record is the new token,
/// subsequent are workspace-relative paths. A single `/` anywhere
/// signals fresh instance.
fn parse_response(stdout: &[u8], new_token_out: &mut Option<String>) -> ChangeSet {
    let mut iter = stdout.split(|b| *b == 0);
    let Some(tok_bytes) = iter.next() else {
        return ChangeSet::FreshInstance;
    };
    let Ok(tok) = std::str::from_utf8(tok_bytes) else {
        return ChangeSet::FreshInstance;
    };
    *new_token_out = Some(tok.to_string());

    let mut paths = HashSet::new();
    for rec in iter {
        if rec.is_empty() {
            continue;
        }
        if rec == FRESH_INSTANCE_MARKER {
            return ChangeSet::FreshInstance;
        }
        let Ok(s) = std::str::from_utf8(rec) else {
            return ChangeSet::FreshInstance;
        };
        paths.insert(PathBuf::from(s));
    }
    ChangeSet::Delta(paths)
}

fn read_token(state_dir: &Path) -> io::Result<String> {
    let s = std::fs::read_to_string(state_dir.join(TOKEN_FILE))?;
    Ok(s.trim_end_matches('\n').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_response_is_delta_empty() {
        // Empty input produces an empty token + no paths → Delta(empty).
        // Permissive but safe; subsequent fresh-instance tests cover
        // the harder degradation path.
        let mut tok = None;
        assert!(matches!(parse_response(b"", &mut tok), ChangeSet::Delta(s) if s.is_empty()));
        assert!(tok.is_some());
    }

    #[test]
    fn parse_token_only_means_nothing_changed() {
        let mut tok = None;
        let r = parse_response(b"abc123\0", &mut tok);
        assert!(matches!(r, ChangeSet::Delta(s) if s.is_empty()));
        assert_eq!(tok.as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_token_with_paths() {
        let mut tok = None;
        let r = parse_response(b"tok\0src/foo.go\0Cargo.toml\0", &mut tok);
        match r {
            ChangeSet::Delta(s) => {
                assert!(s.contains(Path::new("src/foo.go")));
                assert!(s.contains(Path::new("Cargo.toml")));
                assert_eq!(s.len(), 2);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_fresh_instance_marker() {
        let mut tok = None;
        let r = parse_response(b"tok\0/\0", &mut tok);
        assert!(matches!(r, ChangeSet::FreshInstance));
    }

    #[test]
    fn parse_invalid_utf8_in_path_degrades_to_fresh() {
        let mut tok = None;
        let r = parse_response(b"tok\0bad\xff\xff\0", &mut tok);
        assert!(matches!(r, ChangeSet::FreshInstance));
    }

    #[test]
    fn changeset_file_matching() {
        let mut set = HashSet::new();
        set.insert(PathBuf::from("src/a.go"));
        let cs = ChangeSet::Delta(set);
        assert!(cs.file_might_have_changed(Path::new("src/a.go")));
        assert!(!cs.file_might_have_changed(Path::new("src/b.go")));
    }

    #[test]
    fn changeset_dir_prefix_match() {
        let mut set = HashSet::new();
        set.insert(PathBuf::from("src/pkg/a.go"));
        let cs = ChangeSet::Delta(set);
        assert!(cs.dir_might_have_changed(Path::new("src/pkg")));
        assert!(!cs.dir_might_have_changed(Path::new("src/other")));
    }
}
