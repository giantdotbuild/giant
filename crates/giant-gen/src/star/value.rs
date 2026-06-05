//! The Starlark-facing surface: the `ws` handle and its capability methods,
//! the `target()` constructor, and the `parse_*` data builtins (TDD-0024 §C/§D).
//!
//! The host exposes *capabilities* (filesystem, process, parsing) only. Every
//! language- or domain-specific opinion (what a Go package is, hierarchical
//! metadata merge) lives in Starlark stdlib on top of these, never here.

use std::cell::RefCell;
use std::fmt;
use std::path::Path;

use allocative::Allocative;
use giant_schema::{GlobPattern, Input, WireTarget};
use starlark::collections::SmallMap;
use starlark::environment::{GlobalsBuilder, Methods, MethodsBuilder, MethodsStatic};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::list::UnpackList;
use starlark::values::none::NoneType;
use starlark::values::structs::AllocStruct;
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, Value, ValueLike,
    starlark_value,
};

/// The workspace handle passed to `generate(ws)`. Holds the workspace root so
/// its capability methods resolve paths and run processes relative to it.
#[derive(Debug, ProvidesStaticType, NoSerialize, StarlarkPagable, Allocative)]
pub(crate) struct Ws {
    root: String,
}

impl Ws {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_string_lossy().into_owned(),
        }
    }

    fn root(&self) -> &Path {
        Path::new(&self.root)
    }
}

impl fmt::Display for Ws {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<ws {}>", self.root)
    }
}

starlark_simple_value!(Ws);

#[starlark_value(type = "ws")]
impl<'v> StarlarkValue<'v> for Ws {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new("ws", ws_methods);
        Some(RES.methods())
    }
}

fn this_ws<'v>(this: Value<'v>) -> starlark::Result<&'v Ws> {
    this.downcast_ref::<Ws>()
        .ok_or_else(|| starlark::Error::new_other(anyhow::anyhow!("not a ws handle")))
}

#[starlark_module]
fn ws_methods(builder: &mut MethodsBuilder) {
    /// Workspace-relative paths matching a glob, sorted, gitignore-aware.
    fn glob<'v>(this: Value<'v>, pattern: &str) -> starlark::Result<Vec<String>> {
        let ws = this_ws(this)?;
        crate::star::io::glob(ws.root(), pattern).map_err(into_star)
    }

    /// The contents of a workspace-relative file.
    fn read<'v>(this: Value<'v>, path: &str) -> starlark::Result<String> {
        let ws = this_ws(this)?;
        std::fs::read_to_string(ws.root().join(path)).map_err(|e| into_star(e.into()))
    }

    /// Run a subprocess from the workspace root (or `cwd`, a workspace-relative
    /// dir - e.g. a sub-module's `src/`), capturing output. Returns a
    /// `struct(stdout, stderr, code)`; raises on a nonzero exit when `check`.
    fn exec<'v>(
        this: Value<'v>,
        args: UnpackList<String>,
        cwd: Option<String>,
        #[starlark(default = true)] check: bool,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let ws = this_ws(this)?;
        let out = crate::star::io::exec(ws.root(), &args.items, cwd.as_deref(), check)
            .map_err(into_star)?;
        Ok(heap.alloc(AllocStruct([
            ("stdout", heap.alloc(out.stdout)),
            ("stderr", heap.alloc(out.stderr)),
            ("code", heap.alloc(out.code)),
        ])))
    }

    /// Relativize an absolute or `//`-rooted path against the workspace root.
    fn rel<'v>(this: Value<'v>, path: &str) -> starlark::Result<String> {
        let ws = this_ws(this)?;
        Ok(crate::star::io::rel(ws.root(), path))
    }

    /// A filename without its directory or extension.
    fn stem<'v>(this: Value<'v>, path: &str) -> starlark::Result<String> {
        let _ = this;
        Ok(Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default())
    }
}

/// One constructed target with the package whose `giant.<infix>.yaml` it lands
/// in (TDD-0024 §E).
#[derive(Debug)]
pub(crate) struct Emitted {
    pub(crate) package: String,
    pub(crate) wire: WireTarget,
}

/// Side collector for `target()` calls. Stored in `Evaluator::extra` so the
/// builtin can register targets as it runs (the documented starlark pattern for
/// pulling data out of a script). Not a heap value, so it carries no Starlark
/// trait baggage - just `ProvidesStaticType` for the `extra` downcast.
#[derive(Debug, Default, ProvidesStaticType)]
pub(crate) struct Collector(RefCell<Vec<Emitted>>);

impl Collector {
    fn push(&self, e: Emitted) {
        self.0.borrow_mut().push(e);
    }

    pub(crate) fn take(self) -> Vec<Emitted> {
        self.0.into_inner()
    }
}

/// Map a host error into a Starlark error carrying its message.
fn into_star(e: anyhow::Error) -> starlark::Error {
    starlark::Error::new_other(e)
}

/// The host globals: `target()` and the `parse_*` data builtins.
#[starlark_module]
pub(crate) fn host_globals(builder: &mut GlobalsBuilder) {
    /// Construct a target, validated against the wire schema at this call.
    #[allow(clippy::too_many_arguments)]
    fn target(
        name: String,
        command: String,
        inputs: Option<UnpackList<String>>,
        outputs: Option<UnpackList<String>>,
        deps: Option<UnpackList<String>>,
        env: Option<SmallMap<String, String>>,
        cwd: Option<String>,
        cache: Option<bool>,
        #[starlark(default = true)] remote_cache: bool,
        exists: Option<String>,
        timeout_secs: Option<u64>,
        #[starlark(default = false)] test: bool,
        tags: Option<UnpackList<String>>,
        label: Option<String>,
        package: Option<String>,
        eval: &mut Evaluator,
    ) -> starlark::Result<NoneType> {
        let inputs = unpack(inputs)
            .into_iter()
            .map(|g| GlobPattern::new(g).map(|glob| Input::File { glob }))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| into_star(anyhow::anyhow!("invalid input glob: {e}")))?;

        let pkg = resolve_package(package, cwd.as_deref());

        let wire = WireTarget {
            name,
            inputs,
            outputs: unpack(outputs),
            deps: unpack(deps),
            command,
            cwd,
            env: env.map(|m| m.into_iter().collect()).unwrap_or_default(),
            cache,
            remote_cache,
            exists,
            timeout_secs,
            test,
            tags: unpack(tags).into_iter().collect(),
            label,
        };

        let collector = eval
            .extra
            .and_then(|e| e.downcast_ref::<Collector>())
            .ok_or_else(|| into_star(anyhow::anyhow!("internal: target() collector missing")))?;
        collector.push(Emitted { package: pkg, wire });
        Ok(NoneType)
    }

    /// Parse one JSON value into Starlark data.
    fn parse_json<'v>(s: &str, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let v: serde_json::Value = serde_json::from_str(s).map_err(|e| into_star(e.into()))?;
        crate::star::json::to_value(&v, heap).map_err(into_star)
    }

    /// Parse concatenated JSON objects (e.g. `go list -json`) into a list.
    fn parse_json_stream<'v>(s: &str, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let mut out = Vec::new();
        let de = serde_json::Deserializer::from_str(s).into_iter::<serde_json::Value>();
        for item in de {
            let v = item.map_err(|e| into_star(e.into()))?;
            out.push(crate::star::json::to_value(&v, heap).map_err(into_star)?);
        }
        Ok(heap.alloc(out))
    }

    /// Parse one YAML document into Starlark data.
    fn parse_yaml<'v>(s: &str, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let v: serde_json::Value = serde_yaml_ng::from_str(s).map_err(|e| into_star(e.into()))?;
        crate::star::json::to_value(&v, heap).map_err(into_star)
    }

    /// Parse one TOML document into Starlark data.
    fn parse_toml<'v>(s: &str, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let v: serde_json::Value = toml_to_json(s).map_err(into_star)?;
        crate::star::json::to_value(&v, heap).map_err(into_star)
    }
}

/// TOML has no first-class null and a richer scalar set, but for config data
/// round-tripping through `serde_json::Value` is adequate. Deferred: a native
/// TOML path if a generator needs TOML-specific types.
fn toml_to_json(_s: &str) -> anyhow::Result<serde_json::Value> {
    anyhow::bail!("parse_toml is not yet wired in this skeleton")
}

/// Owning package for a target (TDD-0024 §E): an explicit `package=`, else the
/// directory of a relative `cwd`, else the root package.
fn resolve_package(package: Option<String>, cwd: Option<&str>) -> String {
    if let Some(p) = package {
        return normalize_package(&p);
    }
    if let Some(c) = cwd
        && !c.starts_with("//")
        && !Path::new(c).is_absolute()
    {
        return normalize_package(c);
    }
    String::new()
}

fn normalize_package(p: &str) -> String {
    let p = p.strip_prefix("//").unwrap_or(p);
    let p = p.strip_prefix("./").unwrap_or(p);
    p.trim_end_matches('/').to_string()
}

/// An optional Starlark list of strings as a plain `Vec` (empty if absent).
fn unpack(list: Option<UnpackList<String>>) -> Vec<String> {
    list.map(|l| l.items).unwrap_or_default()
}
