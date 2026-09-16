//! `fjo repo sync` — bring a fork up to date, locally or on the server.
//!
//! Two jobs behind one verb, split the way `gh repo sync` splits them:
//!
//! * **No destination** — sync *this checkout* from the base repository. That is `git fetch` plus a
//!   fast-forward, and it is the case `--force` exists for, because a diverged branch cannot be
//!   fast-forwarded and the only way through is a hard reset the user has to ask for.
//! * **A destination** — sync a fork *on the instance*, through Forgejo's own
//!   `POST /repos/{owner}/{repo}/sync_fork[/{branch}]`. Reimplementing that as a clone-merge-push
//!   dance would be slower, would need a work tree, and would attribute the merge to whoever ran
//!   the command. The endpoint also comes with a `GET` companion that says whether the sync is
//!   *possible* and how far behind the branch is, which is what turns "it failed" into "it
//!   diverged, and here is by how much".
//!
//! `--force` is only meaningful locally: Forgejo's `sync_fork` fast-forwards or refuses, and there
//! is no server-side force. Saying so is better than accepting the flag and ignoring it.

use clap::Args as ClapArgs;
use forgejo_client::Api;
use forgejo_core::context::git::{FetchSpec, GitCtx};
use forgejo_core::types::RepoSlug;
use forgejo_core::{Error, ErrorKind, Result};
use forgejo_model::SyncForkInfo;

use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Update a fork from its source repository.

Without a destination, fetches and fast-forwards the current checkout.
-f uses a hard reset if the branch has diverged; local changes can be lost.

With a destination, asks Forgejo to sync that fork. Server-side sync does not
support -f.

  fjo repo sync                     # this checkout, from upstream
  fjo repo sync -b main -f
  fjo repo sync me/proj             # my fork, on the server
  fjo repo sync -s forgejo/forgejo  # from a specific base")]
pub struct Args {
    /// Fork to sync on the instance. Omit to sync this checkout instead
    #[arg(value_name = "DESTINATION")]
    pub destination: Option<String>,

    /// Branch to sync. Defaults to the current branch locally, or the default branch on the server
    #[arg(short = 'b', long, value_name = "BRANCH")]
    pub branch: Option<String>,

    /// Repository to sync *from*, when syncing this checkout
    #[arg(short = 's', long, value_name = "OWNER/NAME")]
    pub source: Option<String>,

    /// Reset a diverged local branch instead of refusing. The long form is the global `--force`,
    /// which means something else, so only `-f` is available here
    #[arg(short = 'f')]
    pub force: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        match &args.destination {
            Some(dest) => {
                if args.force {
                    return Err(Error::new(ErrorKind::Usage(
                        "server-side sync only supports fast-forward updates. Remove -f, or sync locally (omit the destination) and push."
                            .to_owned(),
                    )));
                }
                let slug = super::slug_from_arg(&rt, &api, dest).await?;
                remote_sync(&rt, &api, &slug, args.branch.as_deref()).await
            }
            None => local_sync(&rt, globals, &api, args).await,
        }
    })
}

// ------------------------------------------------------------------------------ server-side sync

async fn remote_sync(rt: &Runtime, api: &Api, slug: &RepoSlug, branch: Option<&str>) -> Result<()> {
    let info = match branch {
        Some(b) => api.repo().sync_fork_branch_info(&slug.owner, &slug.name, b).await?,
        None => api.repo().sync_fork_default_info(&slug.owner, &slug.name).await?,
    };
    // Ask first, act second. The `GET` is what lets a refusal say *why*, and it is what lets an
    // already-current fork be a no-op with exit 0 instead of a pointless write.
    if let Some(reason) = refusal(&info, slug, branch) {
        return Err(reason);
    }
    if info.commits_behind == 0 {
        support::note(rt.term(), &format!("{slug} is already up to date"));
        return Ok(());
    }
    match branch {
        Some(b) => api.repo().sync_fork_branch(&slug.owner, &slug.name, b).await?,
        None => api.repo().sync_fork_default(&slug.owner, &slug.name).await?,
    }
    support::note(
        rt.term(),
        &format!(
            "Synced {}{} — {} commit{} fast-forwarded",
            slug,
            branch.map(|b| format!(" ({b})")).unwrap_or_default(),
            info.commits_behind,
            if info.commits_behind == 1 { "" } else { "s" }
        ),
    );
    Ok(())
}

/// Why Forgejo will not sync this fork, if it will not.
///
/// `allowed: false` covers several unrelated situations — not a fork, the branch has diverged, the
/// base branch is gone — and the API does not say which. So the message lists what it *can* be
/// rather than asserting one, and gives the two ways forward.
fn refusal(info: &SyncForkInfo, slug: &RepoSlug, branch: Option<&str>) -> Option<Error> {
    if info.allowed {
        return None;
    }
    let which = branch.map(|b| format!("{slug} ({b})")).unwrap_or_else(|| slug.to_string());
    Some(Error::new(ErrorKind::Usage(format!(
        "Forgejo will not sync {which}: it reports the sync as not allowed, which means the \
         branch has diverged from the base, the base branch is gone, or this is not a fork.\n\
         it is {} commit(s) behind the base.\n\
         sync this checkout instead (`fjo repo sync -b <branch> -f`) and push, or open a pull \
         request from the base branch",
        info.commits_behind
    ))))
}

// ------------------------------------------------------------------------------------ local sync

async fn local_sync(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &Args) -> Result<()> {
    forgejo_core::context::require_git_repo(rt.git())?;
    let git = rt.git();

    // Where to sync *from*. With no `--source`, that is whatever resolution decided the repository
    // is — which in a fork clone is `upstream`, thanks to remote-name scoring. That is exactly the
    // repository a `fjo pr create` here would target, so the two commands agree by construction.
    let base = match &args.source {
        Some(s) => super::slug_from_arg(rt, api, s).await?,
        None => rt.repo(globals)?.slug.clone(),
    };
    let remote = super::remote_for(rt, &base)?.ok_or_else(|| {
        Error::new(ErrorKind::Usage(format!(
            "no git remote in this checkout points at {base}; add one with \
             `git remote add upstream <url>`, or name the fork to sync on the server instead"
        )))
    })?;

    let branch = match &args.branch {
        Some(b) => b.clone(),
        None => rt.git().current_branch()?.ok_or_else(|| {
            Error::new(ErrorKind::Usage(
                "HEAD is detached, so there is no current branch to sync; pass -b <branch>"
                    .to_owned(),
            ))
        })?,
    };

    let current = rt.git().current_branch()?;
    if current.as_deref() == Some(branch.as_str()) {
        // Fetching into the *checked-out* branch's ref is what git refuses outright, so this path
        // fetches and then moves HEAD, and the other path updates the ref directly.
        git.fetch(&FetchSpec::new(&remote).with_refspec(&branch))?;
        return fast_forward(rt, git, &remote, &branch, args.force);
    }
    // `<branch>:<branch>` updates the local ref without checking anything out. Non-fast-forward is
    // refused by git unless forced, which is the behaviour we want and is why `--force` is threaded
    // through rather than always passed.
    git.fetch(
        &FetchSpec::new(&remote).with_refspec(format!("{branch}:{branch}")).forced(args.force),
    )?;
    support::note(rt.term(), &format!("Updated local {branch} from {remote}/{branch}"));
    Ok(())
}

/// Fast-forward the checked-out branch, or explain what to do about a divergence.
fn fast_forward(
    rt: &Runtime,
    git: &dyn GitCtx,
    remote: &str,
    branch: &str,
    force: bool,
) -> Result<()> {
    let target = format!("{remote}/{branch}");
    if git.merge_ff_only(&target)? {
        support::note(rt.term(), &format!("Fast-forwarded {branch} to {target}"));
        return Ok(());
    }
    if !force {
        return Err(Error::new(ErrorKind::Usage(format!(
            "{branch} cannot be fast-forwarded to {target}: it has commits {target} does not.\n\
             pass -f to reset {branch} to {target} and lose them, or rebase them yourself with \
             `git rebase {target}`"
        ))));
    }
    // Refusing to throw away *uncommitted* work even under `-f`: the flag is about the branch's
    // history, and nobody types `-f` meaning "and also delete the file I am editing". `git reset
    // --hard` would do exactly that, silently.
    let dirty = git.porcelain_status()?;
    if !dirty.is_empty() {
        return Err(Error::new(ErrorKind::Usage(format!(
            "-f would reset {branch} to {target}, but this work tree has uncommitted changes:\n{}\n\
             commit or stash local changes first; -f does not discard uncommitted files",
            dirty
        ))));
    }
    git.reset_hard(&target)?;
    support::note(rt.term(), &format!("Reset {branch} to {target}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(allowed: bool, behind: i64) -> SyncForkInfo {
        SyncForkInfo { allowed, commits_behind: behind, ..SyncForkInfo::default() }
    }

    /// Bug this prevents: reporting a refusal as a bare failure. `allowed: false` is the API's
    /// only signal and it covers several causes, so the message has to name them and say what to
    /// do — this is the same discipline that keeps `pr merge` from printing "failed to merge".
    #[test]
    fn a_refusal_names_the_possible_causes_and_a_way_forward() {
        let slug = RepoSlug::new("me", "proj");
        let e = refusal(&info(false, 4), &slug, Some("main")).expect("a refusal");
        let message = e.to_string();
        assert!(message.contains("me/proj (main)"), "{message}");
        assert!(message.contains("diverged"), "{message}");
        assert!(message.contains("4 commit(s) behind"), "{message}");
        assert!(message.contains("fjo repo sync"), "{message}");
    }

    #[test]
    fn an_allowed_sync_is_not_a_refusal() {
        assert!(refusal(&info(true, 0), &RepoSlug::new("me", "proj"), None).is_none());
    }

    /// Bug this prevents: `-f` being silently accepted for a server-side sync, where Forgejo has
    /// no force at all, so the user believes a divergence was resolved when nothing happened.
    #[test]
    fn force_with_a_destination_is_refused_rather_than_ignored() {
        // Parsed, not hand-built, so the flag's own wiring is under test too.
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            args: Args,
        }
        let h =
            <Harness as clap::Parser>::try_parse_from(["fjo", "me/proj", "-f"]).expect("parses");
        assert!(h.args.force);
        assert_eq!(h.args.destination.as_deref(), Some("me/proj"));
    }
}
