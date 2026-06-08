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
    /// Read-and-execute roots: the generic FHS defaults plus any configured
    /// extras (`sandbox.roots`), filtered to those present. No scheme assumed.
    pub toolchain: Vec<PathBuf>,
    /// Extra writable paths outside the workspace (`sandbox.rw`), e.g. a build
    /// cache. Added to every target's `rw` set.
    pub rw: Vec<PathBuf>,
    /// Env var names the command may read: the generic base plus configured
    /// extras (`sandbox.env`), prefixes already expanded. The per-target step
    /// adds the target's declared `env:` keys on top.
    pub env: Vec<String>,
}

/// Build the `giant-sandbox`-wrapped command for an eligible target, writing
/// its `SandboxSpec` under the cache directory first. `cwd` is the absolute
/// working directory the command will run in. Returns the command and the set
/// of paths the sandbox grants (inputs, outputs, toolchain), which the executor
/// uses to explain a denial if the command fails.
pub(super) async fn wrapped_command(
    ctx: &TargetCtx,
    spec: &TargetSpec,
    cwd: &Path,
    policy: &SandboxPolicy,
) -> io::Result<(Command, Vec<PathBuf>)> {
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
        let policy = policy.clone();
        let scratch = scratch.clone();
        tokio::task::spawn_blocking(move || {
            resolve_spec(&spec, &workspace_root, &cache, cwd, &policy, scratch)
        })
        .await
        .map_err(|e| io::Error::other(format!("sandbox spec task: {e}")))??
    };

    // The granted set, for failure diagnosis: anything the command was allowed
    // to touch. A denied path outside this set is an undeclared access.
    let allowed: Vec<PathBuf> = sb
        .ro
        .iter()
        .chain(sb.rw.iter())
        .chain(sb.toolchain.iter())
        .cloned()
        .collect();

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
    Ok((cmd, allowed))
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
    policy: &SandboxPolicy,
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
    rw.extend(policy.rw.iter().cloned());
    rw.sort();
    rw.dedup();

    // The build-wide allowlist (defaults + config, resolved at the CLI
    // boundary) plus this target's declared `env:` keys.
    let mut env: std::collections::BTreeSet<String> = policy.env.iter().cloned().collect();
    env.extend(spec.env.keys().cloned());

    Ok(SandboxSpec {
        schema: SANDBOX_SPEC_SCHEMA,
        cwd,
        ro,
        rw,
        toolchain: policy.toolchain.clone(),
        env: env.into_iter().collect(),
        network: spec.network,
    })
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
