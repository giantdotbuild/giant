//! Sandbox wiring (ADR-0030, TDD-0025).
//!
//! When `--sandbox` mode is on, the executor runs an eligible target's command
//! through the `giant-sandbox` porcelain instead of directly. This module
//! resolves the bind set - the same inputs/outputs the cache key already
//! resolved - writes it as a `SandboxSpec`, and builds the wrapped command
//! (`giant-sandbox run --spec <file> -- sh -c <command>`). All the mechanism
//! lives in the porcelain; the engine only produces the spec and the argv.

use std::io;
use std::path::{Path, PathBuf};

use giant_schema::{SANDBOX_SPEC_SCHEMA, SandboxSpec};
use tokio::process::Command;

use super::TargetCtx;
use crate::model::TargetSpec;
use crate::paths::AbsPath;

/// Resolved sandbox configuration for a build, present only under `--sandbox`.
/// The helper path is found once at the CLI boundary so a missing
/// `giant-sandbox` fails before any target runs (ADR-0030 §6).
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    /// Absolute path to the `giant-sandbox` helper.
    pub helper: PathBuf,
    /// Toolchain paths granted read + execute (v1: `/nix/store` when present).
    pub toolchain: Vec<PathBuf>,
}

/// Build the `giant-sandbox`-wrapped command for an eligible target, writing
/// its `SandboxSpec` under the cache directory first. `cwd` is the absolute
/// working directory the command will run in.
pub(super) async fn wrapped_command(
    ctx: &TargetCtx,
    spec: &TargetSpec,
    cwd: &Path,
    policy: &SandboxPolicy,
) -> io::Result<Command> {
    // A per-target scratch dir, granted read-write and pointed at via TMPDIR.
    // We deliberately do *not* grant the whole system temp dir: a workspace
    // under /tmp would then be readable, defeating enforcement.
    let scratch = ctx
        .cache
        .root()
        .as_path()
        .join("sandbox")
        .join("scratch")
        .join(sanitize(spec.id.as_str()));
    tokio::fs::create_dir_all(&scratch).await?;

    // Glob expansion walks the workspace; keep it off the async worker.
    let sb = {
        let spec = spec.clone();
        let workspace_root = ctx.workspace_root.clone();
        let cache = ctx.cache.clone();
        let cwd = cwd.to_path_buf();
        let toolchain = policy.toolchain.clone();
        let scratch = scratch.clone();
        tokio::task::spawn_blocking(move || {
            resolve_spec(&spec, &workspace_root, &cache, cwd, toolchain, scratch)
        })
        .await
        .map_err(|e| io::Error::other(format!("sandbox spec task: {e}")))??
    };

    let spec_path = write_spec(ctx, spec, &sb).await?;

    let mut cmd = Command::new(&policy.helper);
    cmd.arg("run")
        .arg("--spec")
        .arg(&spec_path)
        .arg("--")
        .arg("sh")
        .arg("-c")
        .arg(&spec.command);
    // Tools that honour TMPDIR write scratch into the granted dir rather than
    // the ungranted /tmp. Set on the helper process; birdcage forwards it.
    cmd.env("TMPDIR", &scratch);
    Ok(cmd)
}

/// Resolve the bind set. `ro` is the target's declared file inputs (which, by
/// output-based dep inference, already include any dependency outputs the
/// command actually reads); `rw` is each declared output's directory plus the
/// per-target scratch dir. Network rides the per-target field.
fn resolve_spec(
    spec: &TargetSpec,
    workspace_root: &AbsPath,
    cache: &crate::cache::LocalCache,
    cwd: PathBuf,
    toolchain: Vec<PathBuf>,
    scratch: PathBuf,
) -> io::Result<SandboxSpec> {
    let mut ro = super::key::resolve_input_paths(spec, workspace_root, cache)?;
    ro.sort();
    ro.dedup();

    let mut rw: Vec<PathBuf> = spec
        .outputs
        .iter()
        .filter_map(|o| super::run::output_parent_to_create(o.as_path()))
        .map(|dir| workspace_root.as_path().join(dir))
        .collect();
    rw.push(scratch);
    rw.sort();
    rw.dedup();

    Ok(SandboxSpec {
        schema: SANDBOX_SPEC_SCHEMA,
        cwd,
        ro,
        rw,
        toolchain,
        env: env_allowlist(spec),
        network: spec.network,
    })
}

/// The environment a sandboxed command may read (ADR-0030 §4). Unlike Bazel's
/// fixed `PATH`, giant runs inside devenv where `PATH` and a handful of vars
/// *are* the toolchain, so we keep those plus the target's declared `env:` and
/// drop everything else (random user/CI vars - the non-hermetic part).
fn env_allowlist(spec: &TargetSpec) -> Vec<String> {
    // Always-keep names: the shell/runtime basics plus the Nix/locale/TLS vars
    // a devenv toolchain relies on. TMPDIR is set by the wrapper to the scratch
    // dir, so the child must be allowed to read it.
    const BASE: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "SHELL",
        "TERM",
        "TZ",
        "TMPDIR",
        "LANG",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "LOCALE_ARCHIVE",
        "PKG_CONFIG_PATH",
        "LD_LIBRARY_PATH",
    ];
    // Whole families a Nix/devenv environment populates.
    const PREFIXES: &[&str] = &["NIX_", "LC_", "DEVENV_", "GIANT_"];

    let mut names: std::collections::BTreeSet<String> =
        BASE.iter().map(|s| (*s).to_string()).collect();
    names.extend(spec.env.keys().cloned());
    names.extend(
        std::env::vars()
            .map(|(k, _)| k)
            .filter(|k| PREFIXES.iter().any(|p| k.starts_with(p))),
    );
    names.into_iter().collect()
}

/// Write the spec under `<cache>/sandbox/<target>.json`. The cache dir is
/// gitignored and watch-excluded, so it is a safe transient home.
async fn write_spec(ctx: &TargetCtx, spec: &TargetSpec, sb: &SandboxSpec) -> io::Result<PathBuf> {
    let dir = ctx.cache.root().as_path().join("sandbox");
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{}.json", sanitize(spec.id.as_str())));
    let bytes = serde_json::to_vec(sb).map_err(io::Error::other)?;
    tokio::fs::write(&path, bytes).await?;
    Ok(path)
}

/// Turn a target label (`//pkg/sub:name`) into a safe filename stem.
fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}
