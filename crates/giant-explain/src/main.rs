//! giant-explain - show what feeds a target's cache key, and whether it is
//! currently cached. The first thing to reach for when "why did this rebuild?"
//! comes up.
//!
//! Porcelain (ADR-0034), dispatched as `giant explain`. It does not recompute
//! anything: it asks a `giant session` over the protocol (`query.explain`) and
//! renders the `query.explained` reply. `--diff` runs two queries and compares
//! them client-side.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use anyhow::Result;
use clap::Parser;
use giant_protocol::commands::Command;
use giant_protocol::events::{Event, ExplainEnv};

#[derive(Parser, Debug)]
#[command(name = "giant-explain", about = "Show what feeds a target's cache key")]
struct Cli {
    /// Target ID to explain.
    target: String,

    /// Compare this target's cache-key breakdown against another target's.
    /// Useful for "why does target X have a different key than target Y?" The
    /// output is a unified diff of command, cwd, env, file inputs, and deps.
    #[arg(long, value_name = "OTHER_TARGET")]
    diff: Option<String>,

    /// Path to giant.yaml (defaults to walking up from the current directory).
    #[arg(long)]
    config: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("giant explain: {e:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let config = cli.config.as_deref();

    let left = explain(config, &cli.target).await?;

    if let Some(other) = &cli.diff {
        let right = explain(config, other).await?;
        print_diff(&left, &right);
    } else {
        print_breakdown(&left);
    }
    Ok(())
}

/// Ask a session for one target's `query.explained` reply.
async fn explain(config: Option<&std::path::Path>, target: &str) -> Result<Explained> {
    let command = Command::QueryExplain {
        command_id: Some("e1".into()),
        target: giant_protocol::TargetId::new(target),
    };
    let events = giant_protocol::query_session(config, command, |e| {
        matches!(e, Event::QueryExplained { command_id, .. } if command_id.as_deref() == Some("e1"))
    })
    .await?;

    events
        .into_iter()
        .find_map(|e| match e {
            Event::QueryExplained { .. } => Explained::from_event(e),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("session did not return a breakdown for {target}"))
}

/// The fields of a `query.explained` event, flattened for rendering.
struct Explained {
    target: String,
    key: String,
    command: String,
    cwd: String,
    user_env: EnvPairs,
    built_in_env: EnvPairs,
    file_inputs: Vec<FileInput>,
    deps: EnvPairs,
    cache_hit: Option<giant_protocol::events::ExplainCacheHit>,
}

struct FileInput {
    path: String,
    hash: String,
    size: u64,
}

/// `(key, value)` env pairs, the renderer's working shape.
type EnvPairs = Vec<(String, String)>;

impl Explained {
    fn from_event(e: Event) -> Option<Self> {
        let Event::QueryExplained {
            target,
            key,
            command,
            cwd,
            file_inputs,
            deps,
            env,
            cache_hit,
            ..
        } = e
        else {
            return None;
        };
        let (user_env, built_in_env) = split_env(env);
        Some(Self {
            target: target.as_str().to_string(),
            key,
            command,
            cwd,
            user_env,
            built_in_env,
            file_inputs: file_inputs
                .into_iter()
                .map(|f| FileInput {
                    path: f.path,
                    hash: f.hash,
                    size: f.size,
                })
                .collect(),
            deps: deps
                .into_iter()
                .map(|d| (d.id.as_str().to_string(), d.output_hash))
                .collect(),
            cache_hit,
        })
    }
}

/// The protocol carries env as a single list with a `built_in` flag; the
/// renderer wants them grouped the way `query.explain` filled them.
fn split_env(env: Vec<ExplainEnv>) -> (EnvPairs, EnvPairs) {
    let mut user = Vec::new();
    let mut built_in = Vec::new();
    for e in env {
        if e.built_in {
            built_in.push((e.key, e.value));
        } else {
            user.push((e.key, e.value));
        }
    }
    (user, built_in)
}

fn print_breakdown(ex: &Explained) {
    let stdout = std::io::stdout();
    let mut w = stdout.lock();

    let _ = writeln!(w, "target:      {}", ex.target);
    let _ = writeln!(w, "cache key:   {}", ex.key);
    match &ex.cache_hit {
        Some(hit) => {
            let _ = writeln!(
                w,
                "cache state: HIT (built {}, {}ms, exit {})",
                hit.built_at, hit.duration_ms, hit.exit_code
            );
        }
        None => {
            let _ = writeln!(w, "cache state: miss (next build will populate)");
        }
    }
    let _ = writeln!(w);

    let _ = writeln!(w, "command:");
    let _ = writeln!(w, "  {}", ex.command);
    let _ = writeln!(w);

    let cwd_display = if ex.cwd.is_empty() {
        "<workspace root>"
    } else {
        &ex.cwd
    };
    let _ = writeln!(w, "cwd:         {cwd_display}");
    let _ = writeln!(w);

    let total_env = ex.user_env.len() + ex.built_in_env.len();
    let _ = writeln!(w, "env ({total_env}):");
    for (k, v) in &ex.user_env {
        let _ = writeln!(w, "  {k}={v}");
    }
    for (k, v) in &ex.built_in_env {
        let _ = writeln!(w, "  {k}={v}  (built-in)");
    }
    let _ = writeln!(w);

    let _ = writeln!(w, "file inputs ({}):", ex.file_inputs.len());
    for f in &ex.file_inputs {
        let _ = writeln!(
            w,
            "  {:<60} {}  {}",
            f.path,
            short(&f.hash),
            human_bytes(f.size)
        );
    }
    if ex.file_inputs.is_empty() {
        let _ = writeln!(w, "  (none)");
    }
    let _ = writeln!(w);

    let _ = writeln!(w, "deps ({}):", ex.deps.len());
    for (id, oh) in &ex.deps {
        let _ = writeln!(w, "  {:<60} {}", id, short(oh));
    }
    if ex.deps.is_empty() {
        let _ = writeln!(w, "  (none)");
    }
    let _ = writeln!(w);

    if let Some(hit) = &ex.cache_hit {
        let _ = writeln!(w, "outputs (from cache, {}):", hit.outputs.len());
        for o in &hit.outputs {
            let _ = writeln!(
                w,
                "  {:<60} {}  {} {}",
                o.path,
                short(&o.hash),
                human_bytes(o.size),
                o.mode
            );
        }
        let _ = writeln!(w);
        let _ = writeln!(w, "outputs_content_hash: {}", hit.outputs_content_hash);
    }

    let _ = w.flush();
}

/// Render a diff of two breakdowns: any field that differs gets a section.
fn print_diff(left: &Explained, right: &Explained) {
    let stdout = std::io::stdout();
    let mut w = stdout.lock();

    let _ = writeln!(w, "comparing:");
    let _ = writeln!(w, "  -  {}  ({})", left.target, left.key);
    let _ = writeln!(w, "  +  {}  ({})", right.target, right.key);
    if left.key == right.key {
        let _ = writeln!(w);
        let _ = writeln!(w, "(cache keys are identical - no diff to show)");
        return;
    }
    let _ = writeln!(w);

    diff_scalar(&mut w, "command", &left.command, &right.command);
    diff_scalar(&mut w, "cwd", &left.cwd, &right.cwd);
    diff_pairs(&mut w, "env (user)", &left.user_env, &right.user_env);
    diff_pairs(
        &mut w,
        "env (built-in)",
        &left.built_in_env,
        &right.built_in_env,
    );
    diff_file_inputs(&mut w, &left.file_inputs, &right.file_inputs);
    diff_padded(&mut w, "deps", &left.deps, &right.deps);

    let _ = w.flush();
}

fn diff_scalar<W: Write>(w: &mut W, label: &str, left: &str, right: &str) {
    if left == right {
        return;
    }
    let _ = writeln!(w, "── {label} ──");
    let _ = writeln!(w, "  - {left}");
    let _ = writeln!(w, "  + {right}");
    let _ = writeln!(w);
}

/// Walk the union of `left`/`right` keyed by `K`, skipping entries `eq` deems
/// unchanged. The first change writes the `── label ──` header (lazily, so an
/// all-equal section prints nothing); each change writes its `-`/`+` rows via
/// `row`; a non-empty section ends with a trailing blank line.
fn diff_keyed<'a, T, K, R>(
    w: &mut impl Write,
    label: &str,
    left: &'a [T],
    right: &'a [T],
    key: impl Fn(&'a T) -> K,
    eq: impl Fn(&T, &T) -> bool,
    mut row: R,
) where
    K: Ord,
    R: FnMut(&mut dyn Write, &K, Option<&'a T>, Option<&'a T>),
{
    let lmap: BTreeMap<K, &T> = left.iter().map(|t| (key(t), t)).collect();
    let rmap: BTreeMap<K, &T> = right.iter().map(|t| (key(t), t)).collect();
    let mut keys: BTreeSet<&K> = lmap.keys().collect();
    keys.extend(rmap.keys());
    let mut wrote_header = false;
    for k in keys {
        let (l, r) = (lmap.get(k).copied(), rmap.get(k).copied());
        if let (Some(l), Some(r)) = (l, r)
            && eq(l, r)
        {
            continue;
        }
        if !wrote_header {
            let _ = writeln!(w, "── {label} ──");
            wrote_header = true;
        }
        row(w, k, l, r);
    }
    if wrote_header {
        let _ = writeln!(w);
    }
}

fn diff_pairs<W: Write>(
    w: &mut W,
    label: &str,
    left: &[(String, String)],
    right: &[(String, String)],
) {
    let value =
        |p: Option<&(String, String)>| p.map(|(_, v)| v.as_str()).unwrap_or("<unset>").to_string();
    diff_keyed(
        w,
        label,
        left,
        right,
        |(k, _)| k.as_str(),
        |l, r| l.1 == r.1,
        |w, k, l, r| {
            let _ = writeln!(w, "  - {k}={}", value(l));
            let _ = writeln!(w, "  + {k}={}", value(r));
        },
    );
}

fn diff_file_inputs<W: Write>(w: &mut W, left: &[FileInput], right: &[FileInput]) {
    let hash = |f: Option<&FileInput>| f.map(|f| short(&f.hash)).unwrap_or("<absent>".into());
    diff_keyed(
        w,
        "file inputs",
        left,
        right,
        |f| f.path.as_str(),
        |l, r| l.hash == r.hash && l.size == r.size,
        |w, p, l, r| {
            let _ = writeln!(w, "  - {:<60} {}", p, hash(l));
            let _ = writeln!(w, "  + {:<60} {}", p, hash(r));
        },
    );
}

/// Like `diff_pairs` but renders the value as a short hash with a `<60` pad,
/// matching the dep-output diff layout.
fn diff_padded<W: Write>(
    w: &mut W,
    label: &str,
    left: &[(String, String)],
    right: &[(String, String)],
) {
    let hash = |p: Option<&(String, String)>| p.map(|(_, h)| short(h)).unwrap_or("<absent>".into());
    diff_keyed(
        w,
        label,
        left,
        right,
        |(k, _)| k.as_str(),
        |l, r| l.1 == r.1,
        |w, k, l, r| {
            let _ = writeln!(w, "  - {:<60} {}", k, hash(l));
            let _ = writeln!(w, "  + {:<60} {}", k, hash(r));
        },
    );
}

/// First 16 chars of a hash, matching the engine's explain output.
fn short(hash: &str) -> String {
    hash.chars().take(16).collect()
}

/// Human-readable byte sizes; short and stable.
fn human_bytes(n: u64) -> String {
    const KB: u64 = 1_024;
    const MB: u64 = KB * 1_024;
    const GB: u64 = MB * 1_024;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}
