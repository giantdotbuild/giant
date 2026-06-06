//! The Linux birdcage backend (ADR-0030 §2a). Translates a [`SandboxSpec`] into
//! birdcage exceptions and spawns the build command under them.
//!
//! Mechanism choice is deliberate (ADR-0030 §2a): for *enforcement* - catching
//! a target that reads an undeclared file or reaches the network - filesystem
//! deny plus network on/off is exactly enough. birdcage gives that as a
//! pure-Rust library (seccomp + Landlock + namespaces under the hood), so the
//! porcelain stays a single static binary with no `bwrap` to provision.

use std::ffi::OsString;

use anyhow::{Context, Result};
use birdcage::process::Command;
use birdcage::{Birdcage, Exception, Sandbox};
use giant_schema::SandboxSpec;

pub fn run(spec: &SandboxSpec, command: &[OsString]) -> Result<u8> {
    let (program, args) = command.split_first().context("empty command after `--`")?;

    let mut cage = Birdcage::new();

    // Toolchain paths host the binaries the command execs (e.g. /nix/store),
    // so they need execute as well as read.
    for p in &spec.toolchain {
        cage.add_exception(Exception::ExecuteAndRead(p.clone()))
            .with_context(|| format!("granting ro+x {}", p.display()))?;
    }
    // Declared inputs: read-only.
    for p in &spec.ro {
        cage.add_exception(Exception::Read(p.clone()))
            .with_context(|| format!("granting ro {}", p.display()))?;
    }
    // Declared outputs and scratch: read-write.
    for p in &spec.rw {
        cage.add_exception(Exception::WriteAndRead(p.clone()))
            .with_context(|| format!("granting rw {}", p.display()))?;
    }
    if spec.network {
        cage.add_exception(Exception::Networking)
            .context("granting network access")?;
    }
    // birdcage scrubs the environment by default. An empty allowlist means the
    // engine wants the whole ambient env (back-compat); otherwise grant exactly
    // the listed names - PATH + toolchain essentials + declared `env:`
    // (ADR-0030 §4).
    if spec.env.is_empty() {
        cage.add_exception(Exception::FullEnvironment)
            .context("granting environment access")?;
    } else {
        for name in &spec.env {
            cage.add_exception(Exception::Environment(name.clone()))
                .with_context(|| format!("granting env {name}"))?;
        }
    }

    // birdcage's Command carries only program + args (no cwd/env setters), so
    // set the working directory on ourselves before the fork - the child
    // inherits it. This runs before the sandbox is applied, so it is not
    // restricted; the cwd itself must still fall under an allowed path.
    std::env::set_current_dir(&spec.cwd)
        .with_context(|| format!("chdir to {}", spec.cwd.display()))?;

    let mut cmd = Command::new(program);
    cmd.args(args);

    // birdcage applies the restrictions as it forks the child; stdio is
    // inherited, so the engine's capture sees the command's output directly.
    let mut child = cage.spawn(cmd).context("entering the sandbox")?;
    let status = child.wait().context("waiting for the sandboxed command")?;

    // Propagate the child's exit code. A signal-terminated child has no code;
    // report a generic failure rather than masking it as success.
    Ok(status.code().unwrap_or(1).clamp(0, 255) as u8)
}
