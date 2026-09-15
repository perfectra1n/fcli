//! `fcli mirror` — push and pull mirrors.
//!
//! # Two different features with one name
//!
//! Forgejo calls both of these "mirroring" and they behave nothing alike:
//!
//! | | pull mirror | push mirror |
//! | --- | --- | --- |
//! | direction | remote → here | here → remote |
//! | how many | **one**, and only settable **when the repository is created** | any number |
//! | the repository | read-only; you cannot push to it | a normal repository |
//! | API | `POST /repos/migrate` with `mirror: true` | `POST …/push_mirrors` |
//!
//! The asymmetry is the thing to communicate, because it is not a limitation anyone expects:
//! **a pull mirror cannot be added to an existing repository.** There is no endpoint for it. It
//! belongs to `fcli repo create --mirror-from`, which is another wave's file — so this group's
//! help names that command rather than leaving the user to conclude the feature is missing.
//!
//! # What a push mirror actually does
//!
//! Four behaviours that are not obvious from the API's field names, all surfaced in `--help` and
//! in the `list` table:
//!
//! * **It is a force push.** The remote's history is replaced, not merged. A push mirror pointed
//!   at a repository someone else commits to will discard their commits.
//! * **No branch filter means `git push --mirror`**: every branch and every tag, and refs deleted
//!   here are deleted there. A filter narrows it to a comma-separated list of globs
//!   (`main, release/*`), and then only those refs are touched.
//! * **`--sync-on-commit`** pushes on every commit instead of only on the interval.
//! * **LFS is not mirrored over SSH.** Forgejo's LFS transfer needs the HTTP endpoint, so a
//!   mirror configured with `--ssh` silently leaves LFS objects behind. That is a data-loss
//!   surprise for anyone using a push mirror as a backup, so `add --ssh` says so out loud.

use clap::{Args as ClapArgs, Subcommand};
use forgejo_client::Api;
use forgejo_core::error::{Error, ErrorKind, Result};
use forgejo_core::types::RepoSlug;
use forgejo_model::{PushMirror, Repository};
use futures::StreamExt;

use crate::cmd::times::duration;
use crate::cmd::times::porcelain::{self, Fields, Machine};
use crate::global::GlobalOpts;
use crate::output::Term;
use crate::runtime::Runtime;

const MIRROR_FIELDS: Fields = Fields::Generated(forgejo_client::fields::FIELDS_PUSH_MIRROR);

const LONG_ABOUT: &str = "\
Manage push mirrors and sync existing pull mirrors.

A push mirror copies this repository to a remote. It force-pushes changes and can
overwrite remote history. Without --branch-filter, it mirrors all branches, tags,
and deletions. Filters are comma-separated globs, such as 'main, release/*'.
--sync-on-commit syncs on each commit as well as at the configured interval.
Use HTTPS to mirror LFS objects; SSH mirroring does not include them.

A pull mirror is read-only and must be set up when creating the repository:
`fcli repo create <name> --mirror-from <url>`. Once created, use `mirror sync`
and `mirror status` to manage it.

  fcli repo create <name> --mirror-from <url>
  fcli mirror list
  fcli mirror add https://github.com/me/proj.git --username me --interval 8h
  fcli mirror add https://codeberg.org/me/proj.git --branch-filter 'main, release/*'
  fcli mirror sync
  fcli mirror status";

#[derive(Debug, ClapArgs)]
#[command(long_about = LONG_ABOUT)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List this repository's push mirrors
    List,
    /// Add a push mirror
    Add(AddArgs),
    /// Remove a push mirror
    Delete(DeleteArgs),
    /// Sync now, rather than waiting for the interval
    Sync(SyncArgs),
    /// Mirror configuration and the last result for each
    Status,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Add a push mirror.

Syncs force-push this repository to the remote. Commits that exist only on the
remote will be lost. Without --branch-filter, all branches, tags, and deletions
are mirrored. Use comma-separated globs to filter branches.

Pass --username and enter the password at the prompt. Passwords in URLs can be
saved in shell history and Forgejo's stored remote. Use HTTPS for LFS support.

  fcli mirror add https://github.com/me/proj.git --username me
  fcli mirror add git@codeberg.org:me/proj.git --ssh
  fcli mirror add https://git.example.org/me/proj.git --branch-filter 'main, release/*'  --interval 30m --sync-on-commit")]
pub struct AddArgs {
    /// Where to push: an https:// or ssh:// URL, or git@host:owner/name
    #[arg(value_name = "ADDRESS")]
    pub address: String,

    /// How often to sync: 8h, 30m, 10m0s. `0` disables scheduled syncing
    #[arg(long, value_name = "DURATION")]
    pub interval: Option<String>,

    /// Comma-separated globs; omit to mirror every branch and tag
    #[arg(long, value_name = "GLOBS")]
    pub branch_filter: Option<String>,

    /// Also push whenever a commit arrives, not only on the interval
    #[arg(long)]
    pub sync_on_commit: bool,

    /// Authenticate with the repository's SSH key rather than a password. LFS is not mirrored
    /// over SSH
    #[arg(long)]
    pub ssh: bool,

    /// Username on the remote
    #[arg(long, value_name = "USER")]
    pub username: Option<String>,

    /// Password or token; prompted for when --username is given and this is not
    #[arg(long, value_name = "SECRET")]
    pub password: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct DeleteArgs {
    /// The mirror's remote name, from the REMOTE column of `fcli mirror list`
    #[arg(value_name = "REMOTE")]
    pub remote: String,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Sync mirrors now.

Without flags, syncs both pull and push mirrors for this repository.
Forgejo syncs all push mirrors together; individual mirrors cannot be selected.")]
pub struct SyncArgs {
    /// Only the push mirrors (this repository → remotes)
    #[arg(long)]
    pub push: bool,

    /// Only the pull mirror (remote → this repository)
    #[arg(long, conflicts_with = "push")]
    pub pull: bool,
}

impl Cmd {
    fn fields(&self) -> Option<Fields> {
        match self {
            Self::List | Self::Add(_) | Self::Status => Some(MIRROR_FIELDS),
            // Both are 204s.
            Self::Delete(_) | Self::Sync(_) => None,
        }
    }
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    if let Some(fields) = args.command.fields()
        && porcelain::discovery(globals, fields)?
    {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        let slug = rt.repo(globals)?.slug.clone();
        match &args.command {
            Cmd::List => list(&rt, &api, globals, &slug).await,
            Cmd::Add(a) => add(&rt, &api, globals, &slug, a).await,
            Cmd::Delete(a) => delete(&rt, &api, &slug, a).await,
            Cmd::Sync(a) => sync(&rt, &api, &slug, a).await,
            Cmd::Status => status(&rt, &api, globals, &slug).await,
        }
    })
}

// -------------------------------------------------------------------------------------- list

async fn list(rt: &Runtime, api: &Api, globals: &GlobalOpts, slug: &RepoSlug) -> Result<()> {
    let mirrors = fetch(api, globals, slug).await?;
    let machine = Machine::compile(globals, MIRROR_FIELDS)?;
    if mirrors.is_empty() {
        return porcelain::empty(
            globals,
            rt.term(),
            machine.as_ref(),
            &format!(
                "{slug} has no push mirrors. Add one with `fcli mirror add <address>`. For a new pull mirror, use `fcli repo create --mirror-from`."
            ),
        );
    }
    if let Some(m) = machine {
        return m.write(globals, rt.term(), porcelain::json_of(&mirrors)?);
    }

    porcelain::print(globals, &render_list(rt.term(), &mirrors))?;
    if mirrors.iter().any(|m| !m.last_error.trim().is_empty()) {
        porcelain::note(
            rt.term(),
            "A mirror with an ERROR is not retrying on its own schedule until the cause is fixed; \
             `fcli mirror sync --push` retries now.",
        );
    }
    Ok(())
}

/// The `list` table.
pub(crate) fn render_list(term: &Term, mirrors: &[PushMirror]) -> String {
    let mut t = porcelain::table(term);
    t.headers(["REMOTE", "ADDRESS", "BRANCHES", "INTERVAL", "ON COMMIT", "LAST SYNC", "ERROR"]);
    for m in mirrors {
        t.row([
            porcelain::dash(&m.remote_name),
            porcelain::dash(&m.remote_address),
            branches(&m.branch_filter),
            porcelain::dash(&m.interval),
            if m.sync_on_commit { "yes".to_owned() } else { "no".to_owned() },
            porcelain::when(m.last_update),
            porcelain::dash(&first_line(&m.last_error)),
        ]);
    }
    porcelain::rendered_table(term, t, "push mirrors", None)
}

/// `(all branches and tags)` rather than an empty cell.
///
/// An empty `branch_filter` is not "no branches" — it is *every* branch and tag, which is the
/// opposite reading and the one a blank cell invites.
fn branches(filter: &str) -> String {
    if filter.trim().is_empty() { "(all, --mirror)".to_owned() } else { filter.to_owned() }
}

// --------------------------------------------------------------------------------------- add

async fn add(
    rt: &Runtime,
    api: &Api,
    globals: &GlobalOpts,
    slug: &RepoSlug,
    args: &AddArgs,
) -> Result<()> {
    if let Some(warning) = credentials_in_address(&args.address) {
        porcelain::note(rt.term(), &warning);
    }
    if args.ssh {
        // Said before the call, not after: someone mirroring an LFS repository as a backup needs
        // to know this while they can still choose an https:// address.
        porcelain::note(
            rt.term(),
            "note: SSH mirroring excludes LFS objects. Use an https:// address to include them.",
        );
    }

    let interval = match &args.interval {
        Some(text) => Some(go_interval(text)?),
        None => None,
    };
    let password = match (&args.username, &args.password) {
        (Some(user), None) => Some(porcelain::ask_secret(
            rt,
            &format!("Password or token for {user} on the remote"),
            "--password (or omit --username for an unauthenticated remote)",
        )?),
        (_, given) => given.clone(),
    };

    let body = forgejo_model::CreatePushMirrorOption {
        branch_filter: args.branch_filter.clone(),
        interval,
        remote_address: Some(args.address.clone()),
        remote_password: password,
        remote_username: args.username.clone(),
        sync_on_commit: Some(args.sync_on_commit),
        use_ssh: Some(args.ssh),
    };
    let mirror =
        api.repo().add_push_mirror(&slug.owner, &slug.name, &body).await.map_err(explain)?;

    porcelain::note(
        rt.term(),
        &format!(
            "Mirroring {slug} to {} as {}: {}, every {}",
            porcelain::dash(&mirror.remote_address),
            porcelain::dash(&mirror.remote_name),
            if mirror.branch_filter.trim().is_empty() {
                "all branches and tags (force push, like git push --mirror)".to_owned()
            } else {
                format!("branches matching {} (force push)", mirror.branch_filter)
            },
            porcelain::dash(&mirror.interval)
        ),
    );
    match Machine::compile(globals, MIRROR_FIELDS)? {
        Some(m) => m.write(globals, rt.term(), porcelain::json_of(&mirror)?),
        None => Ok(()),
    }
}

/// A warning when the address embeds credentials.
///
/// Forgejo stores the remote address verbatim, so a password in the URL is persisted server-side
/// *and* is now in the user's shell history. `--username`/`--password` keep it out of both.
fn credentials_in_address(address: &str) -> Option<String> {
    let after_scheme = address.split_once("://").map(|(_, rest)| rest).unwrap_or(address);
    let authority = after_scheme.split('/').next().unwrap_or("");
    // `git@host:owner/name` is SSH's own syntax and carries no secret; a colon before the `@` is
    // what makes it a password.
    let (userinfo, _) = authority.split_once('@')?;
    if !userinfo.contains(':') {
        return None;
    }
    Some(
        "note: this URL contains a password that may be saved in shell history and Forgejo. Use --username and enter the password at the prompt instead."
            .to_owned(),
    )
}

/// Validate an interval and render it the way Go's `time.ParseDuration` reads back.
///
/// Forgejo parses the field with `time.ParseDuration` and rejects anything below its configured
/// `MIN_INTERVAL` (ten minutes by default). Reusing the [`duration`] grammar means `--interval 8h`
/// and `--interval 30m` both work, and re-emitting the canonical `8h0m0s` form avoids depending on
/// which spellings that parser accepts.
fn go_interval(text: &str) -> Result<String> {
    let nanos = duration::parse_nanos(text)?;
    let seconds = (nanos / 1_000_000_000) as i64;
    if seconds == 0 {
        // Forgejo reads an empty interval as "never on a schedule"; `0` is how a user asks for
        // that, and `0s` is what Go's parser accepts for it.
        return Ok("0s".to_owned());
    }
    if seconds < 0 {
        return Err(porcelain::usage("a mirror interval cannot be negative"));
    }
    Ok(format!("{}h{}m{}s", seconds / 3_600, seconds % 3_600 / 60, seconds % 60))
}

// ------------------------------------------------------------------------------------ delete

async fn delete(rt: &Runtime, api: &Api, slug: &RepoSlug, args: &DeleteArgs) -> Result<()> {
    porcelain::confirm(rt, &format!("Stop mirroring {slug} to {}?", args.remote), args.yes)?;
    api.repo()
        .delete_push_mirror(&slug.owner, &slug.name, &args.remote)
        .await
        .map_err(|e| explain_named(e, &args.remote))?;
    porcelain::note(rt.term(), &format!("Removed push mirror {}", args.remote));
    Ok(())
}

// -------------------------------------------------------------------------------------- sync

async fn sync(rt: &Runtime, api: &Api, slug: &RepoSlug, args: &SyncArgs) -> Result<()> {
    // Which endpoints apply is a property of the repository, so read it first rather than making
    // the user know. `mirror-sync` on a repository that is not a pull mirror is an error, and
    // `push_mirrors-sync` on one with no push mirrors does nothing quietly.
    let repo: Repository = api.repo().get(&slug.owner, &slug.name).await.map_err(explain)?;
    let mirrors = api
        .repo()
        .list_push_mirrors(&slug.owner, &slug.name, &Default::default())
        .take(1)
        .collect::<Vec<_>>()
        .await;
    let has_push = mirrors.first().is_some_and(std::result::Result::is_ok);

    let do_pull = args.pull || (!args.push && repo.mirror);
    let do_push = args.push || (!args.pull && has_push);

    if !do_pull && !do_push {
        return Err(porcelain::usage(format!(
            "{slug} has no mirrors to sync. Add a push mirror with `fcli mirror add <address>`. Pull mirrors must be created with `fcli repo create --mirror-from`."
        )));
    }

    if do_pull {
        if !repo.mirror {
            return Err(porcelain::usage(format!(
                "{slug} is not a pull mirror. Pull mirroring must be enabled when creating the repository."
            )));
        }
        api.repo().mirror_sync(&slug.owner, &slug.name).await.map_err(explain)?;
        porcelain::note(
            rt.term(),
            &format!(
                "Queued a pull-mirror sync of {slug} from {}",
                porcelain::dash(&repo.original_url)
            ),
        );
    }
    if do_push {
        api.repo().push_mirror_sync(&slug.owner, &slug.name).await.map_err(explain)?;
        porcelain::note(
            rt.term(),
            &format!(
                "Queued a push to all mirrors of {slug}. Check results with `fcli mirror list`."
            ),
        );
    }
    Ok(())
}

// ------------------------------------------------------------------------------------ status

async fn status(rt: &Runtime, api: &Api, globals: &GlobalOpts, slug: &RepoSlug) -> Result<()> {
    let repo: Repository = api.repo().get(&slug.owner, &slug.name).await.map_err(explain)?;
    let mirrors = fetch(api, globals, slug).await?;

    if let Some(m) = Machine::compile(globals, MIRROR_FIELDS)? {
        return m.write(globals, rt.term(), porcelain::json_of(&mirrors)?);
    }

    porcelain::print(globals, &render_status(rt.term(), slug, &repo, &mirrors))
}

/// The `status` detail view: the pull mirror (or the fact that there is none, and why one cannot
/// be added), then every push mirror with the four behaviours that surprise people.
pub(crate) fn render_status(
    term: &Term,
    slug: &RepoSlug,
    repo: &Repository,
    mirrors: &[PushMirror],
) -> String {
    // `let _ =` throughout: `fmt::Write` on a `String` cannot fail.
    use std::fmt::Write as _;
    let mut o = String::new();

    if !term.tty {
        // One line per mirror, prefixed by direction, so both kinds share one stable shape:
        // `direction<TAB>name<TAB>address<TAB>branches<TAB>interval<TAB>error`.
        if repo.mirror {
            let _ = writeln!(o, "pull\t-\t{}\t-\t{}\t", repo.original_url, repo.mirror_interval);
        }
        for m in mirrors {
            let _ = writeln!(
                o,
                "push\t{}\t{}\t{}\t{}\t{}",
                m.remote_name,
                m.remote_address,
                m.branch_filter,
                m.interval,
                first_line(&m.last_error)
            );
        }
        return o;
    }

    let _ = writeln!(o, "{slug}");
    let _ = writeln!(o);
    if repo.mirror {
        let _ = writeln!(o, "pull mirror  (this repository is read-only)");
        let _ = writeln!(o, "  from      {}", porcelain::dash(&repo.original_url));
        let _ = writeln!(o, "  interval  {}", porcelain::dash(&repo.mirror_interval));
        let _ = writeln!(o, "  last      {}", porcelain::when(repo.mirror_updated));
    } else {
        let _ = writeln!(o, "pull mirror  none");
        let _ = writeln!(
            o,
            "  Forgejo can only make a repository a pull mirror when it is created:\n  \
             `fcli repo create <name> --mirror-from <url>`."
        );
    }
    let _ = writeln!(o);
    if mirrors.is_empty() {
        let _ = writeln!(o, "push mirrors none  (`fcli mirror add <address>`)");
        return o;
    }
    let _ = writeln!(o, "push mirrors  {} (force push)", mirrors.len());
    for m in mirrors {
        let _ = writeln!(o);
        let _ = writeln!(o, "  {}", porcelain::dash(&m.remote_name));
        let _ = writeln!(o, "    to        {}", porcelain::dash(&m.remote_address));
        let _ = writeln!(o, "    branches  {}", branches(&m.branch_filter));
        let _ = writeln!(o, "    interval  {}", porcelain::dash(&m.interval));
        let _ = writeln!(o, "    on commit {}", if m.sync_on_commit { "yes" } else { "no" });
        let _ = writeln!(o, "    last sync {}", porcelain::when(m.last_update));
        if !m.last_error.trim().is_empty() {
            let _ = writeln!(o, "    error     {}", first_line(&m.last_error));
        }
        if !m.public_key.trim().is_empty() {
            // The key the *remote* has to authorise. Nothing else in the tool prints it, and a
            // mirror configured with --ssh does not work until it is installed there.
            let _ = writeln!(o, "    ssh key   {}", m.public_key.trim());
        }
    }
    o
}

// ------------------------------------------------------------------------------------ shared

async fn fetch(api: &Api, globals: &GlobalOpts, slug: &RepoSlug) -> Result<Vec<PushMirror>> {
    let query = forgejo_client::query::RepoListPushMirrorsQuery::default();
    let take = porcelain::item_limit(globals).unwrap_or(usize::MAX);
    let mut stream = api.repo().list_push_mirrors(&slug.owner, &slug.name, &query).take(take);
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        out.push(item.map_err(explain)?);
    }
    Ok(out)
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_owned()
}

/// Push-mirror endpoints 404 when the *instance* has mirroring switched off, which is
/// indistinguishable from a missing repository by status alone.
fn explain(e: Error) -> Error {
    match &*e.kind {
        ErrorKind::RouteNotFound { .. } => Error::new(ErrorKind::Usage(
            "this instance did not answer the mirror endpoint. Mirroring can be switched off \
             instance-wide with [mirror] ENABLED = false (and push mirrors separately with \
             ALLOW_PUSH_MIRRORS = false) — ask an administrator, or check `fcli nodeinfo`."
                .to_owned(),
        )),
        _ => e,
    }
}

fn explain_named(e: Error, remote: &str) -> Error {
    match &*e.kind {
        ErrorKind::ResourceNotFound { .. } => Error::new(ErrorKind::Usage(format!(
            "no push mirror named {remote:?}. Use the generated name from the REMOTE column of `fcli mirror list`."
        ))),
        _ => explain(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::times::porcelain::testing;
    use forgejo_core::http::transport::{Canned, FakeTransport};
    use std::sync::Arc;

    const MIRRORS: &str = r#"[
      {"remote_name":"remote_mirror_abc123",
       "remote_address":"https://github.com/me/proj.git",
       "branch_filter":"","interval":"8h0m0s","sync_on_commit":true,
       "last_error":"","last_update":null,"repo_name":"proj","public_key":""},
      {"remote_name":"remote_mirror_def456",
       "remote_address":"https://codeberg.org/me/proj.git",
       "branch_filter":"main, release/*","interval":"10m0s","sync_on_commit":false,
       "last_error":"authentication required\nsee the log","last_update":null,
       "repo_name":"proj","public_key":""}
    ]"#;

    fn mirrors() -> Vec<PushMirror> {
        serde_json::from_str(MIRRORS).expect("the fixture is valid PushMirror JSON")
    }

    /// Bug this prevents: `add` putting the branch filter, the interval, or `use_ssh` in the wrong
    /// field — a mirror configured with `sync_on_commit` where the user asked for a branch filter
    /// force-pushes every ref on every commit, which is a data-loss shape.
    #[tokio::test]
    async fn add_posts_every_option_under_the_apis_own_field_name() {
        let fake = Arc::new(FakeTransport::new().on(
            testing::method("POST"),
            "/api/v1/repos/them/proj/push_mirrors",
            Canned::json(
                201,
                r#"{"remote_name":"remote_mirror_abc123",
              "remote_address":"https://github.com/me/proj.git",
              "branch_filter":"main, release/*","interval":"0h30m0s","sync_on_commit":true}"#,
            ),
        ));
        let api = testing::api(fake.clone());
        let body = forgejo_model::CreatePushMirrorOption {
            branch_filter: Some("main, release/*".to_owned()),
            interval: Some(go_interval("30m").unwrap()),
            remote_address: Some("https://github.com/me/proj.git".to_owned()),
            remote_password: Some("hunter2".to_owned()),
            remote_username: Some("me".to_owned()),
            sync_on_commit: Some(true),
            use_ssh: Some(false),
        };
        api.repo().add_push_mirror("them", "proj", &body).await.unwrap();

        let call =
            &fake.calls_to(&testing::method("POST"), "/api/v1/repos/them/proj/push_mirrors")[0];
        let sent: serde_json::Value =
            serde_json::from_slice(call.body.as_ref().expect("a JSON body")).unwrap();
        assert_eq!(sent["branch_filter"], "main, release/*");
        assert_eq!(sent["interval"], "0h30m0s");
        assert_eq!(sent["sync_on_commit"], true);
        assert_eq!(sent["use_ssh"], false);
        assert_eq!(sent["remote_username"], "me");
    }

    /// Bug this prevents: `sync` calling the wrong endpoint. `mirror-sync` pulls *in* and
    /// `push_mirrors-sync` pushes *out*; the names are one hyphen apart and the directions are
    /// opposite, so getting it wrong on a pull mirror overwrites the upstream.
    #[tokio::test]
    async fn the_two_sync_endpoints_are_not_interchangeable() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    testing::method("POST"),
                    "/api/v1/repos/them/proj/mirror-sync",
                    Canned::new(200),
                )
                .on(
                    testing::method("POST"),
                    "/api/v1/repos/them/proj/push_mirrors-sync",
                    Canned::new(200),
                )
                .fallback(Canned::json(404, r#"{"message":"no"}"#)),
        );
        let api = testing::api(fake.clone());
        api.repo().mirror_sync("them", "proj").await.unwrap();
        api.repo().push_mirror_sync("them", "proj").await.unwrap();
        assert_eq!(
            fake.calls_to(&testing::method("POST"), "/api/v1/repos/them/proj/mirror-sync").len(),
            1
        );
        assert_eq!(
            fake.calls_to(&testing::method("POST"), "/api/v1/repos/them/proj/push_mirrors-sync")
                .len(),
            1
        );
    }

    #[test]
    fn the_list_table_renders_the_same_data_two_ways() {
        insta::assert_snapshot!("mirror_list_human", render_list(&testing::term(), &mirrors()));
        insta::assert_snapshot!("mirror_list_piped", render_list(&Term::piped(), &mirrors()));
    }

    #[test]
    fn the_json_output_uses_the_apis_own_field_names() {
        insta::assert_snapshot!(
            "mirror_list_json",
            testing::as_json(
                MIRROR_FIELDS,
                "remote_name,remote_address,branch_filter,interval,sync_on_commit",
                porcelain::json_of(&mirrors()).unwrap()
            )
        );
    }

    /// The `status` view for a repository that is *not* a pull mirror, which is the common case
    /// and the one where the help has to explain that one cannot be added later.
    #[test]
    fn the_status_view_explains_that_a_pull_mirror_cannot_be_added_later() {
        let repo = Repository::default();
        let out =
            render_status(&testing::term(), &RepoSlug::new("them", "proj"), &repo, &mirrors());
        assert!(out.contains("--mirror-from"), "{out}");
        insta::assert_snapshot!("mirror_status_human", out);
    }

    /// Bug this prevents — the most consequential misreading in this group: an empty
    /// `branch_filter` rendering as a blank cell, which reads as "no branches are mirrored" when
    /// it means the exact opposite: every branch and every tag, deletions included.
    #[test]
    fn an_empty_branch_filter_says_all_branches_not_nothing() {
        assert_eq!(branches(""), "(all, --mirror)");
        assert_eq!(branches("   "), "(all, --mirror)");
        assert_eq!(branches("main, release/*"), "main, release/*");
    }

    #[test]
    fn an_interval_is_validated_and_re_emitted_in_gos_own_form() {
        assert_eq!(go_interval("8h").unwrap(), "8h0m0s");
        assert_eq!(go_interval("30m").unwrap(), "0h30m0s");
        assert_eq!(go_interval("10m0s").unwrap(), "0h10m0s");
        assert_eq!(go_interval("1h30m").unwrap(), "1h30m0s");
        // `0` disables scheduled syncing, which is a real thing to ask for — so unlike a tracked
        // time it must not be refused.
        assert_eq!(go_interval("0").unwrap(), "0s");
        assert!(go_interval("soon").is_err());
        assert!(go_interval("8").is_err(), "a bare number is as ambiguous here as anywhere");
    }

    /// Bug this prevents: a password pasted into the address being stored server-side and left in
    /// the user's shell history with nothing said about it. `git@host:owner/name` must not trip
    /// the warning, because that is ordinary SSH syntax with no secret in it.
    #[test]
    fn a_password_in_the_address_is_warned_about_and_plain_ssh_is_not() {
        assert!(credentials_in_address("https://me:hunter2@github.com/me/proj.git").is_some());
        assert!(credentials_in_address("git@codeberg.org:me/proj.git").is_none());
        assert!(credentials_in_address("https://me@github.com/me/proj.git").is_none());
        assert!(credentials_in_address("https://github.com/me/proj.git").is_none());
        // A colon in the *path* is not a password.
        assert!(credentials_in_address("https://github.com/me/proj:x.git").is_none());
    }

    #[test]
    fn a_missing_remote_name_explains_where_the_name_comes_from() {
        let e = explain_named(
            Error::new(ErrorKind::ResourceNotFound {
                kind: "push mirror",
                id: "origin".to_owned(),
                slug: None,
                // Discovered locally: there was no server reply to quote.
                server_message: None,
            }),
            "origin",
        );
        assert!(e.to_string().contains("REMOTE column"), "{e}");
    }

    /// A 404 from an instance with mirroring disabled must name the setting, not the repository.
    #[test]
    fn a_missing_route_blames_the_instance_setting() {
        let e = explain(Error::new(ErrorKind::RouteNotFound {
            method: "GET".to_owned(),
            path: "/repos/o/r/push_mirrors".to_owned(),
            instance: None,
        }));
        let msg = e.to_string();
        assert!(msg.contains("[mirror] ENABLED"), "{msg}");
        assert!(msg.contains("ALLOW_PUSH_MIRRORS"), "{msg}");
    }
}
