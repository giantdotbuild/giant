//! The slice of the workspace's root `giant.yaml` the runner reads: the
//! `generate:` declaration (ADR-0029 §6, which retired the separate
//! `giant-gen.yaml`). Each entry is either the built-in Starlark host on a
//! `giant.star` script, or an external generator command (TDD-0022's model).
//! Other top-level sections belong to the engine and other porcelains and are
//! ignored here (ADR-0010).

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Default)]
struct File {
    #[serde(default)]
    generate: Vec<Decl>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Decl {
    /// Bare string: sugar for an external `{ name, command: giant-gen-<name> }`.
    Name(String),
    /// The built-in Starlark host on `script`, owning `giant.<infix>.yaml`.
    Builtin {
        script: String,
        #[serde(default)]
        infix: Option<String>,
    },
    /// An external generator command.
    External {
        name: String,
        #[serde(default)]
        command: Option<String>,
    },
}

/// A resolved generator: the built-in host on a script, or an external command.
/// Both own `giant.<name()>.yaml` files and only those (TDD-0022 §C).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Generator {
    /// The embedded Starlark host (ADR-0029): run `script`, emit `giant.<infix>.yaml`.
    Builtin { infix: String, script: PathBuf },
    /// An external program writing `giant.<name>.yaml` under `GIANT_GEN_OUT`.
    External { name: String, command: String },
}

impl Generator {
    /// The filename infix this generator owns (`giant.<name>.yaml`), also its
    /// identity in the `generate:` list and on the CLI.
    pub fn name(&self) -> &str {
        match self {
            Generator::Builtin { infix, .. } => infix,
            Generator::External { name, .. } => name,
        }
    }
}

/// Read and resolve the `generate:` list from the workspace's root config.
/// When `generate:` is absent or empty but a root `giant.star` exists, that
/// file is the implicit built-in host (the zero-config convention, §A). A
/// workspace with neither yields an empty list (a no-op run).
pub fn load(root: &Path) -> Result<Vec<Generator>> {
    let decls = match find_config(root) {
        Some(path) => {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let file: File = parse(&path, &raw)?;
            file.generate
        }
        None => Vec::new(),
    };

    if decls.is_empty() && root.join("giant.star").is_file() {
        return Ok(vec![Generator::Builtin {
            infix: "gen".into(),
            script: PathBuf::from("giant.star"),
        }]);
    }
    resolve(decls)
}

fn find_config(root: &Path) -> Option<PathBuf> {
    ["giant.yaml", "giant.yml", "giant.json"]
        .into_iter()
        .map(|n| root.join(n))
        .find(|p| p.is_file())
}

fn parse(path: &Path, raw: &str) -> Result<File> {
    let file = match path.extension().and_then(|e| e.to_str()) {
        Some("json") => serde_json::from_str(raw)?,
        _ => serde_yaml_ng::from_str(raw)?,
    };
    Ok(file)
}

fn resolve(decls: Vec<Decl>) -> Result<Vec<Generator>> {
    let mut out = Vec::with_capacity(decls.len());
    let mut seen = HashSet::new();
    for decl in decls {
        let g = match decl {
            Decl::Name(name) => {
                let command = format!("giant-gen-{name}");
                Generator::External { name, command }
            }
            Decl::External { name, command } => {
                let command = command.unwrap_or_else(|| format!("giant-gen-{name}"));
                Generator::External { name, command }
            }
            Decl::Builtin { script, infix } => Generator::Builtin {
                infix: infix.unwrap_or_else(|| "gen".into()),
                script: PathBuf::from(script),
            },
        };
        let name = g.name();
        if !is_name_safe(name) {
            bail!("generator name '{name}' is not filename-safe (use letters, digits, '-', '_')");
        }
        if !seen.insert(name.to_string()) {
            bail!("duplicate generator name '{name}' in generate:");
        }
        out.push(g);
    }
    Ok(out)
}

fn is_name_safe(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gens(yaml: &str) -> Result<Vec<Generator>> {
        let file: File = serde_yaml_ng::from_str(yaml).unwrap();
        resolve(file.generate)
    }

    #[test]
    fn bare_string_is_external_sugar() {
        let g = gens("generate: [go, proto]").unwrap();
        assert_eq!(
            g[0],
            Generator::External {
                name: "go".into(),
                command: "giant-gen-go".into()
            }
        );
        assert_eq!(g[1].name(), "proto");
    }

    #[test]
    fn builtin_entry_defaults_infix() {
        let g = gens("generate:\n  - script: giant.star\n").unwrap();
        assert_eq!(
            g[0],
            Generator::Builtin {
                infix: "gen".into(),
                script: PathBuf::from("giant.star")
            }
        );
    }

    #[test]
    fn builtin_entry_custom_infix() {
        let g = gens("generate:\n  - script: build/targets.star\n    infix: build\n").unwrap();
        assert_eq!(g[0].name(), "build");
    }

    #[test]
    fn external_object_with_and_without_command() {
        let g =
            gens("generate:\n  - name: docker\n  - name: vendored\n    command: ./tools/gen.sh\n")
                .unwrap();
        assert_eq!(
            g[0],
            Generator::External {
                name: "docker".into(),
                command: "giant-gen-docker".into()
            }
        );
        assert_eq!(
            g[1],
            Generator::External {
                name: "vendored".into(),
                command: "./tools/gen.sh".into()
            }
        );
    }

    #[test]
    fn duplicate_names_rejected() {
        let err = gens("generate: [go, go]").unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn builtin_and_external_sharing_a_name_rejected() {
        let err = gens("generate:\n  - script: giant.star\n    infix: go\n  - go\n").unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn unsafe_names_rejected() {
        let err = gens("generate: [\"go/bad\"]").unwrap_err();
        assert!(err.to_string().contains("filename-safe"), "{err}");
    }

    #[test]
    fn empty_is_noop() {
        assert!(gens("generate: []").unwrap().is_empty());
    }
}
