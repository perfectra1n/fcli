//! `xtask` — the fjo generator and repository tooling.
//!
//! Never a dependency of the binary. Everything here runs on a developer's machine or in CI,
//! which is why it is allowed to shell out to `curl` instead of pulling an HTTP stack into the
//! workspace.
//!
//! The pipeline this crate implements, in order:
//!
//! ```text
//! upstream v1_json.tmpl ──update-spec──> spec/forgejo-vX.json ──lower──> Ir ──emit──> code
//!                                             + spec/lock.toml              + spec/name-lock.toml
//! ```
//!
//! Two invariants make the rest of the project safe to build on:
//!
//! 1. **`spec-stats --verify` is a self-test of the loader**, not a report. Its expected
//!    numbers were established by independent inspection of the spec. If the loader
//!    disagrees, the loader is wrong — and finding that out here is far cheaper than finding
//!    it out after 42k lines of generated code have been shaped by a miscount.
//! 2. **`spec/name-lock.toml` makes every command name a versioned contract.** A spec bump
//!    that would rename a command is a hard error until a human passes `--accept-renames`,
//!    because the alternative is silently breaking every script our users have written.
#![forbid(unsafe_code)]
// The IR is a contract that the four emitters (M4–M6) consume, so its fields necessarily
// exist before their readers do. The alternative — per-field `allow(dead_code)` removed one
// at a time as emitters land — produces churn without catching anything: what actually
// guards against unused work here are the uniqueness assertions and `spec-stats --verify`.
#![allow(dead_code)]

/// `return Err(format!(...))`, for the many places where the useful thing to do with a
/// broken spec is stop and explain.
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err(::std::convert::Into::into(format!($($arg)*)))
    };
}

mod emit;
mod ir;
mod itest;
mod name_lock;
mod overrides;
mod spec;
mod stats;
mod swagger;
mod update_spec;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{ArgAction, Parser, Subcommand};

/// Errors here only ever reach a developer's terminal, so a boxed message beats a taxonomy.
/// `forgejo-core`'s [`Error`](../forgejo_core/error/struct.Error.html) exists because *users*
/// need remedies; `xtask` failures are read by whoever just ran the command.
pub type Result<T, E = Box<dyn std::error::Error + Send + Sync>> = std::result::Result<T, E>;

/// The workspace root, derived from this crate's manifest directory at compile time.
///
/// Deliberately **not** `current_dir()`: `cargo xtask` is run from wherever the developer
/// happens to be standing, and a generator whose output depends on the invocation directory
/// produces diffs that nobody can reproduce.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/xtask is two levels below the workspace root")
        .to_path_buf()
}

#[derive(Parser)]
#[command(name = "xtask", about = "fjo generator and repository tooling", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Vendor the Forgejo Swagger spec at a git tag into `spec/`.
    UpdateSpec {
        /// Forgejo tag, e.g. `v16.0.4`. A bare `16.0.4` is accepted too.
        #[arg(long)]
        version: String,
        /// Fetch, check and canonicalize, but write nothing. Prints what would change.
        #[arg(long, action = ArgAction::SetTrue)]
        dry_run: bool,
    },

    /// Print counts from the vendored spec, and by default assert they are the known-good ones.
    SpecStats {
        /// Redundant — verification is on by default. Accepted so scripts can be explicit.
        #[arg(long, action = ArgAction::SetTrue)]
        verify: bool,
        /// Print the table without asserting the numbers. Only useful while investigating a
        /// spec bump; CI must never pass this.
        #[arg(long, action = ArgAction::SetTrue, conflicts_with = "verify")]
        no_verify: bool,
    },

    /// Lower the spec to the IR and regenerate the committed generated trees.
    Codegen {
        /// Regenerate into a temp dir and report differences without touching the working
        /// tree. What CI runs: it makes a hand-edit under `src/generated/` fail the build,
        /// which is the whole reason 42k lines of generated code can be trusted.
        #[arg(long, action = ArgAction::SetTrue)]
        check: bool,
        /// Assert the naming invariants: 506 unique `(module, fn)` and `(group, command)`.
        #[arg(long, action = ArgAction::SetTrue)]
        check_names: bool,
        /// Pretty-print the whole IR to stdout.
        #[arg(long, action = ArgAction::SetTrue)]
        dump_ir: bool,
        /// Rewrite `spec/name-lock.toml` for renamed commands, printing a CHANGELOG-ready diff.
        #[arg(long, action = ArgAction::SetTrue)]
        accept_renames: bool,
        /// Allow operations that disappeared upstream to be dropped from the name lock.
        #[arg(long, action = ArgAction::SetTrue)]
        accept_removals: bool,
    },

    /// Run the integration suite against a real Forgejo, booting one in Docker if needed.
    Itest {
        /// Leave the container running afterwards for inspection.
        #[arg(long, action = ArgAction::SetTrue)]
        keep: bool,
        /// Use a different Forgejo image than the vendored spec targets.
        #[arg(long)]
        image: Option<String>,
        /// Permit a skip when no Forgejo is reachable. CI must never pass this.
        #[arg(long, action = ArgAction::SetTrue)]
        allow_skip: bool,
        /// Only run tests whose name contains this.
        filter: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let root = workspace_root();
    match cli.cmd {
        Cmd::UpdateSpec { version, dry_run } => update_spec::run(&root, &version, dry_run),

        Cmd::SpecStats { no_verify, .. } => {
            let loaded = spec::load(&root)?;
            let stats = stats::Stats::compute(&loaded.spec);
            print!("{}", stats.render());
            if no_verify {
                eprintln!("note: --no-verify given; the loader self-test did not run");
                Ok(())
            } else {
                stats.verify(&loaded.lock.version)
            }
        }

        Cmd::Itest { keep, image, allow_skip, filter } => {
            itest::run(&root, itest::Options { keep, image, allow_skip, filter })
        }

        Cmd::Codegen { check, check_names, dump_ir, accept_renames, accept_removals } => {
            let loaded = spec::load(&root)?;
            let overrides = overrides::Overrides::load()?;
            let ir = ir::lower::lower(&loaded, &overrides)?;

            let mode = name_lock::Mode { accept_renames, accept_removals };
            name_lock::reconcile(&root, &ir, mode)?;

            if check_names {
                println!(
                    "ok: {} operations, {} unique (module, fn), {} unique (group, command)",
                    ir.operations.len(),
                    ir.operations.len(),
                    ir.operations.len(),
                );
            }
            if dump_ir {
                print!("{}", ir.dump());
                return Ok(());
            }
            if check_names {
                return Ok(());
            }

            let files = emit::emit_all(&ir)?;
            let write_mode = if check { emit::Mode::Check } else { emit::Mode::Write };
            let report = emit::write_all(&root, &files, write_mode)?;
            print!("{}", report.render());

            if check && !report.is_clean() {
                for p in &report.changed {
                    eprintln!("  would change: {}", p.display());
                }
                for p in &report.removed {
                    eprintln!("  would remove: {}", p.display());
                }
                bail!(
                    "the committed generated tree does not match the spec.\n\
                     Run `cargo xtask codegen` and commit the result.\n\
                     Edit the generator, not files under src/generated/. See CONTRIBUTING.md."
                );
            }
            Ok(())
        }
    }
}
