//! `load()` resolution (TDD-0024 §B/§I): `@giant//<name>` resolves to a stdlib
//! module embedded in the binary (nothing to install); any other path is a
//! repo-local `.star` file read relative to the workspace root. Loaded modules
//! are evaluated with the same host globals and cached.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use starlark::environment::{FrozenModule, Globals, Module};
use starlark::eval::{Evaluator, FileLoader};
use starlark::syntax::{AstModule, Dialect};

/// Resolves `load()` paths for the host. Holds the workspace root (for
/// repo-local loads), the host globals (loaded modules see the same `target()`
/// / `parse_*`), and a cache so a module loads once.
pub(crate) struct Loader<'g> {
    root: PathBuf,
    globals: &'g Globals,
    cache: RefCell<HashMap<String, FrozenModule>>,
}

impl<'g> Loader<'g> {
    pub(crate) fn new(root: &Path, globals: &'g Globals) -> Self {
        Self {
            root: root.to_path_buf(),
            globals,
            cache: RefCell::new(HashMap::new()),
        }
    }

    fn source(&self, path: &str) -> starlark::Result<String> {
        if let Some(name) = path.strip_prefix("@giant//") {
            embedded(name)
                .map(str::to_owned)
                .ok_or_else(|| err(format!("unknown stdlib module '@giant//{name}'")))
        } else {
            let p = self.root.join(path);
            std::fs::read_to_string(&p).map_err(|e| err(format!("load('{path}'): {e}")))
        }
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

/// The embedded stdlib, compiled into the binary.
fn embedded(name: &str) -> Option<&'static str> {
    match name {
        "go.star" => Some(include_str!("stdlib/go.star")),
        _ => None,
    }
}

fn err(msg: String) -> starlark::Error {
    starlark::Error::new_other(anyhow::anyhow!(msg))
}
