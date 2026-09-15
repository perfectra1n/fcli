//! `cargo xtask itest` — run the integration suite against a real Forgejo.
//!
//! The container lifecycle deliberately lives in `crates/fcli-itest`, not here: the tests
//! themselves need to boot, reap and inspect instances, and duplicating that in the generator
//! crate would mean two implementations drifting apart. This subcommand's whole job is to run
//! `cargo test` with the right environment, and to make a skip impossible.
//!
//! That last part is the reason this exists rather than being a line in the README. The suite
//! *skips* when no Forgejo is reachable, so that `cargo test --workspace` works on a laptop
//! without Docker — and a skip prints "ok". A CI job that silently tested nothing is worse than
//! one that failed, so `xtask itest` sets `FCLI_ITEST_REQUIRE=1` and turns a skip into an error.

use std::path::Path;
use std::process::Command;

use crate::Result;

pub struct Options {
    /// Leave the container running afterwards, for poking at by hand.
    pub keep: bool,
    /// Override the image, e.g. to try a newer Forgejo than the vendored spec targets.
    pub image: Option<String>,
    /// Permit a skip. Only for a developer without Docker; CI must never pass this.
    pub allow_skip: bool,
    /// Passed through to `cargo test` as a filter.
    pub filter: Option<String>,
}

pub fn run(root: &Path, opts: Options) -> Result<()> {
    // Deliberately `cargo test`, not `cargo nextest run`, even though the rest of the
    // workspace moved to nextest in `mise run test`.
    //
    // nextest runs every test in its OWN PROCESS. This crate's harness shares one Forgejo
    // instance per process (the `OnceLock` in `fcli_itest::shared()`) and force-reaps every
    // labelled container on boot. Under `cargo test` that is one container per test BINARY —
    // six — and the reap only collects corpses from an earlier run. Under nextest it becomes
    // one container per TEST, each new boot deleting the server the running tests are using.
    // Measured, not assumed: `cargo nextest run --workspace` left 11+ `fcli-itest-<pid>`
    // containers up at once and failed 8 tests, in a suite `cargo test` runs green.
    //
    // .config/nextest.toml serialises the crate into a `forgejo-container` test group so the
    // deliberate `--ignore-default-filter` path works, but that is ~3x slower and boots one
    // container per test. This is the supported runner, and the faster one.
    //
    // The exit code this function depends on survives either way, for the record: `cargo test`
    // exits 101 on a failure and nextest exits 100, and this checks `status.success()`, not a
    // specific number. So the choice above is about the container model alone.
    let mut cmd = Command::new(env!("CARGO"));
    cmd.current_dir(root).args(["test", "--package", "fcli-itest"]);

    // The suite drives the `fcli` binary, so it has to exist and be current.
    build_binary(root)?;

    if let Some(filter) = &opts.filter {
        cmd.arg("--").arg(filter);
    }

    if !opts.allow_skip {
        cmd.env("FCLI_ITEST_REQUIRE", "1");
    }
    if opts.keep {
        cmd.env("FCLI_ITEST_KEEP", "1");
    }
    if let Some(image) = &opts.image {
        cmd.env("FCLI_ITEST_IMAGE", image);
    }

    let status = cmd.status().map_err(|e| format!("could not run cargo test: {e}"))?;
    if !status.success() {
        bail!(
            "the integration suite failed.\n\
             If the failure was \"no Forgejo instance available\", either make Docker usable or \
             point the suite at an existing instance:\n\
             \n    FCLI_TEST_HOST=git.example.org FCLI_TEST_TOKEN=... cargo xtask itest\n\
             \nTo inspect a failing instance, re-run with --keep."
        );
    }
    Ok(())
}

/// The integration tests invoke `fcli` as a subprocess, so a stale binary would silently test
/// the previous build — the kind of failure that wastes an afternoon.
fn build_binary(root: &Path) -> Result<()> {
    let status = Command::new(env!("CARGO"))
        .current_dir(root)
        .args(["build", "--package", "fcli"])
        .status()
        .map_err(|e| format!("could not build the fcli binary: {e}"))?;
    if !status.success() {
        bail!("`cargo build -p fcli` failed, so there is nothing to integration-test");
    }
    Ok(())
}
