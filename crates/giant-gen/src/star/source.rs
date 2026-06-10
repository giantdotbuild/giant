//! Where `@std//` modules come from: an on-disk collection (`GIANT_STD`,
//! the root config's `std.path`, or an install-relative `share/giant/std`),
//! or the pinned online collection declared as `std.ref`. Pinned modules are
//! fetched once per (repo, ref, module) and cached under the engine cache
//! dir, so `giant gen` touches the network only on a cold cache.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Where the std collection lives unless the `std:` block says otherwise.
pub(crate) const DEFAULT_STD_REPO: &str = "giantdotbuild/giant-std";

const RAW_BASE: &str = "https://raw.githubusercontent.com";

/// A pinned online collection: `<base>/<repo>/<rev>/<module>` per module,
/// cached at `<cache>/<repo>/<rev>/<module>`.
#[derive(Clone, Debug)]
pub(crate) struct StdPin {
    pub repo: String,
    pub rev: String,
    /// Raw-content base URL; tests point this at a mock server.
    pub base: String,
    /// Cache root for fetched modules (`<cache.dir>/std`).
    pub cache: PathBuf,
}

impl StdPin {
    pub(crate) fn new(repo: String, rev: String, cache: PathBuf) -> Self {
        Self {
            repo,
            rev,
            base: RAW_BASE.into(),
            cache,
        }
    }
}

/// Resolves a std module name to its source. Precedence: the on-disk
/// collection (an explicit local override), then the pin.
#[derive(Clone, Debug)]
pub(crate) struct StdSource {
    dir: Option<PathBuf>,
    pin: Option<StdPin>,
}

impl StdSource {
    pub(crate) fn new(dir: Option<PathBuf>, pin: Option<StdPin>) -> Self {
        Self { dir, pin }
    }

    /// The source of std module `name`. Fetches and caches on a cold pin;
    /// errors name the failing step (unknown module, network, no source).
    pub(crate) fn source(&self, name: &str) -> Result<String> {
        check_module_name(name)?;
        if let Some(p) = self.dir.as_ref().map(|d| d.join(name))
            && p.is_file()
        {
            return std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()));
        }
        if let Some(pin) = &self.pin {
            let cached = pin.cache.join(&pin.repo).join(&pin.rev).join(name);
            if cached.is_file() {
                return std::fs::read_to_string(&cached)
                    .with_context(|| format!("reading cached {}", cached.display()));
            }
            let src = fetch(pin, name)?;
            write_atomic(&cached, &src)?;
            return Ok(src);
        }
        match &self.dir {
            Some(d) => bail!("no std module named '{name}' in {}", d.display()),
            None => bail!(
                "no std module source: pin one in giant.yaml (`std: {{ ref: <tag-or-commit> }}`), \
                 point `std: {{ path: <dir> }}` or GIANT_STD at a local collection"
            ),
        }
    }
}

/// GET one module from the pinned repo. A 404 means no module by that name at
/// that ref; anything else is reported with the URL so a proxy/offline failure
/// is diagnosable.
fn fetch(pin: &StdPin, name: &str) -> Result<String> {
    let url = format!("{}/{}/{}/{}", pin.base, pin.repo, pin.rev, name);
    match ureq::get(&url).call() {
        Ok(mut resp) => resp
            .body_mut()
            .read_to_string()
            .with_context(|| format!("reading response from {url}")),
        Err(ureq::Error::StatusCode(404)) => {
            bail!("no std module named '{name}' in {}@{}", pin.repo, pin.rev)
        }
        Err(e) => bail!(
            "fetching {url}: {e}\n(offline? a fetched copy is reused from {}; \
             or set GIANT_STD / vendor the module with `giant gen vendor {name}`)",
            pin.cache.display()
        ),
    }
}

/// A safe path/URL segment: a plain name with no separator or traversal
/// potential. Shared with the `std:` block's repo/ref validation.
pub(crate) fn safe_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Module names are plain filenames; anything path-like is rejected before it
/// reaches a join or a URL.
fn check_module_name(name: &str) -> Result<()> {
    if !safe_segment(name) {
        bail!("invalid std module name '{name}' (use letters, digits, '.', '_', '-')");
    }
    Ok(())
}

/// Write-fsync-rename, so a crash never leaves a torn file that a later run
/// would read back as a valid cached module.
fn write_atomic(path: &Path, content: &str) -> Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent dir for {}", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let tmp = path.with_extension("tmp");
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()
    };
    write()
        .and_then(|()| std::fs::rename(&tmp, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
        .with_context(|| format!("caching {}", path.display()))
}

/// Locate the on-disk std collection: `GIANT_STD` (the per-user override)
/// wins, then the workspace's `std.path`, then the install-relative
/// `share/giant/std` next to the binary (`<prefix>/bin/giant-gen` ->
/// `<prefix>/share/giant/std`).
pub(crate) fn detect_dir(config_path: Option<PathBuf>) -> Option<PathBuf> {
    if let Ok(d) = std::env::var("GIANT_STD") {
        let p = PathBuf::from(d);
        if p.is_dir() {
            return Some(p);
        }
    }
    config_path.or_else(|| {
        let exe = std::env::current_exe().ok()?;
        let prefix = exe.parent()?.parent()?;
        let p = prefix.join("share/giant/std");
        p.is_dir().then_some(p)
    })
}
