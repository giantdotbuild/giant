//! `giant affected` - list targets that would rebuild given a set of
//! changed files. Doesn't actually run anything.
//!
//! Output: one target ID per line, sorted, on stdout. Designed for
//! piping into `xargs` / `jq` in CI.
//!
//! Same change-source flags as `giant build --affected`: `--base <ref>`
//! for git diff, `--file <path>` (repeatable) for explicit lists.

use crate::git;
use crate::model::TargetId;
use crate::selection;
use clap::Args;
use std::path::{Path, PathBuf};

use super::prep;

#[derive(Args, Debug)]
pub struct AffectedArgs {
    /// Git ref used as the diff baseline. Affected files = everything
    /// changed in the worktree (committed + uncommitted) relative to
    /// this ref, plus untracked-but-not-gitignored files.
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,

    /// Explicit changed-file list. Repeatable; overrides `--base`.
    #[arg(long, value_name = "PATH")]
    pub file: Vec<PathBuf>,

    /// Restrict to targets matching these patterns. Same selection
    /// language as `giant build`: exact ids, globs (`go:*`, `**:test:*`),
    /// and exclusions (`!go:test:*`). See TDD-0011.
    #[arg(add = clap_complete::ArgValueCompleter::new(super::dynamic::complete_target_ids))]
    pub patterns: Vec<String>,

    /// Include only targets carrying this tag. Repeatable.
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,

    /// Exclude targets carrying this tag. Repeatable.
    #[arg(long = "no-tag", value_name = "TAG")]
    pub no_tags: Vec<String>,

    /// Restrict to test targets (mirrors `giant test --affected ...`).
    #[arg(long, conflicts_with = "with_tests")]
    pub tests_only: bool,

    /// Include test targets alongside non-test ones.
    #[arg(long)]
    pub with_tests: bool,
}

pub async fn execute(args: AffectedArgs, global: &super::GlobalFlags) -> anyhow::Result<()> {
    // Bootstrap silently: discovery still has to run so its targets
    // appear in the graph, but per-target log lines would just be
    // noise for a list-affected query.
    let (tx, sink_handle) = prep::null_event_sink();
    let cancel = tokio_util::sync::CancellationToken::new();
    let parallelism = prep::num_cpus_estimate();

    let prepared = match prep::prepare(
        global.config.as_deref(),
        parallelism,
        global.fresh,
        tx,
        cancel,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            sink_handle.abort();
            return Err(e);
        }
    };
    // Allow the sink to drain any final events, then move on.
    drop(prepared.cache);
    let _ = sink_handle.await;

    let changed = resolve_changed_files(&args, prepared.workspace_root.as_path())?;
    let changed_refs: Vec<&Path> = changed.iter().map(|p| p.as_path()).collect();
    let affected = selection::affected_targets(&prepared.graph, &changed_refs);

    // Pattern selection over the full graph, then intersect with the
    // affected set. The order matters: we want a glob-missing-everything
    // to be a silent empty, not an error, while a typo'd exact id should
    // still bail - and `resolve_patterns` already handles that.
    let opts = selection::SelectionOpts {
        tags: args.tags.clone(),
        no_tags: args.no_tags.clone(),
    };
    let test_mode = match (args.tests_only, args.with_tests) {
        (true, _) => selection::TestMode::Only,
        (_, true) => selection::TestMode::Include,
        _ => selection::TestMode::Exclude,
    };
    let pattern_set: Vec<TargetId> =
        selection::resolve_patterns(&prepared.graph, &args.patterns, test_mode, &opts)?;
    let mut out: Vec<TargetId> = pattern_set
        .into_iter()
        .filter(|id| affected.contains(id))
        .collect();
    out.sort();

    // Plain stdout: one ID per line. Empty result is exit 0 with no
    // output - most CI scripts want this (`if [ -z "$(giant affected …)" ]`).
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    for id in &out {
        let _ = writeln!(w, "{id}");
    }
    let _ = w.flush();

    Ok(())
}

fn resolve_changed_files(
    args: &AffectedArgs,
    workspace_root: &Path,
) -> anyhow::Result<Vec<PathBuf>> {
    if !args.file.is_empty() {
        return Ok(args
            .file
            .iter()
            .map(|p| {
                p.strip_prefix(workspace_root)
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|_| p.clone())
            })
            .collect());
    }
    let base = args.base.as_deref().ok_or_else(|| {
        anyhow::anyhow!("`giant affected` requires --base <ref> or one or more --file <path>")
    })?;
    git::affected_files_since(workspace_root, base)
        .map_err(|e| anyhow::anyhow!("affected detection: {e}"))
}
