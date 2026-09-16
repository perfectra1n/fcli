//! `fcli quota` — storage quotas.
//!
//! **Forgejo-only.** Not in Gitea, and nothing remotely like it in GitHub, so there is no `gh` or
//! `tea` command to imitate here and no muscle memory to respect. The shape below is chosen to
//! answer one question well.
//!
//! # The question
//!
//! Someone pushes, and gets `413 Payload Too Large`. [`forgejo_core::ErrorKind::QuotaExceeded`]
//! already tells them the cause is a quota and points them here. `fcli quota status` has to
//! finish the sentence: *which* limit, how far over, and what is taking the space.
//!
//! That is harder than printing the API's response, for a reason worth stating: the API returns
//! the **leaves** of the usage tree (`used.size.repos.public`, `used.size.git.LFS`, …) while a
//! rule is written against an **aggregate** (`size:all`, `size:git:all`). There is no
//! `used.size.all` field. So comparing usage to limits requires the composition in [`subject`],
//! and without it the answer is a wall of numbers that do not line up with any rule.
//!
//! # The misconception the command exists to correct
//!
//! A quota is not a repository-size limit. `size:all` counts LFS objects, registry packages,
//! release assets, issue attachments and Actions artifacts **together with** the git data. A
//! 20 MiB repository can be over a 1 GiB quota, and the 413 will not say why. So the human view
//! always prints the full breakdown, even when only one category is non-zero.

pub(crate) mod size;
pub(crate) mod subject;

use clap::{Args as ClapArgs, Subcommand};
use forgejo_client::Api;
use forgejo_core::error::{Error, ErrorKind, Result};
use forgejo_model::{QuotaGroup, QuotaInfo, QuotaRuleInfo};

use crate::cmd::times::porcelain::{self, Fields, Machine};
use crate::global::GlobalOpts;
use crate::output::Term;
use crate::runtime::Runtime;

const INFO_FIELDS: Fields = Fields::Generated(forgejo_client::fields::FIELDS_QUOTA_INFO);
const RULE_FIELDS: Fields = Fields::Generated(forgejo_client::fields::FIELDS_QUOTA_RULE_INFO);
const GROUP_FIELDS: Fields = Fields::Generated(forgejo_client::fields::FIELDS_QUOTA_GROUP);
const USER_FIELDS: Fields = Fields::Generated(forgejo_client::fields::FIELDS_USER);

const LONG_ABOUT: &str = "\
Show storage usage and manage quotas.

Quotas apply to users or organizations, not individual repositories. size:all
includes repositories, LFS objects, packages, attachments, and Actions artifacts.
A limit of -1 means unlimited.

Subjects:
  size:all                          all storage
  size:repos:all / :public / :private   Git repositories
  size:git:all                      repositories and LFS
  size:git:lfs                      LFS objects
  size:assets:all                   attachments, artifacts, and packages
  size:assets:attachments:issues    issue and comment attachments
  size:assets:attachments:releases  release attachments
  size:assets:artifacts             Actions artifacts
  size:assets:packages:all          packages

If no rules appear, ask an administrator whether [quota] ENABLED is set in app.ini.

  fcli quota status                    # your usage against the rules that apply to you
  fcli quota status --org acme         # an organization's
  fcli quota rules list                # the rules that apply to you
  fcli quota groups list               # the groups you are in
  fcli quota rules list --all          # every rule on the instance (admin)";

#[derive(Debug, ClapArgs)]
#[command(long_about = LONG_ABOUT)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Usage against the rules that apply
    Status(StatusArgs),
    /// Quota rules: a limit and the subjects it covers
    #[command(subcommand)]
    Rules(RulesCmd),
    /// Quota groups: named bundles of rules, with users in them
    #[command(subcommand)]
    Groups(GroupsCmd),
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Show usage against the quota rules for a user or organization.

No checkout is required. The USED column totals the usage covered by each rule.")]
pub struct StatusArgs {
    /// An organization's quota instead of your own
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,

    /// Another user's quota; needs admin. `@me` is you
    #[arg(long, value_name = "USER", conflicts_with = "org")]
    pub user: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum RulesCmd {
    /// The rules that apply to you, or every rule with --all
    List(RulesListArgs),
    /// One rule (admin)
    View(NameArgs),
    /// Create a rule (admin)
    Create(RuleCreateArgs),
    /// Change a rule's limit or subjects (admin)
    Edit(RuleEditArgs),
    /// Delete a rule (admin)
    Delete(DeleteNameArgs),
}

#[derive(Debug, ClapArgs)]
pub struct RulesListArgs {
    /// Every rule on the instance, not just yours; needs admin
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, ClapArgs)]
pub struct NameArgs {
    /// Rule or group name
    #[arg(value_name = "NAME")]
    pub name: String,
}

#[derive(Debug, ClapArgs)]
pub struct DeleteNameArgs {
    /// Rule or group name
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Create a quota rule (admin required).

Attach it to a group with `fcli quota groups add-rule <group> <rule>` to apply it.

  fcli quota rules create small --bytes 1GiB --subject size:all
  fcli quota rules create no-lfs --bytes 0 --subject size:git:lfs
  fcli quota rules create exempt --bytes unlimited --subject size:all")]
pub struct RuleCreateArgs {
    /// Rule name
    #[arg(value_name = "NAME")]
    pub name: String,

    /// The limit: 1GiB, 500MB, 1048576, or `unlimited` / -1
    ///
    /// `--limit` is reserved for result counts.
    #[arg(long = "bytes", value_name = "SIZE")]
    pub limit: String,

    /// What it covers; repeatable. See `fcli quota --help` for the vocabulary
    #[arg(long, value_name = "SUBJECT", required = true)]
    pub subject: Vec<String>,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Edit a quota rule (admin required).

--subject replaces the full subject list. Omitted settings keep their current values.")]
pub struct RuleEditArgs {
    /// Rule to change
    #[arg(value_name = "NAME")]
    pub name: String,

    /// New limit. `--bytes`, not `--limit`; see `quota rules create`.
    #[arg(long = "bytes", value_name = "SIZE")]
    pub limit: Option<String>,

    /// Replace the subject list; repeatable
    #[arg(long, value_name = "SUBJECT")]
    pub subject: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum GroupsCmd {
    /// The groups you are in, or every group with --all
    List(GroupsListArgs),
    /// One group and its rules (admin)
    View(NameArgs),
    /// Create a group (admin)
    Create(NameArgs),
    /// Delete a group (admin)
    Delete(DeleteNameArgs),
    /// Attach an existing rule to a group (admin)
    AddRule(GroupRuleArgs),
    /// Detach a rule from a group (admin)
    RemoveRule(GroupRuleArgs),
    /// Put a user in a group (admin)
    AddUser(GroupUserArgs),
    /// Take a user out of a group (admin)
    RemoveUser(GroupUserArgs),
    /// Who is in a group (admin)
    Users(NameArgs),
}

#[derive(Debug, ClapArgs)]
pub struct GroupsListArgs {
    /// Every group on the instance, not just yours; needs admin
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, ClapArgs)]
pub struct GroupRuleArgs {
    /// Group name
    #[arg(value_name = "GROUP")]
    pub group: String,

    /// Rule name
    #[arg(value_name = "RULE")]
    pub rule: String,
}

#[derive(Debug, ClapArgs)]
pub struct GroupUserArgs {
    /// Group name
    #[arg(value_name = "GROUP")]
    pub group: String,

    /// User login; `@me` is you
    #[arg(value_name = "USER")]
    pub user: String,
}

impl Cmd {
    fn fields(&self) -> Option<Fields> {
        match self {
            Self::Status(_) => Some(INFO_FIELDS),
            Self::Rules(RulesCmd::Delete(_)) => None,
            Self::Rules(_) => Some(RULE_FIELDS),
            Self::Groups(GroupsCmd::Users(_)) => Some(USER_FIELDS),
            Self::Groups(
                GroupsCmd::Delete(_)
                | GroupsCmd::AddRule(_)
                | GroupsCmd::RemoveRule(_)
                | GroupsCmd::AddUser(_)
                | GroupsCmd::RemoveUser(_),
            ) => None,
            Self::Groups(_) => Some(GROUP_FIELDS),
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
        // Deliberately never touches `rt.repo`: a quota belongs to an account, so this whole
        // group works from a home directory.
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        match &args.command {
            Cmd::Status(a) => status(&rt, &api, globals, a).await,
            Cmd::Rules(c) => rules(&rt, &api, globals, c).await,
            Cmd::Groups(c) => groups(&rt, &api, globals, c).await,
        }
    })
}

// ------------------------------------------------------------------------------------ status

async fn status(rt: &Runtime, api: &Api, globals: &GlobalOpts, args: &StatusArgs) -> Result<()> {
    let (who, info) = match (&args.org, &args.user) {
        (Some(org), _) => (org.clone(), api.org().get_quota(org).await.map_err(explain)?),
        (_, Some(user)) => {
            let user = porcelain::resolve_user(rt, user).await?;
            let info = api.admin().get_user_quota(&user).await.map_err(explain)?;
            (user, info)
        }
        _ => ("you".to_owned(), api.user().get_quota().await.map_err(explain)?),
    };

    if let Some(m) = Machine::compile(globals, INFO_FIELDS)? {
        return m.write(globals, rt.term(), porcelain::json_of(&info)?);
    }
    porcelain::print(globals, &render_status(rt.term(), &rt.host().to_string(), &who, &info))
}

/// The `status` view: the usage breakdown, then every rule with the usage it is measured against.
///
/// Written as a `String` rather than straight to a writer so it can be snapshotted — this is the
/// view the whole group exists for, and a regression in it is a regression in the answer to
/// "why did I get a 413".
pub(crate) fn render_status(term: &Term, host: &str, who: &str, info: &QuotaInfo) -> String {
    // `let _ =` throughout: `fmt::Write` on a `String` cannot fail.
    use std::fmt::Write as _;
    let used = info.used.as_ref().map(subject::Used::from).unwrap_or_default();
    let rows = rule_rows(info, used);
    let mut o = String::new();

    if !term.tty {
        // TSV: one row per rule, so `fcli quota status | awk -F'\t' '$4>90'` finds the rule that
        // is about to bite. The breakdown is not in the piped form — `--json` carries it, and a
        // stable column count matters more here than completeness.
        for r in &rows {
            let _ = writeln!(
                o,
                "{}\t{}\t{}\t{}\t{}",
                r.subject,
                r.limit,
                r.used.map(|u| u.to_string()).unwrap_or_default(),
                r.percent.map(|p| format!("{p:.0}")).unwrap_or_default(),
                r.rule
            );
        }
        return o;
    }

    let _ = writeln!(o, "Quota for {who} on {host}");
    let _ = writeln!(o);
    let _ = writeln!(o, "used");
    for (label, bytes) in used.breakdown() {
        let _ = writeln!(o, "  {label:<24}{}", size::human(bytes));
    }
    let _ = writeln!(o, "  {:<24}{}", "total", size::human(used.all()));
    let _ = writeln!(o);
    let _ = writeln!(o, "  Quotas include Git data, LFS objects, packages, and release assets.");
    let _ = writeln!(o, "  Each rule applies to the storage categories listed under SUBJECT.");
    let _ = writeln!(o);

    if rows.is_empty() {
        let _ = writeln!(o, "No quota rules apply to {who}.");
        let _ = writeln!(
            o,
            "This account has no applicable limits, or quotas are disabled.\n\
             Quotas default to off: [quota] ENABLED = false in app.ini."
        );
        return o;
    }

    let _ = writeln!(o, "rules");
    let mut t = porcelain::table(term);
    t.headers(["RULE", "SUBJECT", "LIMIT", "USED", "USAGE"]);
    for r in &rows {
        t.row([
            porcelain::dash(&r.rule),
            r.subject.clone(),
            size::human(r.limit),
            r.used.map(size::human).unwrap_or_else(|| "?".to_owned()),
            usage_cell(term, r),
        ]);
    }
    // Indented into this view rather than emitted through `write_table`, because it is a detail
    // view that happens to contain a table — so no `Showing N of M` banner.
    o.push_str(&indent(&t.render_to_string(), "  "));

    let groups: Vec<&str> = info.groups.iter().map(|g| g.name.as_str()).collect();
    if !groups.is_empty() {
        let _ = writeln!(o);
        let _ = writeln!(o, "groups  {}", groups.join(", "));
    }
    if rows.iter().filter_map(|r| r.percent).any(|p| p >= 100.0) {
        let _ = writeln!(o);
        let _ = writeln!(
            o,
            "Over quota. Uploads and pushes may fail with HTTP 413.\n\
             Reduce usage in the affected category or ask an administrator to increase the limit."
        );
    }
    o
}

/// One rule, joined to the usage it is measured against.
#[derive(Debug)]
struct Row {
    rule: String,
    subject: String,
    limit: i64,
    /// `None` when the subject is one this build does not know — see [`subject::Used::for_subject`].
    used: Option<i64>,
    percent: Option<f64>,
}

/// Flatten the groups-of-rules-of-subjects tree into one row per (rule, subject).
///
/// One row per *subject*, not per rule: a rule covering both `size:git:lfs` and
/// `size:assets:packages:all` applies its limit to each separately, and collapsing them into one
/// row would have to invent a combined usage number that means nothing.
///
/// Duplicates are dropped. The same rule commonly arrives through two groups, and printing it
/// twice makes a user think they have two limits.
fn rule_rows(info: &QuotaInfo, used: subject::Used) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    for group in &info.groups {
        for rule in &group.rules {
            for subj in &rule.subjects {
                if rows.iter().any(|r| r.rule == rule.name && r.subject == *subj) {
                    continue;
                }
                let bytes = used.for_subject(subj);
                rows.push(Row {
                    rule: rule.name.clone(),
                    subject: subj.clone(),
                    limit: rule.limit,
                    used: bytes,
                    percent: bytes.and_then(|u| size::percent(u, rule.limit)),
                });
            }
        }
    }
    rows
}

fn usage_cell(term: &Term, row: &Row) -> String {
    match (row.used, row.percent) {
        (None, _) => "?".to_owned(),
        (_, None) => "unlimited".to_owned(),
        (Some(_), Some(pct)) => {
            let bar = size::bar(row.used.unwrap_or(0), row.limit, 10);
            if pct >= 100.0 {
                crate::output::color::paint(
                    term,
                    anstyle::Style::new().fg_color(Some(anstyle::AnsiColor::Red.into())),
                    &format!("{bar} OVER"),
                )
            } else if pct >= 90.0 {
                crate::output::color::paint(
                    term,
                    anstyle::Style::new().fg_color(Some(anstyle::AnsiColor::Yellow.into())),
                    &bar,
                )
            } else {
                bar
            }
        }
    }
}

fn indent(text: &str, prefix: &str) -> String {
    text.lines().map(|l| format!("{prefix}{l}\n")).collect()
}

// ------------------------------------------------------------------------------------- rules

async fn rules(rt: &Runtime, api: &Api, globals: &GlobalOpts, cmd: &RulesCmd) -> Result<()> {
    match cmd {
        RulesCmd::List(a) => {
            let rules = if a.all {
                api.admin().list_quota_rules().await.map_err(explain_admin)?
            } else {
                // Without admin there is no endpoint that lists rules — but a user's own quota
                // response embeds every rule that applies to them, which is the question
                // `rules list` is actually asking. Reading it from there means the common case
                // needs no admin token at all.
                let info = api.user().get_quota().await.map_err(explain)?;
                own_rules(&info)
            };
            write_rules(rt, globals, &rules, a.all).await
        }
        RulesCmd::View(a) => {
            let rule = api.admin().get_quota_rule(&a.name).await.map_err(explain_admin)?;
            write_rules(rt, globals, std::slice::from_ref(&rule), true).await
        }
        RulesCmd::Create(a) => {
            let limit = size::parse(&a.limit)?;
            warn_unknown_subjects(rt, &a.subject);
            let body = forgejo_model::CreateQuotaRuleOptions {
                limit: Some(limit),
                name: Some(a.name.clone()),
                subjects: Some(a.subject.clone()),
            };
            let rule = api.admin().create_quota_rule(&body).await.map_err(explain_admin)?;
            porcelain::note(
                rt.term(),
                &format!(
                    "Created rule {} ({} on {}). Apply it with `fcli quota groups add-rule <group> {}`.",
                    rule.name,
                    size::human(rule.limit),
                    rule.subjects.join(", "),
                    rule.name
                ),
            );
            report_rule(rt, globals, &rule)
        }
        RulesCmd::Edit(a) => {
            // PATCH replaces both fields, so read first and resend whatever was not named.
            // Otherwise `--limit 2GiB` alone would silently clear the rule's subjects, and a
            // rule with no subjects limits nothing.
            let current = api.admin().get_quota_rule(&a.name).await.map_err(explain_admin)?;
            let limit = match &a.limit {
                Some(text) => size::parse(text)?,
                None => current.limit,
            };
            let subjects =
                if a.subject.is_empty() { current.subjects.clone() } else { a.subject.clone() };
            warn_unknown_subjects(rt, &subjects);
            let body = forgejo_model::EditQuotaRuleOptions {
                limit: Some(limit),
                subjects: Some(subjects),
            };
            let rule = api.admin().edit_quota_rule(&a.name, &body).await.map_err(explain_admin)?;
            porcelain::note(
                rt.term(),
                &format!(
                    "Rule {} is now {} on {}",
                    rule.name,
                    size::human(rule.limit),
                    rule.subjects.join(", ")
                ),
            );
            report_rule(rt, globals, &rule)
        }
        RulesCmd::Delete(a) => {
            porcelain::confirm(
                rt,
                &format!(
                    "Delete quota rule {:?}? Every group it is attached to loses that limit.",
                    a.name
                ),
                a.yes,
            )?;
            api.admin().delete_quota_rule(&a.name).await.map_err(explain_admin)?;
            porcelain::note(rt.term(), &format!("Deleted rule {}", a.name));
            Ok(())
        }
    }
}

/// Every rule that applies to the authenticated user, de-duplicated across groups.
fn own_rules(info: &QuotaInfo) -> Vec<QuotaRuleInfo> {
    let mut out: Vec<QuotaRuleInfo> = Vec::new();
    for group in &info.groups {
        for rule in &group.rules {
            // Rule names are only visible to admins, so an unnamed rule is compared on its
            // limit and subjects instead — otherwise every anonymous rule would look like the
            // same one and all but the first would vanish.
            if !out.iter().any(|r| r == rule) {
                out.push(rule.clone());
            }
        }
    }
    out
}

async fn write_rules(
    rt: &Runtime,
    globals: &GlobalOpts,
    rules: &[QuotaRuleInfo],
    instance_wide: bool,
) -> Result<()> {
    let machine = Machine::compile(globals, RULE_FIELDS)?;
    if rules.is_empty() {
        return porcelain::empty(
            globals,
            rt.term(),
            machine.as_ref(),
            if instance_wide {
                "No quota rules on this instance. Quotas are off unless [quota] ENABLED = true."
            } else {
                "No quota rules apply to this account. Quotas may be disabled. Admins can list all rules with `fcli quota rules list --all`."
            },
        );
    }
    if let Some(m) = machine {
        return m.write(globals, rt.term(), porcelain::json_of(rules)?);
    }
    porcelain::print(globals, &render_rules(rt.term(), rules))
}

/// The `rules list` table.
///
/// A rule's `name` is admin-only in the API, so a non-admin sees `-` in that column. Said in the
/// column rather than in a footnote: an empty cell reads as a bug.
pub(crate) fn render_rules(term: &Term, rules: &[QuotaRuleInfo]) -> String {
    let mut t = porcelain::table(term);
    t.headers(["NAME", "LIMIT", "SUBJECTS"]);
    for r in rules {
        t.row([porcelain::dash(&r.name), size::human(r.limit), r.subjects.join(", ")]);
    }
    porcelain::rendered_table(term, t, "rules", None)
}

fn report_rule(rt: &Runtime, globals: &GlobalOpts, rule: &QuotaRuleInfo) -> Result<()> {
    match Machine::compile(globals, RULE_FIELDS)? {
        Some(m) => m.write(globals, rt.term(), porcelain::json_of(rule)?),
        None => Ok(()),
    }
}

/// Warn — never refuse — about a subject this build does not recognise.
///
/// A newer Forgejo may know subjects this build's vocabulary does not, and refusing one would
/// make `fcli` the reason an admin cannot configure their own instance. A typo is far more likely
/// though, so it is worth saying out loud.
fn warn_unknown_subjects(rt: &Runtime, subjects: &[String]) {
    for s in subjects {
        if !subject::is_known(s) {
            porcelain::note(
                rt.term(),
                &format!(
                    "note: {s:?} is not a subject this build knows; sending it anyway. \
                     `fcli quota --help` lists the ones it does."
                ),
            );
        }
    }
}

// ------------------------------------------------------------------------------------ groups

async fn groups(rt: &Runtime, api: &Api, globals: &GlobalOpts, cmd: &GroupsCmd) -> Result<()> {
    match cmd {
        GroupsCmd::List(a) => {
            let groups = if a.all {
                api.admin().list_quota_groups().await.map_err(explain_admin)?
            } else {
                api.user().get_quota().await.map_err(explain)?.groups
            };
            write_groups(rt, globals, &groups, a.all)
        }
        GroupsCmd::View(a) => {
            let group = api.admin().get_quota_group(&a.name).await.map_err(explain_admin)?;
            write_groups(rt, globals, std::slice::from_ref(&group), true)
        }
        GroupsCmd::Create(a) => {
            let body = forgejo_model::CreateQuotaGroupOptions {
                name: Some(a.name.clone()),
                rules: Some(Vec::new()),
            };
            let group = api.admin().create_quota_group(&body).await.map_err(explain_admin)?;
            porcelain::note(
                rt.term(),
                &format!(
                    "Created group {}. Add a rule with `fcli quota groups add-rule {} <rule>`.",
                    group.name, group.name
                ),
            );
            match Machine::compile(globals, GROUP_FIELDS)? {
                Some(m) => m.write(globals, rt.term(), porcelain::json_of(&group)?),
                None => Ok(()),
            }
        }
        GroupsCmd::Delete(a) => {
            porcelain::confirm(
                rt,
                &format!(
                    "Delete quota group {:?}? Its members lose the limits it carried.",
                    a.name
                ),
                a.yes,
            )?;
            api.admin().delete_quota_group(&a.name).await.map_err(explain_admin)?;
            porcelain::note(rt.term(), &format!("Deleted group {}", a.name));
            Ok(())
        }
        GroupsCmd::AddRule(a) => {
            api.admin().add_rule_to_quota_group(&a.group, &a.rule).await.map_err(explain_admin)?;
            porcelain::note(rt.term(), &format!("{} now applies to group {}", a.rule, a.group));
            Ok(())
        }
        GroupsCmd::RemoveRule(a) => {
            api.admin()
                .remove_rule_from_quota_group(&a.group, &a.rule)
                .await
                .map_err(explain_admin)?;
            porcelain::note(
                rt.term(),
                &format!("{} no longer applies to group {}", a.rule, a.group),
            );
            Ok(())
        }
        GroupsCmd::AddUser(a) => {
            let user = porcelain::resolve_user(rt, &a.user).await?;
            api.admin().add_user_to_quota_group(&a.group, &user).await.map_err(explain_admin)?;
            porcelain::note(rt.term(), &format!("{user} is now in group {}", a.group));
            Ok(())
        }
        GroupsCmd::RemoveUser(a) => {
            let user = porcelain::resolve_user(rt, &a.user).await?;
            api.admin()
                .remove_user_from_quota_group(&a.group, &user)
                .await
                .map_err(explain_admin)?;
            porcelain::note(rt.term(), &format!("{user} is no longer in group {}", a.group));
            Ok(())
        }
        GroupsCmd::Users(a) => {
            let users =
                api.admin().list_users_in_quota_group(&a.name).await.map_err(explain_admin)?;
            let machine = Machine::compile(globals, USER_FIELDS)?;
            if users.is_empty() {
                return porcelain::empty(
                    globals,
                    rt.term(),
                    machine.as_ref(),
                    &format!("Nobody is in group {}", a.name),
                );
            }
            if let Some(m) = machine {
                return m.write(globals, rt.term(), porcelain::json_of(&users)?);
            }
            let mut t = porcelain::table(rt.term());
            t.headers(["LOGIN", "NAME", "EMAIL"]);
            for u in &users {
                t.row([u.login.clone(), porcelain::dash(&u.full_name), porcelain::dash(&u.email)]);
            }
            porcelain::write_table(globals, rt.term(), t, "users", None)
        }
    }
}

fn write_groups(
    rt: &Runtime,
    globals: &GlobalOpts,
    groups: &[QuotaGroup],
    instance_wide: bool,
) -> Result<()> {
    let machine = Machine::compile(globals, GROUP_FIELDS)?;
    if groups.is_empty() {
        return porcelain::empty(
            globals,
            rt.term(),
            machine.as_ref(),
            if instance_wide {
                "No quota groups on this instance. Quotas are off unless [quota] ENABLED = true."
            } else {
                "This account has no quota groups. Admins can list all groups with `fcli quota groups list --all`."
            },
        );
    }
    if let Some(m) = machine {
        return m.write(globals, rt.term(), porcelain::json_of(groups)?);
    }
    porcelain::print(globals, &render_groups(rt.term(), groups))
}

/// The `groups list` table.
///
/// Both a count and the limits themselves: the count is what you scan, the limits are what you
/// needed to know, and a group of three `unlimited` rules is a materially different thing from a
/// group of three real ones.
pub(crate) fn render_groups(term: &Term, groups: &[QuotaGroup]) -> String {
    let mut t = porcelain::table(term);
    t.headers(["NAME", "RULES", "LIMITS"]);
    for g in groups {
        t.row([
            g.name.clone(),
            g.rules.len().to_string(),
            g.rules
                .iter()
                .map(|r| format!("{} on {}", size::human(r.limit), r.subjects.join("+")))
                .collect::<Vec<_>>()
                .join("; "),
        ]);
    }
    porcelain::rendered_table(term, t, "groups", None)
}

// ------------------------------------------------------------------------------------ errors

/// A 404 on a quota endpoint almost always means the feature is switched off, not that the
/// account is missing.
///
/// `[quota] ENABLED = false` is Forgejo's default, and with it the routes answer 404. Reporting
/// that as "not found" sends the user looking for a typo in their own username.
fn explain(e: Error) -> Error {
    match &*e.kind {
        ErrorKind::RouteNotFound { .. } | ErrorKind::ResourceNotFound { .. } => {
            Error::new(ErrorKind::Usage(
                "quota endpoint unavailable. Quotas are off by default and require Forgejo with [quota] ENABLED = true in app.ini (restart required). If enabled, check the account name."
                    .to_owned(),
            ))
        }
        _ => e,
    }
}

/// The same, plus the fact that the endpoint needs an admin token.
fn explain_admin(e: Error) -> Error {
    match &*e.kind {
        ErrorKind::Forbidden { .. } => Error::new(ErrorKind::Usage(
            "that quota endpoint is administrator-only. Without admin you can still see what \
             applies to you: `fcli quota status`, `fcli quota rules list`, \
             `fcli quota groups list`."
                .to_owned(),
        )),
        _ => explain(e),
    }
}

/// A JSON summary of one account's quota, for tests and for future reuse.
///
/// Not wired into `--json` — that emits the API's own `QuotaInfo` document, per
/// `docs/output.md` — but the row computation is worth exercising without a terminal.
#[cfg(test)]
fn summary(info: &QuotaInfo) -> serde_json::Value {
    let used = info.used.as_ref().map(subject::Used::from).unwrap_or_default();
    serde_json::json!({
        "total_used": used.all(),
        "rules": rule_rows(info, used)
            .iter()
            .map(|r| serde_json::json!({
                "rule": r.rule,
                "subject": r.subject,
                "limit": r.limit,
                "used": r.used,
            }))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::times::porcelain::testing;
    use forgejo_core::http::transport::{Canned, FakeTransport};
    use forgejo_model::{QuotaUsed, QuotaUsedSize, QuotaUsedSizeGit, QuotaUsedSizeRepos};
    use std::sync::Arc;

    /// One instance's response, as Forgejo actually shapes it: the usage tree's *leaves* under
    /// `used.size`, and rules that name *aggregates*. The mismatch is the whole reason
    /// `subject::Used` exists.
    const QUOTA: &str = r#"{
      "groups":[{"name":"default","rules":[
        {"name":"small","limit":1073741824,"subjects":["size:all"]},
        {"name":"no-lfs","limit":0,"subjects":["size:git:lfs"]},
        {"name":"exempt","limit":-1,"subjects":["size:assets:artifacts"]}
      ]}],
      "used":{"size":{
        "repos":{"public":524288000,"private":0},
        "git":{"LFS":104857600},
        "assets":{"artifacts":1048576,
                  "attachments":{"issues":2048,"releases":4096},
                  "packages":{"all":209715200}}
      }}
    }"#;

    fn quota() -> QuotaInfo {
        serde_json::from_str(QUOTA).expect("the fixture is valid QuotaInfo JSON")
    }

    fn info() -> QuotaInfo {
        QuotaInfo {
            groups: vec![
                QuotaGroup {
                    name: "default".to_owned(),
                    rules: vec![QuotaRuleInfo {
                        limit: 1_024,
                        name: "small".to_owned(),
                        subjects: vec!["size:all".to_owned(), "size:git:lfs".to_owned()],
                    }],
                },
                // The same rule arriving through a second group, which is the normal shape once
                // an admin has more than one group.
                QuotaGroup {
                    name: "extra".to_owned(),
                    rules: vec![QuotaRuleInfo {
                        limit: 1_024,
                        name: "small".to_owned(),
                        subjects: vec!["size:all".to_owned()],
                    }],
                },
            ],
            used: Some(QuotaUsed {
                size: Some(QuotaUsedSize {
                    repos: Some(QuotaUsedSizeRepos { public: 512, private: 0 }),
                    git: Some(QuotaUsedSizeGit { lfs: 1_024 }),
                    assets: None,
                }),
            }),
        }
    }

    /// Bug this prevents: the same rule reaching a user through two groups and being reported as
    /// two independent limits, so `fcli quota status` shows one 1 KiB quota twice and the reader
    /// concludes something is doubly restricted.
    #[test]
    fn a_rule_reached_through_two_groups_is_reported_once() {
        let rows = rule_rows(&info(), subject::Used::from(info().used.as_ref().unwrap()));
        assert_eq!(rows.len(), 2, "one row per subject, not per group: {rows:?}");
        assert_eq!(rows[0].subject, "size:all");
        assert_eq!(rows[1].subject, "size:git:lfs");
    }

    /// Bug this prevents: comparing a `size:all` limit against the repository size alone. Here
    /// 512 bytes of repository plus 1024 of LFS is 1536 against a 1024 limit — over quota — and
    /// a reader looking only at the repository would see 50% and be reassured.
    #[test]
    fn usage_is_summed_across_categories_so_over_quota_is_visible() {
        let i = info();
        let used = subject::Used::from(i.used.as_ref().unwrap());
        let rows = rule_rows(&i, used);
        let all = rows.iter().find(|r| r.subject == "size:all").unwrap();
        assert_eq!(all.used, Some(1_536));
        assert!(all.percent.unwrap() > 100.0, "{:?}", all.percent);

        let lfs = rows.iter().find(|r| r.subject == "size:git:lfs").unwrap();
        assert_eq!(lfs.used, Some(1_024));
    }

    /// Bug this prevents: `status` reaching for the wrong endpoint. Three accounts, three paths,
    /// and the admin one is the only way to read someone else's — asking `/user/quota` with
    /// `--user bob` would silently report your own numbers under bob's name.
    #[tokio::test]
    async fn each_account_kind_has_its_own_endpoint() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(testing::method("GET"), "/api/v1/user/quota", Canned::json(200, QUOTA))
                .on(testing::method("GET"), "/api/v1/orgs/acme/quota", Canned::json(200, QUOTA))
                .on(
                    testing::method("GET"),
                    "/api/v1/admin/users/bob/quota",
                    Canned::json(200, QUOTA),
                )
                .fallback(Canned::json(404, r#"{"message":"no"}"#)),
        );
        let api = testing::api(fake.clone());
        api.user().get_quota().await.unwrap();
        api.org().get_quota("acme").await.unwrap();
        api.admin().get_user_quota("bob").await.unwrap();
        for path in
            ["/api/v1/user/quota", "/api/v1/orgs/acme/quota", "/api/v1/admin/users/bob/quota"]
        {
            assert_eq!(fake.calls_to(&testing::method("GET"), path).len(), 1, "{path}");
        }
        // None of them takes a repository, which is why this group works outside a checkout.
        assert!(fake.calls().iter().all(|c| !c.path.contains("/repos/")));
    }

    /// The view that answers "why did I get a 413", both ways. The `size:all` row is over its
    /// limit *because* of LFS and packages, which no single leaf of the response would show.
    #[test]
    fn the_status_view_renders_the_same_data_two_ways() {
        insta::assert_snapshot!(
            "quota_status_human",
            render_status(&testing::term(), "git.example.org", "you", &quota())
        );
        insta::assert_snapshot!(
            "quota_status_piped",
            render_status(&Term::piped(), "git.example.org", "you", &quota())
        );
    }

    /// An account with nothing limiting it, which is what the *default* container answers with —
    /// quotas are off unless `[quota] ENABLED = true`. The view has to say so rather than print an
    /// empty rules table.
    #[test]
    fn an_account_with_no_rules_says_quotas_may_be_switched_off() {
        let out = render_status(&testing::term(), "localhost:3000", "you", &QuotaInfo::default());
        assert!(out.contains("[quota] ENABLED = false"), "{out}");
        insta::assert_snapshot!("quota_status_no_rules", out);
    }

    #[test]
    fn the_rules_and_groups_tables_render() {
        let info = quota();
        insta::assert_snapshot!(
            "quota_rules_human",
            render_rules(&testing::term(), &own_rules(&info))
        );
        insta::assert_snapshot!(
            "quota_groups_human",
            render_groups(&testing::term(), &info.groups)
        );
    }

    #[test]
    fn the_json_output_uses_the_apis_own_field_names() {
        insta::assert_snapshot!(
            "quota_rules_json",
            testing::as_json(
                RULE_FIELDS,
                "name,limit,subjects",
                porcelain::json_of(&own_rules(&quota())).unwrap()
            )
        );
        insta::assert_snapshot!(
            "quota_status_json",
            testing::as_json(INFO_FIELDS, "groups,used", porcelain::json_of(&quota()).unwrap())
        );
    }

    #[test]
    fn the_summary_document_carries_every_rule_and_its_usage() {
        insta::assert_json_snapshot!(summary(&info()));
    }

    /// Bug this prevents: a 404 from an instance with quotas switched off being reported as "not
    /// found", which reads as a mistyped username. Quotas are off by default, so this is the
    /// *common* case, not an edge one.
    #[test]
    fn a_404_blames_the_feature_switch_rather_than_the_account() {
        let e = explain(Error::new(ErrorKind::RouteNotFound {
            method: "GET".to_owned(),
            path: "/user/quota".to_owned(),
            instance: None,
        }));
        let msg = e.to_string();
        assert!(msg.contains("off by default"), "{msg}");
        assert!(msg.contains("[quota] ENABLED"), "{msg}");
    }

    /// A 403 on an admin-only quota endpoint must name the three commands that do work without
    /// admin, rather than just refusing.
    #[test]
    fn an_admin_only_refusal_names_the_non_admin_alternatives() {
        let e = explain_admin(Error::new(ErrorKind::Forbidden {
            server_message: "admin only".to_owned(),
        }));
        let msg = e.to_string();
        assert!(msg.contains("fcli quota status"), "{msg}");
        assert!(msg.contains("rules list"), "{msg}");
    }
}
