//! `fjo admin runner` — Actions runners registered anywhere on the instance.
//!
//! `GET /admin/actions/runners` is the only view that sees *all* of them at once: a runner can be
//! scoped to the instance, to an organization, to a user, or to a single repository, and the
//! per-scope endpoints each show one slice. When a job is stuck in `waiting`, "which runners exist
//! and are any of them online" is the first question, and this is the command that answers it.
//!
//! `ownership` in the human view is derived, not a field: the API reports `owner_id` and `repo_id`
//! with `0` meaning "not this kind", so an operator would otherwise have to know that
//! `owner_id: 0, repo_id: 4` means "repository-scoped".

use clap::{Args as ClapArgs, Subcommand};
use forgejo_client::Api;
use forgejo_client::forgejo_model::ActionRunner;
use forgejo_core::error::Result;
use forgejo_core::http::Paging;

use crate::cmd::support::{self, Emit};
use crate::global::GlobalOpts;

pub const OP_RUNNER: &str = "getAdminRunners";

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List every runner on this instance, whatever it is scoped to
    List(List),

    /// Remove a runner's registration, so it can no longer pick up jobs
    ///
    /// This does not stop the runner process; it revokes its registration. A running job is not
    /// interrupted, but nothing new is handed to it.
    Delete(Delete),
}

#[derive(Debug, ClapArgs)]
pub struct List {
    /// Only runners owned directly by the instance, not those scoped to an org, user or repository
    #[arg(long)]
    pub instance_only: bool,

    /// Only runners whose status matches, e.g. online, offline, idle, active
    #[arg(long, value_name = "STATUS")]
    pub status: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct Delete {
    /// The runner's numeric id, as shown by `fjo admin runner list`
    #[arg(value_name = "RUNNER-ID")]
    pub id: String,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

pub fn op(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::List(_) => OP_RUNNER,
        Cmd::Delete(_) => "",
    }
}

pub fn writes(cmd: &Cmd) -> bool {
    matches!(cmd, Cmd::Delete(_))
}

pub async fn run(api: &Api, globals: &GlobalOpts, emit: &mut Emit<'_>, cmd: &Cmd) -> Result<()> {
    match cmd {
        Cmd::List(a) => {
            // `visible=false` is the API's way of saying "only what the instance owns directly",
            // which reads backwards on the wire and is why the flag is spelled the other way here.
            let q = forgejo_client::query::GetAdminRunnersQuery::default()
                .with_visible(!a.instance_only);
            let cap = support::item_cap(globals);
            let (runners, total) = if globals.paginate {
                let runners = support::drain(api.admin().get_admin_runners(&q), cap).await?;
                let n = runners.len() as u64;
                (runners, Some(n))
            } else {
                let (runners, info) = api
                    .admin()
                    .get_admin_runners_page(&q, Paging { limit: cap, per_page: None })
                    .await?;
                (runners, info.total_count)
            };
            let runners: Vec<ActionRunner> = match &a.status {
                Some(want) => runners
                    .into_iter()
                    .filter(|r| r.status.as_str().eq_ignore_ascii_case(want))
                    .collect(),
                None => runners,
            };
            emit.many(&runners, total, "runners", |table, runners| {
                table.headers(["ID", "NAME", "STATUS", "OWNERSHIP", "LABELS", "VERSION"]);
                for r in runners {
                    table.row([
                        r.id.to_string(),
                        r.name.clone(),
                        r.status.to_string(),
                        ownership(r),
                        r.labels.join(","),
                        r.version.clone(),
                    ]);
                }
            })
        }

        Cmd::Delete(a) => {
            support::confirm_term(
                emit.term(),
                a.yes,
                &format!("unregister runner {} from this instance", a.id),
            )?;
            api.admin().delete_admin_runner(&a.id).await?;
            emit.done(&format!(
                "unregistered runner {}; the runner process is still running but will get no new \
                 jobs",
                a.id
            ));
            Ok(())
        }
    }
}

/// What a runner is scoped to, from the two ids the API reports.
///
/// The encoding is `0` for "not this kind", which is exactly the sort of thing a human should not
/// have to decode from a table.
fn ownership(r: &ActionRunner) -> String {
    match (r.owner_id, r.repo_id) {
        (0, 0) => "instance".to_owned(),
        (0, repo) => format!("repository {repo}"),
        (owner, 0) => format!("user or org {owner}"),
        (owner, repo) => format!("user or org {owner}, repository {repo}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use forgejo_core::http::FakeTransport;
    use forgejo_core::http::transport::Canned;
    use std::sync::Arc;

    const RUNNERS: &str = r#"[
        {"id":1,"name":"builder","status":"online","labels":["docker","ubuntu-latest"],
         "version":"6.0.1"},
        {"id":2,"name":"repo-local","status":"offline","repo_id":9,"labels":["self-hosted"]},
        {"id":3,"name":"org-wide","status":"online","owner_id":4,"labels":["arm64"]}]"#;

    /// Bug this prevents: an operator reading `owner_id: 0, repo_id: 9` and concluding the runner
    /// is unowned, when it is scoped to one repository and will never pick up their job.
    #[test]
    fn ownership_is_spelled_out_rather_than_left_as_two_zeroes() {
        let runners: Vec<ActionRunner> = serde_json::from_str(RUNNERS).unwrap();
        assert_eq!(ownership(&runners[0]), "instance");
        assert_eq!(ownership(&runners[1]), "repository 9");
        assert_eq!(ownership(&runners[2]), "user or org 4");
    }

    #[tokio::test]
    async fn list_shows_every_scope_by_default() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/admin/actions/runners",
            Canned::json(200, RUNNERS),
        ));
        let api = testing::api(fake.clone());
        let globals = GlobalOpts::default();
        let args = List { instance_only: false, status: None };
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::tty(100), &mut buf).unwrap();
            run(&api, &globals, &mut emit, &Cmd::List(args)).await.unwrap();
        }
        assert_eq!(fake.calls()[0].query_param("visible"), Some("true"));
        insta::assert_snapshot!(String::from_utf8(buf).unwrap());
    }

    /// Bug this prevents: `--instance-only` sending `visible=true`. The parameter reads backwards
    /// on the wire, so it is exactly the kind of flag that ends up inverted.
    #[tokio::test]
    async fn instance_only_sends_visible_false() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/admin/actions/runners",
            Canned::json(200, "[]"),
        ));
        let api = testing::api(fake.clone());
        let globals = GlobalOpts::default();
        let args = List { instance_only: true, status: None };
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
            run(&api, &globals, &mut emit, &Cmd::List(args)).await.unwrap();
        }
        assert_eq!(fake.calls()[0].query_param("visible"), Some("false"));
    }

    #[tokio::test]
    async fn status_filtering_happens_here_because_the_endpoint_has_no_such_parameter() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/admin/actions/runners",
            Canned::json(200, RUNNERS),
        ));
        let api = testing::api(fake);
        let globals = GlobalOpts::default();
        let args = List { instance_only: false, status: Some("OFFLINE".into()) };
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
            run(&api, &globals, &mut emit, &Cmd::List(args)).await.unwrap();
        }
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("repo-local"), "{out}");
        assert!(!out.contains("builder"), "{out}");
    }
}
