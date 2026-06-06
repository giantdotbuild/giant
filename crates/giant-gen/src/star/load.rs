//! `load()` resolution (TDD-0024 §B/§I): `@std//<name>` (and the legacy
//! `@giant//<name>` alias) resolves to a module in giant's Starlark std
//! collection -- shipped as files alongside the binary, not embedded (ADR-0031),
//! and located via `GIANT_STD` or an install-relative `share/giant/std`. Any
//! other path is a repo-local `.star` file read relative to the workspace root.
//! Loaded modules are evaluated with the same host globals and cached.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use starlark::environment::{FrozenModule, Globals, Module};
use starlark::eval::{Evaluator, FileLoader};
use starlark::syntax::{AstModule, Dialect};

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
        let p = if let Some(name) = std_rel {
            let dir = self.std_dir.as_ref().ok_or_else(|| {
                err(format!(
                    "load('{path}'): no giant std collection found; set GIANT_STD, or vendor it with `giant gen vendor {name}` and load(\"star/{name}\")"
                ))
            })?;
            dir.join(name)
        } else {
            self.root.join(path)
        };
        std::fs::read_to_string(&p).map_err(|e| err(format!("load('{path}'): {e}")))
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

/// Locate giant's Starlark std collection: `GIANT_STD` if it points at a real
/// directory, else the install-relative `share/giant/std` next to the binary
/// (`<prefix>/bin/giant-gen` -> `<prefix>/share/giant/std`). `None` when neither
/// exists -- `@std//` loads then fail with a vendoring hint.
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
