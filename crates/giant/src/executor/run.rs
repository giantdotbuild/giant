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
    // Restore each output: write blob bytes into the workspace path -
    // unless the file already there is byte-identical (outputs are
    // content-addressed, so a matching hash means it's current). Skipping
    // the rewrite turns a no-op build over large artifacts (e.g. hundreds
    // of MB of Go binaries) from a full re-copy into a few stats + hashes.
    for out in &entry.outputs {
        let Some(hash) = ContentHash::from_hex(&out.content_hash) else {
            continue;
        };
        let path = workspace_root.as_path().join(&out.path);
        if output_already_current(&path, hash).await {
            continue;
        }
        let Some(blob) = cache.get_cas(&hash).await? else {
            return Ok(None);
        };
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

/// Whether the workspace file at `path` already has content `hash`, so a
/// cache-hit restore can skip rewriting it. Hashing runs on the blocking pool
/// (artifacts can be large); a missing or unreadable file counts as not
/// current, so the caller restores it.
async fn output_already_current(path: &std::path::Path, hash: ContentHash) -> bool {
    let path = path.to_path_buf();
    let read = tokio::task::spawn_blocking(move || ContentHash::of_file(&path)).await;
    matches!(read, Ok(Ok(h)) if h == hash)
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
        .stderr(Stdio::piped());
    apply_giant_env(&mut cmd, ctx, spec, key);
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

    // Under `--sandbox`, an eligible target (`sandbox != false`)
    // runs through the `giant-sandbox` wrapper instead of `sh` directly. An
    // exempt target, or sandbox mode off, takes the plain path unchanged.
    // `sandbox_allowed` carries the granted path set for an enforced target, so
    // a denial can be explained on failure. `None` = the target ran directly.
    let (mut cmd, sandbox_allowed) = match ctx.sandbox.as_ref().filter(|_| spec.sandbox) {
        Some(policy) => match super::sandbox::wrapped_command(ctx, spec, &cwd, policy).await {
            Ok((cmd, allowed)) => (cmd, Some(allowed)),
            Err(e) => {
                return TargetResult::Failed {
                    error: format!("sandbox setup failed: {e}"),
                };
            }
        },
        None => {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(&spec.command);
            (cmd, None)
        }
    };
    cmd.current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_giant_env(&mut cmd, ctx, spec, key);

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
        let code = exit.code().unwrap_or(-1);
        let mut error = format!("exit code {code}");
        // Under enforcement, turn a bare exit code into a likely cause - an
        // undeclared read, a blocked socket, or a sandbox that wouldn't start
        // Reads the captured stderr, so it needs log capture on
        // (the default; `giant verify` always captures).
        if let Some(allowed) = &sandbox_allowed
            && let Some(hint) = diagnose_sandbox_failure(code, &stderr_bytes, allowed, spec.network)
        {
            error = format!("{error}; {hint}");
        }
        return TargetResult::Failed { error };
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
/// every matching file, and store it in CAS.
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
pub(super) fn output_parent_to_create(pattern: &std::path::Path) -> Option<std::path::PathBuf> {
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

/// Inject the `GIANT_*` variables every command (and `exists` check) can rely
/// on: the cache key, the workspace root, and the target's own package
/// directory. `//` is not rewritten inside a `command` string, so these are the
/// portable way to reference workspace-root or package paths from the shell -
/// e.g. `go build -o $GIANT_WORKSPACE_ROOT/bin/server`. The user's `env:` map is
/// applied after these and can override them.
fn apply_giant_env(cmd: &mut Command, ctx: &TargetCtx, spec: &TargetSpec, key: CacheKey) {
    let (package, _) = spec.id.split();
    cmd.env("GIANT_CACHE_KEY", key.to_hex())
        .env("GIANT_WORKSPACE_ROOT", ctx.workspace_root.as_path())
        .env(
            "GIANT_PACKAGE_DIR",
            ctx.workspace_root.as_path().join(package),
        );
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

/// giant-sandbox's reserved exit code for "could not set up the sandbox",
/// distinct from the child's own status (mirrors the `env`/`docker` convention;
/// see `giant-sandbox`'s `SETUP_FAILURE`).
const SANDBOX_SETUP_FAILURE: i32 = 125;

/// Best-effort explanation for why an enforced command failed, from its exit
/// code and stderr. The sandbox surfaces a denial as the
/// child's own `EACCES` / network error, so we pattern-match those and point at
/// the declared set. `None` when nothing recognisable stands out - the caller
/// keeps the bare exit code.
fn diagnose_sandbox_failure(
    exit_code: i32,
    stderr: &[u8],
    allowed: &[std::path::PathBuf],
    network: bool,
) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);

    // The sandbox failing to start (no unprivileged namespaces / Landlock, or a
    // configured `sandbox.roots` path missing) shows up as the 125 setup code or
    // a recognisable backend error - not as the command's own failure.
    if exit_code == SANDBOX_SETUP_FAILURE || looks_like_setup_failure(&text) {
        return Some(
            "could not enter the sandbox - this host may not allow unprivileged \
             namespaces or Landlock, or a configured `sandbox.roots` path is \
             missing"
                .to_string(),
        );
    }

    if !network && looks_like_network_denial(&text) {
        return Some(
            "looks like it reached for the network, which the sandbox denies; \
             set `network: true` on the target to allow it"
                .to_string(),
        );
    }

    // A hard denial (EACCES/EPERM) is unambiguous. Name the offending path when
    // the tool printed an absolute one, otherwise a generic hint.
    if looks_like_fs_denial(&text) {
        if let Some(path) = first_denied_path(&text) {
            let undeclared = !allowed.iter().any(|a| path.starts_with(a));
            return Some(if undeclared {
                format!(
                    "looks like it accessed `{}`, which is not a declared input \
                     or output; add it to `inputs:`/`outputs:` or set \
                     `sandbox: false`",
                    path.display()
                )
            } else {
                format!(
                    "the sandbox denied access to `{}`; it is declared, so this \
                     may be a read-vs-write or permissions mismatch",
                    path.display()
                )
            });
        }
        return Some(
            "a file access was denied by the sandbox; the command likely uses a \
             path it does not declare - add it to `inputs:`/`outputs:` or set \
             `sandbox: false`"
                .to_string(),
        );
    }

    // Landlock often hides an undeclared path as a plain "not found" rather than
    // a permission error, so under enforcement a failed open is a strong hint
    // that an input is missing from the declared set.
    if looks_like_missing_path(&text) {
        return Some(
            "a file the command opened was not found under the sandbox; if it \
             exists in your tree, the target likely reads a path it does not \
             declare - add it to `inputs:` or set `sandbox: false`"
                .to_string(),
        );
    }

    None
}

fn looks_like_setup_failure(text: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "giant-sandbox:",
        "sandboxing failure",
        "Landlock",
        "seccomp",
        "unprivileged",
        "namespace",
    ];
    NEEDLES.iter().any(|n| text.contains(n))
}

fn looks_like_network_denial(text: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "Network is unreachable",
        "ENETUNREACH",
        "Could not resolve host",
        "Temporary failure in name resolution",
        "Name or service not known",
        "EAI_AGAIN",
        "getaddrinfo",
    ];
    NEEDLES.iter().any(|n| text.contains(n))
}

fn looks_like_fs_denial(text: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "Permission denied",
        "Operation not permitted",
        "EACCES",
        "EPERM",
    ];
    NEEDLES.iter().any(|n| text.contains(n))
}

fn looks_like_missing_path(text: &str) -> bool {
    const NEEDLES: &[&str] = &["No such file or directory", "ENOENT", "cannot open"];
    NEEDLES.iter().any(|n| text.contains(n))
}

/// Pull the first absolute path out of a permission-denied line, if one is
/// recognisable. Tools format these many ways ("cat: /x: Permission denied",
/// "open '/x': Operation not permitted"), so this is heuristic: on a denial
/// line, take the longest `/`-rooted token after trimming surrounding
/// punctuation and quotes.
fn first_denied_path(text: &str) -> Option<std::path::PathBuf> {
    for line in text.lines() {
        if !looks_like_fs_denial(line) {
            continue;
        }
        let token = line
            .split(|c: char| c.is_whitespace() || c == '\'' || c == '"' || c == '`')
            .map(|tok| tok.trim_matches(|c: char| matches!(c, ':' | ',' | ')' | '(' | '.')))
            .filter(|tok| tok.starts_with('/') && tok.len() > 1)
            .max_by_key(|tok| tok.len());
        if let Some(p) = token {
            return Some(std::path::PathBuf::from(p));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{diagnose_sandbox_failure, first_denied_path, output_parent_to_create};
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

    #[test]
    fn diagnose_setup_failure_by_code_or_signature() {
        // The 125 setup code, with no stderr at all.
        let d = diagnose_sandbox_failure(125, b"", &[], false).unwrap();
        assert!(d.contains("could not enter the sandbox"), "{d}");
        // A recognisable backend error at any exit code.
        let d =
            diagnose_sandbox_failure(1, b"giant-sandbox: entering the sandbox: ...", &[], false)
                .unwrap();
        assert!(d.contains("could not enter the sandbox"), "{d}");
    }

    #[test]
    fn diagnose_network_only_when_denied() {
        let err = b"npm error network getaddrinfo EAI_AGAIN registry.npmjs.org";
        let d = diagnose_sandbox_failure(1, err, &[], false).unwrap();
        assert!(d.contains("network"), "{d}");
        // With `network: true` the same output is not a sandbox network denial.
        let allowed_net = diagnose_sandbox_failure(1, err, &[], true);
        assert!(
            allowed_net.is_none() || !allowed_net.as_deref().unwrap().contains("network"),
            "{allowed_net:?}",
        );
    }

    #[test]
    fn diagnose_undeclared_vs_declared_path() {
        // A denied path outside the granted set reads as undeclared.
        let err = b"cat: /etc/secret: Permission denied";
        let allowed = vec![PathBuf::from("/work/data.txt")];
        let d = diagnose_sandbox_failure(1, err, &allowed, false).unwrap();
        assert!(
            d.contains("/etc/secret") && d.contains("not a declared"),
            "{d}"
        );

        // A denied path *inside* the granted set is a read/write mismatch.
        let err = b"open '/work/out/x': Operation not permitted";
        let allowed = vec![PathBuf::from("/work/out")];
        let d = diagnose_sandbox_failure(1, err, &allowed, false).unwrap();
        assert!(d.contains("/work/out/x") && d.contains("declared"), "{d}");
    }

    #[test]
    fn diagnose_generic_denial_and_silence() {
        // Permission denied with no extractable path → generic FS hint.
        let d = diagnose_sandbox_failure(1, b"error: Permission denied", &[], false).unwrap();
        assert!(d.contains("file access was denied"), "{d}");
        // An ordinary build failure is left as a bare exit code.
        let none = diagnose_sandbox_failure(1, b"error[E0308]: mismatched types", &[], false);
        assert!(none.is_none(), "{none:?}");
    }

    #[test]
    fn diagnose_landlock_enoent_as_undeclared() {
        // Landlock commonly surfaces an undeclared (relative) path as a plain
        // "No such file or directory", so verify still explains it.
        let err = b"cat: data.txt: No such file or directory";
        let d = diagnose_sandbox_failure(1, err, &[], false).unwrap();
        assert!(
            d.contains("not found under the sandbox") && d.contains("declare"),
            "{d}"
        );
    }

    #[test]
    fn first_denied_path_picks_the_path() {
        assert_eq!(
            first_denied_path("cat: /etc/secret: Permission denied"),
            Some(PathBuf::from("/etc/secret")),
        );
        assert_eq!(
            first_denied_path("open '/a/b': Operation not permitted"),
            Some(PathBuf::from("/a/b")),
        );
        // No denial line → nothing.
        assert_eq!(first_denied_path("just some log output"), None);
    }
}
