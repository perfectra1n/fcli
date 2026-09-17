//! The instance-wide corners: packages, settings, templates, markup rendering, notifications,
//! topics, quotas, mirrors and ActivityPub.
//!
//! What these have in common is that a mock proves almost nothing about them. They are mostly
//! *reads of server state that the server alone decides*: which gitignore templates exist, what
//! `max_response_items` is, whether `/nodeinfo` is even routed. A `FakeTransport` answers with
//! whatever the test author believed, so it agrees with itself by construction.
//!
//! # What this file measured about ActivityPub, which the design note got wrong
//!
//! `docs/superpowers/specs/2026-09-16-command-coverage-design.md` lists "ActivityPub `GET` (6) —
//! Drivable directly". Measured against `codeberg.org/forgejo/forgejo:16.0.4` with federation
//! enabled, **two** of them are:
//!
//! * `getNodeInfo` and `activitypubInstanceActor` answer 200. Both are covered below.
//! * `activitypubPerson`, `activitypubPersonFeed`, `activitypubRepository` and the two activity
//!   reads answer `request signature verification failed`. Forgejo demands an HTTP-signed request
//!   even for a `GET`, so they need exactly the second-instance topology the five inbox/outbox
//!   `POST`s need. They are recorded in `spec/live-coverage.toml`, and the test at the end of
//!   this file asserts the refusal so the finding is executable rather than a note in a file.
//!
//! `/nodeinfo` belongs on that list with the ActivityPub routes, which is not obvious: without
//! `[federation] ENABLED` it answers the router's bare `404 page not found` before any handler
//! runs, exactly as `/activitypub/*` does. Since federation cannot be switched on for the shared
//! instance — it breaks starring, see `FEDERATION_ENV` in `crates/fjo-itest/src/lib.rs` — the one
//! test that needs it takes [`fjo_itest::federated`]'s second container instead. Everything else
//! here runs on the shared instance, where `[quota] ENABLED` is the switch this file depends on.

use std::process::Command;

use fjo_itest::{Instance, TestRepo, cover, instance_or_skip};

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

/// Upload one file into Forgejo's **generic** package registry.
///
/// Three things here are not obvious and each one cost a round trip to find:
///
/// 1. The registry lives at `/api/packages/…`, **not** `/api/v1/packages/…`. The `v1` path is
///    the read/delete API; the upload is a different surface entirely. So this builds its URL
///    from [`Instance::base_url`] rather than from [`Instance::api_base`].
/// 2. It is a plain `PUT` with the file as the whole body — no multipart, no JSON envelope.
/// 3. The `Content-Type` has to be something other than the form types. Left at curl's default
///    (`application/x-www-form-urlencoded`) Forgejo answers `500 request Content-Type isn't
///    multipart/form-data`, which reads like a server fault and is really a missing header.
///
/// Out of band on purpose, like [`Instance::api`]: the fixture that creates the thing under test
/// must not go through the code under test.
fn upload_generic_package(
    inst: &Instance,
    owner: &str,
    name: &str,
    version: &str,
    filename: &str,
    contents: &str,
) {
    let url = format!("{}/api/packages/{owner}/generic/{name}/{version}/{filename}", inst.base_url);
    let out = Command::new("curl")
        .args(["-sS", "-w", "\n%{http_code}", "--max-time", "30", "-X", "PUT"])
        .args(["-H", &format!("Authorization: token {}", inst.token)])
        .args(["-H", "Content-Type: application/octet-stream"])
        .args(["--data-binary", contents])
        .arg(&url)
        .output()
        .expect("curl should run to upload a package");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines: Vec<&str> = text.lines().collect();
    let code: i32 = lines.pop().unwrap_or("0").trim().parse().unwrap_or(0);
    assert!(
        (200..300).contains(&code),
        "uploading {name}/{version}/{filename} to the generic registry failed: HTTP {code}: {}",
        lines.join("\n")
    );
}

/// A package name unique to this process, so parallel tests sharing one instance — and one
/// package owner — never see each other's uploads.
fn unique_package_name(inst: &Instance, prefix: &str) -> String {
    // `unique_repo_name` is just "a name nothing else in this process will pick"; the fact that
    // its callers mostly make repositories out of it is incidental. Reusing it keeps the one
    // counter that guarantees uniqueness in one place.
    inst.unique_repo_name(prefix)
}

// ---------------------------------------------------------------------------------------------
// Packages — the generic registry, then every read and both link directions
// ---------------------------------------------------------------------------------------------

/// The whole package surface in one lifecycle: upload, list, read, list files, link to a
/// repository, unlink, delete, confirm gone.
///
/// Every assertion is against server state the upload created, which is the point — a mock
/// cannot tell you that Forgejo stores `Size` with a capital S in the file listing (it does),
/// nor that `repository` comes back `null` until something links it.
///
/// `link`/`unlink` are the pair a mock is least able to check: they are `POST`s that return
/// nothing useful, so the only evidence they did anything is reading the package back.
#[test]
fn a_generic_package_upload_is_visible_to_every_read_and_survives_link_unlink_and_delete() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "listPackages",
        "getPackage",
        "listPackageFiles",
        "linkPackage",
        "unlinkPackage",
        "deletePackage",
    ]);

    let pkg = unique_package_name(inst, "rawpkg");
    let body = "generic package payload\n";
    upload_generic_package(inst, &inst.user, &pkg, "1.0.0", "payload.txt", body);
    let repo = TestRepo::create(inst, "pkglink");

    // list-packages is owner-scoped and every test in this file shares the owner, so this
    // asserts presence rather than a count — see this crate's note on test independence.
    let listed = inst.fjo(["raw", "package", "list-packages", &inst.user, "--limit", "200"]);
    listed.assert_ok("fjo raw package list-packages");
    let names: Vec<String> = listed
        .json()
        .as_array()
        .expect("list-packages returns an array")
        .iter()
        .filter_map(|p| p["name"].as_str().map(str::to_owned))
        .collect();
    assert!(names.contains(&pkg), "the uploaded package is missing from the listing: {names:?}");

    let view = inst.fjo(["raw", "package", "get-package", &inst.user, "generic", &pkg, "1.0.0"]);
    view.assert_ok("fjo raw package get-package");
    let got = view.json();
    assert_eq!(got["type"], "generic", "the registry type came back wrong: {got}");
    assert_eq!(got["version"], "1.0.0");
    assert!(got["repository"].is_null(), "a fresh package must not be linked to anything: {got}");

    let files =
        inst.fjo(["raw", "package", "list-package-files", &inst.user, "generic", &pkg, "1.0.0"]);
    files.assert_ok("fjo raw package list-package-files");
    let files = files.json();
    let file = &files[0];
    assert_eq!(file["name"], "payload.txt", "the file listing names the wrong file: {files}");
    assert_eq!(
        file["Size"].as_u64(),
        Some(body.len() as u64),
        "the stored size does not match what was uploaded, so the PUT body was mangled: {files}"
    );
    assert!(
        file["sha256"].as_str().is_some_and(|s| s.len() == 64),
        "a package file must carry a sha256 digest: {files}"
    );

    inst.fjo(["raw", "package", "link-package", &inst.user, "generic", &pkg, &repo.name])
        .assert_ok("fjo raw package link-package");
    // Read back out of band: `link-package` answers with nothing, so its own exit code is not
    // evidence that the server attached anything.
    let (code, linked) =
        inst.api("GET", &format!("packages/{}/generic/{pkg}/1.0.0", inst.user), None);
    assert_eq!(code, 200, "{linked}");
    let linked: serde_json::Value = serde_json::from_str(&linked).expect("a package");
    assert_eq!(
        linked["repository"]["full_name"].as_str(),
        Some(repo.slug().as_str()),
        "link-package reported success but the package is not attached to the repository"
    );

    inst.fjo(["raw", "package", "unlink-package", &inst.user, "generic", &pkg])
        .assert_ok("fjo raw package unlink-package");
    let (_, unlinked) =
        inst.api("GET", &format!("packages/{}/generic/{pkg}/1.0.0", inst.user), None);
    let unlinked: serde_json::Value = serde_json::from_str(&unlinked).expect("a package");
    assert!(
        unlinked["repository"].is_null(),
        "unlink-package reported success but the link is still there: {unlinked}"
    );

    inst.fjo(["raw", "package", "delete-package", &inst.user, "generic", &pkg, "1.0.0"])
        .assert_ok("fjo raw package delete-package");
    let (code, _) = inst.api("GET", &format!("packages/{}/generic/{pkg}/1.0.0", inst.user), None);
    assert_eq!(code, 404, "the package version is still readable after delete-package");
}

/// The package porcelain's whole value is the inference: `fjo package view NAME` works out the
/// owner, the registry type and the version for you.
///
/// That inference is three extra round trips a mock decides the answers to. Here it is driven
/// against a registry whose contents the test put there, so an inference that picks the wrong
/// package shows up as the wrong version rather than as a passing test.
///
/// Two versions are uploaded deliberately: with only one, "infer the version" and "take the
/// only thing you found" are indistinguishable, and the naming below asserts which happened.
#[test]
fn the_package_porcelain_infers_owner_type_and_version_from_the_registry() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["package list", "package view", "package files", "package delete"],
        hits: ["listPackages", "getPackage", "listPackageFiles", "deletePackage"],
    );

    let pkg = unique_package_name(inst, "porcpkg");
    upload_generic_package(inst, &inst.user, &pkg, "1.0.0", "one.txt", "first\n");
    upload_generic_package(inst, &inst.user, &pkg, "2.0.0", "two.txt", "second payload\n");

    let listed = inst.fjo(["package", "list", "--limit", "200", "--json", "name,version"]);
    listed.assert_ok("fjo package list");
    let rows = listed.json();
    let mine: Vec<&serde_json::Value> = rows
        .as_array()
        .expect("package list --json is an array")
        .iter()
        .filter(|r| r["name"].as_str() == Some(pkg.as_str()))
        .collect();
    assert_eq!(mine.len(), 2, "both uploaded versions should be listed: {rows}");

    // An explicit version and type: this is the path that reaches `getPackage`, where the
    // inferring path resolves everything out of the listing instead.
    let view = inst.fjo([
        "package",
        "view",
        &pkg,
        "2.0.0",
        "--type",
        "generic",
        "--owner",
        &inst.user,
        "--json",
        "name,version,type",
    ]);
    view.assert_ok("fjo package view with an explicit version");
    let v = view.json();
    assert_eq!(v["version"], "2.0.0", "view resolved the wrong version: {v}");
    assert_eq!(v["type"], "generic");

    let files = inst.fjo(["package", "files", &pkg, "2.0.0", "--json", "name,Size"]);
    files.assert_ok("fjo package files");
    let files = files.json();
    assert_eq!(
        files[0]["name"], "two.txt",
        "`package files` resolved the wrong version's files: {files}"
    );

    inst.fjo(["package", "delete", &pkg, "1.0.0", "--yes"]).assert_ok("fjo package delete");
    // Out of band, and asserting on *both* versions: a delete that removed the whole package
    // rather than the named version would still leave 1.0.0 gone.
    let (gone, _) = inst.api("GET", &format!("packages/{}/generic/{pkg}/1.0.0", inst.user), None);
    let (kept, _) = inst.api("GET", &format!("packages/{}/generic/{pkg}/2.0.0", inst.user), None);
    assert_eq!(gone, 404, "`package delete 1.0.0` left the version behind");
    assert_eq!(kept, 200, "`package delete 1.0.0` took 2.0.0 with it");

    let _ = inst.api("DELETE", &format!("packages/{}/generic/{pkg}/2.0.0", inst.user), None);
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/// The four `/settings/*` documents, each checked for a field the spec says is there.
///
/// These are the endpoints whose whole content is the server's own configuration, so a mock
/// test of them is a tautology. The specific risk they guard is a field being renamed or moved
/// by a Forgejo release: `max_response_items` in particular is what every `--limit` in `fjo` is
/// silently clamped to, so losing it degrades pagination everywhere without an error.
#[test]
fn the_four_settings_documents_carry_the_fields_the_spec_names() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "getGeneralAPISettings",
        "getGeneralAttachmentSettings",
        "getGeneralRepositorySettings",
        "getGeneralUISettings",
    ]);

    let api = inst.fjo(["raw", "settings", "get-general-api-settings"]);
    api.assert_ok("fjo raw settings get-general-api-settings");
    let api = api.json();
    assert!(
        api["max_response_items"].as_u64().is_some_and(|n| n > 0),
        "every --limit is clamped to max_response_items, so it must be a positive number: {api}"
    );
    assert!(api["default_paging_num"].as_u64().is_some(), "{api}");

    let att = inst.fjo(["raw", "settings", "get-general-attachment-settings"]);
    att.assert_ok("fjo raw settings get-general-attachment-settings");
    let att = att.json();
    assert!(att["enabled"].is_boolean(), "attachment settings must say whether they are on: {att}");
    assert!(
        att["allowed_types"].as_str().is_some_and(|s| s.contains(".png")),
        "the default allow-list should mention a common type: {att}"
    );

    let repo = inst.fjo(["raw", "settings", "get-general-repository-settings"]);
    repo.assert_ok("fjo raw settings get-general-repository-settings");
    let repo = repo.json();
    assert!(repo["mirrors_disabled"].is_boolean(), "{repo}");
    assert!(repo["http_git_disabled"].is_boolean(), "{repo}");

    let ui = inst.fjo(["raw", "settings", "get-general-ui-settings"]);
    ui.assert_ok("fjo raw settings get-general-ui-settings");
    let ui = ui.json();
    assert!(
        ui["default_theme"].as_str().is_some_and(|s| !s.is_empty()),
        "the UI settings must name a default theme: {ui}"
    );
    assert!(
        ui["allowed_reactions"].as_array().is_some_and(|a| !a.is_empty()),
        "the reaction list is what `fjo issue react` validates against: {ui}"
    );
}

/// `fjo nodeinfo limits` exists so a user can find the page-size ceiling without reading the
/// swagger. It must report the server's number, not one baked into `fjo`.
///
/// Cross-checked against the raw document out of band: the porcelain formats
/// `default_max_blob_size` as `10 MiB`, and a formatter that ignored its input entirely would
/// still print something plausible.
#[test]
fn nodeinfo_limits_reports_the_servers_own_page_size_ceiling() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["nodeinfo limits"], hits: ["getGeneralAPISettings"]);

    let (code, raw) = inst.api("GET", "settings/api", None);
    assert_eq!(code, 200, "{raw}");
    let raw: serde_json::Value = serde_json::from_str(&raw).expect("the API settings document");
    let ceiling = raw["max_response_items"].as_u64().expect("a numeric max_response_items");

    let run = inst.fjo(["nodeinfo", "limits"]);
    run.assert_ok("fjo nodeinfo limits");
    run.assert_says("max_response_items");
    run.assert_says(&ceiling.to_string());
}

// ---------------------------------------------------------------------------------------------
// Templates, version, signing keys, markup
// ---------------------------------------------------------------------------------------------

/// Each template catalogue lists names, and each listed name must be fetchable.
///
/// The bug this prevents is a listing whose entries cannot be used: `/licenses` returns objects
/// with a `key`, `/gitignore/templates` returns bare strings, and `/label/templates` returns
/// bare strings too. Fetching an entry taken *from the listing* is what proves the two halves
/// agree — a mock would have used whatever identifier the test author guessed.
#[test]
fn every_template_catalogue_lists_names_that_can_be_fetched_back_by_name() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "listGitignoresTemplates",
        "getGitignoreTemplateInfo",
        "listLicenseTemplates",
        "getLicenseTemplateInfo",
        "listLabelTemplates",
        "getLabelTemplateInfo",
    ]);

    let gitignores = inst.fjo(["raw", "misc", "list-gitignores-templates"]);
    gitignores.assert_ok("fjo raw misc list-gitignores-templates");
    let gitignores = gitignores.json();
    let names: Vec<&str> =
        gitignores.as_array().expect("an array").iter().filter_map(|v| v.as_str()).collect();
    assert!(
        names.contains(&"Rust"),
        "a Forgejo ships hundreds of gitignore templates and Rust is one of them: {names:?}"
    );
    let one = inst.fjo(["raw", "misc", "get-gitignore-template-info", "Rust"]);
    one.assert_ok("fjo raw misc get-gitignore-template-info");
    let one = one.json();
    assert_eq!(one["name"], "Rust", "the fetched template is not the one asked for: {one}");
    assert!(
        one["source"].as_str().is_some_and(|s| s.contains("target")),
        "a gitignore template must carry its own text: {one}"
    );

    let licenses = inst.fjo(["raw", "misc", "list-license-templates", "--limit", "500"]);
    licenses.assert_ok("fjo raw misc list-license-templates");
    let licenses = licenses.json();
    let key = licenses
        .as_array()
        .expect("an array")
        .iter()
        .find_map(|l| l["key"].as_str())
        .expect("the licence catalogue is never empty")
        .to_owned();
    let one = inst.fjo(["raw", "misc", "get-license-template-info", &key]);
    one.assert_ok("fjo raw misc get-license-template-info");
    let one = one.json();
    assert_eq!(one["key"], key.as_str(), "the fetched licence is not the one listed: {one}");
    assert!(
        one["body"].as_str().is_some_and(|b| !b.is_empty()),
        "a licence template must carry its text: {one}"
    );

    let labels = inst.fjo(["raw", "misc", "list-label-templates"]);
    labels.assert_ok("fjo raw misc list-label-templates");
    let labels = labels.json();
    let set = labels
        .as_array()
        .expect("an array")
        .iter()
        .find_map(serde_json::Value::as_str)
        .expect("at least one label template")
        .to_owned();
    let one = inst.fjo(["raw", "misc", "get-label-template-info", &set]);
    one.assert_ok("fjo raw misc get-label-template-info");
    let one = one.json();
    let first = &one[0];
    assert!(
        first["name"].as_str().is_some_and(|n| !n.is_empty()),
        "a label template entry must have a name: {one}"
    );
    assert!(
        first["color"].as_str().is_some_and(|c| c.len() == 6),
        "Forgejo returns label colours as bare six-digit hex, with no leading '#': {one}"
    );
}

/// `/version`, `/signing-key.gpg` and `/signing-key.ssh` — three operations that between them
/// exercise every non-JSON exit in `crates/fjo/src/raw.rs`.
///
/// `getSigningKey` is `Produces::Text`, so its output is written through the plain-text path
/// rather than the JSON one; a default container has no GPG signing key, so the correct answer
/// is 200 with an empty body, and asserting "exit 0 and nothing that looks like an error" is
/// the honest assertion there.
///
/// `getSSHSigningKey` is the interesting one. On this container it **404s** — and the handler
/// really does run (`misc.SSHSigningKey`), it simply has no key to hand back. That is a
/// documented answer from a registered route, so the assertion is on the exit code rather than
/// on the server's wording, per this repository's rule about error strings.
#[test]
fn the_version_and_signing_key_endpoints_answer_outside_json() {
    let inst = instance_or_skip!();
    cover!(raw: ["getVersion", "getSigningKey", "getSSHSigningKey"]);

    let version = inst.fjo(["raw", "misc", "get-version"]);
    version.assert_ok("fjo raw misc get-version");
    let version = version.json();
    assert!(
        version["version"].as_str().is_some_and(|v| v.contains('.')),
        "the version endpoint must report a dotted version: {version}"
    );

    let gpg = inst.fjo(["raw", "misc", "get-signing-key"]);
    gpg.assert_ok("fjo raw misc get-signing-key");
    assert!(
        gpg.stdout.trim().is_empty() || gpg.stdout.contains("BEGIN PGP PUBLIC KEY"),
        "signing-key.gpg is plain text: either empty (no key configured, the default) or an \
         ASCII-armoured block. Got: {:?}",
        gpg.stdout
    );

    let ssh = inst.fjo(["raw", "misc", "get-ssh-signing-key"]);
    assert!(
        !ssh.ok(),
        "a container with no SSH signing key must refuse this, not answer it:\n{}\n{}",
        ssh.stdout,
        ssh.stderr
    );
    ssh.assert_code(5, "fjo raw misc get-ssh-signing-key without a configured key");
    ssh.assert_says("/signing-key.ssh");
}

/// The three markup renderers, each asked to turn a heading into HTML.
///
/// # Why `render-markdown-raw` is asserted more weakly than the other two
///
/// Its request body is `text/plain`, and `fjo raw` has no way to send one. `--body-file` runs
/// every body through `serde_json::from_str` (`crates/fjo-raw/src/bodyfile.rs`), the plan
/// carries a `serde_json::Value`, and the serializer emits JSON regardless of the declared
/// content type. So the only body reachable from the command line is a **JSON-encoded string**:
/// `--body-file` holding `"# x"` sends the five bytes `"# x"` — quotes included — under
/// `content-type: text/plain`, and Forgejo faithfully renders the quotes.
///
/// That is a real defect, found here and not by any mock: a `FakeTransport` test asserts that
/// the body is the `Value` we built, which it is. The test therefore asserts what is true today
/// — the endpoint answers with HTML containing the payload — and this comment is the record of
/// what it *should* assert once a plain-text body can be sent: `# x` becoming an `<h1>`, exactly
/// as `render-markdown` does below.
#[test]
fn the_markup_renderers_turn_a_heading_into_real_html() {
    let inst = instance_or_skip!();
    cover!(raw: ["renderMarkdown", "renderMarkup", "renderMarkdownRaw"]);

    let md =
        inst.fjo(["raw", "misc", "render-markdown", "--text", "# hello", "--mode", "markdown"]);
    md.assert_ok("fjo raw misc render-markdown");
    assert!(
        md.stdout.contains("<h1") && md.stdout.contains("hello"),
        "`# hello` must render as an h1, not be echoed back: {:?}",
        md.stdout
    );

    let markup = inst.fjo([
        "raw",
        "misc",
        "render-markup",
        "--text",
        "# heading",
        "--mode",
        "markdown",
        "--context",
        "/",
    ]);
    markup.assert_ok("fjo raw misc render-markup");
    assert!(
        markup.stdout.contains("<h1") && markup.stdout.contains("heading"),
        "render-markup in markdown mode must produce the same h1 render-markdown does: {:?}",
        markup.stdout
    );

    // See the doc comment: a JSON string is the only body shape `fjo raw` can produce, so the
    // assertion is "HTML came back carrying the payload", not "the markdown was interpreted".
    let scratch = std::env::temp_dir().join(format!("fjo-itest-rawmd-{}.json", std::process::id()));
    std::fs::write(&scratch, "\"raw renderer payload\"").expect("write the body file");
    let raw =
        inst.fjo(["raw", "misc", "render-markdown-raw", "--body-file", &scratch.to_string_lossy()]);
    let _ = std::fs::remove_file(&scratch);
    raw.assert_ok("fjo raw misc render-markdown-raw");
    assert!(
        raw.stdout.contains("<p") && raw.stdout.contains("raw renderer payload"),
        "the raw renderer must answer with HTML carrying the text it was given: {:?}",
        raw.stdout
    );
}

/// `/actions/run` is reachable only with the automatic token a workflow job is handed, so an
/// ordinary personal token must be refused — by the handler, with a reason.
///
/// This is worth a live test precisely because it cannot succeed: what is being checked is that
/// `fjo` reaches the right route and surfaces the server's refusal as a permission failure
/// rather than as "no such endpoint". Asserting the exit code and the request path rather than
/// Forgejo's wording keeps it stable across releases.
#[test]
fn the_actions_run_endpoint_refuses_an_ordinary_token_as_a_permission_failure() {
    let inst = instance_or_skip!();
    cover!(raw: ["getActionsRun"]);

    let run = inst.fjo(["raw", "misc", "get-actions-run"]);
    assert!(
        !run.ok(),
        "a personal access token must not be able to read a workflow run:\n{}\n{}",
        run.stdout,
        run.stderr
    );
    run.assert_code(1, "fjo raw misc get-actions-run with a personal token");
    run.assert_says("/actions/run");
}

// ---------------------------------------------------------------------------------------------
// Topics
// ---------------------------------------------------------------------------------------------

/// A topic attached to a repository must be findable through the instance-wide topic search,
/// through both `fjo raw topic search` and `fjo search topics`.
///
/// The topic is attached out of band, so this tests the *search*, not the writing. `repo_count`
/// is the assertion that matters: the search index is populated asynchronously in some Forgejo
/// configurations, and a search that returned a topic with no repositories behind it would mean
/// the index and the repository disagree.
#[test]
fn a_topic_on_a_repository_is_findable_through_the_instance_wide_topic_search() {
    let inst = instance_or_skip!();
    cover!(raw: ["topicSearch"]);
    cover!(porcelain: ["search topics"], hits: ["topicSearch"]);

    let repo = TestRepo::create(inst, "topicsearch");
    // Forgejo topics are lowercase, and this one has to be unique across the instance so a
    // parallel test's topic cannot satisfy the assertions below.
    let topic = format!("fjoitest{}t", std::process::id());
    let (code, body) = repo.api("PUT", &format!("topics/{topic}"), None);
    assert!((200..300).contains(&code), "could not attach the topic: HTTP {code}: {body}");

    let raw = inst.fjo(["raw", "topic", "search", "--q", &topic]);
    raw.assert_ok("fjo raw topic search");
    let found = raw.json();
    let hit = found["topics"]
        .as_array()
        .expect("topicSearch answers with a `topics` array")
        .iter()
        .find(|t| t["topic_name"].as_str() == Some(topic.as_str()))
        .unwrap_or_else(|| panic!("the topic just attached is not in the search results: {found}"));
    assert_eq!(
        hit["repo_count"].as_u64(),
        Some(1),
        "the search found the topic but claims no repository carries it: {found}"
    );

    let porcelain = inst.fjo(["search", "topics", &topic, "--json", "topic_name,repo_count"]);
    porcelain.assert_ok("fjo search topics");
    let rows = porcelain.json();
    assert!(
        rows.as_array()
            .expect("search topics --json is an array")
            .iter()
            .any(|t| t["topic_name"].as_str() == Some(topic.as_str())),
        "the porcelain search did not find the topic the raw search did: {rows}"
    );
}

// ---------------------------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------------------------

/// All seven notification operations, walked as one story: somebody else files issues, the
/// inbox fills, one thread is read individually, the rest of the repository is marked read, and
/// finally the account-wide mark clears what is left.
///
/// # Why this is one test rather than seven
///
/// `notifyReadList` is `PUT /notifications` with no scope — it marks **every** unread thread on
/// the account. Two tests doing that in parallel against the shared instance would each destroy
/// the other's fixture. Keeping the whole flow in one test makes the account-wide mark the last
/// thing that happens, with nothing left to race.
///
/// # Why a second account
///
/// Forgejo does not notify you about your own actions, so an admin-only test of this would be a
/// test of an empty list. The repository is made public so the second account can file issues
/// in it without a collaborator grant.
///
/// # Why the polling
///
/// Notifications are written from a queue, so they are not there the instant the issue is
/// created — measured at over a second on an idle container. A fixed sleep is either flaky or
/// slow; polling is neither.
#[test]
fn the_notification_endpoints_walk_a_thread_from_unread_to_read() {
    const THREADS: usize = 3;

    let inst = instance_or_skip!();
    cover!(raw: [
        "notifyGetList",
        "notifyGetRepoList",
        "notifyGetThread",
        "notifyNewAvailable",
        "notifyReadList",
        "notifyReadRepoList",
        "notifyReadThread",
    ]);

    let reporter = inst.scoped_user("miscnotifier", &["all"]).expect(
        "a second account is needed to fill the inbox: Forgejo never notifies you \
                 about your own actions",
    );
    let repo = TestRepo::create_initialized(inst, "notify");
    repo.api("PATCH", "", Some(r#"{"private":false}"#));

    for i in 1..=THREADS {
        let (code, body) = inst.api_as(
            &reporter.token,
            "POST",
            &format!("repos/{}/issues", repo.slug()),
            Some(&format!(r#"{{"title":"notify fixture {i}"}}"#)),
        );
        assert!((200..300).contains(&code), "seeding issue {i} failed: HTTP {code}: {body}");
    }

    let repo_threads = || -> Vec<serde_json::Value> {
        let run = inst.fjo([
            "raw",
            "notify",
            "get-repo-list",
            &repo.owner,
            &repo.name,
            "--status-types",
            "unread",
        ]);
        run.assert_ok("fjo raw notify get-repo-list");
        run.json().as_array().cloned().unwrap_or_default()
    };

    let mut waited = 0;
    while repo_threads().len() < THREADS && waited < 60 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        waited += 1;
    }
    let threads = repo_threads();
    assert_eq!(
        threads.len(),
        THREADS,
        "the instance never delivered {THREADS} notifications, so everything below would have \
         been asserted against an empty inbox"
    );
    assert!(
        threads.iter().all(|t| t["subject"]["type"] == "Issue"),
        "every seeded thread is about an issue: {threads:?}"
    );

    let available = inst.fjo(["raw", "notify", "new-available"]);
    available.assert_ok("fjo raw notify new-available");
    assert!(
        available.json()["new"].as_u64().is_some_and(|n| n >= THREADS as u64),
        "notifications/new must count at least the {THREADS} threads just seen: {}",
        available.stdout
    );

    let global =
        inst.fjo(["raw", "notify", "get-list", "--status-types", "unread", "--limit", "100"]);
    global.assert_ok("fjo raw notify get-list");
    let global = global.json();
    let ids: Vec<i64> =
        global.as_array().expect("an array").iter().filter_map(|t| t["id"].as_i64()).collect();
    for t in &threads {
        let id = t["id"].as_i64().expect("a thread id");
        assert!(ids.contains(&id), "thread {id} is in the repository list but not the global one");
    }

    let first = threads[0]["id"].as_i64().expect("a thread id").to_string();
    let thread = inst.fjo(["raw", "notify", "get-thread", &first]);
    thread.assert_ok("fjo raw notify get-thread");
    let thread = thread.json();
    assert_eq!(thread["unread"], serde_json::Value::Bool(true), "a fresh thread is unread");
    assert_eq!(
        thread["repository"]["full_name"].as_str(),
        Some(repo.slug().as_str()),
        "get-thread returned a thread belonging to another repository: {thread}"
    );

    inst.fjo(["raw", "notify", "read-thread", &first]).assert_ok("fjo raw notify read-thread");
    // Out of band: the PATCH answers with the thread it changed, so reading its own reply back
    // would prove nothing about what was stored.
    let (code, after) = inst.api("GET", &format!("notifications/threads/{first}"), None);
    assert_eq!(code, 200, "{after}");
    let after: serde_json::Value = serde_json::from_str(&after).expect("a thread");
    assert_eq!(
        after["unread"],
        serde_json::Value::Bool(false),
        "read-thread reported success but the thread is still unread: {after}"
    );

    inst.fjo(["raw", "notify", "read-repo-list", &repo.owner, &repo.name])
        .assert_ok("fjo raw notify read-repo-list");
    let (_, left) =
        inst.api("GET", &format!("repos/{}/notifications?status-types=unread", repo.slug()), None);
    let left: serde_json::Value = serde_json::from_str(&left).unwrap_or_default();
    assert_eq!(
        left.as_array().map(Vec::len),
        Some(0),
        "read-repo-list left unread threads behind: {left}"
    );

    // Account-wide, and therefore last. Nothing else in this file creates notifications for the
    // admin — every other test acts as the admin, and Forgejo does not notify you about your
    // own actions — so this cannot pull a fixture out from under a parallel test.
    inst.fjo(["raw", "notify", "read-list"]).assert_ok("fjo raw notify read-list");
    let (_, new) = inst.api("GET", "notifications/new", None);
    let new: serde_json::Value = serde_json::from_str(&new).expect("a counter");
    assert_eq!(
        new["new"].as_u64(),
        Some(0),
        "read-list reported success but unread notifications remain: {new}"
    );
}

// ---------------------------------------------------------------------------------------------
// Mirrors
// ---------------------------------------------------------------------------------------------

/// A remote address that Forgejo will accept but nothing will ever answer.
///
/// Two constraints pull in opposite directions and this is the only value that satisfies both.
///
/// * The obvious choice — another repository on *this* instance — is refused. Forgejo runs
///   `IsMigrateURLAllowed` over the address and `[migrations] ALLOW_LOCALNETWORKS` is false by
///   default, so anything resolving to loopback or an RFC1918 address comes back
///   `401 Permission denied`. That is a 401 about the *mirror target*, which reads exactly like
///   a bad token and sent this test down a long wrong path once already.
/// * A real hostname would need DNS, and the container has no outbound network guarantee.
///
/// An IP **literal** in TEST-NET-2 (RFC 5737, reserved for documentation) is neither: Go's
/// `net.LookupIP` short-circuits a literal without asking a resolver, and the address is global
/// unicast rather than private, so the check passes with no packet leaving the machine.
const UNROUTABLE_MIRROR: &str = "https://198.51.100.10/example/mirror.git";

/// A push mirror survives a round trip: added with options, listed, shown by `status`, removed.
///
/// The options are the part worth checking against a real server. `--interval 8h` has to arrive
/// as Forgejo's `8h0m0s`, and `--branch-filter` as a comma-joined string; a mock would have
/// accepted whatever shape the command sent. Everything is read back with `repo.api`, because
/// `mirror add` answers with the object it created and believing that reply would be believing
/// the code under test.
#[test]
fn a_push_mirror_keeps_its_interval_and_branch_filter_through_a_round_trip() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["mirror add", "mirror list", "mirror status", "mirror delete"],
        hits: ["repoAddPushMirror", "repoListPushMirrors", "repoDeletePushMirror", "repoGet"],
    );

    let repo = TestRepo::create_initialized(inst, "mirror");

    inst.fjo([
        "mirror",
        "add",
        UNROUTABLE_MIRROR,
        "--interval",
        "8h",
        "--branch-filter",
        "main,rel/*",
        "-R",
        &repo.slug(),
    ])
    .assert_ok("fjo mirror add");

    let (code, body) = repo.api("GET", "push_mirrors", None);
    assert_eq!(code, 200, "{body}");
    let mirrors: serde_json::Value = serde_json::from_str(&body).expect("a mirror list");
    let stored = &mirrors[0];
    assert_eq!(stored["remote_address"], UNROUTABLE_MIRROR, "the address was rewritten: {body}");
    assert_eq!(
        stored["interval"], "8h0m0s",
        "`--interval 8h` must reach the server as Forgejo's own duration spelling: {body}"
    );
    assert_eq!(
        stored["branch_filter"], "main,rel/*",
        "the branch filter did not survive the round trip: {body}"
    );
    let remote = stored["remote_name"].as_str().expect("a remote name").to_owned();

    let listed = inst.fjo(["mirror", "list", "-R", &repo.slug(), "--json", "remote_name,interval"]);
    listed.assert_ok("fjo mirror list");
    let listed = listed.json();
    assert_eq!(
        listed[0]["remote_name"].as_str(),
        Some(remote.as_str()),
        "`mirror list` does not show the mirror that was just added: {listed}"
    );

    let status = inst.fjo(["mirror", "status", "-R", &repo.slug()]);
    status.assert_ok("fjo mirror status");
    status.assert_says(&remote);
    status.assert_says("198.51.100.10");

    inst.fjo(["mirror", "delete", &remote, "--yes", "-R", &repo.slug()])
        .assert_ok("fjo mirror delete");
    let (_, after) = repo.api("GET", "push_mirrors", None);
    let after: serde_json::Value = serde_json::from_str(&after).unwrap_or_default();
    assert_eq!(
        after.as_array().map(Vec::len),
        Some(0),
        "`mirror delete` reported success but the mirror is still configured: {after}"
    );
}

/// `fjo mirror sync` decides which of two endpoints to call by reading the repository first,
/// and refuses when neither applies.
///
/// # Why `--push` on a repository with no push mirrors
///
/// `POST …/push_mirrors-sync` is **synchronous** in Forgejo: with a mirror configured it runs
/// the push inline, and against an address nothing answers that is 132 seconds of TCP timeout —
/// measured — which is past this crate's 180s per-test kill and would report as an anonymous
/// hang rather than as anything about mirrors. With no mirrors configured the same endpoint
/// answers in milliseconds, so this drives the real route without buying the stall. What is
/// being tested is the routing decision and the request, which is the part `fjo` owns.
///
/// The second half is the refusal: without a flag, a repository that is neither a pull mirror
/// nor has push mirrors must be told so as a usage error, not sent to an endpoint that would
/// quietly do nothing.
#[test]
fn mirror_sync_reaches_the_push_endpoint_and_refuses_when_there_is_no_mirror_at_all() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["mirror sync"],
        hits: ["repoPushMirrorSync", "repoListPushMirrors", "repoGet"],
    );

    let repo = TestRepo::create_initialized(inst, "mirrorsync");

    // `FJO_FORCE_TTY` because the confirmation is a `porcelain::note`, which is deliberately
    // terminal-only: piped output carries data, not chatter. Without it this test would have to
    // settle for the exit code, and exit 0 alone cannot tell "the POST was made" apart from "the
    // command decided there was nothing to do and said so quietly".
    let forced = inst.fjo_env(
        std::path::Path::new("."),
        &[("FJO_FORCE_TTY", "1")],
        ["mirror", "sync", "--push", "-R", &repo.slug()],
    );
    forced.assert_ok("fjo mirror sync --push");
    forced.assert_says("Queued a push");

    let refused = inst.fjo(["mirror", "sync", "-R", &repo.slug()]);
    refused.assert_code(2, "fjo mirror sync on a repository with no mirrors");
    refused.assert_says("no mirrors to sync");

    let pull_only = inst.fjo(["mirror", "sync", "--pull", "-R", &repo.slug()]);
    pull_only.assert_code(2, "fjo mirror sync --pull on a repository that is not a pull mirror");
    pull_only.assert_says("not a pull mirror");
}

// ---------------------------------------------------------------------------------------------
// Quotas
// ---------------------------------------------------------------------------------------------

/// Deletes a quota rule and group even when an assertion between here and the end panicked.
///
/// Quota rules and groups are **instance-wide**: unlike a repository they have no owner to scope
/// them, so debris from a failed run is visible to every later test's `--all` listing and to
/// every user's `quota status`. `TestRepo` solves the same problem the same way, and for the
/// same reason — the first failure must not make the next run worse.
struct QuotaFixture<'a> {
    inst: &'a Instance,
    rule: String,
    group: String,
}

impl Drop for QuotaFixture<'_> {
    fn drop(&mut self) {
        let _ = self.inst.api(
            "DELETE",
            &format!("admin/quota/groups/{}/rules/{}", self.group, self.rule),
            None,
        );
        let _ = self.inst.api("DELETE", &format!("admin/quota/groups/{}", self.group), None);
        let _ = self.inst.api("DELETE", &format!("admin/quota/rules/{}", self.rule), None);
    }
}

/// The whole `fjo quota` group, driven as one story: make a rule, make a group, attach the rule,
/// put a user in it, read the result back from three different angles, then take it all apart.
///
/// # Why one test for fifteen leaves
///
/// Quota rules and groups are instance-wide, and `quota rules list --all` is an instance-wide
/// listing. Two tests creating rules in parallel would each see the other's, which is fine for
/// a presence assertion and not fine for the interesting one below — that a user who is in the
/// group inherits exactly the rule that was attached to it. Keeping the lifecycle in one test
/// keeps that assertion meaningful.
///
/// # Why the rule is `unlimited` and the member is a throwaway account
///
/// A quota rule takes effect the moment a user is in a group carrying it. Attaching a real size
/// limit to the admin — the account every other test in this file uploads packages and creates
/// repositories with — would make this test able to fail *other* tests, in a way that would look
/// like a package bug. `unlimited` on a dedicated account cannot.
///
/// # What a mock could not have caught
///
/// `--bytes unlimited` has to arrive as `-1`, `--subject` has to arrive as an array under the
/// server's own vocabulary, and a group's rules come back nested inside the group rather than as
/// ids. All three are the server's shape, not ours.
#[test]
fn the_quota_lifecycle_creates_a_rule_attaches_it_to_a_group_and_takes_it_all_apart() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: [
            "quota status",
            "quota rules create",
            "quota rules list",
            "quota rules view",
            "quota rules edit",
            "quota rules delete",
            "quota groups create",
            "quota groups list",
            "quota groups view",
            "quota groups add-rule",
            "quota groups remove-rule",
            "quota groups add-user",
            "quota groups remove-user",
            "quota groups users",
            "quota groups delete",
        ],
        hits: [
            "userGetQuota",
            "adminGetUserQuota",
            "adminCreateQuotaRule",
            "adminListQuotaRules",
            "adminGetQuotaRule",
            "adminEditQuotaRule",
            "adminDeleteQuotaRule",
            "adminCreateQuotaGroup",
            "adminListQuotaGroups",
            "adminGetQuotaGroup",
            "adminAddRuleToQuotaGroup",
            "adminRemoveRuleFromQuotaGroup",
            "adminAddUserToQuotaGroup",
            "adminRemoveUserFromQuotaGroup",
            "adminListUsersInQuotaGroup",
        ],
    );

    let member = inst.scoped_user("miscquota", &["all"]).expect(
        "a throwaway account to put in the quota group: putting the admin in one would \
                 apply a quota to the account every other test in this file writes with",
    );
    let rule = format!("fjoq{}rule", std::process::id());
    let group = format!("fjoq{}group", std::process::id());
    let fixture = QuotaFixture { inst, rule: rule.clone(), group: group.clone() };

    // `fjo quota status` on the admin, who is in no group: the assertion is on the shape of the
    // document the server sends, which is the half a mock decides for itself.
    // Fields named explicitly: a bare `--json` *lists* the available fields rather than emitting
    // a document, which is documented behaviour and not what this assertion wants.
    let status = inst.fjo(["quota", "status", "--json", "used,groups"]);
    status.assert_ok("fjo quota status --json");
    let status = status.json();
    assert!(
        status["used"]["size"]["repos"].is_object(),
        "a quota document reports usage under used.size.repos: {status}"
    );

    inst.fjo([
        "quota",
        "rules",
        "create",
        &rule,
        "--bytes",
        "unlimited",
        "--subject",
        "size:all",
        "--subject",
        "size:assets:packages:all",
    ])
    .assert_ok("fjo quota rules create");

    // Read back out of band. `--bytes unlimited` has to reach the server as -1, and a limit that
    // arrived as 0 would be an accidental "nothing is allowed" that the command would still
    // report as success.
    let (code, body) = inst.api("GET", &format!("admin/quota/rules/{rule}"), None);
    assert_eq!(code, 200, "{body}");
    let stored: serde_json::Value = serde_json::from_str(&body).expect("a quota rule");
    assert_eq!(
        stored["limit"].as_i64(),
        Some(-1),
        "`--bytes unlimited` must reach Forgejo as -1, not 0: {body}"
    );
    let subjects: Vec<&str> = stored["subjects"]
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|s| s.as_str())
        .collect();
    assert!(
        subjects.contains(&"size:all") && subjects.contains(&"size:assets:packages:all"),
        "both repeated --subject values must survive: {body}"
    );

    let rules = inst.fjo(["quota", "rules", "list", "--all", "--json", "name,limit"]);
    rules.assert_ok("fjo quota rules list --all");
    let rules = rules.json();
    assert!(
        rules.as_array().expect("an array").iter().any(|r| r["name"].as_str() == Some(&rule)),
        "the new rule is missing from the instance-wide listing: {rules}"
    );

    // `rules view` emits a one-element array, not a bare object: `--json` renders rows, and a
    // single-row view is still a row. Indexing straight into it would have looked right and read
    // `null` forever.
    let one = inst.fjo(["quota", "rules", "view", &rule, "--json", "name,limit,subjects"]);
    one.assert_ok("fjo quota rules view");
    let one = one.json();
    assert_eq!(
        one[0]["name"].as_str(),
        Some(rule.as_str()),
        "`quota rules view` returned a different rule: {one}"
    );
    assert_eq!(
        one[0]["limit"].as_i64(),
        Some(-1),
        "the viewed limit disagrees with the API: {one}"
    );

    inst.fjo(["quota", "groups", "create", &group]).assert_ok("fjo quota groups create");
    inst.fjo(["quota", "groups", "add-rule", &group, &rule]).assert_ok("fjo quota groups add-rule");

    let groups = inst.fjo(["quota", "groups", "list", "--all", "--json", "name,rules"]);
    groups.assert_ok("fjo quota groups list --all");
    let groups = groups.json();
    let mine = groups
        .as_array()
        .expect("an array")
        .iter()
        .find(|g| g["name"].as_str() == Some(&group))
        .unwrap_or_else(|| panic!("the new group is missing from the listing: {groups}"));
    assert_eq!(
        mine["rules"][0]["name"].as_str(),
        Some(rule.as_str()),
        "Forgejo nests a group's rules inside the group rather than listing ids, and the rule \
         just attached should be there: {groups}"
    );

    inst.fjo(["quota", "groups", "add-user", &group, &member.name])
        .assert_ok("fjo quota groups add-user");
    let users = inst.fjo(["quota", "groups", "users", &group, "--json", "login"]);
    users.assert_ok("fjo quota groups users");
    let users = users.json();
    assert!(
        users
            .as_array()
            .expect("an array")
            .iter()
            .any(|u| u["login"].as_str() == Some(&member.name)),
        "the member was added but is not in the group's user list: {users}"
    );

    // The assertion the whole fixture exists for: membership makes the group's rule apply, which
    // only a real server can decide.
    let applied = inst.fjo(["quota", "status", "--user", &member.name, "--json", "groups"]);
    applied.assert_ok("fjo quota status --user");
    let applied = applied.json();
    let names: Vec<&str> = applied["groups"]
        .as_array()
        .map(|gs| gs.iter().filter_map(|g| g["name"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        names.contains(&group.as_str()),
        "a user in the group must inherit it in their quota status: {applied}"
    );

    inst.fjo(["quota", "rules", "edit", &rule, "--bytes", "1GiB"])
        .assert_ok("fjo quota rules edit");
    let (_, body) = inst.api("GET", &format!("admin/quota/rules/{rule}"), None);
    let edited: serde_json::Value = serde_json::from_str(&body).expect("a quota rule");
    assert_eq!(
        edited["limit"].as_i64(),
        Some(1024 * 1024 * 1024),
        "`--bytes 1GiB` must be sent as bytes, not as the string it was typed as: {body}"
    );
    let subjects_after: Vec<&str> = edited["subjects"]
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|s| s.as_str())
        .collect();
    assert_eq!(
        subjects_after.len(),
        2,
        "editing only the limit must not drop the subjects, which is what a PATCH built from a \
         zero value rather than from the current rule would do: {body}"
    );

    let view = inst.fjo(["quota", "groups", "view", &group]);
    view.assert_ok("fjo quota groups view");
    view.assert_says(&group);

    inst.fjo(["quota", "groups", "remove-user", &group, &member.name])
        .assert_ok("fjo quota groups remove-user");
    let (_, body) = inst.api("GET", &format!("admin/quota/groups/{group}/users"), None);
    let left: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    assert_eq!(
        left.as_array().map(Vec::len),
        Some(0),
        "remove-user reported success but the member is still in the group: {body}"
    );

    inst.fjo(["quota", "groups", "remove-rule", &group, &rule])
        .assert_ok("fjo quota groups remove-rule");
    let (_, body) = inst.api("GET", &format!("admin/quota/groups/{group}"), None);
    let bare: serde_json::Value = serde_json::from_str(&body).expect("a quota group");
    assert!(
        bare["rules"].as_array().is_none_or(Vec::is_empty),
        "remove-rule reported success but the rule is still attached: {body}"
    );

    inst.fjo(["quota", "groups", "delete", &group, "--yes"]).assert_ok("fjo quota groups delete");
    inst.fjo(["quota", "rules", "delete", &rule, "--yes"]).assert_ok("fjo quota rules delete");
    let (group_code, _) = inst.api("GET", &format!("admin/quota/groups/{group}"), None);
    let (rule_code, _) = inst.api("GET", &format!("admin/quota/rules/{rule}"), None);
    assert_eq!(group_code, 404, "the group is still readable after `quota groups delete`");
    assert_eq!(rule_code, 404, "the rule is still readable after `quota rules delete`");

    // Everything is already gone; the guard is only insurance against a panic above.
    drop(fixture);
}

// ---------------------------------------------------------------------------------------------
// ActivityPub and NodeInfo
// ---------------------------------------------------------------------------------------------

/// The two federation reads that a single instance can actually answer, and the five that cannot.
///
/// # Why this test alone uses [`fjo_itest::federated`]
///
/// `getNodeInfo` and `activitypubInstanceActor` are unauthenticated, instance-level documents,
/// and both need `[federation] ENABLED` — **including `/nodeinfo`**, which is easy to assume is
/// always routed and is not: on the shared instance it answers the router's bare `404 page not
/// found`, measured. So neither can run there.
///
/// The switch cannot simply be turned on globally either, for a reason the repo-core suite found:
/// with federation enabled, Forgejo 16.0.4 answers `PUT /user/starred/{owner}/{repo}` with HTTP
/// 500 (`invalid host for HostMatcher`). That would have traded these two reads for the ability
/// to star a repository at all, silently, in whichever unrelated test happened to star one. Hence
/// the second, lazily-booted container — and hence this test paying for it rather than the whole
/// file doing so.
///
/// # What is covered and what is not
///
/// The two instance-level reads are covered. The rest are not, and this test is the evidence:
/// `activitypubPerson`, `activitypubPersonFeed`, `activitypubRepository` and the two activity
/// reads are `GET`s that Forgejo puts behind HTTP signature verification, so a token — any token
/// — is refused. That is a documented answer from a registered handler rather than a routing
/// accident, which is why the assertion is on the exit code and the URL `fjo` built rather than
/// on Forgejo's wording.
///
/// No `cover!` for those five: nothing about the response body was proven, so claiming them would
/// be claiming a round trip that did not happen. `spec/live-coverage.toml` records them with the
/// second-instance topology that would unblock them, and the assertion below is what stops that
/// exemption from rotting — the day a signed request becomes possible, this test fails and says
/// so.
#[test]
fn the_instance_level_federation_documents_answer_and_the_actor_scoped_ones_demand_a_signature() {
    // The `instance_or_skip!` dance, against the federating instance instead of the shared one.
    // Written out rather than wrapped in a helper because the early `return` is the whole point:
    // a skipped test must reach no `cover!`, and a helper cannot return out of its caller.
    let inst = match fjo_itest::federated() {
        Ok(Some(i)) => i,
        Ok(None) => {
            let msg = format!(
                "no federating Forgejo instance available: {}.\n\
                 This test needs [federation] ENABLED, which the shared instance deliberately \
                 does not have — see FEDERATION_ENV in crates/fjo-itest/src/lib.rs.",
                fjo_itest::unavailable_reason()
            );
            if std::env::var_os("FJO_ITEST_REQUIRE").is_some() {
                panic!("FJO_ITEST_REQUIRE is set but {msg}");
            }
            println!("SKIPPED: {msg}");
            eprintln!("SKIPPED: {msg}");
            return;
        }
        Err(e) => panic!("could not obtain a federating Forgejo instance: {e}"),
    };
    cover!(raw: ["getNodeInfo", "activitypubInstanceActor"]);

    let node = inst.fjo(["raw", "misc", "get-node-info"]);
    node.assert_ok("fjo raw misc get-node-info");
    let node = node.json();
    assert_eq!(node["version"], "2.1", "NodeInfo 2.1 is what Forgejo serves here: {node}");
    assert_eq!(
        node["software"]["name"], "forgejo",
        "the discovery document must identify the software: {node}"
    );
    assert!(
        node["protocols"].as_array().is_some_and(|p| p.iter().any(|x| x == "activitypub")),
        "a federating instance advertises activitypub in its protocols: {node}"
    );

    let actor = inst.fjo(["raw", "activitypub", "instance-actor"]);
    actor.assert_ok("fjo raw activitypub instance-actor");
    let actor = actor.json();
    // `Produces::LdJson` sends `Accept: application/ld+json`, and `@context` is the field that
    // proves the server honoured it rather than falling back to plain JSON.
    assert!(
        actor["@context"].as_array().is_some_and(|c| !c.is_empty()),
        "an ActivityPub actor is JSON-LD and must carry an @context: {actor}"
    );
    assert_eq!(actor["type"], "Application", "the instance actor is an Application: {actor}");
    assert!(
        actor["inbox"].as_str().is_some_and(|i| i.ends_with("/activitypub/actor/inbox")),
        "the actor must advertise the inbox the five unreachable POSTs target: {actor}"
    );

    // `user-id` and `repository-id` take database ids; 1 is the admin this harness bootstrapped.
    let signed_only: [(&[&str], &str); 5] = [
        (&["raw", "activitypub", "person", "1"], "/api/v1/activitypub/user-id/1"),
        (&["raw", "activitypub", "person-feed", "1"], "/api/v1/activitypub/user-id/1/outbox"),
        (&["raw", "activitypub", "repository", "1"], "/api/v1/activitypub/repository-id/1"),
        (
            &["raw", "activitypub", "person-activity", "1", "1"],
            "/api/v1/activitypub/user-id/1/activities/1/activity",
        ),
        (
            &["raw", "activitypub", "person-activity-note", "1", "1"],
            "/api/v1/activitypub/user-id/1/activities/1",
        ),
    ];

    for (args, path) in signed_only {
        let run = inst.fjo(args.iter().copied());
        assert!(
            !run.ok(),
            "`fjo {}` succeeded. Forgejo used to demand an HTTP-signed request for this GET, so \
             either that changed or the harness grew a way to sign one — either way, delete the \
             matching entry from spec/live-coverage.toml and add a `cover!`, because the \
             operation is now genuinely testable.\n{}",
            args.join(" "),
            run.stdout
        );
        run.assert_code(1, &format!("fjo {}", args.join(" ")));
        // The request line is `fjo`'s own rendering of the URL it built, so asserting on it
        // checks path substitution against the server rather than against our own spec reading.
        run.assert_says(path);
    }
}
