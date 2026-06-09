//! `load()` resolution: `@std//<name>` (and the legacy
//! `@giant//<name>` alias) resolves to a module in giant's Starlark std
//! collection. The collection is compiled into the binary; an on-disk copy
//! (`GIANT_STD` or an install-relative `share/giant/std`) overrides it when
//! present. Any other path is a repo-local `.star` file read relative to the
//! workspace root. Loaded modules are evaluated with the same host globals
//! and cached.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use starlark::environment::{FrozenModule, Globals, Module};
use starlark::eval::{Evaluator, FileLoader};
use starlark::syntax::{AstModule, Dialect};

/// The std collection compiled into the binary, from the repo's `std/`.
/// An on-disk collection (see [`std_dir`]) takes precedence module-by-module.
const EMBEDDED_STD: &[(&str, &str)] = &[
    ("cargo.star", include_str!("../../../../std/cargo.star")),
    ("go.star", include_str!("../../../../std/go.star")),
];

/// The embedded source of a std module, if one by that name ships.
fn embedded_std(name: &str) -> Option<&'static str> {
    EMBEDDED_STD
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, src)| *src)
}

/// The source of std module `name`: the on-disk collection's copy when `dir`
/// holds one, else the embedded copy. `Ok(None)` if no module by that name
/// ships; `Err` only when an on-disk copy exists but cannot be read.
pub(crate) fn std_source(dir: Option<&Path>, name: &str) -> anyhow::Result<Option<String>> {
    use anyhow::Context;
    if let Some(p) = dir.map(|d| d.join(name))
        && p.is_file()
    {
        return std::fs::read_to_string(&p)
            .map(Some)
            .with_context(|| format!("reading {}", p.display()));
    }
    Ok(embedded_std(name).map(str::to_string))
}

/// Resolves `load()` paths for the host. Holds the workspace root (for
/// repo-local loads), the std collection dir (for `@std//` loads, `None` if
/// none is installed), the host globals (loaded modules see the same `target()`
/// / `parse_*`), and a cache so a module loads once.
pub(crate) struct Loader<'g> {
    root: PathBuf,
    std_dir: Option<PathBuf>,
    globals: &'g Globals,
    cache: RefCell<HashMap<String, FrozenModule>>,
}

impl<'g> Loader<'g> {
    pub(crate) fn new(root: &Path, globals: &'g Globals, std_dir: Option<PathBuf>) -> Self {
        Self {
            root: root.to_path_buf(),
            std_dir,
            globals,
            cache: RefCell::new(HashMap::new()),
        }
    }

    fn source(&self, path: &str) -> starlark::Result<String> {
        let std_rel = path
            .strip_prefix("@std//")
            .or_else(|| path.strip_prefix("@giant//"));
        if let Some(name) = std_rel {
            return std_source(self.std_dir.as_deref(), name)
                .map_err(|e| err(format!("load('{path}'): {e:#}")))?
                .ok_or_else(|| err(format!("load('{path}'): no std module named '{name}'")));
        }
        std::fs::read_to_string(self.root.join(path))
            .map_err(|e| err(format!("load('{path}'): {e}")))
    }
}

impl FileLoader for Loader<'_> {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        if let Some(cached) = self.cache.borrow().get(path) {
            return Ok(cached.clone());
        }
        let src = self.source(path)?;
        let ast = AstModule::parse(path, src, &Dialect::Standard)?;
        let frozen = Module::with_temp_heap(|module| -> starlark::Result<FrozenModule> {
            {
                let mut eval = Evaluator::new(&module);
                eval.set_loader(self);
                eval.eval_module(ast, self.globals)?;
            }
            module
                .freeze()
                .map_err(|e| err(format!("freezing '{path}': {e:?}")))
        })?;
        self.cache
            .borrow_mut()
            .insert(path.to_string(), frozen.clone());
        Ok(frozen)
    }
}

/// Locate an on-disk std collection that overrides the embedded one:
/// `GIANT_STD` if it points at a real directory, else the install-relative
/// `share/giant/std` next to the binary (`<prefix>/bin/giant-gen` ->
/// `<prefix>/share/giant/std`). `None` means `@std//` loads resolve to the
/// embedded modules.
pub(crate) fn std_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("GIANT_STD") {
        let p = PathBuf::from(d);
        if p.is_dir() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let prefix = exe.parent()?.parent()?;
    let p = prefix.join("share/giant/std");
    p.is_dir().then_some(p)
}

fn err(msg: String) -> starlark::Error {
    starlark::Error::new_other(anyhow::anyhow!(msg))
}
