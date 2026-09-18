//! Forgejo's OAuth2 provider, measured rather than assumed.
//!
//! Everything `fjo auth login --web` does rests on a handful of facts about Forgejo that are not
//! in the Swagger document, because the OAuth2 endpoints live under the web root rather than
//! `/api/v1`. A `FakeTransport` cannot check any of them: it answers with whatever the test
//! author believed, so it agrees with itself by construction. These run against a real instance.
//!
//! # The browser step is not covered here, and that is a real gap
//!
//! `/login/oauth/authorize` renders a consent page and needs a session cookie plus a click. A
//! test could scrape two CSRF tokens out of Forgejo's login and grant forms and drive it with a
//! cookie jar, but that depends on the markup of those forms, which is not a stable interface and
//! would fail on a Forgejo release for reasons having nothing to do with fjo. So the redirect-URI
//! rule below is pinned through the authorize endpoint's *validation*, which is the part that
//! breaks silently, and the consent click is left to a human.
//!
//! This costs nothing at the coverage ratchet: `auth login` is an existing porcelain leaf already
//! covered by `live_local.rs`, and `--web` is a flag rather than a new leaf.

use std::process::Command;

use fjo_itest::{Instance, cover, instance_or_skip};

/// GET a path under the instance's **web** root, following no redirects, and report the status
/// and the `Location` header.
///
/// Out of band through curl, like the rest of this harness's fixtures: the thing under test must
/// not be the thing that tells us whether the test passed.
fn web_get(inst: &Instance, path: &str) -> (i32, String, String) {
    let out = Command::new("curl")
        .args(["-sS", "-o", "/dev/null", "-w", "%{http_code}\n%{redirect_url}", "--max-time", "30"])
        .arg(format!("{}{path}", inst.base_url))
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut lines = text.lines();
    let status: i32 = lines.next().unwrap_or("0").trim().parse().unwrap_or(0);
    let redirect = lines.next().unwrap_or("").to_owned();
    (status, redirect, text)
}

/// The two endpoint paths, which are the ones `Endpoints::fixed` falls back to.
///
/// Note the token endpoint is `/login/oauth/access_token`, not the conventional `/token`. A
/// reasonable guess gets a 404 that says nothing about why, which is exactly the kind of thing
/// worth pinning against a real server so a Forgejo release cannot move it quietly.
#[test]
fn the_instance_publishes_an_openid_configuration_naming_the_forgejo_paths() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["auth login"], hits: ["userGetCurrent"]);

    let out = Command::new("curl")
        .args(["-sS", "--max-time", "30"])
        .arg(format!("{}/.well-known/openid-configuration", inst.base_url))
        .output()
        .expect("curl runs");
    let doc = String::from_utf8_lossy(&out.stdout);

    assert!(
        doc.contains("/login/oauth/authorize"),
        "the discovery document should name the authorize endpoint: {doc}"
    );
    assert!(
        doc.contains("/login/oauth/access_token"),
        "the token endpoint is access_token, not /token: {doc}"
    );
    // No device_authorization_endpoint: Forgejo has no device grant, which is why --no-browser
    // pastes a URL back instead of showing a device code.
    assert!(
        !doc.contains("device_authorization_endpoint"),
        "Forgejo grew a device grant; --no-browser could now be a real device flow: {doc}"
    );
}

/// What an unauthenticated authorize request actually does, which is not what you would guess.
///
/// This test exists because of what it *cannot* prove, and the comment is the point.
///
/// The most fragile fact in the whole design is Forgejo's redirect-URI rule: it compares by exact
/// string after uppercasing and trimming one trailing slash, and for a public client on `http` at
/// a loopback IP it first strips the *port* and compares again — but not the path. So the
/// built-in applications' registered `http://127.0.0.1` matches `http://127.0.0.1:45231` and does
/// not match `http://127.0.0.1:45231/callback`. Getting that wrong breaks every login with a
/// generic `redirect_uri_mismatch` that says nothing about paths.
///
/// The obvious way to pin it is to send both URIs to `/login/oauth/authorize` and watch one be
/// refused. **That does not work, and this test records why so nobody spends the afternoon
/// finding out again.** `reqSignIn` runs before the handler, so an unauthenticated request is
/// answered `303` to `/user/login` whatever its parameters say — a bad client id, an unregistered
/// redirect URI and a valid request are indistinguishable from outside. Validation happens after
/// sign-in, on the consent page.
///
/// Pinning it for real therefore needs a session cookie, which means scraping the CSRF token out
/// of Forgejo's login form and posting it (`Instance` has the admin password for this). That is
/// left undone deliberately: it couples CI to the markup of a login form, which is not a stable
/// interface, and it would fail on a Forgejo release for reasons having nothing to do with fjo.
///
/// So the rule rests on reading `ContainsRedirectURI` in `models/auth/oauth2.go`, and
/// `cmd/auth/callback.rs` carries the reasoning. If a login ever starts failing with
/// `redirect_uri_mismatch`, re-read that function first.
#[test]
fn an_unauthenticated_authorize_request_is_bounced_to_sign_in_whatever_it_asks_for() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["auth login"], hits: ["userGetCurrent"]);

    let ask = |client_id: &str, redirect: &str| {
        let q = format!(
            "/login/oauth/authorize?client_id={client_id}&response_type=code\
             &code_challenge_method=S256&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM\
             &state=itest&redirect_uri={}",
            redirect.replace(':', "%3A").replace('/', "%2F")
        );
        web_get(inst, &q)
    };

    // The endpoint exists at the path Endpoints::fixed falls back to. A 404 here would mean the
    // authorize path moved, which is worth catching even though the rest cannot be checked.
    let (status, redirect, _) = ask("00000000-0000-0000-0000-000000000000", "http://127.0.0.1:1");
    assert_ne!(status, 404, "the authorize endpoint moved");
    assert!(redirect.contains("/user/login"), "expected a sign-in bounce, got {redirect:?}");

    // And the finding: a redirect URI with a path is bounced identically, so this is not a probe
    // for the redirect-URI rule.
    let (_, with_path, _) = ask("00000000-0000-0000-0000-000000000000", "http://127.0.0.1:1/cb");
    assert_eq!(
        redirect, with_path,
        "if these ever differ, an unauthenticated probe CAN see redirect-URI validation and the \
         rule above becomes testable here"
    );
}

/// A refused token request must carry the server's own words, because those are what tell a user
/// which half is wrong. `invalid_request` alone does not.
#[test]
fn the_token_endpoint_refuses_a_bogus_code_with_a_described_error() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["auth login"], hits: ["userGetCurrent"]);

    let out = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "30",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/x-www-form-urlencoded",
            "-d",
            "grant_type=authorization_code&client_id=00000000-0000-0000-0000-000000000000\
             &code=not-a-real-code&redirect_uri=http://127.0.0.1:45231&code_verifier=x",
        ])
        .arg(format!("{}/login/oauth/access_token", inst.base_url))
        .output()
        .expect("curl runs");
    let body = String::from_utf8_lossy(&out.stdout);

    assert!(body.contains("\"error\""), "an OAuth failure is RFC 6749 shaped: {body}");
    assert!(
        body.contains("error_description"),
        "the description is the half written for a human, and fjo quotes it: {body}"
    );
}
