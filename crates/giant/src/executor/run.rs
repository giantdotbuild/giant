//! Running a target's command, capturing and storing its outputs, and
//! restoring outputs on a cache hit (local AC, remote AC, or `exists:`).

use super::{ExecutorError, OutputFile, TargetCtx, TargetResult};
use crate::cache::{AcEntry, LocalCache, OutputEntry};
use crate::events::{Event, EventSender, LogStream};
use crate::model::{CacheKey, ContentHash, TargetId, TargetSpec};
use crate::paths::AbsPath;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

pub(super) fn result_output_hash(r: &TargetResult) -> Option<ContentHash> {
    match r {
        TargetResult::Built { outputs, .. } => Some(compute_outputs_content_hash(outputs)),
        TargetResult::CacheHit { output_hash, .. }
        | TargetResult::RemoteCacheHit { output_hash, .. }
        | TargetResult::ExternalCacheHit { output_hash, .. } => Some(*output_hash),
        TargetResult::Failed { .. } => None,
    }
}

/// Try a local-cache lookup; if hit, restore outputs to workspace and
/// return the (result, output_content_hash) tuple. Returning the hash
/// here saves a re-read on the dispatcher side. Also replays captured
/// stdout/stderr (if the AC entry has blobs and replay is enabled) so
/// cache hits aren't silent.
pub(super) async fn try_cache_hit(
    ctx: &TargetCtx,
    target_id: &TargetId,
    key: &CacheKey,
) -> Result<Option<(TargetResult, ContentHash)>, ExecutorError> {
    let cache = &ctx.cache;
    let workspace_root = &ctx.workspace_root;
    let Some(entry) = cache.get_ac(key).await? else {
        return Ok(None);
    };
    // Verify each output blob exists. If any are missing, treat as miss.
    for out in &entry.outputs {
        let Some(hash) = ContentHash::from_hex(&out.content_hash) else {
            return Ok(None);
        };
        if !cache.has_cas(&hash).await {
            return Ok(None);
        }
    }
    // Restore each output: write blob bytes into the workspace path.
    for out in &entry.outputs {
        let Some(hash) = ContentHash::from_hex(&out.content_hash) else {
            continue;
        };
        let Some(blob) = cache.get_cas(&hash).await? else {
            return Ok(None);
        };
        let path = workspace_root.as_path().join(&out.path);
        // `atomic_write_output` does the create_dir_all + tmp-then-rename
        // dance so a target writing over its own running binary works
        // (Linux ETXTBSY blocks open-for-write but allows rename-over).
        crate::cache::atomic_write_output(path, blob, out.executable).await?;
    }

    // Read the outputs_content_hash from the entry; this is the value
    // downstream targets feed into their cache keys (early cutoff).
    let Some(output_hash) = ContentHash::from_hex(&entry.outputs_content_hash) else {
        return Err(ExecutorError::Cache(crate::cache::CacheError::Corrupt {
            path: std::path::PathBuf::from(format!("ac/{}", key.to_hex())),
            detail: "outputs_content_hash field is not 32-byte hex".into(),
        }));
    };

    if ctx.log_capture.replay {
        replay_logs(ctx, target_id, &entry).await;
    }

    Ok(Some((TargetResult::CacheHit { output_hash }, output_hash)))
}

/// Emit captured stdout/stderr from an AC entry as `TargetLog`
/// events. Missing blobs / empty entries are silently skipped - the
/// target predates log capture or had no output to begin with.
async fn replay_logs(ctx: &TargetCtx, target_id: &TargetId, entry: &crate::cache::AcEntry) {
    if let Some(hex) = entry.stdout_blob.as_deref() {
        replay_one_stream(ctx, target_id, hex, LogStream::Stdout).await;
    }
    if let Some(hex) = entry.stderr_blob.as_deref() {
        replay_one_stream(ctx, target_id, hex, LogStream::Stderr).await;
    }
}

async fn replay_one_stream(
    ctx: &TargetCtx,
    target_id: &TargetId,
    blob_hex: &str,
    stream: LogStream,
) {
    let Some(hash) = ContentHash::from_hex(blob_hex) else {
        return;
    };
    let Ok(Some(blob)) = ctx.cache.get_cas(&hash).await else {
        return;
    };
    let text = String::from_utf8_lossy(&blob);
    for line in text.lines() {
        let _ = ctx
            .events
            .send(Event::TargetLog {
                build: ctx.build_id.clone(),
                id: target_id.clone(),
                stream,
                line: line.to_string(),
                truncated: false,
            })
            .await;
    }
}

/// Try the remote cache. Hits restore outputs to the workspace AND
/// populate the local cache so the next run hits locally without
/// touching the remote. Misses / errors return `Ok(None)` so the
/// dispatcher falls through.
///
/// Feature-gated: a no-op stub when `remote` is off.
#[cfg(feature = "remote")]
pub(super) async fn try_remote_hit(
    ctx: &TargetCtx,
    target_id: &TargetId,
    key: &CacheKey,
) -> Result<Option<(TargetResult, ContentHash)>, ExecutorError> {
    let Some(remote) = ctx.remote.as_ref() else {
        return Ok(None);
    };
    let Ok(Some(entry)) = remote.get_ac(key).await else {
        return Ok(None);
    };

    // Fetch every referenced blob from remote, write to local CAS,
    // restore to the workspace. Any failure mid-restore → treat as a
    // miss (the local AC entry would be inconsistent if we wrote it).
    for out in &entry.outputs {
        let Some(hash) = ContentHash::from_hex(&out.content_hash) else {
            return Ok(None);
        };

        let blob = match remote.get_cas(&hash).await {
            Ok(Some(b)) => b,
            _ => return Ok(None),
        };
        // Verify the blob we got actually hashes to what the AC claims.
        // Cheap insurance against a corrupted or hostile server.
        if ContentHash::of_bytes(&blob) != hash {
            tracing::warn!(
                "remote CAS blob {} content does not match its name; treating as miss",
                hash.to_hex()
            );
            return Ok(None);
        }
        ctx.cache.put_cas(blob.clone()).await?;

        let dst = ctx.workspace_root.as_path().join(&out.path);
        crate::cache::atomic_write_output(dst, blob, out.executable).await?;
    }

    // Fetch the captured stdout/stderr blobs into local CAS too, so a
    // future local hit can replay without touching the remote. Missing
    // blobs on the remote are tolerated - older entries may not have
    // logs at all.
    for hex in entry
        .stdout_blob
        .iter()
        .chain(entry.stderr_blob.iter())
        .map(|s| s.as_str())
    {
        let Some(hash) = ContentHash::from_hex(hex) else {
            continue;
        };
        if ctx.cache.has_cas(&hash).await {
            continue;
        }
        if let Ok(Some(blob)) = remote.get_cas(&hash).await
            && ContentHash::of_bytes(&blob) == hash
        {
            ctx.cache.put_cas(blob).await?;
        }
    }

    // Write the AC entry to local cache too so the next run goes
    // straight to the local fast path.
    ctx.cache.put_ac(key, &entry).await?;

    let Some(output_hash) = ContentHash::from_hex(&entry.outputs_content_hash) else {
        return Ok(None);
    };

    if ctx.log_capture.replay {
        replay_logs(ctx, target_id, &entry).await;
    }

    Ok(Some((
        TargetResult::RemoteCacheHit { output_hash },
        output_hash,
    )))
}

/// No-op stub used when the `remote` feature is off so the dispatcher
/// chain stays uniform.
#[cfg(not(feature = "remote"))]
pub(super) async fn try_remote_hit(
    _ctx: &TargetCtx,
    _target_id: &TargetId,
    _key: &CacheKey,
) -> Result<Option<(TargetResult, ContentHash)>, ExecutorError> {
    Ok(None)
}

/// Run the target's `exists:` shell command, if any, to ask "does this
/// artifact already live somewhere external?" (registry, S3, …).
///
/// - No `exists:` declared → return `None` (the dispatcher falls through
///   to `run_target`).
/// - `exists:` exits 0 → external cache hit; the build is skipped.
/// - `exists:` exits non-zero → cache miss; the dispatcher runs the
///   target normally.
/// - The command failing to spawn → log a warning and fall through; we
///   prefer a clean miss over a confusing skipped/failed signal.
///
/// `$GIANT_CACHE_KEY` is in env so users can craft commands like
/// `docker manifest inspect reg.io/img:$GIANT_CACHE_KEY` that key the
/// artifact name on Giant's cache identity.
pub(super) async fn try_exists_check(
    ctx: &TargetCtx,
    spec: &TargetSpec,
    key: CacheKey,
) -> Option<(TargetResult, ContentHash)> {
    let exists_cmd = spec.exists.as_deref()?;

    let cwd = ctx.workspace_root.as_path().join(spec.cwd.as_path());
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(exists_cmd)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIANT_CACHE_KEY", key.to_hex())
        .env("GIANT_WORKSPACE_ROOT", ctx.workspace_root.as_path());
    apply_color_env(&mut cmd);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target=%spec.id, error=%e, "exists: command failed to spawn");
            return None;
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // exists-check output is purely informational - don't capture
    // it to CAS (no AC entry to attach it to).
    let pump_o = pump_lines(
        stdout,
        ctx.events.clone(),
        ctx.build_id.clone(),
        spec.id.clone(),
        LogStream::Stdout,
        0,
        false,
    );
    let pump_e = pump_lines(
        stderr,
        ctx.events.clone(),
        ctx.build_id.clone(),
        spec.id.clone(),
        LogStream::Stderr,
        0,
        false,
    );

    let status = tokio::select! {
        s = child.wait() => s,
        _ = ctx.cancel.cancelled() => {
            let _ = child.kill().await;
            return None;
        }
    };
    let (_, _) = tokio::join!(pump_o, pump_e);

    match status {
        Ok(s) if s.success() => {
            // The artifact lives elsewhere; we contribute the empty-outputs
            // hash to downstream cache keys. If a future use case needs
            // local outputs *and* an exists check, we can extend this.
            let oh = compute_outputs_content_hash(&[]);
            Some((TargetResult::ExternalCacheHit { output_hash: oh }, oh))
        }
        _ => None,
    }
}

/// Run a target's command end-to-end and store outputs.
pub(super) async fn run_target(ctx: &TargetCtx, spec: &TargetSpec, key: CacheKey) -> TargetResult {
    let started = Instant::now();

    let cwd = ctx.workspace_root.as_path().join(spec.cwd.as_path());
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&spec.command)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIANT_CACHE_KEY", key.to_hex())
        .env("GIANT_WORKSPACE_ROOT", ctx.workspace_root.as_path());

    // Color preservation: most modern CLIs disable color when they detect
    // stdout is a pipe (we use Stdio::piped). These env vars are the de
    // facto signals to force color anyway. Tools that strictly check
    // isatty(stdout) are unaffected - pty: true (v0.2) covers that case.
    apply_color_env(&mut cmd);

    // Per-target env overrides take precedence over our color signals.
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    // Ensure the parent directory of each declared output exists. Many
    // commands use `> outdir/file` redirects; with parent dirs absent
    // the shell fails before the user's command sees the workspace.
    // Cheap, idempotent, and matches the "you declared this output,
    // the engine handles the boilerplate" philosophy. For glob outputs
    // we create only the literal prefix before the first glob component,
    // so a pattern like `gen/**/*.go` makes `gen/`, never a dir named `**`.
    for out_path in &spec.outputs {
        if let Some(dir) = output_parent_to_create(out_path.as_path()) {
            let _ = tokio::fs::create_dir_all(ctx.workspace_root.as_path().join(dir)).await;
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return TargetResult::Failed {
                error: format!("spawn failed: {e}"),
            };
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let target_id = spec.id.clone();
    let build_id = ctx.build_id.clone();

    let pump_stdout = pump_lines(
        stdout,
        ctx.events.clone(),
        build_id.clone(),
        target_id.clone(),
        LogStream::Stdout,
        ctx.log_capture.cap_bytes,
        ctx.log_capture.capture,
    );
    let pump_stderr = pump_lines(
        stderr,
        ctx.events.clone(),
        build_id,
        target_id,
        LogStream::Stderr,
        ctx.log_capture.cap_bytes,
        ctx.log_capture.capture,
    );

    // A `timeout_secs` of `None` parks forever, so the timeout arm never
    // fires; otherwise it races the child and kills it on expiry. Both the
    // cancel and timeout arms reap the child via `kill().await`.
    let timeout = async {
        match spec.timeout_secs {
            Some(secs) => tokio::time::sleep(Duration::from_secs(secs)).await,
            None => std::future::pending::<()>().await,
        }
    };
    let status = tokio::select! {
        s = child.wait() => s,
        _ = ctx.cancel.cancelled() => {
            let _ = child.kill().await;
            return TargetResult::Failed { error: "cancelled".into() };
        }
        _ = timeout => {
            let _ = child.kill().await;
            let secs = spec.timeout_secs.unwrap_or_default();
            return TargetResult::Failed { error: format!("timed out after {secs}s") };
        }
    };
    let (stdout_bytes, stderr_bytes) = tokio::join!(pump_stdout, pump_stderr);

    let exit = match status {
        Ok(s) => s,
        Err(e) => {
            return TargetResult::Failed {
                error: format!("wait failed: {e}"),
            };
        }
    };
    if !exit.success() {
        return TargetResult::Failed {
            error: format!("exit code {}", exit.code().unwrap_or(-1)),
        };
    }

    // Capture and store outputs.
    let outputs = match capture_outputs(&ctx.cache, &ctx.workspace_root, spec).await {
        Ok(o) => o,
        Err(e) => {
            return TargetResult::Failed {
                error: format!("capture outputs: {e}"),
            };
        }
    };

    // `cache: false` targets run for their side effects only - never
    // store an AC entry, log blobs, or queue an upload. Outputs were
    // still captured above so downstream dep keys see this target's
    // output hash (early cutoff).
    if !spec.is_cacheable() {
        return TargetResult::Built {
            duration: started.elapsed(),
            outputs,
        };
    }

    // Persist captured stdout/stderr to CAS so a future cache hit can
    // replay them. Empty streams (or capture disabled) → None.
    let stdout_blob = if ctx.log_capture.capture && !stdout_bytes.is_empty() {
        match ctx.cache.put_cas(stdout_bytes.clone()).await {
            Ok(h) => Some(h.to_hex()),
            Err(e) => {
                return TargetResult::Failed {
                    error: format!("write stdout blob: {e}"),
                };
            }
        }
    } else {
        None
    };
    let stderr_blob = if ctx.log_capture.capture && !stderr_bytes.is_empty() {
        match ctx.cache.put_cas(stderr_bytes.clone()).await {
            Ok(h) => Some(h.to_hex()),
            Err(e) => {
                return TargetResult::Failed {
                    error: format!("write stderr blob: {e}"),
                };
            }
        }
    } else {
        None
    };

    // Write AC entry.
    let outputs_hash = compute_outputs_content_hash(&outputs);
    let ac = AcEntry {
        schema: crate::cache::AC_SCHEMA,
        target_id: spec.id.as_str().to_string(),
        cache_key: key.to_hex(),
        command: spec.command.clone(),
        cwd: spec.cwd.as_path().to_string_lossy().into_owned(),
        outputs: outputs.iter().map(OutputFile::to_entry).collect(),
        outputs_content_hash: outputs_hash.to_hex(),
        stdout_blob,
        stderr_blob,
        exit_code: 0,
        duration_ms: started.elapsed().as_millis() as u64,
        built_at: chrono::Utc::now().to_rfc3339(),
        built_by: None,
    };
    if let Err(e) = ctx.cache.put_ac(&key, &ac).await {
        return TargetResult::Failed {
            error: format!("cache write: {e}"),
        };
    }

    // Queue background remote upload. Reads each blob back from local
    // CAS - they're hot in the OS page cache from being written moments
    // ago, so the read is cheap. The build never waits on this.
    #[cfg(feature = "remote")]
    if spec.remote_cache
        && let Some(tx) = ctx.upload_tx.as_ref()
    {
        let mut blobs = Vec::with_capacity(outputs.len() + 2);
        for o in &outputs {
            match ctx.cache.get_cas(&o.content_hash).await {
                Ok(Some(bytes)) => blobs.push((o.content_hash, bytes)),
                _ => {
                    tracing::warn!("local CAS read failed for upload of {}", o.rel_path);
                }
            }
        }
        // Also ship captured stdout/stderr so other machines hitting
        // this AC entry can replay the logs.
        for hex in ac
            .stdout_blob
            .iter()
            .chain(ac.stderr_blob.iter())
            .map(|s| s.as_str())
        {
            if let Some(h) = ContentHash::from_hex(hex)
                && let Ok(Some(b)) = ctx.cache.get_cas(&h).await
            {
                blobs.push((h, b));
            }
        }
        let job = crate::remote::UploadJob {
            cache_key: key,
            ac_entry: ac,
            blobs,
        };
        // try_send: if the uploader is backlogged, drop the job rather
        // than block the build. Better local progress than a stuck queue.
        let _ = tx.try_send(job);
    }

    TargetResult::Built {
        duration: started.elapsed(),
        outputs,
    }
}

/// Capture a target's outputs: glob-expand each declared pattern, hash
/// every matching file, and store it in CAS (ADR-0019).
///
/// Each `outputs:` entry is a glob; a literal path is the degenerate
/// single-match case, so the must-exist contract survives (a named file
/// that wasn't produced matches zero files → error). Matched directories
/// are skipped; a pattern that matches no files fails the run. The
/// captured set is deduped and sorted so a multi-file declaration folds
/// deterministically into `outputs_content_hash`.
async fn capture_outputs(
    cache: &LocalCache,
    workspace_root: &AbsPath,
    spec: &TargetSpec,
) -> Result<Vec<OutputFile>, std::io::Error> {
    let ws = workspace_root.as_path().to_path_buf();
    let patterns: Vec<std::path::PathBuf> = spec
        .outputs
        .iter()
        .map(|o| o.as_path().to_path_buf())
        .collect();

    // Enumerate matches off the async runtime - `glob` does blocking stats.
    let mut files = tokio::task::spawn_blocking(move || glob_output_files(&ws, &patterns))
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))??;
    files.sort();
    files.dedup();

    let mut outputs = Vec::with_capacity(files.len());
    for abs in files {
        let rel = abs.strip_prefix(workspace_root.as_path()).map_err(|_| {
            std::io::Error::other(format!("captured output {abs:?} escaped the workspace"))
        })?;
        let metadata = tokio::fs::metadata(&abs).await?;
        let bytes = tokio::fs::read(&abs).await?;
        let size = bytes.len() as u64;
        let (executable, mode) = file_perms(&metadata);
        let hash = cache
            .put_cas(bytes)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        outputs.push(OutputFile {
            rel_path: rel.to_string_lossy().into_owned(),
            content_hash: hash,
            size,
            executable,
            mode,
        });
    }
    Ok(outputs)
}

/// Glob-expand each output pattern (relative to `ws`) into the absolute
/// paths of the regular files it matches. Each pattern must match at
/// least one file; a pattern matching only directories, or nothing, is a
/// run error (with a directory-specific hint).
fn glob_output_files(
    ws: &std::path::Path,
    patterns: &[std::path::PathBuf],
) -> Result<Vec<std::path::PathBuf>, std::io::Error> {
    let mut files = Vec::new();
    for pat in patterns {
        let joined = ws.join(pat);
        let Some(pat_str) = joined.to_str() else {
            return Err(std::io::Error::other(format!(
                "output pattern {:?} is not valid UTF-8",
                pat
            )));
        };
        let entries = glob::glob(pat_str)
            .map_err(|e| std::io::Error::other(format!("bad output pattern {pat:?}: {e}")))?;
        let mut matched_file = false;
        let mut matched_dir = false;
        for entry in entries {
            let path = entry.map_err(|e| std::io::Error::other(e.to_string()))?;
            match std::fs::metadata(&path) {
                Ok(m) if m.is_file() => {
                    files.push(path);
                    matched_file = true;
                }
                Ok(m) if m.is_dir() => matched_dir = true,
                _ => {}
            }
        }
        if !matched_file {
            return Err(std::io::Error::other(if matched_dir {
                format!(
                    "output {:?} matched only directories; directory outputs are not yet \
                     supported - declare files or a recursive glob like {:?}",
                    pat,
                    pat.join("**").join("*")
                )
            } else {
                format!("output {pat:?} matched no files after the command ran")
            }));
        }
    }
    Ok(files)
}

/// The directory to pre-create for an output pattern, if any. For a
/// literal path it is the file's parent; for a glob it is the literal
/// prefix before the first component containing a glob metacharacter
/// (so `gen/**/*.go` yields `gen`, not a literal `gen/**`). Returns
/// `None` when there's nothing above the workspace root to create.
fn output_parent_to_create(pattern: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::path::Component;
    let is_glob = |c: &std::ffi::OsStr| c.to_string_lossy().contains(['*', '?', '[']);
    let mut prefix = std::path::PathBuf::new();
    let mut saw_glob = false;
    for comp in pattern.components() {
        if let Component::Normal(c) = comp {
            if is_glob(c) {
                saw_glob = true;
                break;
            }
            prefix.push(c);
        }
    }
    if saw_glob {
        // The literal dir before the first glob component (may be empty
        // for a top-level glob like `*.txt`).
        (!prefix.as_os_str().is_empty()).then_some(prefix)
    } else {
        // Literal path: create its parent directory.
        pattern
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(std::path::Path::to_path_buf)
    }
}

/// Executable bit + octal mode string for a captured file.
fn file_perms(metadata: &std::fs::Metadata) -> (bool, String) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = metadata.permissions().mode();
        (m & 0o111 != 0, format!("{:o}", m & 0o7777))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        (false, "0644".into())
    }
}

/// Hash of the sorted outputs vector, for early-cutoff and AC metadata.
fn compute_outputs_content_hash(outputs: &[OutputFile]) -> ContentHash {
    let mut h = ContentHash::hasher();
    let mut sorted: Vec<&OutputFile> = outputs.iter().collect();
    sorted.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    for o in sorted {
        h.update(o.rel_path.as_bytes());
        h.update(b"\0");
        h.update(o.content_hash.as_bytes());
        h.update(b"\0");
    }
    h.finalize()
}

impl OutputFile {
    fn to_entry(&self) -> OutputEntry {
        OutputEntry {
            path: self.rel_path.clone(),
            content_hash: self.content_hash.to_hex(),
            size: self.size,
            executable: self.executable,
            mode: self.mode.clone(),
            symlink_target: None,
        }
    }
}

/// Set color-forcing env vars on a child command. Each variable is the
/// well-known signal for an ecosystem; tools that don't recognise theirs
/// just ignore it. The user's `env:` map is applied *after* these and
/// can override any of them.
fn apply_color_env(cmd: &mut Command) {
    // npm / node ecosystem
    cmd.env("FORCE_COLOR", "1");
    // BSD / macOS convention; respected by many CLIs
    cmd.env("CLICOLOR_FORCE", "1");
    cmd.env("CLICOLOR", "1");
    // python's "do you want color?" hint
    cmd.env("PY_COLORS", "1");
    // cargo
    cmd.env("CARGO_TERM_COLOR", "always");
    // many TUI-aware tools probe TERM; set something modest if absent.
    // Don't override if the parent already passed a TERM through.
    if std::env::var_os("TERM").is_none() {
        cmd.env("TERM", "xterm-256color");
    }
}

/// Pump stdout/stderr from a child into log events while also
/// accumulating the bytes into a buffer that can be written to CAS
/// after the target completes (for cache-hit replay).
///
/// Returns the captured bytes. Accumulation stops at `cap_bytes` per
/// stream; lines beyond the cap still stream live but aren't written
/// to the blob. A `[truncated]` marker is appended so a future replay
/// shows the cutoff.
async fn pump_lines<R>(
    reader: Option<R>,
    events: EventSender,
    build_id: String,
    target_id: TargetId,
    stream: LogStream,
    cap_bytes: usize,
    capture: bool,
) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut buf: Vec<u8> = if capture {
        Vec::with_capacity(1024)
    } else {
        Vec::new()
    };
    let mut hit_cap = false;
    let Some(r) = reader else { return buf };
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if capture && !hit_cap {
            // +1 for the newline we re-append.
            let needed = line.len() + 1;
            if buf.len() + needed <= cap_bytes {
                buf.extend_from_slice(line.as_bytes());
                buf.push(b'\n');
            } else {
                buf.extend_from_slice(b"[giant: log truncated at capture cap]\n");
                hit_cap = true;
            }
        }
        let truncated = line.len() > 8 * 1024;
        let line = if truncated {
            line[..8 * 1024].to_string()
        } else {
            line
        };
        let _ = events
            .send(Event::TargetLog {
                build: build_id.clone(),
                id: target_id.clone(),
                stream,
                line,
                truncated,
            })
            .await;
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::output_parent_to_create;
    use std::path::{Path, PathBuf};

    fn parent(p: &str) -> Option<PathBuf> {
        output_parent_to_create(Path::new(p))
    }

    #[test]
    fn literal_output_creates_its_parent() {
        assert_eq!(parent("bin/app"), Some(PathBuf::from("bin")));
        assert_eq!(parent("a/b/c.txt"), Some(PathBuf::from("a/b")));
        // Top-level literal has no parent above the workspace root.
        assert_eq!(parent("app.txt"), None);
    }

    #[test]
    fn glob_output_creates_only_the_literal_prefix() {
        assert_eq!(parent("gen/*.txt"), Some(PathBuf::from("gen")));
        // The `**` must never become a literal directory.
        assert_eq!(parent("gen/**/*.go"), Some(PathBuf::from("gen")));
        assert_eq!(parent("a/b/**/c.txt"), Some(PathBuf::from("a/b")));
        // A top-level glob has no literal prefix to create.
        assert_eq!(parent("*.txt"), None);
    }
}
