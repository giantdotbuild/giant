//! The slice of `giant-gen.yaml` the runner reads: the `generators:`
//! declaration (ADR-0027 §4). Per-generator sections (`go:`, `docker:`, …)
//! belong to the generators themselves, so the schema here is intentionally
//! open - unknown keys are ignored.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Deserialize, Default)]
struct File {
    #[serde(default)]
    generators: Vec<Decl>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Decl {
    /// Bare string: sugar for `{ name, command: giant-gen-<name> }`.
    Name(String),
    /// Object form with an optional explicit command.
    Full {
        name: String,
        command: Option<String>,
    },
}

/// A declared generator with its resolved command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generator {
    pub name: String,
    pub command: String,
}

/// Read and validate the `generators:` list from `<root>/giant-gen.yaml`.
/// A missing file yields an empty list (a no-op run).
pub fn load(root: &Path) -> Result<Vec<Generator>> {
    let path = root.join("giant-gen.yaml");
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let file: File =
        serde_yaml_ng::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    resolve(file.generators)
}

fn resolve(decls: Vec<Decl>) -> Result<Vec<Generator>> {
    let mut out = Vec::with_capacity(decls.len());
    let mut seen = HashSet::new();
    for decl in decls {
        let (name, command) = match decl {
            Decl::Name(name) => {
                let command = format!("giant-gen-{name}");
                (name, command)
            }
            Decl::Full { name, command } => {
                let command = command.unwrap_or_else(|| format!("giant-gen-{name}"));
                (name, command)
            }
        };
        if !is_name_safe(&name) {
            bail!("generator name '{name}' is not filename-safe (use letters, digits, '-', '_')");
        }
        if !seen.insert(name.clone()) {
            bail!("duplicate generator name '{name}' in giant-gen.yaml");
        }
        out.push(Generator { name, command });
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
        resolve(file.generators)
    }

    #[test]
    fn bare_string_defaults_command() {
        let g = gens("generators: [go, proto]").unwrap();
        assert_eq!(
            g[0],
            Generator {
                name: "go".into(),
                command: "giant-gen-go".into()
            }
        );
        assert_eq!(g[1].command, "giant-gen-proto");
    }

    #[test]
    fn object_form_with_and_without_command() {
        let g = gens(
            "generators:\n  - name: docker\n  - name: vendored\n    command: ./tools/gen.sh\n",
        )
        .unwrap();
        assert_eq!(g[0].command, "giant-gen-docker");
        assert_eq!(g[1].command, "./tools/gen.sh");
    }

    #[test]
    fn duplicate_names_rejected() {
        let err = gens("generators: [go, go]").unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn unsafe_names_rejected() {
        let err = gens("generators: [\"go/bad\"]").unwrap_err();
        assert!(err.to_string().contains("filename-safe"), "{err}");
    }

    #[test]
    fn empty_is_noop() {
        assert!(gens("generators: []").unwrap().is_empty());
    }
}
