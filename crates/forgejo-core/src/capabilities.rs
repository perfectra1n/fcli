//! What this instance can do — discovered, never inferred from a version number.
//!
//! # Feature-detect; never parse version strings
//!
//! This is the rule the module exists to enforce. `tea` gated behaviour on the instance's
//! version string, and it went wrong in every way that approach can: a Forgejo fork reporting a
//! Gitea version, a Gitea version numbered past the Forgejo range, `+dev` suffixes on
//! self-built instances, and reverse proxies serving a version endpoint from a different
//! deployment. The result was a `--no-version-check` flag — a switch that turns off the safety
//! feature, which every user eventually has to set, which means the feature was never load-bearing
//! in the first place.
//!
//! So: [`Capabilities`] carries *numbers the instance reported about itself* and nothing derived
//! from a version. [`Instance::version`] exists and is displayed in error messages ("this
//! instance is forgejo 9.0.1, and this endpoint was added later") because naming the version is
//! useful **to a human**. There is deliberately no version comparison API here for code to
//! branch on. If you find yourself wanting one, the right move is to attempt the request and
//! classify the `404` as [`crate::ErrorKind::RouteNotFound`].
//!
//! # Both endpoints are optional
//!
//! `GET /settings/api` requires no scope on a current Forgejo but did not always exist, can be
//! disabled, and returns `403` on instances that require authentication for everything.
//! `GET /nodeinfo` is unauthenticated but is served only when the instance enables federation.
//! Either or both being absent is normal, and must degrade to [`Capabilities::conservative`]
//! rather than to an error — a CLI that cannot list issues because an optional metadata endpoint
//! is missing is a broken CLI.

use std::time::Duration;

use serde::Deserialize;

use crate::http::{Client, Request};

/// How long a probe is trusted. Instance settings change when an admin edits `app.ini` and
/// restarts, which is not something a single CLI invocation — or a day of them — needs to
/// notice.
pub const TTL: Duration = Duration::from_secs(60 * 60 * 24);

/// Conservative defaults, matching Forgejo's own shipped values.
///
/// These are the numbers to fall back to when `/settings/api` is unavailable. They are the
/// upstream defaults precisely so that a fallback behaves like an unconfigured instance rather
/// than like an optimistic guess: guessing *high* on `max_response_items` would send a `limit`
/// the server clamps, which is the ambiguity [`crate::http::paginate`] exists to survive.
pub const DEFAULT_MAX_RESPONSE_ITEMS: u32 = 50;
pub const DEFAULT_PAGING_NUM: u32 = 30;
pub const DEFAULT_GIT_TREES_PER_PAGE: u32 = 1000;
pub const DEFAULT_MAX_BLOB_SIZE: u64 = 10_485_760;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// The hard ceiling the instance applies to any `limit` query parameter. Requests above it
    /// are clamped **silently** — no warning, no header, no error.
    pub max_response_items: u32,
    /// The page size used when `limit` is unset.
    pub default_paging_num: u32,
    pub default_git_trees_per_page: u32,
    pub default_max_blob_size: u64,
    /// Whether `/settings/api` actually answered.
    ///
    /// Load-bearing, not merely informational: pagination sends a `limit` only when this is
    /// true. Fields above are upstream defaults when it is false, and defaults are a guess.
    pub settings_known: bool,
    pub instance: Option<Instance>,
}

/// Instance identity from `/nodeinfo`. **Display only** — see the module comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// `forgejo`, `gitea`, or whatever a fork calls itself.
    pub software: String,
    pub version: String,
    pub open_registrations: Option<bool>,
    pub users_total: Option<u64>,
}

impl Instance {
    /// `forgejo 16.0.4`, for an error message. Never parsed back.
    pub fn label(&self) -> String {
        if self.version.is_empty() {
            self.software.clone()
        } else {
            format!("{} {}", self.software, self.version)
        }
    }
}

impl Default for Capabilities {
    fn default() -> Self {
        Self::conservative()
    }
}

impl Capabilities {
    /// What to assume when we could not ask.
    pub fn conservative() -> Self {
        Self {
            max_response_items: DEFAULT_MAX_RESPONSE_ITEMS,
            default_paging_num: DEFAULT_PAGING_NUM,
            default_git_trees_per_page: DEFAULT_GIT_TREES_PER_PAGE,
            default_max_blob_size: DEFAULT_MAX_BLOB_SIZE,
            settings_known: false,
            instance: None,
        }
    }

    /// The instance label for a diagnostic, or `None` when `/nodeinfo` did not answer.
    pub fn instance_label(&self) -> Option<String> {
        self.instance.as_ref().map(Instance::label)
    }
}

// ------------------------------------------------------------------------------- wire shapes

/// `GET /settings/api`. Every field is `#[serde(default)]` so a newer or older instance that
/// omits one still yields usable capabilities instead of a decode error.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct GeneralApiSettings {
    max_response_items: u32,
    default_paging_num: u32,
    default_git_trees_per_page: u32,
    default_max_blob_size: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct NodeInfo {
    software: NodeInfoSoftware,
    /// NodeInfo is camelCase; this is the one place in the API that is.
    #[serde(rename = "openRegistrations")]
    open_registrations: Option<bool>,
    usage: NodeInfoUsage,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct NodeInfoSoftware {
    name: String,
    version: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct NodeInfoUsage {
    users: NodeInfoUsers,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct NodeInfoUsers {
    total: Option<u64>,
}

/// Probe both endpoints, tolerating either being absent.
///
/// Errors are swallowed **by design** and this is the one place in the crate where that is
/// correct: the caller asked "what can this instance do", and "I could not find out" is a
/// complete, actionable answer expressed as [`Capabilities::conservative`]. Propagating the
/// error would turn an optional metadata endpoint into a hard dependency for every command.
pub(crate) async fn probe(client: &Client) -> Capabilities {
    let mut caps = Capabilities::conservative();

    if let Ok(s) = client.json::<GeneralApiSettings>(Request::get("/settings/api")).await {
        // A zero means the instance sent the field with no value, or sent a shape we mis-read.
        // Keep the upstream default rather than adopting a zero, which would make
        // `effective_limit` ask for `limit=0` and return nothing at all.
        if s.max_response_items > 0 {
            caps.max_response_items = s.max_response_items;
        }
        if s.default_paging_num > 0 {
            caps.default_paging_num = s.default_paging_num;
        }
        if s.default_git_trees_per_page > 0 {
            caps.default_git_trees_per_page = s.default_git_trees_per_page;
        }
        if s.default_max_blob_size > 0 {
            caps.default_max_blob_size = s.default_max_blob_size;
        }
        caps.settings_known = true;
    }

    if let Ok(n) = client.json::<NodeInfo>(Request::get("/nodeinfo")).await
        && !n.software.name.is_empty()
    {
        caps.instance = Some(Instance {
            software: n.software.name,
            version: n.software.version,
            open_registrations: n.open_registrations,
            users_total: n.usage.users.total,
        });
    }

    caps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::auth::Auth;
    use crate::http::transport::{Canned, FakeTransport};
    use http::Method;
    use std::sync::Arc;

    const SETTINGS: &str = r#"{
        "max_response_items": 50,
        "default_paging_num": 30,
        "default_git_trees_per_page": 1000,
        "default_max_blob_size": 10485760
    }"#;

    const NODEINFO: &str = r#"{
        "version": "2.1",
        "software": { "name": "forgejo", "version": "16.0.4" },
        "openRegistrations": false,
        "usage": { "users": { "total": 3 } }
    }"#;

    fn client(t: FakeTransport) -> Client {
        Client::builder("https://git.example.org", Auth::None)
            .transport(Arc::new(t))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn both_endpoints_answering_yields_full_capabilities() {
        let c = client(
            FakeTransport::new()
                .on(Method::GET, "/api/v1/settings/api", Canned::json(200, SETTINGS))
                .on(Method::GET, "/api/v1/nodeinfo", Canned::json(200, NODEINFO)),
        );
        let caps = c.capabilities().await.unwrap();
        assert!(caps.settings_known);
        assert_eq!(caps.max_response_items, 50);
        assert_eq!(caps.instance_label().as_deref(), Some("forgejo 16.0.4"));
        assert_eq!(caps.instance.as_ref().unwrap().users_total, Some(3));
    }

    /// An older instance, an instance with the endpoint disabled, or one behind a proxy that
    /// hides it. This must not break a single command.
    #[tokio::test]
    async fn both_endpoints_absent_falls_back_to_conservative_defaults() {
        let c = client(
            FakeTransport::new()
                .on(Method::GET, "/api/v1/settings/api", Canned::html(404, "<html>404</html>"))
                .on(Method::GET, "/api/v1/nodeinfo", Canned::html(404, "<html>404</html>")),
        );
        let caps = c.capabilities().await.unwrap();
        assert!(!caps.settings_known, "we must know that we do not know");
        assert_eq!(caps.max_response_items, DEFAULT_MAX_RESPONSE_ITEMS);
        assert_eq!(caps.instance, None);
    }

    /// An instance that requires authentication for everything answers `403` here. Same
    /// treatment: fall back, do not fail.
    #[tokio::test]
    async fn an_unauthorized_settings_endpoint_is_not_an_error() {
        let c = client(
            FakeTransport::new()
                .on(
                    Method::GET,
                    "/api/v1/settings/api",
                    Canned::json(403, r#"{"message":"token required"}"#),
                )
                .on(Method::GET, "/api/v1/nodeinfo", Canned::json(200, NODEINFO)),
        );
        let caps = c.capabilities().await.unwrap();
        assert!(!caps.settings_known);
        assert_eq!(caps.instance_label().as_deref(), Some("forgejo 16.0.4"));
    }

    /// `max_response_items: 0` would make `effective_limit` request `limit=0` and return
    /// nothing at all — a total-data-loss bug wearing a successful exit code.
    #[tokio::test]
    async fn a_zero_valued_field_keeps_the_upstream_default() {
        let c = client(
            FakeTransport::new()
                .on(
                    Method::GET,
                    "/api/v1/settings/api",
                    Canned::json(200, r#"{"max_response_items":0}"#),
                )
                .on(Method::GET, "/api/v1/nodeinfo", Canned::new(404)),
        );
        let caps = c.capabilities().await.unwrap();
        assert_eq!(caps.max_response_items, DEFAULT_MAX_RESPONSE_ITEMS);
        assert!(caps.settings_known, "the endpoint did answer");
    }

    /// The probe must happen once per client, not once per call — otherwise every paginated
    /// stream adds two round trips.
    #[tokio::test]
    async fn capabilities_are_cached_per_client() {
        let t = Arc::new(
            FakeTransport::new()
                .on(Method::GET, "/api/v1/settings/api", Canned::json(200, SETTINGS))
                .on(Method::GET, "/api/v1/nodeinfo", Canned::json(200, NODEINFO)),
        );
        let c = Client::builder("https://git.example.org", Auth::None)
            .transport(t.clone())
            .build()
            .unwrap();
        for _ in 0..5 {
            c.capabilities().await.unwrap();
        }
        assert_eq!(t.call_count(), 2, "one probe of each endpoint, cached thereafter");
    }
}
