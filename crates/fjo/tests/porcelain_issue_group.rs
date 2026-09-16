//! The clap surface of the `issue`, `label`, `milestone` and `release` groups.
//!
//! # Why this test exists
//!
//! clap answers a **duplicate flag name with a panic**, not with an error, and it only does so
//! when the offending `Command` is actually built. Every global flag in [`fjo::global`] is
//! `global(true)`, so it is propagated into every subcommand — which means a porcelain command
//! declaring `--color` or `-t` crashes the binary on an ordinary command line while every unit
//! test in the module still passes, because a unit test calls the `async fn` and never builds a
//! parser.
//!
//! That is exactly how `fjo label create -c e11d21` shipped as a panic during development:
//! `--color` collided with the global `--color`. `debug_assert` below is the check that would
//! have caught it, and it is why these four groups spell `-c`, `-L`, `-O` and `-T` without a
//! long name — the global already owns `--color`, `--limit`, `--output` and `--template`.
//!
//! The root here is assembled the same way `main` assembles the real one: the derive tree, then
//! `global::augment` with an empty `taken` set.

use std::collections::BTreeSet;

use clap::{CommandFactory, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "fjo")]
struct Root {
    #[command(subcommand)]
    cmd: Group,
}

#[derive(Subcommand)]
enum Group {
    Issue(fjo::cmd::issue::Args),
    Label(fjo::cmd::label::Args),
    Milestone(fjo::cmd::milestone::Args),
    Release(fjo::cmd::release::Args),
}

fn root() -> clap::Command {
    fjo::global::augment(Root::command(), &BTreeSet::new())
}

/// The panic-catcher. `debug_assert` walks every subcommand and every argument, and it is the
/// only way to find a collision without running the command.
#[test]
fn the_command_tree_has_no_duplicate_flag_names() {
    root().debug_assert();
}

/// Every flag `docs/porcelain-conventions.md` fixes the meaning of, spelled the way the
/// conventions spell it, must parse. This is the regression test for someone "tidying up" a
/// short-only flag into a long one and reintroducing a collision.
#[test]
fn the_conventional_flags_all_parse() {
    let lines: &[&[&str]] = &[
        &[
            "fjo", "issue", "create", "--title", "t", "-b", "body", "-a", "@me", "-l", "bug", "-m",
            "1.0",
        ],
        &["fjo", "issue", "create", "-F", "-", "-e", "-w", "-T", "bug.md", "--dry-run"],
        &[
            "fjo", "issue", "list", "-s", "all", "-a", "@me", "-A", "alice", "-l", "bug", "-m",
            "1.0", "-L", "5", "-S", "boom", "-w",
        ],
        &["fjo", "issue", "view", "#42", "-c", "-w"],
        &[
            "fjo",
            "issue",
            "edit",
            "42",
            "--add-label",
            "bug",
            "--remove-label",
            "ci",
            "--add-assignee",
            "@me",
            "--remove-assignee",
            "bob",
        ],
        &["fjo", "issue", "close", "42", "-c", "because"],
        &["fjo", "issue", "comment", "42", "-b", "hi", "--edit", "555"],
        &["fjo", "issue", "delete", "42", "--yes"],
        &["fjo", "issue", "pin", "42", "--position", "1"],
        &["fjo", "issue", "unpin", "42"],
        &["fjo", "issue", "depends", "add", "42", "--blocked-by", "7"],
        &["fjo", "issue", "depends", "remove", "42", "--blocks", "9"],
        &["fjo", "issue", "depends", "list", "42"],
        &["fjo", "label", "list", "-L", "5", "--org", "acme"],
        &["fjo", "label", "create", "bug", "-c", "e11d21", "-d", "broken", "--exclusive"],
        &["fjo", "label", "edit", "bug", "--name", "defect", "-c", "fff"],
        &["fjo", "label", "delete", "bug", "--yes"],
        &["fjo", "label", "clone", "forgejo/forgejo", "--overwrite"],
        &["fjo", "milestone", "list", "-s", "closed", "-L", "5"],
        &["fjo", "milestone", "create", "1.0", "-d", "2026-12-31", "--description", "x"],
        &["fjo", "milestone", "edit", "1.0", "--title", "1.1", "-s", "closed"],
        &["fjo", "milestone", "issues", "1.0", "-s", "all"],
        &[
            "fjo",
            "release",
            "create",
            "v1.0.0",
            "./a.tgz",
            "./b.tgz",
            "-n",
            "notes",
            "--generate-notes",
            "-d",
            "-p",
            "--target",
            "main",
            "--title",
            "First",
            "--verify-tag",
        ],
        &["fjo", "release", "upload", "v1.0.0", "./a.tgz", "--clobber"],
        &["fjo", "release", "download", "v1.0.0", "-p", "*.tgz", "-D", "./dl", "--skip-existing"],
        &["fjo", "release", "download", "v1.0.0", "-A", "tar.gz", "-O", "-"],
        &["fjo", "release", "delete-asset", "v1.0.0", "a.tgz", "--yes"],
        // Globals must still reach a porcelain subcommand, before and after it.
        &["fjo", "--json", "number", "issue", "list"],
        &["fjo", "issue", "list", "--json", "number", "-R", "o/n", "--limit", "3"],
    ];
    for line in lines {
        root()
            .clone()
            .try_get_matches_from(*line)
            .unwrap_or_else(|e| panic!("{line:?} should parse:\n{e}"));
    }
}

/// `-t` is the global `--template`, so it must not have been quietly re-taken for `--title`.
/// The conventions ask for `-t/--title`; clap cannot give both, and the resolution recorded here
/// is that the global wins and `--title` is long-only.
#[test]
fn dash_t_still_means_template() {
    let m = root().try_get_matches_from(["fjo", "-t", "{{.number}}", "issue", "list"]).unwrap();
    let globals = fjo::global::GlobalOpts::from_chain(&[&m]);
    assert_eq!(globals.template.as_deref(), Some("{{.number}}"));
}

/// A wrong-looking issue number must be a usage error, not a panic and not a silent 0.
#[test]
fn a_non_numeric_issue_number_is_refused() {
    let e = root().try_get_matches_from(["fjo", "issue", "view", "not-a-number"]).unwrap_err();
    assert_eq!(e.kind(), clap::error::ErrorKind::ValueValidation, "{e}");
    // A leading `#` is accepted, because that is how people write it.
    assert!(root().try_get_matches_from(["fjo", "issue", "view", "#42"]).is_ok());
}

/// `--blocked-by` and `--blocks` are the two directions of one relation; asking for both, or
/// neither, is a usage error rather than a coin toss about which endpoint gets called.
#[test]
fn a_dependency_needs_exactly_one_direction() {
    assert!(root().try_get_matches_from(["fjo", "issue", "depends", "add", "42"]).is_err());
    assert!(
        root()
            .try_get_matches_from([
                "fjo",
                "issue",
                "depends",
                "add",
                "42",
                "--blocked-by",
                "7",
                "--blocks",
                "9"
            ])
            .is_err()
    );
}

/// `--clobber` and `--skip-existing` say opposite things about a file that already exists.
#[test]
fn download_refuses_contradictory_overwrite_flags() {
    assert!(
        root()
            .try_get_matches_from([
                "fjo",
                "release",
                "download",
                "v1",
                "--clobber",
                "--skip-existing"
            ])
            .is_err()
    );
}
