//! The embedded Starlark generator host.
//!
//! Runs a workspace's `giant.star` in-process: it evaluates the script, calls
//! `generate(ws)`, collects the `target()` values it returns, groups them by
//! package, and emits one `giant.<infix>.yaml` per package. The host exposes
//! only generic capabilities (`ws.glob`/`read`/`exec`, `parse_*`); language
//! opinions live in Starlark stdlib on top of them.

mod emit;
mod io;
mod json;
mod load;
mod source;
mod value;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use starlark::environment::{GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::syntax::{AstModule, Dialect};

pub(crate) use source::{DEFAULT_STD_REPO, StdPin, StdSource, safe_segment};
pub(crate) use value::Emitted;
use value::{Collector, Ws, host_globals};

/// Evaluate `script`, calling `generate(ws)`; collect every `target()` it
/// registers. `target()` records into a side collector as it runs, so the
/// return value of `generate` is not used (a target is emitted when built).
pub(crate) fn generate(script: &Path, root: &Path, std: &StdSource) -> Result<Vec<Emitted>> {
    let src =
        std::fs::read_to_string(script).with_context(|| format!("reading {}", script.display()))?;
    let disp = script.display().to_string();
    let ast = AstModule::parse(&disp, src, &Dialect::Standard).map_err(star_err)?;
    let globals = GlobalsBuilder::standard().with(host_globals).build();
    let root = root.to_path_buf();
    let std = std.clone();

    Module::with_temp_heap(move |module| -> Result<Vec<Emitted>> {
        let collector = Collector::default();
        let loader = load::Loader::new(&root, &globals, std);
        let mut eval = Evaluator::new(&module);
        eval.set_loader(&loader);
        eval.extra = Some(&collector);
        eval.eval_module(ast, &globals).map_err(star_err)?;

        let generate_fn = module
            .get("generate")
            .ok_or_else(|| anyhow!("{disp}: must define a `generate(ws)` function"))?;

        let ws = module.heap().alloc(Ws::new(&root));
        eval.eval_function(generate_fn, &[ws], &[])
            .map_err(star_err)?;

        drop(eval);
        Ok(collector.take())
    })
}

/// Run the host end to end: evaluate `script` and write its targets as
/// `giant.<infix>.yaml` files under `out_root`, pruning files it no longer owns.
/// Returns the written paths.
pub(crate) fn run(
    script: &Path,
    infix: &str,
    out_root: &Path,
    root: &Path,
    std: &StdSource,
) -> Result<Vec<PathBuf>> {
    let targets = generate(script, root, std)?;
    emit::write(targets, infix, out_root)
}

/// Render a Starlark error (which carries a spanned diagnostic) for the user.
fn star_err(e: starlark::Error) -> anyhow::Error {
    anyhow!("{e:?}")
}

#[cfg(test)]
mod tests;
