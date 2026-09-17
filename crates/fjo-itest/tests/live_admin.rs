//! `fjo admin` against a real Forgejo.
//!
//! Instance administration is the group where a `FakeTransport` proves the least. Almost every
//! command here is a single call, so a mock confirms only that we built the URL we meant to
//! build — never that Forgejo routes it, never that it enforces the rule we think it enforces,
//! and never that the side effect actually happened. Three classes of failure live here and
//! none of them are reachable without a server:
//!
//! * **Bodies the server reinterprets.** `POST /admin/quota/groups` takes whole rule
//!   *definitions*, so `fjo` has to send a limit for a rule the user named by `--rule` and sends
//!   `0`. Whether that redefines the existing rule — setting it to "nothing may be stored" for
//!   every group already using it — is Forgejo's decision alone. See
//!   [`naming_an_existing_rule_when_creating_a_group_attaches_it_without_redefining_it`].
//! * **Server-side rules with no client-side shadow.** "A primary email cannot be deleted",
//!   "an account that still owns repositories cannot be deleted without `--purge`", and
//!   "`read:admin` reads but does not write" are all decided by Forgejo. A mock decides them
//!   for us, which is the opposite of a test.
//! * **Asymmetric endpoint pairs.** `POST /admin/hooks` and `GET /admin/hooks` do not describe
//!   the same set — see [`a_webhook_survives_a_round_trip_but_never_reaches_the_admin_listing`].
//!   Only the server knows that.
//!
//! # Sharing one instance with eight other test binaries
//!
//! Everything here is instance-wide by nature, so no assertion counts rows. Each test names its
//! own account, organization, repository or runner via [`fjo_itest::Instance::unique_repo_name`]
//! and asserts on *that* name being present or absent. A test that asserted "there are three
//! organizations" would fail the moment another file created one.
//!
//! Two things here are genuinely instance-wide. `admin cron run` runs a task chosen so that
//! sharing is safe — see [`running_the_update_checker_advances_its_execution_count`]. Quota
//! rules and groups are global and a group *enforces* a storage limit on its members, so every
//! limit used here is larger than anything this suite stores and the only accounts ever enrolled
//! are throwaways; the harness admin, which every other test binary acts as, is never put into
//! a group. Rules and groups clean themselves up through [`QuotaRule`] and [`QuotaGroup`] even
//! when an assertion panics.

use fjo_itest::{Instance, TestRepo, cover, instance_or_skip};

// --------------------------------------------------------------------------------- fixtures

/// An account created over the API that purges itself on drop.
///
/// Over the API rather than through `fjo admin user create` on purpose: a fixture built out of
/// the command under test cannot be used to test that command, and half the tests below need an
/// account they did not have to trust `fjo` to make. This is the same bargain `TestRepo` strikes.
struct TempUser<'a> {
    inst: &'a Instance,
    name: String,
}

impl<'a> TempUser<'a> {
    fn create(inst: &'a Instance, prefix: &str) -> Self {
        let name = inst.unique_repo_name(prefix);
        let (code, body) = inst.api(
            "POST",
            "admin/users",
            Some(&format!(
                r#"{{"username":"{name}","email":"{name}@example.invalid","password":"fjo-itest-admin-pass-1","must_change_password":false}}"#
            )),
        );
        assert!(
            (200..300).contains(&code),
            "could not create the account {name}: HTTP {code}: {body}"
        );
        Self { inst, name }
    }

    fn email(&self) -> String {
        format!("{}@example.invalid", self.name)
    }

    /// What the server currently says about this account, read out of band.
    fn read(&self) -> (i32, serde_json::Value) {
        let (code, body) = self.inst.api("GET", &format!("users/{}", self.name), None);
        (code, serde_json::from_str(&body).unwrap_or(serde_json::Value::Null))
    }
}

impl Drop for TempUser<'_> {
    fn drop(&mut self) {
        // `purge=true` because a test may have left repositories on the account, and a delete
        // that 422s would leave the account behind for every later run to trip over.
        let _ = self.inst.api("DELETE", &format!("admin/users/{}?purge=true", self.name), None);
    }
}

/// Real ed25519 public keys, one per test that needs one.
///
/// Forgejo refuses a key already registered anywhere on the instance, so two tests sharing a
/// constant would fail whichever ran second — and only when run in parallel, which is the worst
/// possible shape for a flake. Generated once with `ssh-keygen -t ed25519`; the private halves
/// were discarded because nothing here ever authenticates with them.
const SSH_KEY_ADMIN_CREATE: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILh+AqIc4PQsBzxA8nLc7jCM3wrIOPXQOchTZOx89Oqn fjo-itest-admin-1@example.invalid";

/// Read one account's SSH keys out of band.
fn keys_of(inst: &Instance, user: &str) -> serde_json::Value {
    let (code, body) = inst.api("GET", &format!("users/{user}/keys"), None);
    assert_eq!(code, 200, "listing {user}'s keys: {body}");
    serde_json::from_str(&body).expect("an array of keys")
}

// ------------------------------------------------------------------------------- accounts

/// `admin user create` and `admin user edit` each send one object, and every field in it is a
/// separate chance to send the wrong thing under the right name. The edit is the dangerous half:
/// it is a `PATCH` built from whichever flags were given, so a flag wired to the wrong key
/// silently changes a *different* setting — and the command still prints the account and exits 0.
///
/// So each named setting is read back from the server, and so is one that was **not** named:
/// `visibility` must still be `public` after an edit that never mentioned it. A `PATCH` that
/// serialised its whole struct would reset it, and nothing else in the output would say so.
///
/// `--no-must-change-password` is on the create because the default leaves the account unable to
/// do anything until it picks a new password at a web sign-in, which no test can drive.
#[test]
fn creating_an_account_and_editing_it_changes_exactly_the_settings_named() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["admin user create", "admin user edit", "admin user list", "admin user delete"],
        hits: ["adminCreateUser", "adminEditUser", "adminSearchUsers", "adminDeleteUser"],
    );

    let name = inst.unique_repo_name("adm-crud");
    let email = format!("{name}@example.invalid");

    inst.fjo([
        "admin",
        "user",
        "create",
        &name,
        "--email",
        &email,
        "--password",
        "fjo-itest-admin-pass-1",
        "--no-must-change-password",
        "--full-name",
        "Before The Edit",
    ])
    .assert_ok("fjo admin user create");

    let (code, before) = inst.api("GET", &format!("users/{name}"), None);
    assert_eq!(code, 200, "the account should exist after create: {before}");
    let before: serde_json::Value = serde_json::from_str(&before).expect("a user");
    assert_eq!(before["full_name"], "Before The Edit", "--full-name did not reach the server");
    assert_eq!(before["email"], email.as_str(), "--email did not reach the server");
    assert_eq!(before["is_admin"], false, "the account must not be an administrator by default");
    assert_eq!(before["active"], true, "a freshly created account must be able to sign in");
    assert_eq!(before["visibility"], "public");

    // The account has to be findable through the command as well as through the API, and
    // `--paginate` is load-bearing: other test binaries create accounts on this same instance,
    // so a new login can easily land past the first page of a default listing.
    let listed = inst.fjo(["admin", "user", "list", "--paginate", "--json", "login"]);
    listed.assert_ok("fjo admin user list");
    let logins: Vec<String> = listed
        .json()
        .as_array()
        .expect("an array of accounts")
        .iter()
        .map(|u| u["login"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(logins.contains(&name), "admin user list did not include {name}: {logins:?}");

    inst.fjo([
        "admin",
        "user",
        "edit",
        &name,
        "--full-name",
        "After The Edit",
        "--deactivate",
        "--restrict",
        "--max-repo-creation",
        "7",
    ])
    .assert_ok("fjo admin user edit");

    let (code, after) = inst.api("GET", &format!("users/{name}"), None);
    assert_eq!(code, 200, "{after}");
    let after: serde_json::Value = serde_json::from_str(&after).expect("a user");
    assert_eq!(after["full_name"], "After The Edit", "--full-name was not applied");
    assert_eq!(after["active"], false, "--deactivate was not applied");
    assert_eq!(after["restricted"], true, "--restrict was not applied");
    // The setting nobody named. A PATCH that serialised the whole option struct would have
    // reset this to the type's default and the command would still have exited 0.
    assert_eq!(after["visibility"], "public", "an unnamed setting was rewritten by the edit");
    assert_eq!(after["is_admin"], false, "an unnamed setting was rewritten by the edit");
    assert_eq!(after["email"], email.as_str(), "an unnamed setting was rewritten by the edit");

    inst.fjo(["admin", "user", "delete", &name, "--yes"]).assert_ok("fjo admin user delete");

    let (code, body) = inst.api("GET", &format!("users/{name}"), None);
    assert_eq!(code, 404, "the account should be gone after delete: HTTP {code}: {body}");
}

/// Forgejo refuses to delete an account that still owns repositories, and that refusal is worth a
/// live test for two reasons. It is enforced entirely server-side, so a mock decides the outcome
/// itself; and the failing call is a `DELETE`, the one shape where "the command exited non-zero"
/// is not enough — the account could have been half-removed. So the account is read back after
/// the refusal and must still be intact.
///
/// It also pins `admin repo create`, which creates *on behalf of another account* rather than
/// for the caller — the one thing that distinguishes `POST /admin/users/{u}/repos` from the
/// ordinary create, and the thing a mock cannot check because ownership is assigned by the
/// server.
#[test]
fn deleting_an_account_that_still_owns_a_repository_is_refused_until_purge() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["admin repo create", "admin repo list", "admin user delete"],
        hits: ["adminCreateRepo", "repoSearch", "adminDeleteUser"],
    );

    let owner = TempUser::create(inst, "adm-owner");
    let repo = inst.unique_repo_name("adm-owned");

    inst.fjo([
        "admin",
        "repo",
        "create",
        &repo,
        "--owner",
        &owner.name,
        "--private",
        "-d",
        "owned by somebody else",
    ])
    .assert_ok("fjo admin repo create");

    // Ownership is the whole point of this endpoint and it is decided by the server, so it is
    // read back rather than inferred from the exit code.
    let (code, body) = inst.api("GET", &format!("repos/{}/{repo}", owner.name), None);
    assert_eq!(code, 200, "the repository should exist: {body}");
    let created: serde_json::Value = serde_json::from_str(&body).expect("a repository");
    assert_eq!(created["owner"]["login"], owner.name.as_str(), "created under the wrong account");
    assert_eq!(created["private"], true, "--private did not reach the server");
    assert_eq!(created["description"], "owned by somebody else");

    // Scoped by name rather than by owner: `admin repo list --owner` filters client-side over
    // `/repos/search`, whose `q` matches repository *names*, so it finds nothing for an owner
    // whose name is not part of the repository's. `--query` is the filter that works.
    let listed = inst.fjo(["admin", "repo", "list", "--query", &repo, "--json", "full_name"]);
    listed.assert_ok("fjo admin repo list --query");
    let names: Vec<String> = listed
        .json()
        .as_array()
        .expect("an array of repositories")
        .iter()
        .map(|r| r["full_name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        names.contains(&format!("{}/{repo}", owner.name)),
        "admin repo list --query did not find {repo}: {names:?}"
    );

    let refused = inst.fjo(["admin", "user", "delete", &owner.name, "--yes"]);
    assert!(!refused.ok(), "deleting a repository owner should be refused:\n{}", refused.stderr);
    // fjo's own rendering of the class of failure, not the server's sentence — the wording of
    // "user still has ownership of repositories" is Forgejo's to change.
    refused.assert_says("HTTP 422");

    let (code, still) = owner.read();
    assert_eq!(code, 200, "a refused delete must leave the account intact: {still}");
    assert_eq!(still["login"], owner.name.as_str());

    inst.fjo(["admin", "user", "delete", &owner.name, "--purge", "--yes"])
        .assert_ok("fjo admin user delete --purge");

    let (code, body) = owner.read();
    assert_eq!(code, 404, "--purge should have removed the account: HTTP {code}: {body}");
    let (code, body) = inst.api("GET", &format!("repos/{}/{repo}", owner.name), None);
    assert_eq!(code, 404, "--purge should have removed the repository too: HTTP {code}: {body}");
}

/// Adding an SSH key to *somebody else's* account is an admin-only route, and the only evidence
/// it worked is the key appearing on that account rather than on the caller's. A mock returns a
/// `PublicKey` either way.
///
/// The fingerprint is compared as well as the title, because a body that sent the title under
/// the right name and the key material under the wrong one would still produce a 201 and a
/// plausible-looking object.
#[test]
fn an_admin_added_ssh_key_lands_on_the_named_account_and_disappears_with_it() {
    let inst = instance_or_skip!();
    cover!(raw: ["adminCreatePublicKey", "adminDeleteUserPublicKey"]);

    let user = TempUser::create(inst, "adm-key");

    let created = inst.fjo([
        "raw",
        "admin",
        "create-public-key",
        &user.name,
        "--title",
        "fjo-itest-admin-key",
        "--key",
        SSH_KEY_ADMIN_CREATE,
    ]);
    created.assert_ok("fjo raw admin create-public-key");
    let key_id = created.json()["id"].as_i64().expect("the new key's id");

    let keys = keys_of(inst, &user.name);
    let listed = keys.as_array().expect("an array");
    assert_eq!(listed.len(), 1, "expected exactly one key on a fresh account: {keys}");
    assert_eq!(listed[0]["title"], "fjo-itest-admin-key", "the title did not reach the server");
    assert_eq!(
        listed[0]["key"], SSH_KEY_ADMIN_CREATE,
        "the server stored different key material than we sent"
    );

    inst.fjo(["raw", "admin", "delete-user-public-key", &user.name, &key_id.to_string()])
        .assert_ok("fjo raw admin delete-user-public-key");

    let keys = keys_of(inst, &user.name);
    assert_eq!(
        keys.as_array().map(Vec::len),
        Some(0),
        "the key should be gone after delete-user-public-key: {keys}"
    );
}

/// A token minted for another account is only useful if it *is* that account, and that is a fact
/// about the server's session handling that no mock can assert. So the token is used: it reads
/// `/user` and that call must come back as the account it was minted for, not as the admin who
/// minted it.
///
/// The token's own listing is checked for the same reason `fjo` never prints one twice — the
/// listing must show the last eight characters and **not** the secret, so an operator reading a
/// list cannot accidentally leak a live credential.
#[test]
fn a_token_minted_for_another_account_authenticates_as_that_account() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "adminCreateUserAccessToken",
        "adminListUserAccessTokens",
        "adminDeleteUserAccessToken",
    ]);

    let user = TempUser::create(inst, "adm-tok");
    let token_name = "fjo-itest-minted";

    let made = inst.fjo([
        "raw",
        "admin",
        "create-user-access-token",
        &user.name,
        "--name",
        token_name,
        "--scopes",
        "read:user",
    ]);
    made.assert_ok("fjo raw admin create-user-access-token");
    let made = made.json();
    let secret = made["sha1"].as_str().expect("the minted secret").to_owned();
    assert!(!secret.is_empty(), "a minted token with no secret is useless: {made}");

    // The proof: this credential is that account, decided by Forgejo rather than by us.
    let (code, who) = inst.api_as(&secret, "GET", "user", None);
    assert_eq!(code, 200, "the minted token was not accepted: {who}");
    let who: serde_json::Value = serde_json::from_str(&who).expect("a user");
    assert_eq!(who["login"], user.name.as_str(), "the token authenticated as the wrong account");

    let listed = inst.fjo(["raw", "admin", "list-user-access-tokens", &user.name]);
    listed.assert_ok("fjo raw admin list-user-access-tokens");
    let listed = listed.json();
    let rows = listed.as_array().expect("an array of tokens");
    assert_eq!(rows.len(), 1, "expected exactly one token on a fresh account: {listed}");
    assert_eq!(rows[0]["name"], token_name);
    assert_eq!(
        rows[0]["sha1"], "",
        "a token listing must never repeat the secret; only creation may show it: {listed}"
    );

    inst.fjo(["raw", "admin", "delete-user-access-token", &user.name, token_name])
        .assert_ok("fjo raw admin delete-user-access-token");

    // Revocation has to actually revoke, which is again the server's call and not ours.
    let (code, _) = inst.api_as(&secret, "GET", "user", None);
    assert_eq!(code, 401, "the deleted token still authenticates");
}

/// `DELETE /admin/users/{u}/emails` takes a list, and both halves of that are server-decided: a
/// secondary address goes, a primary one is refused. A mock picks whichever answer the test
/// author expected, which makes the refusal — the branch an operator actually meets — untested.
///
/// The refusal is the important half. `fjo` must exit non-zero *and* the address must still be
/// on the account: a partially applied delete that removed the secondary while rejecting the
/// primary would look identical from the exit code alone.
///
/// The secondary address is added with the account's own token, because there is no admin route
/// that adds one — `/user/emails` is self-service only.
#[test]
fn an_admin_can_remove_a_secondary_address_but_never_the_primary_one() {
    let inst = instance_or_skip!();
    cover!(raw: ["adminListUserEmails", "adminDeleteUserEmails"]);

    let user = TempUser::create(inst, "adm-mail");
    let secondary = format!("{}-alt@example.invalid", user.name);

    let minted = inst.fjo([
        "raw",
        "admin",
        "create-user-access-token",
        &user.name,
        "--name",
        "fjo-itest-email",
        "--scopes",
        "write:user",
    ]);
    minted.assert_ok("minting a token so the account can add its own address");
    let as_user = minted.json()["sha1"].as_str().expect("a secret").to_owned();
    let (code, body) = inst.api_as(
        &as_user,
        "POST",
        "user/emails",
        Some(&format!(r#"{{"emails":["{secondary}"]}}"#)),
    );
    assert!((200..300).contains(&code), "could not add a secondary address: HTTP {code}: {body}");

    let listed = inst.fjo(["raw", "admin", "list-user-emails", &user.name]);
    listed.assert_ok("fjo raw admin list-user-emails");
    let addresses: Vec<String> = listed
        .json()
        .as_array()
        .expect("an array of addresses")
        .iter()
        .map(|e| e["email"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(addresses.contains(&user.email()), "the primary address is missing: {addresses:?}");
    assert!(addresses.contains(&secondary), "the secondary address is missing: {addresses:?}");

    let refused =
        inst.fjo(["raw", "admin", "delete-user-emails", &user.name, "--emails", &user.email()]);
    assert!(!refused.ok(), "deleting a primary address should be refused:\n{}", refused.stderr);
    refused.assert_says("HTTP 422");

    inst.fjo(["raw", "admin", "delete-user-emails", &user.name, "--emails", &secondary])
        .assert_ok("fjo raw admin delete-user-emails on a secondary address");

    let listed = inst.fjo(["raw", "admin", "list-user-emails", &user.name]);
    listed.assert_ok("fjo raw admin list-user-emails after the deletion");
    let addresses: Vec<String> = listed
        .json()
        .as_array()
        .expect("an array of addresses")
        .iter()
        .map(|e| e["email"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        addresses.contains(&user.email()),
        "the refused delete must have left the primary address alone: {addresses:?}"
    );
    assert!(!addresses.contains(&secondary), "the secondary address survived: {addresses:?}");
}

/// `admin email search` is one leaf over **two** endpoints: a keyword goes to
/// `/admin/emails/search`, and no keyword goes to `/admin/emails`, because searching with an
/// empty keyword returns nothing on some releases and would read as "no such address" for a
/// command that was asked to list everything. That split is invisible to a mock, which answers
/// whichever one the test wires up.
///
/// Both forms are driven, and both must find the same account — which is the only way to notice
/// if the two routes ever stop agreeing.
#[test]
fn searching_for_an_address_finds_the_account_that_owns_it_by_either_route() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["admin email search"],
        hits: ["adminSearchEmails", "adminGetAllEmails"],
    );

    let user = TempUser::create(inst, "adm-find");

    let found = inst.fjo(["admin", "email", "search", &user.name, "--json", "email,username"]);
    found.assert_ok("fjo admin email search <keyword>");
    let rows = found.json();
    let rows = rows.as_array().expect("an array of addresses");
    assert_eq!(rows.len(), 1, "a unique keyword should match one address: {rows:?}");
    assert_eq!(rows[0]["email"], user.email().as_str());
    assert_eq!(
        rows[0]["username"],
        user.name.as_str(),
        "the address is attributed to the wrong account"
    );

    // No keyword: the other endpoint. `--paginate` because this lists every address on the
    // instance and other test binaries are adding accounts to it concurrently.
    let all = inst.fjo(["admin", "email", "search", "--paginate", "--json", "email,username"]);
    all.assert_ok("fjo admin email search with no keyword");
    let owner = all
        .json()
        .as_array()
        .expect("an array of addresses")
        .iter()
        .find(|e| e["email"].as_str() == Some(user.email().as_str()))
        .map(|e| e["username"].as_str().unwrap_or_default().to_owned());
    assert_eq!(
        owner.as_deref(),
        Some(user.name.as_str()),
        "the keyword-less listing disagrees with the search about who owns {}",
        user.email()
    );
}

/// Renaming is a `POST` that answers `204` and carries the whole result in a side effect, which
/// is the shape with the least evidence available from the response. Both ends are therefore
/// read back, and the account's id must survive: a rename that re-created the account would
/// orphan every repository, issue and token pointing at the old id.
///
/// # What the old login does afterwards, which no mock would have guessed
///
/// It does **not** 404. Forgejo records the rename and answers `307` at the old path with a
/// `Location` naming the new one, so links and scripts written before the rename keep working.
/// This test asserts that redirect rather than an absence, for two reasons: writing the
/// intuitive assertion is how this test failed the first time it was run against a real server,
/// and a client that stopped following redirects would otherwise silently start reporting
/// renamed accounts as missing.
///
/// The one thing the old path must never do is answer `200` with the account inline — that
/// would mean two live logins for one account.
#[test]
fn renaming_an_account_moves_it_rather_than_copying_it() {
    let inst = instance_or_skip!();
    cover!(raw: ["adminRenameUser", "adminSearchUsers"]);

    let user = TempUser::create(inst, "adm-rename");
    let (code, before) = user.read();
    assert_eq!(code, 200, "{before}");
    let id_before = before["id"].as_i64().expect("an account id");

    let renamed = format!("{}-moved", user.name);
    inst.fjo(["raw", "admin", "rename-user", &user.name, "--new-username", &renamed])
        .assert_ok("fjo raw admin rename-user");

    let (code, body) = inst.api("GET", &format!("users/{renamed}"), None);
    assert_eq!(code, 200, "the account should answer at its new name: HTTP {code}: {body}");
    let after: serde_json::Value = serde_json::from_str(&body).expect("a user");
    assert_eq!(
        after["id"].as_i64(),
        Some(id_before),
        "the rename changed the account id, which would orphan everything pointing at it"
    );

    let (code, body) = user.read();
    assert_eq!(
        code, 307,
        "the old login must redirect to the new one rather than answer directly: HTTP {code}: \
         {body}"
    );
    let headers = inst.api_headers(&format!("users/{}", user.name));
    assert!(
        headers.lines().any(|h| {
            let h = h.trim();
            h.to_ascii_lowercase().starts_with("location:")
                && h.ends_with(&format!("/users/{renamed}"))
        }),
        "the redirect from the old login does not point at {renamed}:\n{headers}"
    );

    let listed = inst.fjo(["raw", "admin", "search-users", "--paginate"]);
    listed.assert_ok("fjo raw admin search-users");
    let logins: Vec<String> = listed
        .json()
        .as_array()
        .expect("an array of accounts")
        .iter()
        .map(|u| u["login"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(logins.contains(&renamed), "the renamed account is not listed: {logins:?}");
    assert!(!logins.contains(&user.name), "the old login is still listed: {logins:?}");

    // `TempUser`'s drop targets the old name, where the server answers a redirect that `curl`
    // is not told to follow, so the delete would not land. This one cleans up explicitly.
    let (code, body) = inst.api("DELETE", &format!("admin/users/{renamed}?purge=true"), None);
    assert!((200..300).contains(&code), "could not clean up {renamed}: HTTP {code}: {body}");
}

// --------------------------------------------------------------------------- organizations

/// `admin org create` posts to `/admin/users/{owner}/orgs`, so the owner is in the **path** and
/// the organization's own name is in the **body** — two names in one request, with nothing in the
/// reply that would look wrong if they were swapped. A mock cannot tell; the server can, because
/// it records who the first administrator is.
///
/// So ownership is read back from `/orgs/{name}/members`, not merely inferred from a 201.
#[test]
fn an_organization_is_created_under_the_account_named_in_the_path() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["admin org create", "admin org list"],
        hits: ["adminCreateOrg", "adminGetAllOrgs"],
    );

    let owner = TempUser::create(inst, "adm-orgown");
    let org = inst.unique_repo_name("adm-org");

    inst.fjo([
        "admin",
        "org",
        "create",
        &org,
        "--owner",
        &owner.name,
        "--description",
        "made by the admin group",
        "--visibility",
        "limited",
    ])
    .assert_ok("fjo admin org create");

    let (code, body) = inst.api("GET", &format!("orgs/{org}"), None);
    assert_eq!(code, 200, "the organization should exist: {body}");
    let created: serde_json::Value = serde_json::from_str(&body).expect("an organization");
    assert_eq!(created["description"], "made by the admin group");
    assert_eq!(created["visibility"], "limited", "--visibility did not reach the server");

    // The two-names-in-one-request check: the account in the path is the one that ended up
    // owning it, rather than the caller.
    let (code, body) = inst.api("GET", &format!("orgs/{org}/members"), None);
    assert_eq!(code, 200, "{body}");
    let members: serde_json::Value = serde_json::from_str(&body).expect("an array of members");
    let logins: Vec<&str> =
        members.as_array().expect("an array").iter().filter_map(|m| m["login"].as_str()).collect();
    assert!(
        logins.contains(&owner.name.as_str()),
        "the organization is not owned by the account named in the path: {logins:?}"
    );

    // `--paginate`: instance-wide, and other test binaries create organizations too.
    let listed = inst.fjo(["admin", "org", "list", "--paginate", "--json", "username"]);
    listed.assert_ok("fjo admin org list");
    let names: Vec<String> = listed
        .json()
        .as_array()
        .expect("an array of organizations")
        .iter()
        .map(|o| o["username"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(names.contains(&org), "admin org list did not include {org}: {names:?}");

    let (code, body) = inst.api("DELETE", &format!("orgs/{org}"), None);
    assert!((200..300).contains(&code), "could not clean up the organization: HTTP {code}: {body}");
}

// ---------------------------------------------------------------------------------- cron

/// `admin cron run` answers `204` and prints nothing, so "it exited 0" is the entire response.
/// That is exactly the situation where a request sent to a *nearly* right URL — or with the task
/// name in a query string instead of the path — looks like success. The only real evidence is on
/// the server, and `/admin/cron` happens to publish it: every task carries an `exec_times`
/// counter and a `prev` timestamp.
///
/// So the counter is read before and after, and it must have moved.
///
/// # Why `update_checker`, and why it is safe to run on a shared instance
///
/// Eight other test binaries share this Forgejo, so the task had to be one that cannot disturb
/// them. `update_checker` only asks whether a newer Forgejo has been released; it touches no
/// repository, no account and no database row belonging to anyone's fixtures, and the harness
/// boots the container with `OFFLINE_MODE`, so even the outbound request has nowhere to go.
/// Every other entry in the list is disqualified: `git_gc_repos`, `repo_health_check` and
/// `check_repo_stats` rewrite repositories another test may be pushing to, `archive_cleanup`,
/// `delete_repo_archives` and `cleanup_packages` delete artifacts, and `delete_missing_repos`
/// and `reinit_missing_repos` are destructive by name.
///
/// This is the one test here that changes instance-wide state and cannot put it back — a run
/// counter only goes up. It is recorded rather than restored, which is safe precisely because
/// the counter is the only thing that moves.
#[test]
fn running_the_update_checker_advances_its_execution_count() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["admin cron list", "admin cron run"],
        hits: ["adminCronList", "adminCronRun"],
    );

    /// Side-effect-free on a shared instance; see this test's documentation.
    const TASK: &str = "update_checker";

    let runs = |inst: &Instance| -> i64 {
        let listed = inst.fjo(["admin", "cron", "list", "--json", "name,exec_times"]);
        listed.assert_ok("fjo admin cron list");
        listed
            .json()
            .as_array()
            .expect("an array of tasks")
            .iter()
            .find(|t| t["name"].as_str() == Some(TASK))
            .unwrap_or_else(|| panic!("this instance has no {TASK} task"))["exec_times"]
            .as_i64()
            .expect("an execution count")
    };

    let before = runs(inst);
    inst.fjo(["admin", "cron", "run", TASK, "--yes"]).assert_ok("fjo admin cron run");
    let after = runs(inst);

    assert!(
        after > before,
        "{TASK} reported {before} runs before and {after} after, so the 204 did not correspond \
         to the task actually running"
    );
}

/// Two gates stand in front of `POST /admin/cron/{task}`, and both must hold without the server
/// being asked to enforce them.
///
/// An unknown task name is caught by listing the tasks first, so the user is told what this
/// instance *does* have instead of receiving a bare 404 — this is the "never swallow the real
/// reason" rule applied to a name that was simply mistyped. It is also why this test cannot
/// claim `adminCronRun`: the run is never sent.
///
/// Running a task is destructive, so without `--yes` and without a terminal to confirm at, the
/// command must refuse. A regression there would let a scripted invocation silently run
/// maintenance on a production instance.
#[test]
fn cron_run_refuses_an_unknown_task_and_refuses_to_run_unconfirmed() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["admin cron run"], hits: ["adminCronList"]);

    let unknown = inst.fjo(["admin", "cron", "run", "no-such-task-at-all", "--yes"]);
    unknown.assert_code(2, "fjo admin cron run with an unknown task");
    unknown.assert_says("no scheduled task");
    // The point of listing first: the message names what this instance actually has.
    unknown.assert_says("update_checker");

    // No `--yes`, and the test harness is not a terminal, so there is nowhere to confirm.
    let unconfirmed = inst.fjo(["admin", "cron", "run", "update_checker"]);
    unconfirmed.assert_code(2, "fjo admin cron run without --yes");
    unconfirmed.assert_says("--yes");
}

// -------------------------------------------------------------------------------- runners

/// Two operations, both deprecated, both documented as "get a runner registration token for
/// registering global runners", on two different paths. Nothing in the specification says
/// whether they are the same token or two independent ones, and `fjo` generates a separate
/// command for each — so the only way to find out what an operator actually gets is to ask the
/// server.
///
/// They agree today. If a future Forgejo splits them, this is the test that says so, which is
/// worth more than either call in isolation.
#[test]
fn both_deprecated_registration_token_routes_hand_out_the_same_token() {
    let inst = instance_or_skip!();
    cover!(raw: ["adminGetRegistrationToken", "adminGetRunnerRegistrationToken"]);

    let old = inst.fjo(["raw", "admin", "get-registration-token"]);
    old.assert_ok("fjo raw admin get-registration-token");
    let new = inst.fjo(["raw", "admin", "get-runner-registration-token"]);
    new.assert_ok("fjo raw admin get-runner-registration-token");

    let old = old.json()["token"].as_str().unwrap_or_default().to_owned();
    let new = new.json()["token"].as_str().unwrap_or_default().to_owned();
    assert!(!old.is_empty(), "the deprecated route returned no token");
    assert!(!new.is_empty(), "the current route returned no token");
    assert_eq!(
        old, new,
        "the two registration-token routes now disagree; an operator following either set of \
         instructions would register against a different secret"
    );
}

/// A whole runner lifecycle without a runner process, which is the finding that makes this
/// group testable at all: `POST /admin/actions/runners` registers by name and returns an id, so
/// nothing has to connect.
///
/// The interesting part is `admin runner list`, which reports a **scope** — instance, org, user
/// or repository — that the API does not send. It is derived from `owner_id` and `repo_id`, and
/// a derivation that got it backwards would label every instance runner as somebody's private
/// one, with the command still exiting 0. So the registered runner is looked up by its id in the
/// listing and its scope column checked, rather than the listing merely being non-empty.
///
/// Counting is avoided throughout: other test binaries share this instance and may register
/// runners of their own.
#[test]
fn a_registered_runner_is_listed_as_instance_scoped_until_it_is_deleted() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["admin runner list", "admin runner delete"],
        hits: ["registerAdminRunner", "getAdminRunner", "getAdminRunners", "deleteAdminRunner"],
    );

    let name = inst.unique_repo_name("adm-runner");
    let registered = inst.fjo([
        "raw",
        "admin",
        "register-admin-runner",
        "--name",
        &name,
        "--description",
        "registered by the admin integration suite",
    ]);
    registered.assert_ok("fjo raw admin register-admin-runner");
    let registered = registered.json();
    let id = registered["id"].as_i64().expect("the new runner's id");
    assert!(
        registered["token"].as_str().is_some_and(|t| !t.is_empty()),
        "registration returned no token, so no runner could ever use it: {registered}"
    );

    let fetched = inst.fjo(["raw", "admin", "get-admin-runner", &id.to_string()]);
    fetched.assert_ok("fjo raw admin get-admin-runner");
    let fetched = fetched.json();
    assert_eq!(fetched["name"], name.as_str(), "the id names a different runner");
    assert_eq!(
        fetched["description"], "registered by the admin integration suite",
        "--description did not reach the server"
    );
    assert_eq!(fetched["owner_id"], 0, "a global runner must not be owned by an account");
    assert_eq!(fetched["repo_id"], 0, "a global runner must not be bound to a repository");

    let listed = inst.fjo(["admin", "runner", "list", "--json", "id,name,owner_id,repo_id"]);
    listed.assert_ok("fjo admin runner list");
    let listed = listed.json();
    let mine = listed
        .as_array()
        .expect("an array of runners")
        .iter()
        .find(|r| r["id"].as_i64() == Some(id))
        .unwrap_or_else(|| panic!("runner {id} is missing from admin runner list: {listed}"));
    assert_eq!(mine["name"], name.as_str());

    // The derived column, checked on the human path where it is actually rendered.
    let human = inst.fjo(["admin", "runner", "list", "--instance-only"]);
    human.assert_ok("fjo admin runner list --instance-only");
    let row = human.stdout.lines().find(|l| l.contains(&name)).unwrap_or_else(|| {
        panic!("runner {name} is missing from --instance-only:\n{}", human.stdout)
    });
    assert!(
        row.contains("instance"),
        "a runner with no owner and no repository must be scoped `instance`, got: {row}"
    );

    inst.fjo(["admin", "runner", "delete", &id.to_string(), "--yes"])
        .assert_ok("fjo admin runner delete");

    let gone = inst.fjo(["raw", "admin", "get-admin-runner", &id.to_string()]);
    assert!(!gone.ok(), "the runner should be gone after delete:\n{}", gone.stdout);
    gone.assert_says("HTTP 404");
}

// ------------------------------------------------------------------------------- unadopted

/// A repository directory on disk that Forgejo has no database row for cannot be manufactured
/// through the API — it takes a shell on the server — so what this pins is the half that is
/// reachable: the listing works, and both mutating routes refuse a path that is not there
/// **without creating anything**.
///
/// That refusal is worth asserting rather than skipping. `POST /admin/unadopted/{o}/{r}` on a
/// missing directory answers Forgejo's own JSON 404, not the router's — which is the evidence
/// that the two path segments went where the specification says they go. A route we had spelled
/// wrong would 404 too, so the second half of the check is that no repository called `{o}/{r}`
/// came into existence: an adopt that silently created a fresh repository instead of adopting an
/// existing directory would be the genuinely dangerous failure here.
///
/// A `TestRepo` is created first because `GET /admin/unadopted` walks the repository root on
/// disk, and on an instance that has never held a repository that directory does not exist and
/// the listing answers `500 lstat ... no such file or directory`. Creating one repository is
/// what makes the empty listing an empty listing rather than an error.
#[test]
fn adopting_a_directory_that_is_not_on_disk_is_refused_and_creates_nothing() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["admin adopt list", "admin adopt adopt", "admin adopt delete"],
        hits: ["adminUnadoptedList", "adminAdoptRepository", "adminDeleteUnadoptedRepository"],
    );

    // See the doc comment: this exists so the repository root exists.
    let anchor = TestRepo::create(inst, "adm-adopt-anchor");

    let listed = inst.fjo(["admin", "adopt", "list", "--paginate"]);
    listed.assert_ok("fjo admin adopt list");

    let missing = inst.unique_repo_name("adm-not-on-disk");
    let slug = format!("{}/{missing}", inst.user);

    let adopt = inst.fjo(["admin", "adopt", "adopt", &slug, "--yes"]);
    assert!(!adopt.ok(), "adopting a directory that is not there should fail:\n{}", adopt.stdout);
    adopt.assert_says("HTTP 404");

    let (code, body) = inst.api("GET", &format!("repos/{slug}"), None);
    assert_eq!(
        code, 404,
        "a failed adopt must not have created a repository instead: HTTP {code}: {body}"
    );

    let delete = inst.fjo(["admin", "adopt", "delete", &slug, "--yes"]);
    assert!(!delete.ok(), "deleting a directory that is not there should fail:\n{}", delete.stdout);
    delete.assert_says("HTTP 404");

    // The anchor is a real, adopted repository; nothing above may have disturbed it.
    let (code, body) = inst.api("GET", &format!("repos/{}", anchor.slug()), None);
    assert_eq!(code, 200, "the anchor repository was disturbed: HTTP {code}: {body}");
}

// ---------------------------------------------------------------------------------- quota

/// A quota rule name reserved for one test, deleted on drop.
///
/// Reserving the *name* rather than creating the rule is deliberate: the creation is the thing
/// under test in [`a_quota_rule_round_trips_and_an_edit_touches_only_what_it_names`], so a
/// fixture that created it would test nothing. What this buys is cleanup after a panicking
/// assertion — quota rules are instance-wide and a failed run must not leave one behind for the
/// next one to collide with.
struct QuotaRule<'a> {
    inst: &'a Instance,
    name: String,
}

impl<'a> QuotaRule<'a> {
    fn reserve(inst: &'a Instance, prefix: &str) -> Self {
        Self { inst, name: inst.unique_repo_name(prefix) }
    }

    /// What the server currently holds, read out of band.
    fn read(&self) -> (i32, serde_json::Value) {
        let (code, body) = self.inst.api("GET", &format!("admin/quota/rules/{}", self.name), None);
        (code, serde_json::from_str(&body).unwrap_or(serde_json::Value::Null))
    }
}

impl Drop for QuotaRule<'_> {
    fn drop(&mut self) {
        let _ = self.inst.api("DELETE", &format!("admin/quota/rules/{}", self.name), None);
    }
}

/// A quota group name reserved for one test, deleted on drop. See [`QuotaRule`].
///
/// Declare a group guard *after* the rule guard it uses, so Rust's reverse drop order removes
/// the group first. Either order in fact works — deleting a rule detaches it from every group —
/// but relying on that would make this cleanup depend on the very behaviour
/// [`deleting_a_rule_detaches_it_from_every_group_that_used_it`] exists to check.
struct QuotaGroup<'a> {
    inst: &'a Instance,
    name: String,
}

impl<'a> QuotaGroup<'a> {
    fn reserve(inst: &'a Instance, prefix: &str) -> Self {
        Self { inst, name: inst.unique_repo_name(prefix) }
    }

    fn read(&self) -> (i32, serde_json::Value) {
        let (code, body) = self.inst.api("GET", &format!("admin/quota/groups/{}", self.name), None);
        (code, serde_json::from_str(&body).unwrap_or(serde_json::Value::Null))
    }

    /// The rule names attached to this group, in the order the server lists them.
    fn rule_names(&self) -> Vec<String> {
        let (code, group) = self.read();
        assert_eq!(code, 200, "reading quota group {}: {group}", self.name);
        group["rules"]
            .as_array()
            .expect("a group carries a rules array")
            .iter()
            .map(|r| r["name"].as_str().unwrap_or_default().to_owned())
            .collect()
    }
}

impl Drop for QuotaGroup<'_> {
    fn drop(&mut self) {
        let _ = self.inst.api("DELETE", &format!("admin/quota/groups/{}", self.name), None);
    }
}

/// Comfortably larger than anything this suite stores, because a quota group *enforces*. A tight
/// limit on an account another test happened to use would turn this file into a source of
/// unexplained push failures elsewhere. For the same reason no test here ever puts the harness
/// admin — the account every other test binary acts as — into a group.
const GENEROUS_LIMIT: i64 = 1_073_741_824;

/// `admin quota rule edit` promises to change "a rule's limit or subjects, leaving the other
/// alone", and that promise is the whole risk in this command: it is a `PATCH` assembled from
/// whichever flags were given, so a body that serialised its whole struct would reset the field
/// the user did not mention — to `0` for a limit, which means *nothing may be stored*. The
/// command would still print the rule and exit 0.
///
/// So both one-sided edits are driven and the untouched half is read back from the server each
/// time. A `FakeTransport` cannot catch this: it replies with whatever the test author wrote
/// down, which is invariably the rule they meant to end up with.
///
/// Subjects are compared as an ordered list, not a set. They arrive as a JSON array and the
/// order is the server's answer to "what did you store"; a rule whose subjects came back
/// reordered would mean the round trip is not the identity it appears to be.
#[test]
fn a_quota_rule_round_trips_and_an_edit_touches_only_what_it_names() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: [
            "admin quota rule create",
            "admin quota rule list",
            "admin quota rule edit",
            "admin quota rule delete",
        ],
        hits: [
            "adminCreateQuotaRule",
            "adminListQuotaRules",
            "adminGetQuotaRule",
            "adminEditQuotaRule",
            "adminDeleteQuotaRule",
        ],
    );

    let rule = QuotaRule::reserve(inst, "q-rule");

    inst.fjo([
        "admin",
        "quota",
        "rule",
        "create",
        &rule.name,
        "--bytes",
        &GENEROUS_LIMIT.to_string(),
        "--subject",
        "size:assets:packages:all",
        "--subject",
        "size:assets:attachments:releases",
    ])
    .assert_ok("fjo admin quota rule create");

    let (code, stored) = rule.read();
    assert_eq!(code, 200, "the rule should exist after create: {stored}");
    assert_eq!(stored["limit"].as_i64(), Some(GENEROUS_LIMIT), "--bytes did not reach the server");
    assert_eq!(
        stored["subjects"],
        serde_json::json!(["size:assets:packages:all", "size:assets:attachments:releases"]),
        "a repeated --subject did not arrive as the list that was typed"
    );

    // `raw admin get-quota-rule` must agree with the direct read. It is a different code path
    // to the same route, and the point of driving both is that a disagreement is visible.
    let fetched = inst.fjo(["raw", "admin", "get-quota-rule", &rule.name]);
    fetched.assert_ok("fjo raw admin get-quota-rule");
    assert_eq!(fetched.json(), stored, "get-quota-rule disagrees with the server's own answer");

    // Instance-wide listing, so scoped by name: other test binaries share this Forgejo.
    let listed = inst.fjo(["admin", "quota", "rule", "list", "--json", "name,limit"]);
    listed.assert_ok("fjo admin quota rule list");
    let listed = listed.json();
    let mine = listed
        .as_array()
        .expect("an array of rules")
        .iter()
        .find(|r| r["name"].as_str() == Some(rule.name.as_str()))
        .unwrap_or_else(|| panic!("{} is missing from the rule listing: {listed}", rule.name));
    assert_eq!(mine["limit"].as_i64(), Some(GENEROUS_LIMIT), "the listing reports a wrong limit");

    // Edit one half. The other must survive — a limit silently reset to 0 here would mean
    // "nothing may be stored" for every account the rule reaches.
    inst.fjo(["admin", "quota", "rule", "edit", &rule.name, "--subject", "size:all"])
        .assert_ok("fjo admin quota rule edit --subject");
    let (_, after) = rule.read();
    assert_eq!(after["subjects"], serde_json::json!(["size:all"]), "--subject did not replace");
    assert_eq!(
        after["limit"].as_i64(),
        Some(GENEROUS_LIMIT),
        "editing only --subject reset the limit the caller never mentioned"
    );

    // And the other half, the same way round.
    let halved = GENEROUS_LIMIT / 2;
    inst.fjo(["admin", "quota", "rule", "edit", &rule.name, "--bytes", &halved.to_string()])
        .assert_ok("fjo admin quota rule edit --bytes");
    let (_, after) = rule.read();
    assert_eq!(after["limit"].as_i64(), Some(halved), "--bytes did not apply");
    assert_eq!(
        after["subjects"],
        serde_json::json!(["size:all"]),
        "editing only --bytes discarded the subjects the caller never mentioned"
    );

    // A second rule under the same name must be refused rather than redefine the first. The
    // server decides this, and the consequence of it going the other way is an operator
    // silently rewriting a limit somebody else's group depends on.
    let clash = inst.fjo([
        "admin",
        "quota",
        "rule",
        "create",
        &rule.name,
        "--bytes",
        "1024",
        "--subject",
        "size:all",
    ]);
    assert!(!clash.ok(), "a duplicate rule name should be refused:\n{}", clash.stderr);
    let (_, unchanged) = rule.read();
    assert_eq!(
        unchanged["limit"].as_i64(),
        Some(halved),
        "the refused create overwrote the existing rule anyway"
    );

    inst.fjo(["admin", "quota", "rule", "delete", &rule.name, "--yes"])
        .assert_ok("fjo admin quota rule delete");
    let (code, body) = rule.read();
    assert_eq!(code, 404, "the rule should be gone after delete: HTTP {code}: {body}");
}

/// The trap in `POST /admin/quota/groups`: its `rules` field is an array of **full rule
/// definitions**, not names. `fjo` has to put something in `limit` for a rule the user named by
/// `--rule`, and it sends `0` — so if Forgejo took that at face value, creating a group that
/// mentions an existing rule would reset that rule to "nothing may be stored", instantly and
/// for every other group already using it. The comment at `cmd/admin/quota.rs` asserts Forgejo
/// does not; this is the test that makes the assertion checkable.
///
/// It is the single highest-consequence thing in this group and it is invisible to a mock,
/// which would return whatever group the test author wrote down and never touch the rule.
///
/// The other half of the same flag is pinned too: naming a rule that does **not** exist really
/// does conjure one with a zero limit. That is the API's behaviour and the command's help warns
/// about it, so it needs to be true rather than merely documented.
#[test]
fn naming_an_existing_rule_when_creating_a_group_attaches_it_without_redefining_it() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: [
            "admin quota group create",
            "admin quota group list",
            "admin quota group delete",
        ],
        hits: [
            "adminCreateQuotaRule",
            "adminCreateQuotaGroup",
            "adminListQuotaGroups",
            "adminGetQuotaGroup",
            "adminDeleteQuotaGroup",
            "adminDeleteQuotaRule",
        ],
    );

    let rule = QuotaRule::reserve(inst, "q-attach-rule");
    let group = QuotaGroup::reserve(inst, "q-attach-grp");
    let conjured = QuotaRule::reserve(inst, "q-conjured");
    let ghost_group = QuotaGroup::reserve(inst, "q-ghost-grp");

    inst.fjo([
        "admin",
        "quota",
        "rule",
        "create",
        &rule.name,
        "--bytes",
        &GENEROUS_LIMIT.to_string(),
        "--subject",
        "size:all",
    ])
    .assert_ok("fjo admin quota rule create");

    inst.fjo(["admin", "quota", "group", "create", &group.name, "--rule", &rule.name])
        .assert_ok("fjo admin quota group create --rule");

    assert_eq!(
        group.rule_names(),
        vec![rule.name.clone()],
        "the named rule was not attached to the new group"
    );

    // The assertion this test exists for.
    let (code, after) = rule.read();
    assert_eq!(code, 200, "the rule vanished when a group named it: {after}");
    assert_eq!(
        after["limit"].as_i64(),
        Some(GENEROUS_LIMIT),
        "creating a group that names an existing rule redefined that rule. `fjo` sends limit 0 \
         for a named rule because the API takes whole rule definitions, and Forgejo used to \
         ignore it; if that has changed, every `quota group create --rule <existing>` now sets \
         the rule to `nothing may be stored` for every group already using it"
    );
    assert_eq!(after["subjects"], serde_json::json!(["size:all"]), "the rule's subjects were lost");

    // `raw admin get-quota-group` must see the same group as the direct read.
    let fetched = inst.fjo(["raw", "admin", "get-quota-group", &group.name]);
    fetched.assert_ok("fjo raw admin get-quota-group");
    let (_, direct) = group.read();
    assert_eq!(fetched.json(), direct, "get-quota-group disagrees with the server's own answer");

    // Instance-wide, so scoped by name.
    let listed = inst.fjo(["admin", "quota", "group", "list", "--json", "name,rules"]);
    listed.assert_ok("fjo admin quota group list");
    let listed = listed.json();
    let mine = listed
        .as_array()
        .expect("an array of groups")
        .iter()
        .find(|g| g["name"].as_str() == Some(group.name.as_str()))
        .unwrap_or_else(|| panic!("{} is missing from the group listing: {listed}", group.name));
    assert_eq!(
        mine["rules"][0]["limit"].as_i64(),
        Some(GENEROUS_LIMIT),
        "the group listing reports a different limit than the rule holds: {mine}"
    );

    // The other half of --rule: a name that does not exist yet becomes a real, zero-limit rule.
    inst.fjo(["admin", "quota", "group", "create", &ghost_group.name, "--rule", &conjured.name])
        .assert_ok("fjo admin quota group create with an unknown rule name");
    let (code, born) = conjured.read();
    assert_eq!(code, 200, "naming an unknown rule should have created it: HTTP {code}: {born}");
    assert_eq!(
        born["limit"].as_i64(),
        Some(0),
        "the conjured rule should carry the zero limit the help warns about, got {born}"
    );

    inst.fjo(["admin", "quota", "group", "delete", &group.name, "--yes"])
        .assert_ok("fjo admin quota group delete");
    let (code, body) = group.read();
    assert_eq!(code, 404, "the group should be gone after delete: HTTP {code}: {body}");

    // Deleting a group must not take its rules with it: they are shared, and a group deletion
    // that cascaded into the rule table would silently disarm every other group using them.
    let (code, survivor) = rule.read();
    assert_eq!(code, 200, "deleting a group deleted the rule it referenced: {survivor}");
}

/// `admin quota rule delete` promises to remove the rule "from every group that used it". That
/// is a server-side cascade across two tables, and the command's own reply says nothing about
/// it — so the only evidence is the group's contents afterwards.
///
/// The failure this guards is the quiet one: a rule row deleted while the group keeps a dangling
/// reference. The group would still be listed as limiting something it can no longer describe,
/// and an operator auditing quotas would read a limit that no longer exists.
#[test]
fn deleting_a_rule_detaches_it_from_every_group_that_used_it() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["admin quota group add-rule", "admin quota group remove-rule"],
        hits: [
            "adminCreateQuotaRule",
            "adminCreateQuotaGroup",
            "adminAddRuleToQuotaGroup",
            "adminRemoveRuleFromQuotaGroup",
            "adminGetQuotaGroup",
            "adminDeleteQuotaRule",
        ],
    );

    let kept = QuotaRule::reserve(inst, "q-kept");
    let doomed = QuotaRule::reserve(inst, "q-doomed");
    let group = QuotaGroup::reserve(inst, "q-cascade-grp");

    for rule in [&kept, &doomed] {
        inst.fjo([
            "admin",
            "quota",
            "rule",
            "create",
            &rule.name,
            "--bytes",
            &GENEROUS_LIMIT.to_string(),
            "--subject",
            "size:all",
        ])
        .assert_ok("fjo admin quota rule create");
    }
    inst.fjo(["admin", "quota", "group", "create", &group.name])
        .assert_ok("fjo admin quota group create");
    assert!(group.rule_names().is_empty(), "a group created with no --rule must limit nothing");

    for rule in [&kept, &doomed] {
        inst.fjo(["admin", "quota", "group", "add-rule", &group.name, &rule.name])
            .assert_ok("fjo admin quota group add-rule");
    }
    let attached = group.rule_names();
    assert!(attached.contains(&kept.name), "add-rule lost {}: {attached:?}", kept.name);
    assert!(attached.contains(&doomed.name), "add-rule lost {}: {attached:?}", doomed.name);

    // Detaching one must leave the other, which is the pair that catches a handler keyed on the
    // group instead of on the (group, rule) it was given.
    inst.fjo(["admin", "quota", "group", "remove-rule", &group.name, &kept.name])
        .assert_ok("fjo admin quota group remove-rule");
    assert_eq!(
        group.rule_names(),
        vec![doomed.name.clone()],
        "remove-rule detached the wrong rule, or detached more than one"
    );
    // Detaching does not delete: the rule is shared and must survive leaving a group.
    let (code, body) = kept.read();
    assert_eq!(code, 200, "remove-rule deleted the rule rather than detaching it: {body}");

    inst.fjo(["admin", "quota", "rule", "delete", &doomed.name, "--yes"])
        .assert_ok("fjo admin quota rule delete");

    assert!(
        group.rule_names().is_empty(),
        "deleting a rule left a dangling reference in {}: {:?}",
        group.name,
        group.rule_names()
    );
}

/// Membership is where a quota stops being bookkeeping and starts limiting somebody, and three
/// separate server-side joins decide what an account actually gets. None of them exist on the
/// client, so a mock can only restate the test author's expectations:
///
/// * `add-user` / `remove-user` change one group's roll, not another's;
/// * `GET /admin/users/{u}/quota` resolves the account's groups **and the rules inside them**,
///   so the limit an operator reads there is a join across three tables;
/// * `set-user-quota-groups` *replaces* the whole list rather than adding to it — the one
///   operation here whose name is its only warning, and the one most likely to be reached for
///   when `add-user` was meant.
///
/// Finally, deleting a group while somebody is still in it must release them. The command's help
/// says "its members lose the quota it applied"; if the membership row outlived the group, an
/// account could be held to a limit nothing describes any more.
///
/// The account is a throwaway created for this test. The harness admin is deliberately never put
/// into a quota group: it is the account every other test binary acts as, and a limit applied to
/// it would surface as unexplained push failures in files that have nothing to do with quota.
#[test]
fn quota_group_membership_reaches_the_accounts_quota_and_is_released_with_the_group() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: [
            "admin quota group add-user",
            "admin quota group remove-user",
            "admin quota group users",
        ],
        hits: [
            "adminAddUserToQuotaGroup",
            "adminRemoveUserFromQuotaGroup",
            "adminListUsersInQuotaGroup",
            "adminGetUserQuota",
            "adminSetUserQuotaGroups",
        ],
    );

    let user = TempUser::create(inst, "q-member");
    let rule = QuotaRule::reserve(inst, "q-member-rule");
    let primary = QuotaGroup::reserve(inst, "q-primary");
    let secondary = QuotaGroup::reserve(inst, "q-secondary");

    inst.fjo([
        "admin",
        "quota",
        "rule",
        "create",
        &rule.name,
        "--bytes",
        &GENEROUS_LIMIT.to_string(),
        "--subject",
        "size:all",
    ])
    .assert_ok("fjo admin quota rule create");
    for group in [&primary, &secondary] {
        inst.fjo(["admin", "quota", "group", "create", &group.name, "--rule", &rule.name])
            .assert_ok("fjo admin quota group create");
    }

    // The account's groups, as the server resolves them.
    let groups_of = |who: &str| -> Vec<String> {
        let run = inst.fjo(["raw", "admin", "get-user-quota", who]);
        run.assert_ok("fjo raw admin get-user-quota");
        let quota = run.json();
        assert!(
            quota["used"]["size"].is_object(),
            "a quota report must say what is in use, not only what is allowed: {quota}"
        );
        quota["groups"]
            .as_array()
            .map(|gs| {
                gs.iter().map(|g| g["name"].as_str().unwrap_or_default().to_owned()).collect()
            })
            .unwrap_or_default()
    };

    assert!(groups_of(&user.name).is_empty(), "a fresh account must not be in a quota group");

    inst.fjo(["admin", "quota", "group", "add-user", &primary.name, &user.name])
        .assert_ok("fjo admin quota group add-user");

    let members = inst.fjo(["admin", "quota", "group", "users", &primary.name, "--json", "login"]);
    members.assert_ok("fjo admin quota group users");
    let logins: Vec<String> = members
        .json()
        .as_array()
        .expect("an array of accounts")
        .iter()
        .map(|u| u["login"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(logins, vec![user.name.clone()], "the group's roll is wrong");

    // Membership must reach the other view too, carrying the rule with it — that is the join no
    // mock performs.
    let run = inst.fjo(["raw", "admin", "get-user-quota", &user.name]);
    run.assert_ok("fjo raw admin get-user-quota");
    let quota = run.json();
    assert_eq!(quota["groups"][0]["name"], primary.name.as_str(), "{quota}");
    assert_eq!(
        quota["groups"][0]["rules"][0]["limit"].as_i64(),
        Some(GENEROUS_LIMIT),
        "the account's quota does not carry the limit its group's rule sets: {quota}"
    );

    // The other group must be unaffected: a handler keyed on the account rather than on the
    // (group, account) pair would have enrolled them in both.
    let (code, other) =
        inst.api("GET", &format!("admin/quota/groups/{}/users", secondary.name), None);
    assert_eq!(code, 200, "{other}");
    let other: serde_json::Value = serde_json::from_str(&other).expect("an array");
    assert_eq!(
        other.as_array().map(Vec::len),
        Some(0),
        "add-user enrolled the account in a group it did not name: {other}"
    );

    inst.fjo(["admin", "quota", "group", "remove-user", &primary.name, &user.name])
        .assert_ok("fjo admin quota group remove-user");
    assert!(groups_of(&user.name).is_empty(), "remove-user left the account in the group");

    // `set` replaces rather than adds, which is the distinction the name is carrying alone.
    inst.fjo([
        "raw",
        "admin",
        "set-user-quota-groups",
        &user.name,
        "--groups",
        &primary.name,
        "--groups",
        &secondary.name,
    ])
    .assert_ok("fjo raw admin set-user-quota-groups");
    let mut both = groups_of(&user.name);
    both.sort();
    let mut want = vec![primary.name.clone(), secondary.name.clone()];
    want.sort();
    assert_eq!(both, want, "set-user-quota-groups did not set the list it was given");

    inst.fjo(["raw", "admin", "set-user-quota-groups", &user.name, "--groups", &secondary.name])
        .assert_ok("fjo raw admin set-user-quota-groups with a shorter list");
    assert_eq!(
        groups_of(&user.name),
        vec![secondary.name.clone()],
        "set-user-quota-groups added to the account's groups instead of replacing them"
    );

    // Deleting a group with a member still in it must release the member.
    inst.fjo(["admin", "quota", "group", "delete", &secondary.name, "--yes"])
        .assert_ok("fjo admin quota group delete with a member still in it");
    assert!(
        groups_of(&user.name).is_empty(),
        "the account is still held by a group that no longer exists"
    );
}

// ---------------------------------------------------------------------------- system hooks

/// The asymmetry no mock would ever reproduce: `POST /admin/hooks` creates a webhook that
/// `GET /admin/hooks` does not return.
///
/// Forgejo's admin listing is filtered to *system* webhooks, while the admin create makes a
/// default one, so a webhook that exists, answers `GET /admin/hooks/{id}`, and accepts a `PATCH`
/// is nevertheless invisible to the command an operator would use to find it. A `FakeTransport`
/// test would have the listing return what the create returned, and the whole trap would stay
/// hidden until somebody could not find a hook they had just made.
///
/// So the lifecycle is driven by id, and the listing is asserted to *not* contain it. The
/// assertion is "this id is absent" rather than "the list is empty" because other test binaries
/// share the instance and may have hooks of their own.
#[test]
fn a_webhook_survives_a_round_trip_but_never_reaches_the_admin_listing() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "adminCreateHook",
        "adminGetHook",
        "adminEditHook",
        "adminListHooks",
        "adminDeleteHook",
    ]);

    // Inactive and pointed at an unroutable host: nothing should ever deliver, and `.invalid`
    // is reserved by RFC 2606 precisely so it cannot resolve.
    let created = inst.fjo([
        "raw",
        "admin",
        "create-hook",
        "--type",
        "forgejo",
        "--active=false",
        "--events",
        "push",
        "--config",
        r#"{"url":"http://fjo-itest-admin.invalid/hook","content_type":"json"}"#,
    ]);
    created.assert_ok("fjo raw admin create-hook");
    let id = created.json()["id"].as_i64().expect("the new hook's id");

    let fetched = inst.fjo(["raw", "admin", "get-hook", &id.to_string()]);
    fetched.assert_ok("fjo raw admin get-hook");
    let fetched = fetched.json();
    assert_eq!(fetched["active"], false, "--active=false did not reach the server");
    assert_eq!(
        fetched["config"]["url"], "http://fjo-itest-admin.invalid/hook",
        "the config object was not stored as sent"
    );

    let edited =
        inst.fjo(["raw", "admin", "edit-hook", &id.to_string(), "--branch-filter", "main"]);
    edited.assert_ok("fjo raw admin edit-hook");
    assert_eq!(
        edited.json()["branch_filter"],
        "main",
        "the PATCH reported success without applying --branch-filter"
    );
    // Read back through a second GET: a reply echoing our own request would look identical.
    let confirmed = inst.fjo(["raw", "admin", "get-hook", &id.to_string()]);
    confirmed.assert_ok("fjo raw admin get-hook after the edit");
    assert_eq!(confirmed.json()["branch_filter"], "main", "the edit did not persist");

    let listed = inst.fjo(["raw", "admin", "list-hooks", "--paginate"]);
    listed.assert_ok("fjo raw admin list-hooks");
    let ids: Vec<i64> = listed
        .json()
        .as_array()
        .expect("an array of hooks")
        .iter()
        .filter_map(|h| h["id"].as_i64())
        .collect();
    assert!(
        !ids.contains(&id),
        "GET /admin/hooks now returns hooks created by POST /admin/hooks. That is a better \
         world, but it is a change in Forgejo's behaviour and the note in this test's \
         documentation — that an admin-created hook is invisible to the admin listing — is now \
         wrong and should be removed. Listed: {ids:?}, created: {id}"
    );

    inst.fjo(["raw", "admin", "delete-hook", &id.to_string()])
        .assert_ok("fjo raw admin delete-hook");

    let gone = inst.fjo(["raw", "admin", "get-hook", &id.to_string()]);
    assert!(!gone.ok(), "the hook should be gone after delete:\n{}", gone.stdout);
    gone.assert_says("HTTP 404");
}

// ------------------------------------------------------------------------------ action jobs

/// Two operations on two paths with the same description, one of them marked deprecated, and
/// nothing in the specification saying whether they are aliases. As with the registration
/// tokens, the server is the only authority, and the answer matters: an operator told to migrate
/// off the deprecated one deserves to know the replacement returns the same thing.
///
/// The harness boots Forgejo with Actions disabled, so both answer `null` rather than a list —
/// which is itself worth pinning, because `null` for an empty collection is the exact shape that
/// makes a decoder expecting `[]` fail at runtime while every mock test passes.
#[test]
fn the_deprecated_and_current_action_job_searches_answer_alike() {
    let inst = instance_or_skip!();
    cover!(raw: ["adminGetActionRunJobs", "adminSearchRunJobs"]);

    let current = inst.fjo(["raw", "admin", "get-action-run-jobs"]);
    current.assert_ok("fjo raw admin get-action-run-jobs");
    let deprecated = inst.fjo(["raw", "admin", "search-run-jobs"]);
    deprecated.assert_ok("fjo raw admin search-run-jobs");

    let current = current.json();
    let deprecated = deprecated.json();
    assert_eq!(
        current, deprecated,
        "the deprecated job search and its replacement now disagree, so the migration advice in \
         the specification would lose results"
    );
    assert!(
        current.is_null() || current.is_array(),
        "a job search must answer null or a list, got {current}"
    );
}

// --------------------------------------------------------------------------------- scopes

/// Forgejo fixes a token's scopes when it is minted and never reports them back, so `fjo` has to
/// infer from a bare `403` which scope was missing. `admin/scope.rs` does that by naming
/// `read:admin` for a read and `write:admin` for a write — and if the server's own division of
/// the admin routes ever differed from ours, the advice would send an operator to mint a token
/// that still does not work. Only a real server can settle where the line is.
///
/// Three credentials, one line each:
///
/// * an admin's token carrying only `read:admin` — reads must work and writes must not;
/// * the same token on a write — the error must name `write:admin`, not `read:admin`;
/// * an ordinary account's token with no admin scope at all — refused on a read.
///
/// The third is the one that proves site-administrator status and token scope are independent
/// checks: that account is not an administrator *and* its token lacks the scope, and `fjo` must
/// still produce an actionable message rather than a bare "forbidden".
///
/// Exit code 4 throughout, because a script distinguishing "your credential is wrong" from
/// "the thing you asked for is not there" (5) keys off exactly that.
#[test]
fn a_read_only_admin_token_may_list_but_may_not_create() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["admin user list", "admin cron list", "admin user create"],
        hits: ["adminCreateUserAccessToken"],
    );

    let minted = inst.fjo([
        "raw",
        "admin",
        "create-user-access-token",
        &inst.user,
        "--name",
        "fjo-itest-readonly-admin",
        "--scopes",
        "read:admin",
    ]);
    minted.assert_ok("minting a read-only admin token");
    let read_only = minted.json()["sha1"].as_str().expect("a secret").to_owned();

    inst.fjo_as(&read_only, ["admin", "user", "list", "--json", "login"])
        .assert_ok("admin user list with a read:admin token");
    inst.fjo_as(&read_only, ["admin", "cron", "list", "--json", "name"])
        .assert_ok("admin cron list with a read:admin token");

    let blocked = inst.unique_repo_name("adm-scope");
    let refused = inst.fjo_as(
        &read_only,
        [
            "admin",
            "user",
            "create",
            &blocked,
            "--email",
            &format!("{blocked}@example.invalid"),
            "--password",
            "fjo-itest-admin-pass-1",
            "--no-must-change-password",
        ],
    );
    refused.assert_code(4, "admin user create with a read-only admin token");
    refused.assert_says("write:admin");

    // The refusal has to have been real: nothing may have been created.
    let (code, body) = inst.api("GET", &format!("users/{blocked}"), None);
    assert_eq!(code, 404, "a refused create still made an account: HTTP {code}: {body}");

    // No admin scope at all, and not an administrator either. The message must still name the
    // scope to ask for, since that is the only thing the reader can act on.
    let outsider = inst
        .scoped_user("admoutsider", &["read:repository"])
        .expect("a second account with a narrow token");
    let refused = inst.fjo_as(&outsider.token, ["admin", "user", "list"]);
    refused.assert_code(4, "admin user list with no admin scope");
    refused.assert_says("read:admin");

    let (code, body) =
        inst.api("DELETE", &format!("admin/users/{}?purge=true", outsider.name), None);
    assert!(
        (200..300).contains(&code),
        "could not clean up {}: HTTP {code}: {body}",
        outsider.name
    );
}
