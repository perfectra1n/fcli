//! `fcli nodeinfo` — what kind of instance this is, **before you log in**.
//!
//! # The gap this fills
//!
//! Every other command in `fcli` assumes a configured host and a token. This one assumes neither,
//! because the question it answers comes *first*: someone has a URL and wants to know whether it
//! is Forgejo, which version, whether they can even register, and what the API will let them ask
//! for. All of that is public — `/nodeinfo` is the NodeInfo 2.1 discovery document the fediverse
//! standardised, and `/settings/api` is unauthenticated on a default instance.
//!
//! `tea` has the inverse of this problem: it probes the version on *every* command, gets it wrong
//! against a Forgejo that is not the Gitea it expects, and ships `--no-version-check` to switch the
//! probe off. Making the probe an explicit command instead means nothing else has to do it.
//!
//! # Why it can bypass [`crate::runtime::Runtime`]
//!
//! `Runtime::new` resolves a host out of `hosts.toml`, and `Hosts::adopt_env_host` only adopts a
//! `--host` that has *no* entry when a token is also in the environment — which is right for every
//! other command, and exactly wrong for this one. So when host resolution fails and `--host` was
//! given, this command builds a bare unauthenticated [`Client`] against that URL. That is what
//! makes
//!
//! ```text
//! fcli --host codeberg.org nodeinfo
//! ```
//!
//! work on a machine with no configuration at all, which is the whole point.
//!
//! # Why `/settings/api` failing is not fatal
//!
//! An instance can require authentication for `/settings/api`, or sit behind a proxy that blocks
//! it. The NodeInfo half still answers "is this Forgejo and which version", which is most of the
//! value, so a failure there is a note on stderr rather than a non-zero exit.

use clap::{Args as ClapArgs, Subcommand};
use forgejo_client::Api;
use forgejo_core::config::HostEntry;
use forgejo_core::error::{Error, ErrorKind, Result};
use forgejo_core::http::{Auth, Client};
use forgejo_model::{GeneralApiSettings, NodeInfo};

use crate::cmd::times::porcelain::{self, Fields, Machine};
use crate::global::GlobalOpts;
use crate::output::Term;

const NODE_FIELDS: Fields = Fields::Generated(forgejo_client::fields::FIELDS_NODE_INFO);
const SETTINGS_FIELDS: Fields =
    Fields::Generated(forgejo_client::fields::FIELDS_GENERAL_API_SETTINGS);

const LONG_ABOUT: &str = "\
Show server software, version, registration status, and API limits.

No authentication is required. Use --host to query an unconfigured server.
`nodeinfo` combines /nodeinfo and /settings/api; `nodeinfo limits` shows API limits,
including max_response_items (the maximum page size).

JSON fields follow each endpoint: NodeInfo uses camelCase; API settings use snake_case.

  fcli nodeinfo
  fcli nodeinfo --json software --jq .software.version
  fcli nodeinfo limits
  fcli --host codeberg.org nodeinfo";

#[derive(Debug, ClapArgs)]
#[command(long_about = LONG_ABOUT)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Only the API limits, from /settings/api
    Limits,
}

impl Args {
    fn fields(&self) -> Fields {
        match self.command {
            Some(Cmd::Limits) => SETTINGS_FIELDS,
            None => NODE_FIELDS,
        }
    }
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    if porcelain::discovery(globals, args.fields())? {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let (client, term, host) = connect(globals)?;
        let api = Api::new(client);
        match args.command {
            Some(Cmd::Limits) => limits(&api, globals, &term).await,
            None => overview(&api, globals, &term, &host).await,
        }
    })
}

/// A client, a terminal, and the host's label — without insisting on a configured host.
///
/// Prefers the full [`crate::runtime::Runtime`], so a configured host keeps its retry policy, its
/// `hosts.toml` URL (including a subpath install), and any token that happens to be present — a
/// token is not *needed* here, but an instance with `REQUIRE_SIGNIN_VIEW` will refuse without one,
/// and silently dropping a credential the user has would break that case.
///
/// Falls back to a bare unauthenticated client when resolution failed *and* `--host` named
/// something, which is the fresh-install case this command exists for.
fn connect(globals: &GlobalOpts) -> Result<(Client, Term, String)> {
    match crate::runtime::Runtime::new(globals) {
        Ok(rt) => {
            let host = rt.host().to_string();
            Ok((rt.client().clone(), *rt.term(), host))
        }
        Err(e) => {
            let unconfigured =
                matches!(&*e.kind, ErrorKind::NoHostConfigured | ErrorKind::UnknownHost { .. });
            let Some(given) = globals.host.as_deref().filter(|_| unconfigured) else {
                return Err(e);
            };
            // `HostEntry::from_input` owns the "did they mean http or https" decision, and it has
            // a test table behind it. Re-deriving the scheme here would be a second, worse copy.
            let entry = HostEntry::from_input(given)?;
            let client = Client::builder(&entry.url, Auth::None)
                .user_agent(crate::runtime::user_agent())
                .build()?;
            Ok((client, Term::detect(), entry.name.to_string()))
        }
    }
}

// ---------------------------------------------------------------------------------- overview

async fn overview(api: &Api, globals: &GlobalOpts, term: &Term, host: &str) -> Result<()> {
    let info = api.misc().get_node_info().await.map_err(explain)?;

    if let Some(m) = Machine::compile(globals, NODE_FIELDS)? {
        return m.write(globals, term, porcelain::json_of(&info)?);
    }

    // Fetched only for the human view, and only best-effort: the NodeInfo half already answers
    // "is this Forgejo", so an instance that guards `/settings/api` still gets a useful answer.
    let settings = match api.settings().get_general_api_settings().await {
        Ok(s) => Some(s),
        Err(e) => {
            porcelain::note(
                term,
                &format!(
                    "note: {host} did not answer /settings/api ({}), so the API limits below are \
                     unknown. Some instances require a token for it.",
                    headline(&e)
                ),
            );
            None
        }
    };
    porcelain::print(globals, &render_overview(term, host, &info, settings.as_ref()))
}

/// The overview: what the instance is, what it allows, and the limits it enforces.
///
/// Two documents in one view, which is the orchestration that earns this a porcelain command:
/// `/nodeinfo` answers "is this Forgejo and which version" and `/settings/api` answers "what will
/// it let me ask for", and nobody wants those separately.
pub(crate) fn render_overview(
    term: &Term,
    host: &str,
    info: &NodeInfo,
    settings: Option<&GeneralApiSettings>,
) -> String {
    // `let _ =` throughout: `fmt::Write` on a `String` cannot fail.
    use std::fmt::Write as _;
    let software = info.software.as_ref();
    let name = software.map(|s| s.name.as_str()).unwrap_or("");
    let version = software.map(|s| s.version.as_str()).unwrap_or("");
    let mut o = String::new();

    if !term.tty {
        // One TAB-separated line: `software<TAB>version<TAB>registrations<TAB>max_response_items`.
        // Four fields chosen so `fcli nodeinfo | cut -f2` is the version check a script wants.
        let _ = writeln!(
            o,
            "{}\t{}\t{}\t{}",
            name,
            version,
            if info.open_registrations { "open" } else { "closed" },
            settings.map(|s| s.max_response_items.to_string()).unwrap_or_default()
        );
        return o;
    }

    let _ = writeln!(o, "{host}");
    let _ = writeln!(o);
    let _ = writeln!(o, "software      {}", porcelain::dash(&describe_software(name, version)));
    if let Some(s) = software
        && !s.homepage.is_empty()
    {
        let _ = writeln!(o, "homepage      {}", s.homepage);
    }
    let _ = writeln!(
        o,
        "registration  {}",
        if info.open_registrations {
            "open (anyone can sign up)"
        } else {
            "closed (admin creates accounts)"
        }
    );
    if !info.protocols.is_empty() {
        let _ = writeln!(o, "protocols     {}", info.protocols.join(", "));
    }
    if let Some(u) = &info.usage {
        let users = u.users.as_ref();
        let _ = writeln!(
            o,
            "users         {} total, {} active this month",
            users.map(|x| x.total).unwrap_or(0),
            users.map(|x| x.active_month).unwrap_or(0)
        );
        let _ = writeln!(o, "content       {} posts, {} comments", u.local_posts, u.local_comments);
    }
    let _ = writeln!(o, "nodeinfo      schema {}", porcelain::dash(&info.version));

    let _ = writeln!(o);
    match settings {
        Some(s) => {
            let _ = writeln!(o, "api limits");
            // `max_response_items` first and explained, because it is the one that silently
            // truncates a paginated walk — the failure `forgejo_core::http::paginate` exists to
            // survive, and the reason most "my loop stopped after 50" reports happen.
            let _ = writeln!(
                o,
                "  max_response_items          {}  every `limit` is clamped to this, silently",
                s.max_response_items
            );
            let _ = writeln!(
                o,
                "  default_paging_num          {}  page size when none is asked for",
                s.default_paging_num
            );
            let _ = writeln!(
                o,
                "  default_git_trees_per_page  {}  entries per page of a git tree",
                s.default_git_trees_per_page
            );
            let _ = writeln!(
                o,
                "  default_max_blob_size       {}  above this, file contents are omitted",
                crate::cmd::quota::size::human(s.default_max_blob_size)
            );
        }
        None => {
            let _ = writeln!(o, "api limits    unknown");
        }
    }

    if let Some(note) = flavour_note(name) {
        let _ = writeln!(o);
        let _ = writeln!(o, "{note}");
    }
    o
}

// ------------------------------------------------------------------------------------ limits

async fn limits(api: &Api, globals: &GlobalOpts, term: &Term) -> Result<()> {
    let settings = api.settings().get_general_api_settings().await.map_err(explain)?;
    if let Some(m) = Machine::compile(globals, SETTINGS_FIELDS)? {
        return m.write(globals, term, porcelain::json_of(&settings)?);
    }
    porcelain::print(globals, &render_limits(term, &settings))
}

/// The `limits` table, with a column saying what each number does to a request.
pub(crate) fn render_limits(term: &Term, settings: &GeneralApiSettings) -> String {
    let mut t = porcelain::table(term);
    t.headers(["SETTING", "VALUE", "MEANING"]);
    t.row([
        "max_response_items".to_owned(),
        settings.max_response_items.to_string(),
        "every `limit` query parameter is clamped to this, without saying so".to_owned(),
    ]);
    t.row([
        "default_paging_num".to_owned(),
        settings.default_paging_num.to_string(),
        "page size when a request does not ask for one".to_owned(),
    ]);
    t.row([
        "default_git_trees_per_page".to_owned(),
        settings.default_git_trees_per_page.to_string(),
        "entries per page when listing a git tree".to_owned(),
    ]);
    t.row([
        "default_max_blob_size".to_owned(),
        crate::cmd::quota::size::human(settings.default_max_blob_size),
        "file contents above this are omitted from responses".to_owned(),
    ]);
    porcelain::rendered_table(term, t, "settings", None)
}

// ------------------------------------------------------------------------------------ shared

fn describe_software(name: &str, version: &str) -> String {
    match (name.is_empty(), version.is_empty()) {
        (true, true) => String::new(),
        (true, false) => version.to_owned(),
        (false, true) => name.to_owned(),
        (false, false) => format!("{name} {version}"),
    }
}

/// A line about what the reported software means for `fcli`.
///
/// The honest version of a version check: `fcli` is generated from Forgejo's specification, so a
/// Gitea instance mostly works and a few endpoints do not exist there at all — quotas being the
/// clearest example. Saying that once, here, is why no other command has to probe.
fn flavour_note(name: &str) -> Option<String> {
    match name.trim().to_ascii_lowercase().as_str() {
        "forgejo" => None,
        "gitea" => Some(
            "This server runs Gitea. Forgejo-specific commands, including fcli quota and AGit, may be unavailable."
                .to_owned(),
        ),
        "" => Some(
            "The instance did not name its software. That is unusual for Forgejo; it may be a \
             proxy answering /nodeinfo, or a fork that trimmed the document."
                .to_owned(),
        ),
        other => Some(format!(
            "This server reports {other:?}, not Forgejo. Unsupported API endpoints will return 404."
        )),
    }
}

/// The first line of a rendered error, for a one-line note.
fn headline(e: &Error) -> String {
    forgejo_core::error::render::headline(e.kind())
}

/// `/nodeinfo` missing means this is almost certainly not a Forgejo or Gitea instance.
///
/// The default 404 message talks about resources and tokens, neither of which is the issue when
/// the whole discovery document is absent. Naming the likely cause — wrong URL, or a plain web
/// server — is the difference between one more command and half an hour.
fn explain(e: Error) -> Error {
    match &*e.kind {
        ErrorKind::RouteNotFound { .. } | ErrorKind::ResourceNotFound { .. } => {
            Error::new(ErrorKind::Usage(
                "/api/v1/nodeinfo was not found. This may not be a Forgejo or Gitea server, or the URL may be wrong. Check the browser address, including any subpath such as https://example.org/forgejo."
                    .to_owned(),
            ))
        }
        ErrorKind::NotAuthenticated { .. } | ErrorKind::TokenRejected { .. } => {
            Error::new(ErrorKind::Usage(
                "this instance requires a credential even for its public metadata, which means \
                 REQUIRE_SIGNIN_VIEW is on. Run `fcli auth login` first, or pass a token in \
                 $FORGEJO_TOKEN."
                    .to_owned(),
            ))
        }
        _ => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::times::porcelain::testing;
    use forgejo_core::http::transport::{Canned, FakeTransport};
    use std::sync::Arc;

    const NODEINFO: &str = r#"{
      "version":"2.1",
      "software":{"name":"forgejo","version":"16.0.4",
                  "repository":"https://codeberg.org/forgejo/forgejo","homepage":"https://forgejo.org/"},
      "protocols":["activitypub"],
      "services":{"inbound":[],"outbound":[]},
      "openRegistrations":false,
      "usage":{"users":{"total":3,"activeHalfyear":2,"activeMonth":1},
               "localPosts":7,"localComments":11},
      "metadata":{}
    }"#;

    const SETTINGS: &str = r#"{"max_response_items":50,"default_paging_num":30,
      "default_git_trees_per_page":1000,"default_max_blob_size":10485760}"#;

    fn info() -> NodeInfo {
        serde_json::from_str(NODEINFO).expect("the fixture is valid NodeInfo JSON")
    }

    fn settings() -> GeneralApiSettings {
        serde_json::from_str(SETTINGS).expect("the fixture is valid GeneralApiSettings JSON")
    }

    /// The property the whole command rests on, and the one the task asks for: **no credential at
    /// all**. `Auth::None` means no `Authorization` header reaches the wire, and both endpoints
    /// still answer.
    #[tokio::test]
    async fn both_endpoints_are_read_with_no_credential() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(testing::method("GET"), "/api/v1/nodeinfo", Canned::json(200, NODEINFO))
                .on(testing::method("GET"), "/api/v1/settings/api", Canned::json(200, SETTINGS))
                .fallback(Canned::json(404, r#"{"message":"no"}"#)),
        );
        // Built by hand rather than through `testing::api`, because that one attaches a token and
        // the absence of one is exactly what is under test.
        let client = Client::builder("https://git.example.org", Auth::None)
            .transport(fake.clone())
            .build()
            .expect("a well-formed base URL");
        let api = Api::new(client);

        let ni = api.misc().get_node_info().await.unwrap();
        assert_eq!(ni.software.as_ref().unwrap().name, "forgejo");
        let s = api.settings().get_general_api_settings().await.unwrap();
        assert_eq!(s.max_response_items, 50);

        for call in fake.calls() {
            assert!(
                call.header("authorization").is_none(),
                "{} {} sent an Authorization header",
                call.method,
                call.path
            );
        }
    }

    /// Bug this prevents: `/settings/api` failing taking the whole command with it. The NodeInfo
    /// half already answers "is this Forgejo and which version", which is most of the value, so a
    /// guarded or proxied `/settings/api` must degrade to "unknown" rather than to a non-zero exit.
    #[tokio::test]
    async fn a_guarded_settings_endpoint_does_not_fail_the_command() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(testing::method("GET"), "/api/v1/nodeinfo", Canned::json(200, NODEINFO))
                .on(
                    testing::method("GET"),
                    "/api/v1/settings/api",
                    Canned::json(401, r#"{"message":"token required"}"#),
                ),
        );
        let api = testing::api(fake);
        let ni = api.misc().get_node_info().await.unwrap();
        assert!(api.settings().get_general_api_settings().await.is_err());
        // The view renders without the settings half, and says the limits are unknown rather than
        // inventing a number.
        let out = render_overview(&testing::term(), "git.example.org", &ni, None);
        assert!(out.contains("api limits    unknown"), "{out}");
    }

    #[test]
    fn the_overview_renders_the_same_data_two_ways() {
        insta::assert_snapshot!(
            "nodeinfo_human",
            render_overview(&testing::term(), "git.example.org", &info(), Some(&settings()))
        );
        insta::assert_snapshot!(
            "nodeinfo_piped",
            render_overview(&Term::piped(), "git.example.org", &info(), Some(&settings()))
        );
    }

    #[test]
    fn the_json_output_uses_the_documents_own_field_names() {
        // camelCase here, because NodeInfo *is* camelCase — `fcli` never translates, so
        // `openRegistrations` is the name to select. See `docs/output.md`, divergence 1.
        insta::assert_snapshot!(
            "nodeinfo_json",
            testing::as_json(
                NODE_FIELDS,
                "software,openRegistrations,protocols,version",
                porcelain::json_of(&info()).unwrap()
            )
        );
        insta::assert_snapshot!(
            "nodeinfo_limits_json",
            testing::as_json(
                SETTINGS_FIELDS,
                "max_response_items,default_paging_num",
                porcelain::json_of(&settings()).unwrap()
            )
        );
    }

    #[test]
    fn the_limits_table_explains_what_each_number_does() {
        insta::assert_snapshot!(
            "nodeinfo_limits_human",
            render_limits(&testing::term(), &settings())
        );
    }

    #[test]
    fn software_is_named_even_when_half_the_document_is_missing() {
        assert_eq!(describe_software("forgejo", "16.0.4"), "forgejo 16.0.4");
        assert_eq!(describe_software("forgejo", ""), "forgejo");
        assert_eq!(describe_software("", "16.0.4"), "16.0.4");
        assert_eq!(describe_software("", ""), "");
    }

    /// Bug this prevents: reporting a Gitea instance as if everything will work. The APIs overlap
    /// enough that most commands do, which is precisely why the difference has to be stated —
    /// otherwise `fcli quota status` 404ing looks like a bug in fcli.
    #[test]
    fn a_non_forgejo_instance_is_named_and_forgejo_is_not_nagged_about() {
        assert!(flavour_note("forgejo").is_none());
        assert!(flavour_note("Forgejo").is_none(), "the field's case is the server's choice");
        let gitea = flavour_note("gitea").unwrap();
        assert!(gitea.contains("fcli quota"), "{gitea}");
        let other = flavour_note("mastodon").unwrap();
        assert!(other.contains("mastodon"), "{other}");
        assert!(flavour_note("").is_some(), "a document with no software name is worth noting");
    }

    /// Bug this prevents: a 404 on `/nodeinfo` being reported as "resource not found", which
    /// sends the user looking for a missing repository rather than at the URL they typed.
    #[test]
    fn a_missing_nodeinfo_document_blames_the_url_not_the_token() {
        let e = explain(Error::new(ErrorKind::RouteNotFound {
            method: "GET".to_owned(),
            path: "/nodeinfo".to_owned(),
            instance: None,
        }));
        let msg = e.to_string();
        assert!(msg.contains("not be a Forgejo or Gitea"), "{msg}");
        assert!(msg.contains("subpath"), "{msg}");
    }

    /// A 401 here means `REQUIRE_SIGNIN_VIEW`, which is a specific setting with a specific
    /// remedy — not the generic "your token was rejected".
    #[test]
    fn a_401_on_public_metadata_names_require_signin_view() {
        let e =
            explain(Error::new(ErrorKind::NotAuthenticated { host: "git.example.org".to_owned() }));
        assert!(e.to_string().contains("REQUIRE_SIGNIN_VIEW"), "{e}");
    }

    /// The command's whole premise, asserted on the help text: it needs no credential, and it
    /// works against a host that is not configured.
    #[test]
    fn the_help_promises_it_works_before_logging_in() {
        assert!(LONG_ABOUT.contains("No authentication is required"), "{LONG_ABOUT}");
        assert!(LONG_ABOUT.contains("--host codeberg.org nodeinfo"), "{LONG_ABOUT}");
        assert!(LONG_ABOUT.contains("max_response_items"), "{LONG_ABOUT}");
    }
}
