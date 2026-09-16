//! `fjo block` — the users you (or an organization you run) have blocked.
//!
//! `gh` has nothing here at all. Forgejo has two parallel collections, and `--org` is what picks
//! between them:
//!
//! ```text
//! /user/list_blocked  ·  PUT /user/block/{username}  ·  PUT /user/unblock/{username}
//! /orgs/{org}/list_blocked  ·  PUT /orgs/{org}/block/{u}  ·  PUT /orgs/{org}/unblock/{u}
//! ```
//!
//! Note that *unblocking* is a `PUT` to a different path rather than a `DELETE`, which is why
//! `remove` cannot be guessed from `add`.
//!
//! # `list` resolves the names, because the API does not
//!
//! `GET /user/list_blocked` answers with `[{"block_id": 4, "created_at": …}]` — and that is the
//! whole model, in the specification and in the server. `block_id` is the blocked account's user
//! id, so the raw response tells you *that* you blocked somebody and never *who*, which makes the
//! endpoint almost unusable on its own. That is the entire justification for this being a
//! porcelain command: `list` follows each id through `GET /users/search?uid=N` and prints logins.
//!
//! Two consequences worth knowing:
//!
//! * It is one extra request per blocked account. `list` is not a hot path and a blocklist is
//!   short, so that is the right trade; `--json` skips the resolution entirely and gives you the
//!   API's own shape, because `--json` promises the API's field names and inventing a `login` key
//!   would break that promise (`docs/output.md`).
//!
//!   The *count* is not negotiable — there is no batch route and `UserSearchQuery` takes a single
//!   `uid` — but the *waiting* is. The lookups are independent reads of one server, so `list`
//!   keeps [`LOOKUPS_AT_ONCE`] of them in flight instead of paying a full round trip per row:
//!   a blocklist of twenty against a WAN instance is three waves rather than twenty waits.
//! * A lookup that fails — the account was deleted, or the token cannot see it — leaves the id in
//!   place rather than dropping the row. A blocklist that silently omits entries would be worse
//!   than one with a bare number in it.

use clap::{Args as ClapArgs, Subcommand};
use forgejo_client::Api;
use forgejo_client::forgejo_model::BlockedUser;
use forgejo_core::error::Result;
use forgejo_core::http::Paging;

use crate::cmd::support::{self, Emit, Json};
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

const OP_LIST: &str = "userListBlockedUsers";

/// How many `GET /users/search?uid=N` lookups `list` keeps in flight at once.
///
/// Bounded rather than "all of them": a self-hosted instance behind a modest reverse proxy answers
/// a wide burst with 429s, and the retry layer then spends back — with backoff on top — exactly the
/// latency the concurrency won. Eight is deep enough that a typical blocklist finishes in one or
/// two waves and shallow enough that no proxy notices.
const LOOKUPS_AT_ONCE: usize = 8;

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List blocked accounts, resolving each id to a login
    List(Scope),
    /// Block an account
    Add(Who),
    /// Unblock an account
    Remove(Who),
}

/// Whose blocklist: yours, or an organization's.
#[derive(Debug, Clone, ClapArgs)]
pub struct Scope {
    /// Act on this organization's blocklist instead of your own account's
    #[arg(long, value_name = "NAME")]
    pub org: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct Who {
    /// The account to block or unblock
    #[arg(value_name = "USER")]
    pub user: String,

    #[command(flatten)]
    pub scope: Scope,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let fields = match &args.command {
        Cmd::List(_) => match Json::resolve(globals, OP_LIST)? {
            Json::Listed => return Ok(()),
            Json::Fields(f) => f,
        },
        // Blocking and unblocking answer 204.
        _ => None,
    };

    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        let mut stdout = std::io::stdout().lock();
        let mut emit = Emit::new(globals, fields, rt.term(), &mut stdout)?;

        match &args.command {
            Cmd::List(scope) => list(&api, scope.org.as_deref(), globals, &mut emit).await,
            Cmd::Add(w) => {
                match &w.scope.org {
                    Some(org) => api.org().block_user(org, &w.user).await?,
                    None => api.user().block_user(&w.user).await?,
                }
                emit.done(&format!("blocked {} {}", &w.user, whose(w.scope.org.as_deref())));
                Ok(())
            }
            Cmd::Remove(w) => {
                match &w.scope.org {
                    Some(org) => api.org().unblock_user(org, &w.user).await?,
                    None => api.user().unblock_user(&w.user).await?,
                }
                emit.done(&format!("unblocked {} {}", &w.user, whose(w.scope.org.as_deref())));
                Ok(())
            }
        }
    })
}

async fn list(
    api: &Api,
    org: Option<&str>,
    globals: &GlobalOpts,
    emit: &mut Emit<'_>,
) -> Result<()> {
    use futures::StreamExt;

    let cap = support::item_cap(globals);
    let (blocked, total) = match (org, globals.paginate) {
        (Some(org), true) => {
            let q = forgejo_client::query::OrgListBlockedUsersQuery::default();
            let v = support::drain(api.org().list_blocked_users(org, &q), cap).await?;
            let n = v.len() as u64;
            (v, Some(n))
        }
        (Some(org), false) => {
            let q = forgejo_client::query::OrgListBlockedUsersQuery::default();
            let (v, info) = api
                .org()
                .list_blocked_users_page(org, &q, Paging { limit: cap, per_page: None })
                .await?;
            (v, info.total_count)
        }
        (None, true) => {
            let q = forgejo_client::query::UserListBlockedUsersQuery::default();
            let v = support::drain(api.user().list_blocked_users(&q), cap).await?;
            let n = v.len() as u64;
            (v, Some(n))
        }
        (None, false) => {
            let q = forgejo_client::query::UserListBlockedUsersQuery::default();
            let (v, info) = api
                .user()
                .list_blocked_users_page(&q, Paging { limit: cap, per_page: None })
                .await?;
            (v, info.total_count)
        }
    };

    // Under `--json`/`--jq`/`--template` the API's own shape is what the caller asked for, and
    // resolving names would neither be visible nor honest.
    if emit.machine() {
        return emit.json(&blocked);
    }

    // `buffered`, never `buffer_unordered`: the rows are consumed positionally just below, so the
    // printed order is the order the server listed the blocklist in. `buffer_unordered` would hand
    // back whichever lookup answered first, which turns a stable list into one that shuffles
    // itself between invocations depending on how the server felt.
    let rows: Vec<(String, String)> = futures::stream::iter(&blocked)
        .map(|b| async move { (login_of(api, b).await, created(b)) })
        .buffered(LOOKUPS_AT_ONCE)
        .collect()
        .await;
    emit.many(&blocked, total, "blocked accounts", |table, _| {
        table.headers(["USER", "SINCE"]);
        for (login, since) in &rows {
            table.row([login.clone(), since.clone()]);
        }
    })
}

/// The blocked account's login, or its bare id when the lookup cannot answer.
///
/// A failed lookup is not propagated: `block list` failing entirely because one blocked account
/// has since been deleted would be a worse outcome than a row that shows `#4`.
async fn login_of(api: &Api, b: &BlockedUser) -> String {
    let q = forgejo_client::query::UserSearchQuery::default().with_uid(b.block_id);
    match api.user().search(&q).await {
        Ok(found) => match found.data.first() {
            Some(u) if !u.login.is_empty() => u.login.clone(),
            _ => format!("#{}", b.block_id),
        },
        Err(_) => format!("#{}", b.block_id),
    }
}

fn created(b: &BlockedUser) -> String {
    b.created_at.as_ref().map(ToString::to_string).unwrap_or_default()
}

fn whose(org: Option<&str>) -> String {
    match org {
        Some(org) => format!("for {org}"),
        None => "for your account".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use forgejo_core::http::FakeTransport;
    use forgejo_core::http::transport::Canned;
    use std::sync::Arc;

    const BLOCKED: &str = r#"[{"block_id":4,"created_at":"2024-03-01T09:00:00Z"},
                              {"block_id":9,"created_at":"2024-03-02T09:00:00Z"}]"#;

    fn resolver(fake: FakeTransport) -> FakeTransport {
        testing::on_fn(fake, "GET", "/api/v1/users/search", |call| {
            match call.query_param("uid") {
                Some("4") => Canned::json(200, r#"{"ok":true,"data":[{"login":"spammer"}]}"#),
                // The second account has been deleted since it was blocked.
                _ => Canned::json(200, r#"{"ok":true,"data":[]}"#),
            }
        })
    }

    /// Bug this prevents: `--org` and the bare form sharing a path, so blocking somebody for an
    /// organization blocks them for you instead. Also pins the surprise that *unblocking* is a
    /// `PUT`, not a `DELETE`.
    #[tokio::test]
    async fn the_org_and_account_scopes_use_different_paths() {
        let fake = testing::on(
            FakeTransport::new(),
            "PUT",
            "/api/v1/user/block/mallory",
            testing::empty(),
        );
        let fake = testing::on(fake, "PUT", "/api/v1/user/unblock/mallory", testing::empty());
        let fake = testing::on(fake, "PUT", "/api/v1/orgs/acme/block/mallory", testing::empty());
        let fake = Arc::new(testing::on(
            fake,
            "PUT",
            "/api/v1/orgs/acme/unblock/mallory",
            testing::empty(),
        ));
        let api = testing::api(fake.clone());

        api.user().block_user("mallory").await.unwrap();
        api.user().unblock_user("mallory").await.unwrap();
        api.org().block_user("acme", "mallory").await.unwrap();
        api.org().unblock_user("acme", "mallory").await.unwrap();

        let seen: Vec<String> =
            fake.calls().into_iter().map(|c| format!("{} {}", c.method.as_str(), c.path)).collect();
        insta::assert_snapshot!(seen.join("\n"));
    }

    /// The reason this command exists: the API answers with ids only, so a human `list` that did
    /// not resolve them would print `4` and `9` and be useless.
    #[tokio::test]
    async fn list_turns_block_ids_into_logins() {
        let fake = testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/user/list_blocked",
            Canned::json(200, BLOCKED),
        );
        let fake = Arc::new(resolver(fake));
        let api = testing::api(fake);
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
            list(&api, None, &globals, &mut emit).await.unwrap();
        }
        // The deleted account keeps its id rather than vanishing from the list.
        insta::assert_snapshot!(String::from_utf8(buf).unwrap());
    }

    /// Bug this prevents: the concurrent id lookups deciding the row order.
    ///
    /// The lookups run `LOOKUPS_AT_ONCE` at a time, so the answers no longer arrive in the order
    /// they were asked for. `buffered` re-serialises them onto the listing's order; a
    /// `buffer_unordered` would print whichever account the server answered for first, so the
    /// same blocklist would render differently from one invocation to the next.
    ///
    /// Ten accounts — more than one `buffered` wave, so the wave boundary is covered — whose ids
    /// are shuffled and whose logins sort in the opposite direction to their ids. The expected
    /// order is therefore neither id order nor alphabetical order, only the listing's own, so a
    /// row/lookup mis-zip or any ordering rule keyed on the *data* fails here.
    ///
    /// What it honestly cannot catch is completion-order reordering: `FakeTransport` answers
    /// synchronously, so every lookup is ready on its first poll and `buffer_unordered` would
    /// happen to drain in input order too. That half of the property needs a fake that can be
    /// slow; `auth status` has one (a host nothing listens on) and pins it there.
    #[tokio::test]
    async fn the_rendered_rows_follow_the_listing_order_not_the_lookup_order() {
        // `login_for` makes a uid's login sort against its uid: uid 20 becomes u79, uid 1 u98.
        fn login_for(uid: u64) -> String {
            format!("u{:02}", 99 - uid)
        }
        const UIDS: [u64; 10] = [20, 1, 13, 4, 17, 8, 11, 2, 19, 6];

        let listing: String = {
            let rows: Vec<String> = UIDS
                .iter()
                .map(|u| format!(r#"{{"block_id":{u},"created_at":"2024-03-01T09:00:00Z"}}"#))
                .collect();
            format!("[{}]", rows.join(","))
        };
        let fake = testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/user/list_blocked",
            Canned::json(200, listing),
        );
        let fake = Arc::new(testing::on_fn(fake, "GET", "/api/v1/users/search", |call| {
            let uid: u64 = call.query_param("uid").expect("a uid").parse().expect("a number");
            Canned::json(200, format!(r#"{{"ok":true,"data":[{{"login":"{}"}}]}}"#, login_for(uid)))
        }));
        let api = testing::api(fake.clone());
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
            list(&api, None, &globals, &mut emit).await.unwrap();
        }
        let out = String::from_utf8(buf).unwrap();
        let printed: Vec<&str> = out.lines().filter_map(|l| l.split('\t').next()).collect();
        let expected: Vec<String> = UIDS.iter().copied().map(login_for).collect();
        assert_eq!(printed, expected, "rows must follow the listing, not the lookups:\n{out}");
        // Still exactly one lookup per blocked account: concurrency changed the waiting, not the
        // request count the module doc justifies.
        assert_eq!(fake.call_count(), 1 + UIDS.len());
    }

    /// Bug this prevents: inventing a `login` key in `--json` output. `--json` promises the API's
    /// own field names, so the machine path must show `block_id` and make no extra request.
    #[tokio::test]
    async fn json_output_keeps_the_apis_own_shape_and_resolves_nothing() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/user/list_blocked",
            Canned::json(200, BLOCKED),
        ));
        let api = testing::api(fake.clone());
        let globals = GlobalOpts { json: Some("block_id".to_owned()), ..GlobalOpts::default() };
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit = Emit::new(
                &globals,
                Some(vec!["block_id".to_owned()]),
                &crate::output::Term::piped(),
                &mut buf,
            )
            .unwrap();
            list(&api, None, &globals, &mut emit).await.unwrap();
        }
        assert_eq!(String::from_utf8(buf).unwrap(), "[{\"block_id\":4},{\"block_id\":9}]\n");
        assert_eq!(fake.calls().len(), 1, "no id lookups under --json: {:?}", fake.calls());
    }
}
