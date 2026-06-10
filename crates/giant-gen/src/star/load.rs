//! `load()` resolution: `@std//<name>` (and the legacy
//! `@giant//<name>` alias) resolves to a module in giant's Starlark std
//! collection via [`StdSource`] - an on-disk copy or the workspace's pinned
//! online collection. Any other path is a repo-local `.star` file read
//! relative to the workspace root. Loaded modules are evaluated with the
//! same host globals and cached.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use starlark::environment::{FrozenModule, Globals, Module};
use starlark::eval::{Evaluator, FileLoader};
use starlark::syntax::{AstModule, Dialect};

use super::source::StdSource;

/// Resolves `load()` paths for the host. Holds the workspace root (for
/// repo-local loads), the std source (for `@std//` loads), the host globals
/// (loaded modules see the same `target()` / `parse_*`), and a cache so a
/// module loads once.
pub(crate) struct Loader<'g> {
    root: PathBuf,
    std: StdSource,
    globals: &'g Globals,
    cache: RefCell<HashMap<String, FrozenModule>>,
}

impl<'g> Loader<'g> {
    pub(crate) fn new(root: &Path, globals: &'g Globals, std: StdSource) -> Self {
        Self {
            root: root.to_path_buf(),
            std,
            globals,
            cache: RefCell::new(HashMap::new()),
        }
    }

    fn source(&self, path: &str) -> starlark::Result<String> {
        let std_rel = path
            .strip_prefix("@std//")
            .or_else(|| path.strip_prefix("@giant//"));
        if let Some(name) = std_rel {
            return self
                .std
                .source(name)
                .map_err(|e| err(format!("load('{path}'): {e:#}")));
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

fn err(msg: String) -> starlark::Error {
    starlark::Error::new_other(anyhow::anyhow!(msg))
}
