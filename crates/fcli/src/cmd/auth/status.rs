//! `fcli auth status` — who you are on each host, and whether the token still works.
//!
//! Two properties are load-bearing and both are pinned by tests:
//!
//! * **No token, ever, in any form.** This is the output people paste into bug reports and
//!   screenshots. There is deliberately no `--show-token`; `fcli auth token` is the scripting
//!   exit, and it warns when it is about to write a secret into scrollback.
//! * **`--json` exits 0 even when a host fails.** That is `gh`'s behaviour and scripts depend on
//!   it: a machine-readable report of a *failure* is a successful report. Without `--json` the
//!   command reports the failure through the taxonomy, so a shell `if` still works.

use std::io::Write;

use clap::Args as ClapArgs;
use forgejo_core::config::secrets::CredentialStore;
use forgejo_core::config::{HostEntry, HostKey};
use forgejo_core::error::{Error, ErrorKind, Result, TokenSource, render};
use serde_json::{Value, json};

use super::common::{self, Setup};
use crate::cmd::support;
use crate::cmd::support::machine::Triad;
use crate::global::GlobalOpts;
use crate::output::{Term, project::FieldKind, project::FieldSpec};

/// `--json` selectable fields.
///
/// Hand-written because this command reports *fcli's* state rather than an API resource, so there
/// is no generated table in `forgejo_client::fields` to borrow. Names are snake_case for the same
/// reason every other `--json` name is (see `docs/output.md`).
const FIELDS: &[FieldSpec] = &[
    FieldSpec { name: "host", kind: FieldKind::Str, doc: "host key as fcli stores it" },
    FieldSpec { name: "url", kind: FieldKind::Str, doc: "instance base URL" },
    FieldSpec { name: "login", kind: FieldKind::Str, doc: "account name on that host" },
    FieldSpec { name: "active", kind: FieldKind::Bool, doc: "the login fcli uses by default" },
    FieldSpec { name: "active_host", kind: FieldKind::Bool, doc: "the host fcli uses by default" },
    FieldSpec {
        name: "credential_store",
        kind: FieldKind::Enum(CredentialStore::VALUES),
        doc: "where the token is kept",
    },
    FieldSpec {
        name: "token_source",
        kind: FieldKind::Str,
        doc: "the exact keyring entry, file or variable; never the token",
    },
    FieldSpec {
        name: "authenticated",
        kind: FieldKind::Bool,
        doc: "whether GET /user succeeded just now",
    },
    FieldSpec { name: "scopes", kind: FieldKind::Array(&FieldKind::Str), doc: "recorded at login" },
    FieldSpec { name: "is_admin", kind: FieldKind::Bool, doc: "site administrator" },
    FieldSpec { name: "error", kind: FieldKind::Str, doc: "why authentication failed, or null" },
];

#[derive(Debug, ClapArgs)]
#[command(after_long_help = LONG_HELP)]
pub struct Args {}

const LONG_HELP: &str = "\
Show saved accounts and check their tokens.

Returns a nonzero exit code if any host fails authentication. With --json, returns
0 and includes authentication failures in the report.

Shows token locations, never token values. Use `fcli auth token` to print a token.";

/// One (host, login) pair, checked.
struct Row {
    host: HostKey,
    url: String,
    login: String,
    active: bool,
    active_host: bool,
    store: CredentialStore,
    source: Option<TokenSource>,
    scopes: Vec<String>,
    /// `Ok(is_admin)` on success; the classified failure otherwise.
    outcome: std::result::Result<bool, Error>,
}

impl Row {
    fn to_json(&self) -> Value {
        json!({
            "host": self.host.as_str(),
            "url": self.url,
            "login": self.login,
            "active": self.active,
            "active_host": self.active_host,
            "credential_store": self.store.as_str(),
            "token_source": self.source.as_ref().map(common::source_label),
            "authenticated": self.outcome.is_ok(),
            "scopes": self.scopes,
            "is_admin": self.outcome.as_ref().ok().copied().unwrap_or(false),
            "error": self.outcome.as_ref().err().map(|e| render::headline(&e.kind)),
        })
    }
}

pub fn run(globals: &GlobalOpts, _args: &Args) -> Result<()> {
    // Field discovery first: asking what `--json` can select must not need a configured host.
    let Some(machine) = Triad::for_local_table(globals, FIELDS)? else { return Ok(()) };

    let mut setup = Setup::load()?;
    let term = Term::detect();

    let selected: Vec<HostKey> = match globals.host.as_deref() {
        Some(h) => vec![setup.hosts.resolve_host(Some(h), common::env())?],
        None => setup.hosts.keys(),
    };
    let mut out = support::writer(globals)?;

    if selected.is_empty() {
        // `[]` and exit 0 under a machine flag, for the same reason an empty list is not an error
        // anywhere else in fcli. Without one, the taxonomy's own remedy ("run fcli auth login")
        // is exactly the message this situation wants, so do not paraphrase it.
        if machine.is_explicit() {
            machine.pipeline().render(Value::Array(Vec::new()), &term, &mut out)?;
            out.flush()?;
            return Ok(());
        }
        return Err(Error::new(ErrorKind::NoHostConfigured));
    }

    // `rows` is filled by reference rather than returned: `runtime::block_on` is typed
    // `Future<Output = Result<()>>` and borrows nothing, so a plain `&mut` out-parameter is the
    // whole adaptation needed.
    let mut rows: Vec<Row> = Vec::new();
    crate::runtime::block_on(async {
        for key in &selected {
            let checked = check_host(&mut setup, key, globals.login.as_deref()).await;
            rows.extend(checked);
        }
        Ok(())
    })?;

    // Records which credential store answered, so the next invocation does not repeat a D-Bus
    // round trip that will not work. Failing the command over it would be absurd.
    if let Err(e) = setup.hosts.save_if_dirty() {
        common::warn(&e.kind);
    }

    if machine.is_explicit() {
        let payload = Value::Array(rows.iter().map(Row::to_json).collect());
        machine.pipeline().render(payload, &term, &mut out)?;
        out.flush()?;
        // Exit 0 even when a host failed: the user asked for a report and got a correct one.
        return Ok(());
    }

    write_human(&rows, &term, &mut *out)?;
    out.flush()?;

    // Report the first real failure through the taxonomy rather than inventing an exit code, so
    // the "what to do" block the renderer attaches to a `TokenRejected` survives. Note this is
    // exit 4 (authentication required), not `gh`'s 1 — see `docs/output.md`'s table, which fcli
    // scripts are written against.
    match rows.into_iter().find_map(|r| r.outcome.err()) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Every login on one host, checked against `GET /user`.
async fn check_host(setup: &mut Setup, key: &HostKey, only: Option<&str>) -> Vec<Row> {
    let Some(entry) = setup.hosts.get(key) else { return Vec::new() };
    let url = entry.url.clone();
    let active_login = entry.active_login.clone();
    let active_host = setup.hosts.active() == Some(key);
    let logins: Vec<(String, Vec<String>)> = entry
        .logins
        .iter()
        .filter(|l| only.is_none_or(|u| u == l.user))
        .map(|l| (l.user.clone(), l.scopes.iter().map(ToString::to_string).collect()))
        .collect();

    let mut rows = Vec::new();
    for (login, scopes) in logins {
        let mut creds = setup.credentials(Some(key));
        let store = creds.effective_store(&setup.hosts, key);
        let token = creds.token(&mut setup.hosts, key, &login);
        for kind in creds.take_warnings() {
            common::warn(&kind);
        }

        let (source, outcome) = match token {
            Err(e) => (None, Err(e)),
            Ok(None) => {
                (None, Err(Error::new(ErrorKind::NotAuthenticated { host: key.to_string() })))
            }
            Ok(Some(t)) => {
                let source = t.source().clone();
                let outcome = match HostEntry::from_input(&url)
                    .and_then(|e| common::client_for(&e, t.expose(), source.clone()))
                {
                    Ok(client) => common::whoami(&client).await.map(|u| u.is_admin),
                    Err(e) => Err(e),
                };
                (Some(source), outcome)
            }
        };

        rows.push(Row {
            host: key.clone(),
            url: url.clone(),
            login: login.clone(),
            active: active_login.as_deref() == Some(login.as_str()),
            active_host,
            store,
            source,
            scopes,
            outcome,
        });
    }

    if rows.is_empty() {
        rows.push(Row {
            host: key.clone(),
            url,
            login: only.unwrap_or_default().to_owned(),
            active: false,
            active_host,
            store: setup.hosts.cached_store(key).unwrap_or_default(),
            source: None,
            scopes: Vec::new(),
            outcome: Err(Error::new(ErrorKind::NotAuthenticated { host: key.to_string() })),
        });
    }
    rows
}

/// The human block, one stanza per host. Shaped like `gh auth status` so the layout transfers.
fn write_human(rows: &[Row], term: &Term, out: &mut dyn Write) -> std::io::Result<()> {
    use crate::output::color::paint;
    let ok_mark = paint(term, anstyle::AnsiColor::Green.on_default(), "✓");
    let bad_mark = paint(term, anstyle::AnsiColor::Red.on_default(), "X");

    let mut current: Option<&HostKey> = None;
    for row in rows {
        if current != Some(&row.host) {
            if current.is_some() {
                writeln!(out)?;
            }
            writeln!(out, "{} ({})", row.host, row.url)?;
            current = Some(&row.host);
        }
        match &row.outcome {
            Ok(is_admin) => {
                writeln!(
                    out,
                    "  {ok_mark} Logged in to {} account {}{}",
                    row.host,
                    row.login,
                    if *is_admin { " (site administrator)" } else { "" }
                )?;
            }
            Err(e) => {
                writeln!(
                    out,
                    "  {bad_mark} Failed to log in to {} account {}",
                    row.host, row.login
                )?;
                writeln!(out, "  - Reason: {}", render::headline(&e.kind))?;
            }
        }
        writeln!(out, "  - Active account: {}", row.active && row.active_host)?;
        writeln!(out, "  - Credential store: {}", common::store_label(row.store))?;
        match &row.source {
            Some(s) => writeln!(out, "  - Token kept in: {}", common::source_label(s))?,
            None => writeln!(out, "  - Token kept in: nowhere fcli can find one")?,
        }
        // Never the token. `fcli auth token` is the way to get the value, and this line says so
        // rather than leaving a reader to look for a flag that does not exist.
        writeln!(out, "  - Token: hidden; use `fcli auth token` to print it")?;
        if row.scopes.is_empty() {
            writeln!(
                out,
                "  - Token scopes: unknown (Forgejo does not report them; record them with \
                 `fcli auth login --scopes`)"
            )?;
        } else {
            writeln!(out, "  - Token scopes: {}", row.scopes.join(", "))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ok: bool) -> Row {
        Row {
            host: HostKey::parse("git.example.org").unwrap(),
            url: "https://git.example.org".to_owned(),
            login: "perf3ct".to_owned(),
            active: true,
            active_host: true,
            store: CredentialStore::Keyring,
            source: Some(TokenSource::Keyring { entry: "fcli:perf3ct@git.example.org".to_owned() }),
            scopes: vec!["read:repository".to_owned(), "write:issue".to_owned()],
            outcome: if ok {
                Ok(false)
            } else {
                Err(Error::new(ErrorKind::TokenRejected {
                    host: "git.example.org".to_owned(),
                    login: Some("perf3ct".to_owned()),
                    settings_url: "https://git.example.org/user/settings/applications".to_owned(),
                }))
            },
        }
    }

    fn rendered(rows: &[Row]) -> String {
        let mut buf = Vec::new();
        write_human(rows, &Term::tty(100), &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    /// Bug this prevents: `--json` and the discovery table drifting apart, which shows up as
    /// `--json <name>` rejecting a field the payload actually has (or projecting away one it
    /// does not). It also pins the security property structurally: the emitted object's keys are
    /// exactly the declared ones, and none of them is a token value.
    #[test]
    fn the_json_row_emits_exactly_the_declared_fields() {
        let v = row(true).to_json();
        let keys: Vec<&str> =
            v.as_object().expect("an object").keys().map(String::as_str).collect();
        let declared: Vec<&str> = FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(keys, declared);
        for forbidden in ["token", "password", "secret"] {
            assert!(!declared.contains(&forbidden), "{forbidden} must not be selectable");
        }
    }

    /// The whole reason `--show-token` does not exist: this output is what people paste into
    /// issues. `Row` carries no token at all, so the guarantee is structural; what the renderer
    /// still owes the reader is a line saying where the value can be had instead.
    #[test]
    fn status_says_where_a_token_is_and_never_what_it_is() {
        for ok in [true, false] {
            let out = rendered(&[row(ok)]);
            assert!(out.contains("Token: hidden"), "{out}");
            assert!(out.contains("fcli auth token"), "{out}");
            assert!(out.contains("Token kept in: keyring entry"), "{out}");
        }
    }

    /// Bug this prevents: reporting a failure as though nothing were wrong. The reason line has
    /// to come from the taxonomy so the server's own message survives.
    #[test]
    fn a_failing_host_says_why() {
        let out = rendered(&[row(false)]);
        assert!(out.contains("Failed to log in"), "{out}");
        assert!(out.contains("refused your token (HTTP 401)"), "{out}");
    }

    #[test]
    fn a_working_host_reports_the_store_and_the_scopes() {
        let out = rendered(&[row(true)]);
        assert!(out.contains("Logged in to git.example.org account perf3ct"), "{out}");
        assert!(out.contains("operating system keyring"), "{out}");
        assert!(out.contains("read:repository, write:issue"), "{out}");
    }

    /// Bug this prevents: an unknown scope list rendering as an empty field, which reads as "this
    /// token has no scopes" — a very different and alarming claim.
    #[test]
    fn unknown_scopes_say_unknown_rather_than_nothing() {
        let mut r = row(true);
        r.scopes.clear();
        let out = rendered(&[r]);
        assert!(out.contains("Token scopes: unknown"), "{out}");
    }
}
