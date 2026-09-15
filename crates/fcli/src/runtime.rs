//! Everything a command needs, assembled once.
//!
//! [`Runtime`] is the seam between the global flags and the runtime crate: it resolves the
//! host, finds a credential, builds a [`Client`], and works out where output is going. The
//! repository is *not* resolved here — see [`Runtime::repo`] — because resolving it shells out
//! to `git` and most commands never need it.
//!
//! # Warnings are warnings
//!
//! [`Hosts::take_warnings`] and `Credentials::take_warnings` carry problems that are real but
//! survivable: a keyring with no D-Bus session behind it, a `hosts.toml` entry this build does
//! not understand, a mistyped `FCLI_CREDENTIAL_STORE`. Those go to stderr through
//! [`crate::exit::warn`] and the command continues. A missing keyring is the *normal* state
//! over SSH, in containers, and in CI — exactly where a CLI runs — so making it fatal would
//! break the tool in its most common environment.

use std::cell::OnceCell;
use std::time::Duration;

use forgejo_core::config::secrets::{CredStore, EnvStore};
use forgejo_core::config::{ColorPref, Config, Env, HostKey, Hosts, SystemEnv};
use forgejo_core::context::{GitCli, GitCtx, RepoContext, ResolveOptions, resolve_repo};
use forgejo_core::error::{Error, ErrorKind, Result, TokenSource, render};
use forgejo_core::http::{Auth, Client, Credentials as HttpCredentials, RetryPolicy, WaitNotice};

use crate::exit;
use crate::global::GlobalOpts;
use crate::output::Term;

/// The process environment, as a `'static` so [`Credentials`] and [`resolve_repo`] can both
/// borrow it for the life of the program.
///
/// [`forgejo_core::config::Env`] is a trait rather than direct `std::env::var` calls because
/// `std::env::set_var` is `unsafe` in Rust 2024, which makes environment-dependent tests in
/// that crate impossible to write hermetically. The binary is the one place that legitimately
/// wants the real thing.
static SYS_ENV: SystemEnv = SystemEnv;

/// Assembled state for one invocation.
pub struct Runtime {
    config: Config,
    hosts: Hosts,
    host: HostKey,
    login: Option<String>,
    client: Client,
    term: Term,
    git: GitCli,
    debug: bool,
    /// Resolved on first use. `OnceCell` rather than a field because ~90% of commands never
    /// ask, and asking costs several `git` subprocesses.
    repo: OnceCell<RepoContext>,
}

impl Runtime {
    /// Build the runtime, printing any non-fatal warnings to stderr as it goes.
    pub fn new(globals: &GlobalOpts) -> Result<Self> {
        let env: &'static dyn Env = &SYS_ENV;
        let color = exit::color();
        let mut warnings: Vec<ErrorKind> = Vec::new();

        if globals.insecure_skip_tls_verify {
            // Honest refusal beats silently verifying anyway. `reqwest` is not a dependency of
            // this crate and `ReqwestTransport` exposes no way to relax verification, so the
            // flag cannot currently be honoured — and a security flag that is accepted and
            // ignored is worse than one that is rejected.
            // One line, because the renderer restates a `Usage` message as both the headline and
            // the `problem:` fact; a paragraph here would be printed twice.
            return Err(Error::new(ErrorKind::Usage(
                "--insecure-skip-tls-verify is not supported by this build; add the instance's \
                 CA certificate to your operating system trust store instead, which fcli reads"
                    .to_owned(),
            )));
        }

        let config = Config::load(env)?;
        let mut hosts = Hosts::load_at(&config.hosts_path())?;
        warnings.extend(hosts.take_warnings());

        // Adopt a host named on the command line or in the environment that is not in
        // `hosts.toml`, provided a token is also in the environment. That is the CI pattern —
        // `FORGEJO_HOST=… FORGEJO_TOKEN=… fcli api user`, with no config file at all — and
        // without this it fails with "not one of your configured hosts". The adopted entry is
        // never persisted; see `Hosts::adopt_env_host`.
        hosts.adopt_env_host(globals.host.as_deref(), env)?;

        let host = hosts.resolve_host(globals.host.as_deref(), env)?;

        // An explicitly named login that does not exist is a usage error. An *absent* default
        // is not: `fcli api version` needs no credential, and a 401 from the server carries a
        // far better message than anything we could say here.
        let login = match globals.login.as_deref() {
            Some(u) => Some(hosts.resolve_login(&host, Some(u))?),
            None => hosts.resolve_login(&host, None).ok(),
        };

        let mut creds = forgejo_core::config::Credentials::new(env)
            .with_preference(config.credential_store(Some(host.as_str())));
        let token = match &login {
            Some(l) => creds.token(&mut hosts, &host, l)?,
            // With no login recorded there is nothing in the keyring or in `hosts.toml` to
            // find, and probing the keyring anyway would emit a "no keyring" warning on a
            // command that needs no credential. An environment token is not login-scoped, so
            // ask only for that.
            None => EnvStore::new(env).get(&host, "", &hosts)?,
        };
        warnings.extend(creds.take_warnings());

        // The credential search caches which store answered, so that the next invocation does
        // not pay for a D-Bus round trip that will not work. Failing to record that is not
        // worth failing the command over.
        if let Err(e) = hosts.save_if_dirty() {
            warnings.push(*e.kind);
        }

        let (auth, token_source) = match &token {
            Some(t) => (Auth::token(t.expose()), Some(t.source().clone())),
            None => (Auth::None, None),
        };
        let mut http_creds = HttpCredentials::new(auth);
        if let Some(user) = &globals.sudo {
            http_creds = http_creds.with_sudo(user);
        }
        if let Some(code) = &globals.otp {
            http_creds = http_creds.with_otp(code);
        }

        let entry = hosts.get(&host).ok_or_else(|| Error::new(ErrorKind::NoHostConfigured))?;
        let mut builder = Client::builder(&entry.url, http_creds)
            .user_agent(user_agent())
            .retry(retry_policy(globals))
            // A wait nobody announced reads as a hang, and the user reaches for Ctrl-C in the
            // middle of a retry that was about to succeed.
            .on_wait(|n: &WaitNotice| eprintln!("{}", wait_line(n)));
        if let Some(source) = token_source.clone() {
            // So a 401 can say *which* token was rejected — the keyring entry, `hosts.toml`,
            // or `$FORGEJO_TOKEN` — which is the whole difference between an actionable auth
            // error and a baffling one.
            builder = builder.token_source(source);
        }
        if let Some(l) = &login {
            builder = builder.login(l.clone());
        }
        let client = builder.build()?;

        let term = term_for(globals, &config, &host);
        let rt = Self {
            config,
            hosts,
            host,
            login,
            client,
            term,
            git: GitCli::default(),
            debug: globals.debug,
            repo: OnceCell::new(),
        };

        for kind in &warnings {
            exit::warn(kind, color);
        }
        if rt.debug {
            rt.trace(&format!(
                "host {} (login {}), credential {}",
                rt.host,
                rt.login.as_deref().unwrap_or("<none>"),
                describe_source(token_source.as_ref()),
            ));
        }
        Ok(rt)
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn host(&self) -> &HostKey {
        &self.host
    }

    pub fn term(&self) -> &Term {
        &self.term
    }

    pub fn git(&self) -> &dyn GitCtx {
        &self.git
    }

    /// The resolved repository, worked out on first use.
    ///
    /// Re-attempted after a failure rather than cached as one: the cost is a handful of `git`
    /// subprocesses, and a command that fails resolution is about to exit anyway.
    pub fn repo(&self, globals: &GlobalOpts) -> Result<&RepoContext> {
        if let Some(ctx) = self.repo.get() {
            return Ok(ctx);
        }
        let opts = ResolveOptions {
            repo: globals.repo.as_ref(),
            host: globals.host.as_deref(),
            login: globals.login.as_deref(),
        };
        let ctx = resolve_repo(&opts, &self.hosts, &self.git, &SYS_ENV)?;
        if self.debug {
            self.trace(&format!("repo {} via {}", ctx.slug, ctx.source));
        }
        Ok(self.repo.get_or_init(|| ctx))
    }

    /// The current branch, for `{branch}` substitution.
    pub fn branch(&self) -> Result<String> {
        self.git.current_branch()?.ok_or_else(|| {
            Error::new(ErrorKind::Usage(
                "{branch} needs a checked-out branch, and HEAD is detached (or this is not a \
                 git repository); name the branch in the path instead"
                    .to_owned(),
            ))
        })
    }

    /// One `--debug` line. Deliberately never prints headers or request bodies: the
    /// `Authorization`, `Sudo`, and `X-FORGEJO-OTP` headers are assembled inside the client,
    /// and a trace that cannot see them cannot leak them. URLs go through
    /// [`forgejo_core::http::redact::url`] so a `?token=` a user pasted into an endpoint is
    /// masked too.
    pub fn trace(&self, message: &str) {
        if self.debug {
            eprintln!("debug: {message}");
        }
    }

    pub fn is_debug(&self) -> bool {
        self.debug
    }
}

/// `fcli/0.1.0 (forgejo-api 16.0.4)`, so an instance's logs can attribute the traffic.
pub fn user_agent() -> String {
    format!("fcli/{} (forgejo-api {})", env!("CARGO_PKG_VERSION"), crate::spec_version())
}

/// `--no-retry` and `--max-retries` on top of the runtime's defaults.
///
/// `RetryPolicy::max` counts total *attempts*, while `--max-retries` counts the extra ones —
/// the classic off-by-one — so the conversion is explicit here rather than at the flag.
fn retry_policy(globals: &GlobalOpts) -> RetryPolicy {
    if globals.no_retry {
        return RetryPolicy::none();
    }
    match globals.max_retries {
        Some(n) => RetryPolicy { max: n.saturating_add(1), ..RetryPolicy::default() },
        None => RetryPolicy::default(),
    }
}

fn wait_line(n: &WaitNotice) -> String {
    let why = match n.reason {
        forgejo_core::http::retry::RetryReason::RateLimited => "rate limited".to_owned(),
        forgejo_core::http::retry::RetryReason::ServerError(s) => format!("HTTP {s}"),
        forgejo_core::http::retry::RetryReason::Transport => "connection failed".to_owned(),
    };
    format!(
        "{} is {why}; waiting {:.1}s before attempt {} of {}",
        n.host,
        n.after.as_secs_f64(),
        n.attempt + 1,
        n.of
    )
}

fn describe_source(source: Option<&TokenSource>) -> String {
    match source {
        None => "none".to_owned(),
        Some(TokenSource::Keyring { entry }) => format!("keyring entry {entry}"),
        Some(TokenSource::File { path }) => format!("{}", path.display()),
        Some(TokenSource::Env { var }) => format!("${var}"),
        Some(TokenSource::Flag) => "--token".to_owned(),
    }
}

/// Terminal detection, then the `--color` flag or the stored preference on top.
fn term_for(globals: &GlobalOpts, config: &Config, host: &HostKey) -> Term {
    let mut term = Term::detect();
    let pref = globals.color.unwrap_or_else(|| config.color(Some(host.as_str())));
    match pref {
        ColorPref::Always => term.color = true,
        ColorPref::Never => term.color = false,
        // `Term::detect` already applied `NO_COLOR`, `CLICOLOR_FORCE`, and TTY-ness.
        ColorPref::Auto => {}
    }
    term
}

/// Run one async command body.
///
/// A **current-thread** runtime, deliberately: a CLI issues a handful of requests and never
/// needs work-stealing, and the multi-thread flavour spends milliseconds spawning worker
/// threads that then sit idle — measurable against a 25 ms startup budget.
pub fn block_on<F: std::future::Future<Output = Result<()>>>(f: F) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| {
        Error::new(ErrorKind::Usage(format!("could not start the async runtime: {e}")))
    })?;
    let out = rt.block_on(f);
    // Do not let a lingering connection pool keep the process alive after the command is done.
    rt.shutdown_timeout(Duration::from_millis(50));
    out
}

/// Colour policy for diagnostics, re-exported so command modules do not each reach for it.
pub fn diagnostic_color() -> render::Color {
    exit::color()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_retries_counts_retries_not_attempts() {
        // Bug this prevents: `--max-retries 3` sending three requests instead of four, or
        // `--no-retry` still retrying once.
        let g = GlobalOpts { max_retries: Some(3), ..GlobalOpts::default() };
        assert_eq!(retry_policy(&g).max, 4);
        let g = GlobalOpts { no_retry: true, max_retries: Some(3), ..GlobalOpts::default() };
        assert_eq!(retry_policy(&g).max, 1);
        assert_eq!(retry_policy(&GlobalOpts::default()).max, RetryPolicy::default().max);
    }

    #[test]
    fn a_wait_notice_names_the_host_the_reason_and_the_next_attempt() {
        let n = WaitNotice {
            host: "git.example.org".to_owned(),
            attempt: 1,
            of: 3,
            after: Duration::from_millis(2500),
            reason: forgejo_core::http::retry::RetryReason::RateLimited,
        };
        let line = wait_line(&n);
        assert!(line.contains("git.example.org"), "{line}");
        assert!(line.contains("rate limited"), "{line}");
        assert!(line.contains("attempt 2 of 3"), "{line}");
    }

    /// The token itself must never appear in a source description; only where it came from.
    #[test]
    fn a_token_source_names_the_place_not_the_secret() {
        assert_eq!(
            describe_source(Some(&TokenSource::Env { var: "FORGEJO_TOKEN".to_owned() })),
            "$FORGEJO_TOKEN"
        );
        assert_eq!(describe_source(None), "none");
    }
}
