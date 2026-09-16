//! `fcli auth git-credential` — git's credential-helper protocol.
//!
//! `git` runs this; a human never does. The protocol is documented in `gitcredentials(7)`: a
//! verb as `argv[1]`, then `key=value` lines on stdin terminated by a blank line or EOF, and for
//! `get` a `key=value` reply on stdout.
//!
//! # Every failure is silent success
//!
//! Nothing here returns an error. That is not laziness — it is the protocol:
//!
//! * On `get`, a helper that cannot answer is expected to say **nothing** and exit 0, so git moves
//!   on to the next helper or prompts. Exiting non-zero makes `git push` fail outright, so a host
//!   `fcli` happens not to know about would break pushes that used to work.
//! * `store` is ignored, because git would otherwise hand us a password a *user* typed and we
//!   would silently overwrite the token `auth login` verified.
//! * `erase` is ignored, and this one matters most: git erases credentials after a 401. Honouring
//!   it would mean one expired-token push silently logged the user out of `fcli` itself, and the
//!   next `fcli pr list` would report "you are not logged in" with no connection to the push.

use std::io::{BufRead, Write};

use clap::Args as ClapArgs;
use forgejo_core::Result;
use forgejo_core::config::HostKey;

use super::common::Setup;
use crate::global::GlobalOpts;

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// `get`, `store` or `erase`, as git passes it
    #[arg(value_name = "OPERATION")]
    pub operation: String,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    run_with(globals, args, &mut std::io::stdin().lock(), &mut std::io::stdout().lock())
}

/// The testable core. Streams are parameters so a test never touches the process's real stdin —
/// which under `cargo test` may be a terminal, and a helper that blocks on it would hang the
/// suite.
fn run_with(
    globals: &GlobalOpts,
    args: &Args,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<()> {
    // Read the request even for `store` and `erase`: git writes the whole block and a helper that
    // exits without draining it makes git see EPIPE and complain.
    let request = read_request(input)?;
    if args.operation != "get" {
        return Ok(());
    }
    let Some((login, token)) = lookup(globals, &request) else { return Ok(()) };

    // Echoing protocol and host back is not required, but it lets git check that the helper
    // answered the question it was asked rather than a cached different one.
    if let Some(p) = request.get("protocol") {
        writeln!(out, "protocol={p}")?;
    }
    if let Some(h) = request.get("host") {
        writeln!(out, "host={h}")?;
    }
    writeln!(out, "username={login}")?;
    writeln!(out, "password={}", token.expose())?;
    out.flush()?;
    Ok(())
}

/// The `key=value` lines git wrote, up to the blank line or EOF.
///
/// A `Vec` rather than a map because git may legally repeat a key (`wwwauth[]` in newer versions),
/// and dropping duplicates would be a silent change of meaning. `get` looks keys up by first
/// occurrence, which is what the protocol specifies.
type Request = Vec<(String, String)>;

trait Lookup {
    fn get(&self, key: &str) -> Option<&str>;
}

impl Lookup for Request {
    fn get(&self, key: &str) -> Option<&str> {
        self.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

fn read_request(input: &mut dyn BufRead) -> Result<Request> {
    let mut out = Request::new();
    for line in input.lines() {
        let line = line?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.push((k.to_owned(), v.to_owned()));
        }
    }
    Ok(out)
}

/// The credential for the host git is asking about, or `None`.
///
/// Longest-prefix matching through [`forgejo_core::config::Hosts::match_prefix`] rather than a
/// bare hostname lookup, because two Forgejo instances can sit behind one authority at different
/// path prefixes — the case `setup-git` enables `credential.useHttpPath` for. Matching on the
/// authority alone would hand instance A's token to instance B.
fn lookup(
    globals: &GlobalOpts,
    request: &Request,
) -> Option<(String, forgejo_core::config::Token)> {
    let host = request.get("host")?;
    let path = request.get("path").unwrap_or("");
    let mut setup = Setup::load().ok()?;

    let key = match setup.hosts.match_prefix(host, path) {
        Some((entry, _)) => entry.name.clone(),
        None => {
            let parsed = HostKey::parse(host).ok()?;
            setup.hosts.get(&parsed).map(|e| e.name.clone())?
        }
    };

    let login = setup.hosts.resolve_login(&key, globals.login.as_deref()).ok()?;
    let mut creds = setup.credentials(Some(&key));
    let token = creds.token(&mut setup.hosts, &key, &login).ok()??;
    // Deliberately not saving `hosts.toml` here: git can invoke a helper many times during one
    // fetch, and rewriting the file under a `git` process's feet buys nothing.
    Some((login, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_read_up_to_the_blank_line() {
        let mut input = "protocol=https\nhost=git.example.org\n\nignored=yes\n".as_bytes();
        let req = read_request(&mut input).unwrap();
        assert_eq!(req.get("protocol"), Some("https"));
        assert_eq!(req.get("host"), Some("git.example.org"));
        assert_eq!(req.get("ignored"), None);
    }

    /// Bug this prevents: splitting on the last `=` (or on every `=`), which mangles a value that
    /// legitimately contains one — git sends `wwwauth[]=Basic realm="x=y"` on a 401.
    #[test]
    fn a_value_may_contain_an_equals_sign() {
        let mut input = "wwwauth[]=Basic realm=\"a=b\"\n".as_bytes();
        let req = read_request(&mut input).unwrap();
        assert_eq!(req.get("wwwauth[]"), Some("Basic realm=\"a=b\""));
    }

    /// Bug this prevents: honouring `erase`. git erases credentials after a 401, so one expired
    /// token during `git push` would silently log the user out of fcli, and the next `fcli`
    /// command would report "you are not logged in" with nothing linking it to the push.
    #[test]
    fn store_and_erase_do_nothing_at_all() {
        for op in ["store", "erase"] {
            let args = Args { operation: op.to_owned() };
            let mut input =
                "protocol=https\nhost=git.example.org\npassword=typed-by-a-human\n".as_bytes();
            let mut out = Vec::new();
            run_with(&GlobalOpts::default(), &args, &mut input, &mut out).unwrap();
            assert!(out.is_empty(), "{op} answered with {:?}", String::from_utf8_lossy(&out));
        }
    }
}
