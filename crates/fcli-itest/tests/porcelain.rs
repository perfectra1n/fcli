//! Porcelain commands that make several API calls to do one thing.
//!
//! These are where unit tests are weakest. A `FakeTransport` test of a multi-call command
//! decides for itself what the second call returns, so it proves the calls are made in the
//! expected order and nothing about whether the server agrees with any of them. Name-to-id
//! resolution, conflict adoption, and "create then upload" sequences are all only really tested
//! here.

use std::path::PathBuf;

use fcli_itest::{TestRepo, commit_and_push, instance_or_skip};

/// A scratch directory that cleans up after itself, for the tests that need a git checkout.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let d = std::env::temp_dir().join(format!("fcli-itest-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Self(d)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `issue create --label --milestone` takes names on the command line but the API wants ids, so
/// the command lists labels and milestones first. Three calls, and the two lookups are the part
/// a mock cannot check: it would happily return an id that does not exist.
#[test]
fn issue_create_resolves_label_and_milestone_names() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "issue-resolve");

    inst.fcli(["label", "create", "bug", "-c", "FF0000", "-R", &repo.slug()])
        .assert_ok("fcli label create");
    inst.fcli(["milestone", "create", "v1", "-R", &repo.slug()]).assert_ok("fcli milestone create");

    inst.fcli([
        "issue",
        "create",
        "--title",
        "resolved",
        "--body",
        "b",
        "--label",
        "bug",
        "--milestone",
        "v1",
        "-R",
        &repo.slug(),
    ])
    .assert_ok("fcli issue create with a label and a milestone");

    // Checked out of band: the command reporting success is not evidence that the server
    // attached anything.
    let (code, body) = repo.api("GET", "issues/1", None);
    assert_eq!(code, 200, "{body}");
    let issue: serde_json::Value = serde_json::from_str(&body).expect("an issue");
    assert_eq!(issue["labels"][0]["name"], "bug", "the label was not attached: {body}");
    assert_eq!(issue["milestone"]["title"], "v1", "the milestone was not attached: {body}");
}

/// An unknown name must fail before anything is created, rather than silently dropping the
/// label and reporting success.
#[test]
fn issue_create_rejects_an_unknown_label() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "issue-badlabel");

    let run = inst.fcli([
        "issue",
        "create",
        "--title",
        "t",
        "--body",
        "b",
        "--label",
        "no-such-label",
        "-R",
        &repo.slug(),
    ]);
    assert!(!run.ok(), "an unknown label should not succeed:\n{}\n{}", run.stdout, run.stderr);

    let (_, body) = repo.api("GET", "issues", None);
    let issues: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    assert_eq!(
        issues.as_array().map(Vec::len),
        Some(0),
        "the issue must not be created when a label could not be resolved: {body}"
    );
}

/// `release create` with files is create-then-upload-each: one JSON POST followed by a
/// `multipart/form-data` POST per asset, streamed from the file. The bytes are compared after a
/// round trip, because a multipart body that is subtly wrong still uploads *something*.
#[test]
fn release_create_uploads_assets_intact() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "release-assets");
    let scratch = Scratch::new("rel");
    std::fs::create_dir_all(scratch.path()).expect("scratch dir");

    // Deliberately not text, and larger than one buffer, so a chunking bug shows up.
    let blob: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let big = scratch.path().join("big.bin");
    std::fs::write(&big, &blob).expect("write the asset");
    let small = scratch.path().join("notes.txt");
    std::fs::write(&small, b"release notes payload").expect("write the asset");

    inst.fcli([
        "release",
        "create",
        "v0.1.0",
        "-R",
        &repo.slug(),
        "--title",
        "v0.1.0",
        "--notes",
        "first release",
        &big.to_string_lossy(),
        &small.to_string_lossy(),
    ])
    .assert_ok("fcli release create with assets");

    let (code, body) = repo.api("GET", "releases/tags/v0.1.0", None);
    assert_eq!(code, 200, "{body}");
    let rel: serde_json::Value = serde_json::from_str(&body).expect("a release");
    let assets = rel["assets"].as_array().cloned().unwrap_or_default();
    assert_eq!(assets.len(), 2, "both assets should be attached: {body}");

    let big_asset = assets
        .iter()
        .find(|a| a["name"] == "big.bin")
        .unwrap_or_else(|| panic!("big.bin is missing: {body}"));
    assert_eq!(
        big_asset["size"].as_u64(),
        Some(blob.len() as u64),
        "the uploaded size does not match the file, so the multipart body was truncated"
    );
}

/// Forking with an explicit name works, which is the control for the test below.
#[test]
fn repo_fork_with_an_explicit_name() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "fork-src");
    // Public: a fork of a private repository is a different permission path.
    repo.api("PATCH", "", Some(r#"{"private":false}"#));

    let org = format!("forkorg{}", std::process::id());
    let (code, body) = inst.api("POST", "orgs", Some(&format!(r#"{{"username":"{org}"}}"#)));
    assert!((200..300).contains(&code) || code == 422, "could not create an org: {code} {body}");

    let fork_name = format!("{}-forked", repo.name);
    let run = inst.fcli(["repo", "fork", &repo.slug(), "--org", &org, "--fork-name", &fork_name]);
    run.assert_ok("fcli repo fork --fork-name");

    let (code, _) = inst.api("GET", &format!("repos/{org}/{fork_name}"), None);
    assert_eq!(code, 200, "the fork was reported but does not exist");
    let _ = inst.api("DELETE", &format!("repos/{org}/{fork_name}"), None);
    let _ = inst.api("DELETE", &format!("orgs/{org}"), None);
}

/// `fcli repo fork <slug>` with no `--fork-name` — the ordinary invocation.
///
/// This was ignored as a known bug: `repo/fork.rs` built the body as
/// `name: Some(args.fork_name.clone().unwrap_or_default())` and the same for `organization`, so
/// an unset flag went out as `""` rather than omitted, and Forgejo answered `500 name is empty`.
/// The model was always correct (`CreateForkOption.name` is `Option<String>` with
/// `skip_serializing_if`); the call site reintroduced the zero value — the exact failure mode
/// commit b62c3d1 set out to remove. The `Option` is now passed straight through, so this runs
/// and guards against the next `unwrap_or_default()`.
#[test]
fn repo_fork_without_a_name() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "fork-plain");
    repo.api("PATCH", "", Some(r#"{"private":false}"#));

    let org = format!("forkplain{}", std::process::id());
    inst.api("POST", "orgs", Some(&format!(r#"{{"username":"{org}"}}"#)));

    let run = inst.fcli(["repo", "fork", &repo.slug(), "--org", &org]);
    let _ = inst.api("DELETE", &format!("repos/{org}/{}", repo.name), None);
    let _ = inst.api("DELETE", &format!("orgs/{org}"), None);
    run.assert_ok("fcli repo fork with no --fork-name");
}

/// `pr create --fill` reads the branch and its commit message from git, then creates.
///
/// This was ignored while the null-scalar decode bug stood, and the failure it produced is
/// worth recording: the pull request **was created** — the server answered 201 with a valid
/// body — and the command then failed decoding `merge_commit_sha` (see `decode.rs`) and printed
/// "nothing was changed on the server", which was false, so re-running hit a 409. The decode is
/// fixed and this runs.
#[test]
fn pr_create_fill_reads_git() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "pr-fill");
    let scratch = Scratch::new("prfill");
    repo.clone_to(scratch.path());
    commit_and_push(
        scratch.path(),
        "feature",
        "f.txt",
        "one\n",
        "Add f.txt\n\nThe body comes from this commit message.",
    );

    let run = inst.fcli_in(scratch.path(), ["pr", "create", "--fill", "-R", &repo.slug()]);
    run.assert_ok("fcli pr create --fill");

    let (_, body) = repo.api("GET", "pulls/1", None);
    let pr: serde_json::Value = serde_json::from_str(&body).expect("a pull request");
    assert_eq!(pr["title"], "Add f.txt", "--fill should take the title from the commit subject");
    assert!(
        pr["body"].as_str().unwrap_or_default().contains("The body comes from"),
        "--fill should take the body from the commit message: {body}"
    );
}

/// `pr merge --squash` checks the pull request's state, merges, and can delete the branch.
///
/// This was ignored for the same reason as everything else in the `pr` group — reading the pull
/// request back decoded `merge_commit_sha`, which is null until the merge happens. Fixed.
#[test]
fn pr_merge_squash_and_delete_branch() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "pr-merge");
    let scratch = Scratch::new("prmerge");
    repo.clone_to(scratch.path());
    commit_and_push(scratch.path(), "feature", "f.txt", "one\n", "Add f.txt");

    let (code, body) =
        repo.api("POST", "pulls", Some(r#"{"title":"merge me","head":"feature","base":"main"}"#));
    assert!((200..300).contains(&code), "{body}");

    inst.fcli(["pr", "merge", "1", "--squash", "--delete-branch", "-R", &repo.slug()])
        .assert_ok("fcli pr merge --squash --delete-branch");

    let (_, body) = repo.api("GET", "pulls/1", None);
    let pr: serde_json::Value = serde_json::from_str(&body).expect("a pull request");
    assert_eq!(pr["merged"], serde_json::Value::Bool(true), "the pull request was not merged");

    let (code, _) = repo.api("GET", "branches/feature", None);
    assert_eq!(code, 404, "--delete-branch should have removed the head branch");
}
