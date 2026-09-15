//! `fcli repo fork` — fork, then leave the checkout pointing at the right two repositories.
//!
//! The API call is one request and `fcli raw repo create-fork` already makes it. What this command
//! is *for* is the state it leaves behind, because a fork with the remotes wired the other way
//! round is worse than no fork at all:
//!
//! ```text
//! origin   -> your fork        (where you push)
//! upstream -> the project      (where pull requests go, and the base repository)
//! ```
//!
//! `upstream` being recorded as the base repository — git config `remote.upstream.fcli-resolved =
//! base` — is the step that makes a later `fcli pr create` open a pull request against the project
//! instead of against your own copy. Remote-name scoring in `forgejo_core::context` would already
//! prefer `upstream` over `origin`, but writing it down means the answer does not change if
//! somebody later adds a third remote.

use clap::Args as ClapArgs;
use forgejo_client::Api;
use forgejo_core::context::git::{CloneSpec, FetchSpec, GitCli, GitCtx};
use forgejo_core::context::{RESOLVED_BASE, resolved_key};
use forgejo_core::types::RepoSlug;
use forgejo_core::{Error, ErrorKind, Result};
use forgejo_model::{CreateForkOption, Repository};

use super::clone::clone_url;
use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Fork a repository.

--clone clones the fork and adds its parent as `upstream`. Pull requests target
the parent by default.

Inside an existing clone, --remote renames `origin` to `upstream` and sets `origin`
to the new fork.

  fcli repo fork forgejo/forgejo --clone
  fcli repo fork --remote            # inside a clone of the project
  fcli repo fork o/r --org my-team --fork-name r-experiment")]
pub struct Args {
    /// Repository to fork. Defaults to the resolved repository
    #[arg(value_name = "REPOSITORY")]
    pub repo: Option<String>,

    /// Clone the fork after creating it
    #[arg(long)]
    pub clone: bool,

    /// Fork into this organization instead of your own account
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,

    /// Name for the fork. Defaults to the source repository's name
    #[arg(long, value_name = "NAME")]
    pub fork_name: Option<String>,

    /// Add a remote for the fork to the current checkout, renaming `origin` to `upstream`
    #[arg(long)]
    pub remote: bool,

    /// Name for the remote added for the fork
    #[arg(long, value_name = "NAME", default_value = "origin")]
    pub remote_name: String,

    /// Name for the remote that ends up pointing at the project
    #[arg(long, value_name = "NAME", default_value = "upstream")]
    pub upstream_name: String,

    /// Clone or add remotes over SSH instead of HTTPS
    #[arg(long)]
    pub ssh: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let wanted = support::machine::plan(globals, forgejo_client::fields::FIELDS_REPOSITORY)?;
    if matches!(wanted, support::machine::Wanted::Listed) {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let source_slug = super::target(&rt, globals, &api, args.repo.as_deref()).await?;
        let source = api.repo().get(&source_slug.owner, &source_slug.name).await?;
        let (fork, adopted) = fork(&api, args, &source_slug).await?;
        if adopted {
            support::note(rt.term(), &format!("{} already exists; using it", fork.full_name));
        }

        if args.clone {
            clone_the_fork(&rt, args, &fork, &source)?;
        } else if args.remote {
            rewire_here(&rt, args, &fork, &source_slug)?;
        }

        match &wanted {
            support::machine::Wanted::Machine(m) => {
                support::machine::emit(&rt, globals, m, support::to_value(&fork)?)
            }
            _ => {
                println!("{}", fork.html_url);
                Ok(())
            }
        }
    })
}

/// Create the fork, or adopt the one that already exists. The `bool` is "adopted".
///
/// A second `fcli repo fork` is answered by Forgejo with **409 Conflict**, and treating that as a
/// failure would make `--clone` unusable the second time round: the fork the user wants exists,
/// and the only reason the command failed is that it succeeded earlier. `gh` adopts the existing
/// fork for the same reason, and says so. If the 409 came from something *else*, the lookup fails
/// too and the original error is what gets reported — never a made-up one.
pub(crate) async fn fork(api: &Api, args: &Args, source: &RepoSlug) -> Result<(Repository, bool)> {
    let body = CreateForkOption { name: args.fork_name.clone(), organization: args.org.clone() };
    match api.repo().create_fork(&source.owner, &source.name, &body).await {
        Ok(repo) => Ok((repo, false)),
        Err(e) if matches!(e.kind(), ErrorKind::Conflict { .. }) => {
            let owner = match &args.org {
                Some(org) => org.clone(),
                None => support::me(api).await?,
            };
            let name = args.fork_name.clone().unwrap_or_else(|| source.name.clone());
            let existing = api.repo().get(&owner, &name).await.map_err(|_| e)?;
            Ok((existing, true))
        }
        Err(e) => Err(e),
    }
}

fn clone_the_fork(rt: &Runtime, args: &Args, fork: &Repository, source: &Repository) -> Result<()> {
    // The directory is named explicitly rather than left to git, so that the remotes below are
    // wired into the directory the clone actually landed in — see `repo clone`, which does the
    // same for the same reason.
    let spec = CloneSpec::new(clone_url(fork, args.ssh)).into_dir(&fork.name);
    let dir = rt.git().clone_repo(&spec)?;
    let git = GitCli::in_dir(&dir);
    let upstream = &args.upstream_name;
    if !git.remote_exists(upstream)? {
        git.remote_add(upstream, &clone_url(source, args.ssh))?;
        if git.fetch(&FetchSpec::new(upstream).quiet()).is_err() {
            support::note(
                rt.term(),
                &format!("could not fetch {upstream}; the remote is configured"),
            );
        }
    }
    git.config_set_local(&resolved_key(upstream), RESOLVED_BASE)?;
    support::note(
        rt.term(),
        &format!(
            "cloned {} into ./{}; {} is {upstream} and is the base repository",
            fork.full_name, fork.name, source.full_name
        ),
    );
    Ok(())
}

/// `--remote`, run inside a clone of the repository being forked.
///
/// The rename is conditional on the remote actually pointing at the source: renaming somebody's
/// `origin` because they happened to type `fcli repo fork other/thing` in an unrelated checkout
/// would be a genuinely destructive surprise, and `git remote rename` also rewrites every
/// `branch.*.remote` that referred to it.
fn rewire_here(rt: &Runtime, args: &Args, fork: &Repository, source: &RepoSlug) -> Result<()> {
    forgejo_core::context::require_git_repo(rt.git())?;
    let git = rt.git();
    let upstream = &args.upstream_name;

    match super::remote_for(rt, source)? {
        Some(existing) if existing == *upstream => {}
        Some(existing) => {
            if git.remote_exists(upstream)? {
                return Err(Error::new(ErrorKind::Usage(format!(
                    "remote {existing} points at {source} and should become {upstream}, but a \
                     remote called {upstream} already exists; sort the remotes out by hand, or \
                     pass --upstream-name"
                ))));
            }
            git.remote_rename(&existing, upstream)?;
            support::note(rt.term(), &format!("renamed remote {existing} to {upstream}"));
        }
        None => {
            support::note(
                rt.term(),
                &format!("no remote in this checkout points at {source}; adding {upstream}"),
            );
            git.remote_add(upstream, &source_url(rt, source, args.ssh))?;
        }
    }

    let name = &args.remote_name;
    if git.remote_exists(name)? {
        support::note(rt.term(), &format!("remote {name} already exists; leaving it alone"));
    } else {
        git.remote_add(name, &clone_url(fork, args.ssh))?;
        support::note(rt.term(), &format!("added remote {name} -> {}", clone_url(fork, args.ssh)));
    }
    git.config_set_local(&resolved_key(upstream), RESOLVED_BASE)?;
    Ok(())
}

/// A clone URL for the source repository built from the instance's own web base, for the rare
/// case where this checkout has no remote pointing at it at all.
fn source_url(rt: &Runtime, source: &RepoSlug, ssh: bool) -> String {
    if ssh {
        // No API field to read here, so this is assembled: `git@<host>:<owner>/<name>.git` is the
        // shape Forgejo advertises, and `insteadOf` rules still get their say.
        return format!("git@{}:{source}.git", rt.client().host());
    }
    format!("{}/{source}.git", rt.client().web_base().trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use forgejo_core::http::transport::Canned;
    use std::sync::Arc;
    use support::testing;

    fn args(words: &[&str]) -> Args {
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            args: Args,
        }
        <Harness as clap::Parser>::try_parse_from(words)
            .unwrap_or_else(|e| panic!("{words:?}: {e}"))
            .args
    }

    #[tokio::test]
    async fn fork_posts_the_name_and_organization_to_the_source_repository() {
        let t = Arc::new(testing::on(
            testing::transport(),
            "POST",
            "/api/v1/repos/them/proj/forks",
            Canned::json(202, r#"{"full_name":"my-team/proj-x","name":"proj-x"}"#),
        ));
        let api = testing::api_at(testing::EXAMPLE, t.clone());
        let args = args(&["fcli", "--org", "my-team", "--fork-name", "proj-x"]);

        let (repo, adopted) =
            fork(&api, &args, &RepoSlug::new("them", "proj")).await.expect("fork");
        assert_eq!(repo.full_name, "my-team/proj-x");
        assert!(!adopted);

        let sent = testing::body(&t, "POST", "/api/v1/repos/them/proj/forks");
        assert_eq!(sent["name"], "proj-x");
        assert_eq!(sent["organization"], "my-team");
    }

    /// Bug this prevents — and it broke the command's *most common* form, `fcli repo fork o/r`
    /// with no flags at all. The body used to be built as
    /// `name: Some(args.fork_name.clone().unwrap_or_default())`, which turns "the user did not
    /// pass --fork-name" into `Some("")`. `CreateForkOption` is `Option<String>` with
    /// `skip_serializing_if`, so `None` omits the key and Forgejo defaults it to the source
    /// repository's name — but an explicit `""` is a value, and the server answers
    /// `500 name is empty`, or `422 org does not exist [id: 0, name: ]` when `--org` is given.
    /// Passing `--fork-name` explicitly always worked, which is why a mock never caught it: a
    /// fake transport accepts any body, so only the *shape* of the request can be asserted.
    #[tokio::test]
    async fn an_unset_flag_is_omitted_from_the_body_rather_than_sent_as_an_empty_string() {
        let t = Arc::new(testing::on(
            testing::transport(),
            "POST",
            "/api/v1/repos/them/proj/forks",
            Canned::json(202, r#"{"full_name":"me/proj","name":"proj"}"#),
        ));
        let api = testing::api_at(testing::EXAMPLE, t.clone());

        fork(&api, &args(&["fcli"]), &RepoSlug::new("them", "proj")).await.expect("fork");

        let sent = testing::body(&t, "POST", "/api/v1/repos/them/proj/forks");
        for key in ["name", "organization"] {
            assert!(
                sent.get(key).is_none(),
                "an unset flag must not reach the wire at all, and {key} did: {sent}"
            );
        }
    }

    /// The other half of the same rule: `--org` on its own still omits `name`, so the fork keeps
    /// the source repository's name instead of being asked for one called `""`.
    #[tokio::test]
    async fn an_org_without_a_fork_name_sends_the_org_and_omits_the_name() {
        let t = Arc::new(testing::on(
            testing::transport(),
            "POST",
            "/api/v1/repos/them/proj/forks",
            Canned::json(202, r#"{"full_name":"my-team/proj","name":"proj"}"#),
        ));
        let api = testing::api_at(testing::EXAMPLE, t.clone());

        fork(&api, &args(&["fcli", "--org", "my-team"]), &RepoSlug::new("them", "proj"))
            .await
            .expect("fork");

        let sent = testing::body(&t, "POST", "/api/v1/repos/them/proj/forks");
        assert_eq!(sent["organization"], "my-team");
        assert!(sent.get("name").is_none(), "no --fork-name means no name key: {sent}");
    }

    /// Bug this prevents: `fcli repo fork o/r --clone` failing the second time it is run, because
    /// Forgejo answers a duplicate fork with 409 and the command reported the conflict instead of
    /// using the fork the user already has.
    #[tokio::test]
    async fn a_second_fork_adopts_the_existing_one() {
        let mut t = testing::transport();
        t = testing::on(
            t,
            "POST",
            "/api/v1/repos/them/proj/forks",
            Canned::json(409, r#"{"message":"repository is already forked by user"}"#),
        );
        t = testing::on(t, "GET", "/api/v1/user", Canned::json(200, r#"{"login":"me"}"#));
        t = testing::on(
            t,
            "GET",
            "/api/v1/repos/me/proj",
            Canned::json(200, r#"{"full_name":"me/proj","name":"proj","fork":true}"#),
        );
        let api = testing::api_at(testing::EXAMPLE, Arc::new(t));

        let (repo, adopted) = fork(&api, &args(&["fcli"]), &RepoSlug::new("them", "proj"))
            .await
            .expect("the existing fork is adopted");
        assert_eq!(repo.full_name, "me/proj");
        assert!(adopted);
    }

    /// Bug this prevents: a 409 that is *not* "already forked" being reported as some invented
    /// lookup failure instead of the server's own message.
    #[tokio::test]
    async fn an_unrelated_conflict_keeps_the_servers_error() {
        let mut t = testing::transport();
        t = testing::on(
            t,
            "POST",
            "/api/v1/repos/them/proj/forks",
            Canned::json(409, r#"{"message":"cannot fork into an archived organization"}"#),
        );
        t = testing::on(t, "GET", "/api/v1/user", Canned::json(200, r#"{"login":"me"}"#));
        t = testing::on(
            t,
            "GET",
            "/api/v1/repos/me/proj",
            Canned::json(404, r#"{"message":"nope"}"#),
        );
        let api = testing::api_at(testing::EXAMPLE, Arc::new(t));

        let err = fork(&api, &args(&["fcli"]), &RepoSlug::new("them", "proj")).await.unwrap_err();
        let ErrorKind::Conflict { server_message } = &*err.kind else {
            panic!("expected the server's 409, got {err:?}")
        };
        assert!(
            server_message.contains("archived organization"),
            "the server's own reason must survive: {server_message}"
        );
    }

    #[test]
    fn the_fallback_source_url_is_built_from_the_web_base() {
        // Used only when a checkout has no remote for the source at all. Getting the shape wrong
        // makes `git remote add` succeed and every later fetch fail.
        let slug = RepoSlug::new("them", "proj");
        assert_eq!(
            format!("{}/{slug}.git", "https://git.example.org"),
            "https://git.example.org/them/proj.git"
        );
    }
}
