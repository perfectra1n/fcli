//! Percent-encoding for path parameters and query values.
//!
//! Encoding is **per-parameter**, not per-path, and that distinction is the whole reason this
//! module exists. Most Forgejo path parameters are single segments (`{owner}`, `{repo}`,
//! `{index}`) where a `/` in the value must become `%2F` or it silently changes which route
//! matches. But a handful of parameters (`filepath`, `treePath`, `ref`, `path`, `filename`)
//! carry a *whole path*, and encoding their `/` turns `GET .../contents/src/main.rs` into a
//! request for a file literally named `src/main.rs` in the root — which 404s for every
//! nested file in every repository.
//!
//! Both directions of the mistake are real, so both are unit-tested below: `src/main.rs` must
//! survive [`path_like`] intact, and an owner literally named `a/b` must come out of [`seg`]
//! as `a%2Fb`.
//!
//! We hand-roll the encoder rather than take a `percent-encoding` dependency: the rule is
//! twelve lines, and the set of characters we preserve is a decision worth having in front of
//! us rather than behind a crate feature flag.

use std::borrow::Cow;

/// RFC 3986 *unreserved*: `ALPHA / DIGIT / "-" / "." / "_" / "~"`.
///
/// We deliberately preserve **only** this set and encode every other byte, including the
/// sub-delimiters (`!$&'()*+,;=`) and `:@` that a path segment is technically allowed to
/// contain. Encoding them is always safe — the server percent-decodes before matching route
/// parameters — whereas *not* encoding them depends on how each reverse proxy in front of the
/// instance normalises a URL. Correct-and-ugly beats pretty-and-conditional.
const fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Shared engine. `extra` names bytes to pass through in addition to the unreserved set.
fn encode<'a>(s: &'a str, extra: &[u8]) -> Cow<'a, str> {
    let keep = |b: u8| is_unreserved(b) || extra.contains(&b);
    if s.bytes().all(keep) {
        // The overwhelmingly common case: nothing to do, and no allocation.
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        if keep(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    Cow::Owned(out)
}

/// Encode a value that occupies exactly one path segment: `{owner}`, `{repo}`, `{index}`, …
///
/// `/` is encoded. If a value containing `/` reached the URL unencoded it would add a path
/// segment and match a *different* route — at best a 404, at worst a request against the
/// wrong object.
pub fn seg(s: &str) -> Cow<'_, str> {
    encode(s, b"")
}

/// Encode a value that is itself a path: `filepath`, `treePath`, `ref`, `path`, `filename`.
///
/// `/` is preserved because it is structural to the value. Everything else is encoded, so a
/// filename with a space or a `#` still survives.
pub fn path_like(s: &str) -> Cow<'_, str> {
    encode(s, b"/")
}

/// Encode a query-string key or value.
///
/// Space becomes `%20`, never `+`. `+` only means space in `application/x-www-form-urlencoded`,
/// and a Go server reading `r.URL.Query()` does apply that rule — which means a literal `+`
/// in a search term (`c++`) would silently become a space if we did not encode it.
pub fn query(s: &str) -> Cow<'_, str> {
    encode(s, b"")
}

/// Encode a `application/x-www-form-urlencoded` body value, where `+` for space is the
/// convention. Used only for [`super::Body::Form`].
pub fn form(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b' ' => out.push('+'),
            b if is_unreserved(b) => out.push(b as char),
            b => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// Join an ordered query list into a query string, without the leading `?`.
///
/// The list is ordered, not a map, because repeated keys are legal in this API (`labels=bug&
/// labels=ci`). Collapsing them into a map would silently drop all but one.
pub fn query_string<'a, I, K, V>(pairs: I) -> String
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str> + 'a,
    V: AsRef<str> + 'a,
{
    let mut out = String::new();
    for (k, v) in pairs {
        if !out.is_empty() {
            out.push('&');
        }
        out.push_str(&query(k.as_ref()));
        out.push('=');
        out.push_str(&query(v.as_ref()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `get-contents` 404: encoding `/` in a path-like parameter asks for a file named
    /// `src/main.rs` in the repository root instead of `main.rs` inside `src/`.
    #[test]
    fn path_like_preserves_slashes_so_nested_files_resolve() {
        assert_eq!(path_like("src/main.rs"), "src/main.rs");
        assert_eq!(path_like("a/b/c/d.txt"), "a/b/c/d.txt");
        // Still encodes everything else, so a space in a filename survives.
        assert_eq!(path_like("docs/my notes.md"), "docs/my%20notes.md");
    }

    /// The mirror-image bug: a single-segment parameter whose value contains `/` would add a
    /// path segment and match a different route entirely.
    #[test]
    fn seg_encodes_slashes_so_a_weird_owner_cannot_change_the_route() {
        assert_eq!(seg("a/b"), "a%2Fb");
        assert_eq!(seg("perf3ct"), "perf3ct");
        assert_eq!(seg("my.repo-1_x~"), "my.repo-1_x~");
    }

    #[test]
    fn seg_encodes_reserved_and_non_ascii() {
        assert_eq!(seg("a b"), "a%20b");
        assert_eq!(seg("a#b?c"), "a%23b%3Fc");
        assert_eq!(seg("a:b@c"), "a%3Ab%40c");
        assert_eq!(seg("é"), "%C3%A9");
        assert_eq!(seg("100%"), "100%25");
    }

    /// A `+` in a query value must arrive as a literal `+`, or searching for `c++` silently
    /// searches for `c  `.
    #[test]
    fn query_encodes_plus_rather_than_treating_it_as_space() {
        assert_eq!(query("c++"), "c%2B%2B");
        assert_eq!(query("a b"), "a%20b");
    }

    #[test]
    fn form_uses_plus_for_space() {
        assert_eq!(form("hello world"), "hello+world");
        assert_eq!(form("c++"), "c%2B%2B");
    }

    /// Repeated keys must both survive: `labels` is legal more than once.
    #[test]
    fn query_string_keeps_repeated_keys_in_order() {
        let q = query_string([("labels", "bug"), ("labels", "ci"), ("state", "open")]);
        assert_eq!(q, "labels=bug&labels=ci&state=open");
    }

    #[test]
    fn empty_input_is_borrowed_and_empty() {
        assert!(matches!(seg(""), Cow::Borrowed("")));
        assert!(matches!(path_like("plain"), Cow::Borrowed("plain")));
    }
}
