//! `fcli admin quota` — the **write** side of Forgejo's storage quotas.
//!
//! Quotas are a Forgejo feature with no Gitea and no GitHub equivalent, and they are built from
//! two things:
//!
//! * a **rule** — a byte limit over a set of *subjects* (`size:all`, `size:assets:packages:all`,
//!   `size:git:lfs`, …);
//! * a **group** — a named bundle of rules that accounts are put into.
//!
//! An account's effective quota is the union of the groups it belongs to. Nothing is enforced until
//! a group with rules has users in it, which is why `group create --rule` exists: creating a group
//! and forgetting to attach a rule looks like a working quota and enforces nothing.
//!
//! # This is the write half; `fcli quota` is the read half
//!
//! `fcli quota` (a different group) answers "what is my quota and what am I using" with an ordinary
//! token. Everything here needs `write:admin` and changes the instance. The split is by *privilege*
//! rather than by noun, so that `fcli quota` is safe to hand to anyone.
//!
//! # `rule edit` builds its own body, for the same reason `admin user edit` does
//!
//! `EditQuotaRuleOptions` has no `Option` fields, so serialising one sends `"limit": 0` — a quota
//! of **zero bytes** — for a command line that only meant to change the subjects. `edit` therefore
//! sends only the keys it was given. See `admin/user.rs` for the full argument.

use clap::{Args as ClapArgs, Subcommand};
use forgejo_client::Api;
use forgejo_client::forgejo_model::{
    CreateQuotaGroupOptions, CreateQuotaRuleOptions, QuotaGroup, QuotaRuleInfo,
};
use forgejo_core::error::Result;
use forgejo_core::http::{Request, encode};
use serde_json::{Map, Value};

use crate::cmd::support::{self, Emit};
use crate::global::GlobalOpts;
use crate::output::Table;

pub const OP_GROUP: &str = "adminListQuotaGroups";
pub const OP_RULE: &str = "adminListQuotaRules";

/// Every key `rule edit` will send. Pinned against `EditQuotaRuleOptions` by the test below; see
/// `admin/user.rs` for why the list is `#[cfg(test)]`.
#[cfg(test)]
const RULE_EDIT_KEYS: &[&str] = &["limit", "subjects"];

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Quota rules: a byte limit over a set of subjects
    #[command(subcommand)]
    Rule(Rule),

    /// Quota groups: named bundles of rules, with accounts in them
    #[command(subcommand)]
    Group(Group),
}

#[derive(Debug, Subcommand)]
pub enum Rule {
    /// List every quota rule on this instance
    List,

    /// Create a rule. It enforces nothing until a group using it has members.
    Create(RuleCreate),

    /// Change a rule's limit or subjects, leaving the other alone
    Edit(RuleEdit),

    /// Delete a rule, removing it from every group that used it
    Delete(Named),
}

#[derive(Debug, Subcommand)]
pub enum Group {
    /// List the quota groups and the rules in each
    List,

    /// Create a group, optionally with rules already attached
    Create(GroupCreate),

    /// Delete a group. Its members lose the quota it applied.
    Delete(Named),

    /// Show which accounts are in a group
    Users(Named),

    /// Attach an existing rule to a group
    AddRule(GroupRule),

    /// Detach a rule from a group. The rule itself survives.
    RemoveRule(GroupRule),

    /// Put an account into a group
    AddUser(GroupUser),

    /// Take an account out of a group
    RemoveUser(GroupUser),
}

#[derive(Debug, ClapArgs)]
pub struct Named {
    /// The rule or group name
    #[arg(value_name = "NAME")]
    pub name: String,
    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
pub struct RuleCreate {
    /// A name for the rule; only administrators ever see it
    #[arg(value_name = "NAME")]
    pub name: String,

    /// The limit in bytes. -1 means unlimited; 0 means nothing may be stored.
    ///
    /// `--limit` is reserved for result counts.
    #[arg(long = "bytes", value_name = "BYTES", allow_hyphen_values = true)]
    pub limit: i64,

    /// What the limit counts, e.g. size:all or size:assets:packages:all. Repeatable.
    #[arg(long = "subject", value_name = "SUBJECT")]
    pub subject: Vec<String>,
}

#[derive(Debug, ClapArgs)]
pub struct RuleEdit {
    #[arg(value_name = "NAME")]
    pub name: String,

    /// New limit in bytes. Leave it out to keep the current one. See `rule create --bytes`.
    #[arg(long = "bytes", value_name = "BYTES", allow_hyphen_values = true)]
    pub limit: Option<i64>,

    /// Replace the subject list. Repeatable; leave it out to keep the current one.
    #[arg(long = "subject", value_name = "SUBJECT")]
    pub subject: Vec<String>,
}

#[derive(Debug, ClapArgs)]
pub struct GroupCreate {
    /// A name for the group
    #[arg(value_name = "NAME")]
    pub name: String,

    /// A rule to attach, by name. Repeatable. A rule that does not exist yet is created empty,
    /// which is the API's behaviour and rarely what you want — prefer `rule create` first.
    #[arg(long = "rule", value_name = "NAME")]
    pub rule: Vec<String>,
}

#[derive(Debug, ClapArgs)]
pub struct GroupRule {
    /// The group
    #[arg(value_name = "GROUP")]
    pub group: String,
    /// The rule
    #[arg(value_name = "RULE")]
    pub rule: String,
}

#[derive(Debug, ClapArgs)]
pub struct GroupUser {
    /// The group
    #[arg(value_name = "GROUP")]
    pub group: String,
    /// The account
    #[arg(value_name = "USERNAME")]
    pub username: String,
}

pub fn op(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::Rule(Rule::List | Rule::Create(_) | Rule::Edit(_)) => OP_RULE,
        Cmd::Rule(Rule::Delete(_)) => "",
        Cmd::Group(Group::List | Group::Create(_)) => OP_GROUP,
        Cmd::Group(Group::Users(_)) => "adminSearchUsers",
        Cmd::Group(_) => "",
    }
}

pub fn writes(cmd: &Cmd) -> bool {
    !matches!(cmd, Cmd::Rule(Rule::List) | Cmd::Group(Group::List | Group::Users(_)))
}

pub async fn run(api: &Api, _globals: &GlobalOpts, emit: &mut Emit<'_>, cmd: &Cmd) -> Result<()> {
    match cmd {
        Cmd::Rule(r) => rule(api, emit, r).await,
        Cmd::Group(g) => group(api, emit, g).await,
    }
}

// ----------------------------------------------------------------------------------- rules

async fn rule(api: &Api, emit: &mut Emit<'_>, cmd: &Rule) -> Result<()> {
    match cmd {
        Rule::List => {
            // Not paginated by the API: one request returns every rule.
            let rules = api.admin().list_quota_rules().await?;
            let n = rules.len() as u64;
            emit.many(&rules, Some(n), "quota rules", |table, rules| {
                table.headers(["RULE", "LIMIT", "SUBJECTS"]);
                for r in rules {
                    table.row([r.name.clone(), limit_of(r.limit), r.subjects.join(",")]);
                }
            })
        }

        Rule::Create(a) => {
            let body = CreateQuotaRuleOptions {
                limit: Some(a.limit),
                name: Some(a.name.clone()),
                subjects: Some(a.subject.clone()),
            };
            let created = api.admin().create_quota_rule(&body).await?;
            emit.done(&format!(
                "created quota rule {} ({}). It enforces nothing until a group using it has \
                 members — see `fcli admin quota group add-rule`.",
                created.name,
                limit_of(created.limit)
            ));
            emit.one(&created, |t| rule_detail(t, &created))
        }

        Rule::Edit(a) => {
            let mut patch = Map::new();
            if let Some(limit) = a.limit {
                patch.insert("limit".to_owned(), Value::from(limit));
            }
            if !a.subject.is_empty() {
                patch.insert(
                    "subjects".to_owned(),
                    Value::Array(a.subject.iter().cloned().map(Value::String).collect()),
                );
            }
            if patch.is_empty() {
                return Err(support::usage(format!(
                    "nothing to change on quota rule {}; give --bytes, --subject, or both",
                    a.name
                )));
            }
            let req = Request::patch(format!("/admin/quota/rules/{}", encode::seg(&a.name)))
                .json_body(&Value::Object(patch))?;
            let updated: QuotaRuleInfo = api.client().json(req).await?;
            emit.done(&format!("updated quota rule {}", updated.name));
            emit.one(&updated, |t| rule_detail(t, &updated))
        }

        Rule::Delete(a) => {
            support::confirm_term(
                emit.term(),
                a.yes,
                &format!(
                    "delete quota rule {} and remove it from every group that uses it",
                    a.name
                ),
            )?;
            api.admin().delete_quota_rule(&a.name).await?;
            emit.done(&format!("deleted quota rule {}", a.name));
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------------- groups

async fn group(api: &Api, emit: &mut Emit<'_>, cmd: &Group) -> Result<()> {
    match cmd {
        Group::List => {
            let groups = api.admin().list_quota_groups().await?;
            let n = groups.len() as u64;
            emit.many(&groups, Some(n), "quota groups", |table, groups| {
                table.headers(["GROUP", "RULES", "LIMITS"]);
                for g in groups {
                    table.row([
                        g.name.clone(),
                        g.rules.iter().map(|r| r.name.clone()).collect::<Vec<_>>().join(","),
                        g.rules.iter().map(|r| limit_of(r.limit)).collect::<Vec<_>>().join(","),
                    ]);
                }
            })
        }

        Group::Create(a) => {
            let body = CreateQuotaGroupOptions {
                name: Some(a.name.clone()),
                // Named rules only: the API creates a missing rule with a zero limit, and passing
                // a limit here would silently redefine an existing rule for every other group.
                rules: Some(
                    a.rule
                        .iter()
                        .map(|name| CreateQuotaRuleOptions {
                            limit: Some(0),
                            name: Some(name.clone()),
                            subjects: Some(Vec::new()),
                        })
                        .collect(),
                ),
            };
            let created = api.admin().create_quota_group(&body).await?;
            if created.rules.is_empty() {
                support::note(
                    emit.term(),
                    &format!(
                        "note: {} has no rules, so it limits nothing yet — attach one with \
                         `fcli admin quota group add-rule {} <RULE>`",
                        created.name, created.name
                    ),
                );
            }
            emit.done(&format!("created quota group {}", created.name));
            emit.one(&created, |t| group_detail(t, &created))
        }

        Group::Delete(a) => {
            support::confirm_term(
                emit.term(),
                a.yes,
                &format!("delete quota group {} — its members lose the quota it applied", a.name),
            )?;
            api.admin().delete_quota_group(&a.name).await?;
            emit.done(&format!("deleted quota group {}", a.name));
            Ok(())
        }

        Group::Users(a) => {
            let users = api.admin().list_users_in_quota_group(&a.name).await?;
            let n = users.len() as u64;
            emit.many(&users, Some(n), "accounts in that group", |table, users| {
                table.headers(["LOGIN", "EMAIL"]);
                for u in users {
                    table.row([u.login.clone(), u.email.clone()]);
                }
            })
        }

        Group::AddRule(a) => {
            api.admin().add_rule_to_quota_group(&a.group, &a.rule).await?;
            emit.done(&format!("attached rule {} to group {}", a.rule, a.group));
            Ok(())
        }

        Group::RemoveRule(a) => {
            api.admin().remove_rule_from_quota_group(&a.group, &a.rule).await?;
            emit.done(&format!(
                "detached rule {} from group {}; the rule itself still exists",
                a.rule, a.group
            ));
            Ok(())
        }

        Group::AddUser(a) => {
            api.admin().add_user_to_quota_group(&a.group, &a.username).await?;
            emit.done(&format!("{} is now subject to quota group {}", a.username, a.group));
            Ok(())
        }

        Group::RemoveUser(a) => {
            api.admin().remove_user_from_quota_group(&a.group, &a.username).await?;
            emit.done(&format!("{} is no longer in quota group {}", a.username, a.group));
            Ok(())
        }
    }
}

// ----------------------------------------------------------------------------- presentation

/// `-1` is Forgejo's "unlimited", and printing it as a number invites somebody to read it as a
/// negative byte count.
fn limit_of(bytes: i64) -> String {
    match bytes {
        -1 => "unlimited".to_owned(),
        0 => "0 (nothing may be stored)".to_owned(),
        n => human_bytes(n),
    }
}

/// Byte counts in the units an operator sets quotas in.
///
/// Binary units, because that is what Forgejo's own UI shows and a quota that reads `1.0 GB` here
/// and `953.7 MiB` there is a support ticket.
fn human_bytes(n: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{n} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

fn rule_detail(table: &mut Table, r: &QuotaRuleInfo) {
    table.row(["rule".to_owned(), r.name.clone()]);
    table.row(["limit".to_owned(), limit_of(r.limit)]);
    table.row(["limit_bytes".to_owned(), r.limit.to_string()]);
    table.row(["subjects".to_owned(), r.subjects.join(", ")]);
}

fn group_detail(table: &mut Table, g: &QuotaGroup) {
    table.row(["group".to_owned(), g.name.clone()]);
    for r in &g.rules {
        table.row([format!("rule.{}", r.name), limit_of(r.limit)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use forgejo_client::forgejo_model::EditQuotaRuleOptions;
    use forgejo_core::http::FakeTransport;
    use forgejo_core::http::transport::Canned;
    use std::sync::Arc;

    fn emit_to<'a>(buf: &'a mut Vec<u8>, globals: &GlobalOpts) -> Emit<'a> {
        Emit::new(globals, None, &crate::output::Term::piped(), buf).unwrap()
    }

    /// The bug this exists to prevent: `EditQuotaRuleOptions` has no `Option` fields, so handing
    /// the typed method a struct built from `--subject` alone would send `"limit": 0` and set the
    /// rule to **zero bytes** — every account in a group using it can store nothing.
    #[tokio::test]
    async fn rule_edit_sends_only_what_was_named() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "PATCH",
            "/api/v1/admin/quota/rules/tight",
            Canned::json(200, r#"{"name":"tight","limit":1024,"subjects":["size:git:lfs"]}"#),
        ));
        let api = testing::api(fake.clone());
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit = emit_to(&mut buf, &globals);
            let args = RuleEdit {
                name: "tight".into(),
                limit: None,
                subject: vec!["size:git:lfs".into()],
            };
            rule(&api, &mut emit, &Rule::Edit(args)).await.unwrap();
        }
        let sent: Value = serde_json::from_str(&fake.calls()[0].body_str()).unwrap();
        assert_eq!(sent, serde_json::json!({"subjects": ["size:git:lfs"]}));
        assert!(sent.get("limit").is_none(), "a limit nobody asked for must not be sent");

        // The typed option now agrees: `limit` is absent rather than zeroed, because a
        // request-body field nobody set is not serialized at all.
        let typed = serde_json::to_value(EditQuotaRuleOptions {
            subjects: Some(vec!["size:git:lfs".to_owned()]),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(typed, serde_json::json!({"subjects": ["size:git:lfs"]}));
    }

    #[test]
    fn every_key_rule_edit_can_send_exists_on_the_generated_option() {
        // Round-tripped rather than read off `EditQuotaRuleOptions::default()`: an unset
        // request-body field is no longer serialized, so the default is `{}`. A key the spec
        // renamed away is silently dropped on the way in — models carry no
        // `deny_unknown_fields` — so it is missing on the way back out.
        let every_key = serde_json::json!({"limit": 1, "subjects": ["size:all"]});
        let model: EditQuotaRuleOptions = serde_json::from_value(every_key).unwrap();
        let round_tripped = serde_json::to_value(&model).unwrap();
        for key in RULE_EDIT_KEYS {
            assert!(
                round_tripped.get(*key).is_some(),
                "EditQuotaRuleOptions has no {key:?} any more"
            );
        }
    }

    #[tokio::test]
    async fn rule_edit_with_nothing_named_is_a_usage_error() {
        let api = testing::api(Arc::new(FakeTransport::new()));
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        let mut emit = emit_to(&mut buf, &globals);
        let args = RuleEdit { name: "tight".into(), limit: None, subject: Vec::new() };
        let e = rule(&api, &mut emit, &Rule::Edit(args)).await.unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("--bytes"), "{e}");
    }

    /// Bug this prevents: printing `-1` as a byte count. `-1` is Forgejo's "unlimited", and a
    /// quota table that shows `-1 B` reads as a bug in the server.
    #[test]
    fn limits_are_spelled_in_words_where_the_number_would_mislead() {
        assert_eq!(limit_of(-1), "unlimited");
        assert_eq!(limit_of(0), "0 (nothing may be stored)");
        assert_eq!(limit_of(512), "512 B");
        assert_eq!(limit_of(1024), "1.0 KiB");
        assert_eq!(limit_of(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }

    /// The four group-membership verbs differ only by method and path segment, which is exactly
    /// how "add user" ends up detaching a rule. All four on the wire, in one test.
    #[tokio::test]
    async fn the_group_membership_verbs_each_reach_their_own_route() {
        let fake = testing::on(
            FakeTransport::new(),
            "PUT",
            "/api/v1/admin/quota/groups/small/rules/tight",
            testing::empty(),
        );
        let fake = testing::on(
            fake,
            "DELETE",
            "/api/v1/admin/quota/groups/small/rules/tight",
            testing::empty(),
        );
        let fake = testing::on(
            fake,
            "PUT",
            "/api/v1/admin/quota/groups/small/users/ada",
            testing::empty(),
        );
        let fake = Arc::new(testing::on(
            fake,
            "DELETE",
            "/api/v1/admin/quota/groups/small/users/ada",
            testing::empty(),
        ));
        let api = testing::api(fake.clone());
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        let mut emit = emit_to(&mut buf, &globals);

        let gr = || GroupRule { group: "small".into(), rule: "tight".into() };
        let gu = || GroupUser { group: "small".into(), username: "ada".into() };
        group(&api, &mut emit, &Group::AddRule(gr())).await.unwrap();
        group(&api, &mut emit, &Group::RemoveRule(gr())).await.unwrap();
        group(&api, &mut emit, &Group::AddUser(gu())).await.unwrap();
        group(&api, &mut emit, &Group::RemoveUser(gu())).await.unwrap();

        let seen: Vec<String> =
            fake.calls().into_iter().map(|c| format!("{} {}", c.method.as_str(), c.path)).collect();
        insta::assert_snapshot!(seen.join("\n"));
    }

    #[tokio::test]
    async fn group_list_shows_the_rules_inside_each_group() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/admin/quota/groups",
            Canned::json(
                200,
                r#"[{"name":"small","rules":[{"name":"tight","limit":1048576,
                     "subjects":["size:all"]}]},
                    {"name":"unlimited","rules":[{"name":"none","limit":-1}]},
                    {"name":"empty","rules":[]}]"#,
            ),
        ));
        let api = testing::api(fake);
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::tty(80), &mut buf).unwrap();
            group(&api, &mut emit, &Group::List).await.unwrap();
        }
        insta::assert_snapshot!(String::from_utf8(buf).unwrap());
    }
}
