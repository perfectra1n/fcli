//! Pagination against a real server.
//!
//! Every termination rule in `forgejo-core::http::paginate` keys off response headers that the
//! Forgejo specification **does not declare** — so a unit test can only confirm that we agree
//! with our own guess about them. These tests confirm the guess.
//!
//! The rule under the most pressure is (c): "a short page ends the walk" is wrong, because
//! Forgejo silently clamps `limit` to `max_response_items`. Ask for 100, receive 50, conclude
//! "done", and lose everything past item 50 — with exit 0 and no warning. That is the highest
//! consequence silent bug available in this tool, so it gets a test against the real clamp
//! rather than a simulated one.

use fcli_itest::{TestRepo, instance_or_skip};

/// Comfortably more than one page at both the default page size (30) and the clamp (50), so the
/// walk has to cross a boundary no matter which the server picks.
const ISSUES: usize = 75;

fn seed_issues(repo: &TestRepo<'_>, n: usize) {
    for i in 1..=n {
        let (code, body) =
            repo.api("POST", "issues", Some(&format!(r#"{{"title":"issue number {i}"}}"#)));
        assert!((200..300).contains(&code), "seeding issue {i} failed: HTTP {code}: {body}");
    }
}

/// Does Forgejo send the headers the whole design rests on?
///
/// Asserted rather than merely observed: if a future Forgejo stops sending `Link`, the walk
/// falls back to the far weaker heuristic in rule (c), and we want to be told.
#[test]
fn forgejo_sends_link_and_total_count() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "paging-headers");
    seed_issues(&repo, ISSUES);

    let first = inst.api_headers(&format!("repos/{}/issues?limit=50&page=1", repo.slug()));
    let lower = first.to_lowercase();
    assert!(lower.contains("x-total-count:"), "no X-Total-Count on page 1:\n{first}");
    assert!(lower.contains("link:"), "no Link header on page 1:\n{first}");
    assert!(
        lower.contains(r#"rel="next""#),
        "page 1 of {ISSUES} issues should advertise a next page:\n{first}"
    );

    // The last page is what actually terminates the walk: `Link` is still present, but carries
    // only `first`/`prev`. Termination rule (a) depends on exactly this shape.
    let last = inst.api_headers(&format!("repos/{}/issues?limit=50&page=2", repo.slug()));
    let lower = last.to_lowercase();
    assert!(lower.contains("link:"), "the last page should still carry a Link header:\n{last}");
    assert!(
        !lower.contains(r#"rel="next""#),
        "the last page must not advertise a next page, or the walk cannot terminate:\n{last}"
    );
}

/// The clamp is real: asking for more than `max_response_items` returns fewer items *without*
/// saying so, and the `Link` it sends back still echoes the limit we asked for.
#[test]
fn server_clamps_limit_below_what_was_requested() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "paging-clamp");
    seed_issues(&repo, ISSUES);

    let (code, body) =
        inst.api("GET", &format!("repos/{}/issues?limit=100&page=1", repo.slug()), None);
    assert_eq!(code, 200, "{body}");
    let items: Vec<serde_json::Value> = serde_json::from_str(&body).expect("an array of issues");

    let (_, caps) = inst.api("GET", "settings/api", None);
    let caps: serde_json::Value = serde_json::from_str(&caps).expect("settings/api is JSON");
    let max = caps["max_response_items"].as_u64().expect("max_response_items");

    assert!(
        (items.len() as u64) <= max && (items.len() as u64) < 100,
        "expected the server to clamp 100 down to {max}, but it returned {}. If Forgejo has \
         stopped clamping, termination rule (c) is no longer load-bearing and its comment is \
         now misleading.",
        items.len()
    );
}

/// The property that matters: every item, exactly once, across a real multi-page walk.
///
/// Both spellings are checked because they take different paths through the paginator — an
/// explicit over-max `limit` is the case that trips the naive termination rule.
#[test]
fn paginate_loses_nothing() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "paging-walk");
    seed_issues(&repo, ISSUES);

    for query in ["", "?limit=100"] {
        let run = inst.fcli([
            "api",
            &format!("repos/{}/issues{query}", repo.slug()),
            "--paginate",
            "--jq",
            ".[].number",
        ]);
        run.assert_ok(&format!("fcli api --paginate '{query}'"));

        let mut got: Vec<u64> = run.stdout.lines().filter_map(|l| l.trim().parse().ok()).collect();
        let total = got.len();
        got.sort_unstable();
        got.dedup();

        assert_eq!(
            total, ISSUES,
            "--paginate '{query}' returned {total} issues, expected {ISSUES}. \
             Fewer means the walk stopped early and silently dropped items."
        );
        assert_eq!(got.len(), ISSUES, "--paginate '{query}' returned duplicates");
        assert_eq!(got.first().copied(), Some(1));
        assert_eq!(got.last().copied(), Some(ISSUES as u64));
    }
}

/// `--limit N` is a user cap (termination rule (e)) and must stop the walk early rather than
/// being rounded up to a page boundary.
#[test]
fn user_limit_caps_the_walk() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "paging-userlimit");
    seed_issues(&repo, ISSUES);

    let run = inst.fcli(["issue", "list", "-R", &repo.slug(), "--limit", "35", "--json", "number"]);
    run.assert_ok("fcli issue list --limit 35");
    let items = run.json();
    assert_eq!(
        items.as_array().map(Vec::len),
        Some(35),
        "--limit 35 should return exactly 35 items, not a whole number of pages"
    );
}

/// Not every list endpoint paginates the same way, and the differences are invisible to a mock.
///
/// `issues` honours `limit`, clamps it to `max_response_items`, and sends both `Link` and
/// `X-Total-Count`. `labels` sends **no `Link` at all**, and ignores a bare `limit` entirely —
/// it only takes effect when `page` is also present. Termination rule (a), the authoritative
/// one, is therefore unavailable on `labels` and the walk falls back to the weaker rule (c).
///
/// Recorded as a test because the paginator's correctness argument depends on which of these
/// shapes a given endpoint has, and the specification declares no response headers at all.
#[test]
fn pagination_headers_differ_between_endpoints() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "paging-shapes");
    for i in 1..=60 {
        repo.api("POST", "labels", Some(&format!(r#"{{"name":"lbl-{i}","color":"00ff00"}}"#)));
    }

    let headers = inst.api_headers(&format!("repos/{}/labels?limit=5", repo.slug()));
    let lower = headers.to_lowercase();
    assert!(lower.contains("x-total-count:"), "labels should still count: {headers}");
    assert!(
        !lower.contains("link:"),
        "labels has never sent a Link header; if it now does, the paginator can use rule (a) \
         there and this note is out of date:\n{headers}"
    );

    let (_, body) = inst.api("GET", &format!("repos/{}/labels?limit=5", repo.slug()), None);
    let bare: Vec<serde_json::Value> = serde_json::from_str(&body).expect("an array");
    let (_, body) = inst.api("GET", &format!("repos/{}/labels?page=1&limit=5", repo.slug()), None);
    let paged: Vec<serde_json::Value> = serde_json::from_str(&body).expect("an array");
    assert!(
        bare.len() > paged.len(),
        "a bare `limit` is ignored on labels ({} returned) while `page=1&limit=5` is honoured \
         ({} returned). Name resolution in `issue create` relies on the first behaviour.",
        bare.len(),
        paged.len()
    );

    // Whatever the endpoint's shape, the generic walk must still collect everything.
    let run = inst.fcli([
        "api",
        &format!("repos/{}/labels", repo.slug()),
        "--paginate",
        "--jq",
        ".[].name",
    ]);
    run.assert_ok("fcli api --paginate over an endpoint with no Link header");
    assert_eq!(
        run.stdout.lines().filter(|l| !l.trim().is_empty()).count(),
        60,
        "--paginate lost labels on an endpoint that sends no Link header"
    );
}

/// A truncated list has to say it was truncated, and by how much.
///
/// The design calls for gh's `Showing N of M` banner, and `X-Total-Count` supplies the M on
/// every list endpoint. The banner used to read `Showing 30 labels`, with no total, so a user
/// looking at a repository with 60 labels was shown 30 and not told that the other 30 existed.
/// That half is fixed; this pins it.
///
/// `FCLI_FORCE_TTY` is not a workaround for the banner being hard to see. The banner is
/// terminal-only *by design*: `docs/porcelain-conventions.md` makes padded columns plus a banner
/// the TTY rendering and headerless TSV with no banner the piped rendering, precisely so
/// `fcli label list | cut -f2` works. A test that captured stdout and then complained about the
/// missing banner would be asserting against the contract other tests in this file depend on,
/// so it asks for terminal rendering instead. (The variable takes a width, an `N%`, or any
/// non-empty value.)
#[test]
fn truncated_list_reports_the_total() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "paging-banner");
    for i in 1..=60 {
        repo.api("POST", "labels", Some(&format!(r#"{{"name":"lbl-{i}","color":"00ff00"}}"#)));
    }

    let run = inst.fcli(["label", "list", "-R", &repo.slug(), "--json", "name"]);
    run.assert_ok("fcli label list");
    let shown = run.json().as_array().map(Vec::len).unwrap_or(0);
    assert!(shown < 60, "this test assumes the default view truncates; it showed {shown}");

    let banner = inst.fcli_env(
        std::path::Path::new("."),
        &[("FCLI_FORCE_TTY", "80")],
        ["label", "list", "-R", &repo.slug()],
    );
    banner.assert_ok("fcli label list (table)");
    assert!(
        banner.stdout.contains(" of 60") || banner.stdout.contains("of 60 "),
        "the banner must say how many were withheld, but it reads:\n{}",
        banner.stdout.lines().next().unwrap_or_default()
    );
}

/// `fcli … | head -1` must not panic or print a broken-pipe backtrace.
///
/// Rust sets `SIGPIPE` to `SIG_IGN` before `main`, which turns a closed pipe into an `Err` that
/// surfaces as a panic, exit 101 and a backtrace note. `main` restores the default disposition,
/// so the process is killed by the signal (141) or finishes first (0). Both are correct; what
/// must never happen is 101, or anything on stderr.
///
/// The output has to exceed a pipe buffer (64 KiB) for the signal to land at all, which is why
/// this needs a seeded repository rather than a trivial command.
#[test]
fn sigpipe_is_not_a_panic() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "sigpipe");
    seed_issues(&repo, ISSUES);

    // No shell: `${PIPESTATUS[0]}` is a bashism, and on a `dash`-based /bin/sh it would read
    // `head`'s status instead — a test that always passes while testing nothing. Instead the
    // pipe is closed from here, which is exactly what `head` does when it has read enough.
    //
    // The Command is hand-rolled because the pipe has to stay under this test's control, but the
    // environment comes from `child_env()`. Spelling it out here is how this test ended up
    // handing fcli a scheme-stripped host and exiting 6 under docker-out-of-docker, long after
    // the harness itself had been fixed.
    let mut child = std::process::Command::new(fcli_itest::fcli_bin())
        .args(["api", &format!("repos/{}/issues?limit=50", repo.slug()), "--paginate"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .envs(inst.child_env())
        .spawn()
        .expect("fcli should start");

    // Read a little, then drop the read end. The next write past the pipe buffer gets EPIPE.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut buf = [0u8; 16];
    let _ = std::io::Read::read(&mut stdout, &mut buf);
    drop(stdout);

    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let mut stderr = String::new();
    let _ = std::io::Read::read_to_string(&mut stderr_pipe, &mut stderr);
    let status = child.wait().expect("fcli should exit");

    let code = status.code();
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal: Option<i32> = None;

    assert!(
        code == Some(0) || signal == Some(libc_sigpipe()),
        "expected a clean exit or death by SIGPIPE, got code {code:?} signal {signal:?}. \
         Exit 101 would mean SIGPIPE was left at Rust's SIG_IGN and the write panicked."
    );
    assert!(stderr.trim().is_empty(), "a closed pipe must be silent, but stderr had:\n{stderr}");
}

/// SIGPIPE's number, without taking a dependency on `libc` for one constant.
fn libc_sigpipe() -> i32 {
    13
}
