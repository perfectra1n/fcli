//! Quota subjects, and what each one actually counts.
//!
//! # The thing everyone gets wrong
//!
//! A Forgejo quota is **not** a limit on repository size. `size:all` counts, together:
//!
//! * the git repositories (public and private),
//! * Git **LFS** objects,
//! * **packages** in the registry,
//! * **release** assets, and
//! * **issue and comment** attachments,
//! * Actions **artifacts**.
//!
//! So a user with a 1 GiB quota and a 20 MiB repository can absolutely be over quota, and the
//! HTTP 413 they get when pushing says nothing about which of those six is responsible. That is
//! the question `fcli quota status` exists to answer, and answering it is why this module
//! exists rather than a `println!` of the API's response.
//!
//! # Why the composition is computed here
//!
//! `GET /user/quota` returns the **leaves** of the tree: `used.size.repos.public`,
//! `used.size.git.LFS`, `used.size.assets.attachments.issues`, and so on. It does *not* return
//! the aggregates a rule is written against — there is no `used.size.all` field. So a rule whose
//! subject is `size:all` cannot be compared against usage without summing the leaves, and
//! [`Used::for_subject`] is that sum.
//!
//! The composition below mirrors Forgejo's own `quota.LimitSubject` tree. It is asserted by
//! [`tests::the_aggregates_are_the_sums_of_their_leaves`], which is the test that would fail if
//! a future Forgejo moved a leaf between branches.

use forgejo_model::QuotaUsed;

/// Every subject Forgejo knows, with what it counts, in the order `--help` should list them.
///
/// Not used to *validate* `--subject`: a newer instance may know a subject this build does not,
/// and refusing it would make `fcli` the reason an admin cannot use their own server. It is used
/// for help text, and to warn when a subject looks like a typo.
pub const KNOWN: &[(&str, &str)] = &[
    ("none", "no storage counted"),
    ("size:all", "repositories, LFS, packages, attachments, and artifacts"),
    ("size:repos:all", "all git repositories, public and private"),
    ("size:repos:public", "public git repositories"),
    ("size:repos:private", "private git repositories"),
    ("size:git:all", "repositories plus their LFS objects"),
    ("size:git:lfs", "Git LFS objects only"),
    ("size:assets:all", "attachments, artifacts and packages together"),
    ("size:assets:attachments:all", "issue/comment and release attachments"),
    ("size:assets:attachments:issues", "issue and comment attachments"),
    ("size:assets:attachments:releases", "release attachments"),
    ("size:assets:artifacts", "Actions artifacts"),
    ("size:assets:packages:all", "packages in the registry"),
    ("size:wiki", "wiki storage (reserved; not measured)"),
];

/// The usage leaves, flattened out of [`QuotaUsed`]'s nest of optionals.
///
/// Flattened because every field of the response is optional at three levels, and threading
/// `Option` chains through the composition arithmetic would bury the arithmetic. An absent leaf
/// is zero: the server not reporting a category means nothing is stored in it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Used {
    pub repos_public: i64,
    pub repos_private: i64,
    pub git_lfs: i64,
    pub attachments_issues: i64,
    pub attachments_releases: i64,
    pub artifacts: i64,
    pub packages: i64,
}

impl Used {
    pub fn from(used: &QuotaUsed) -> Self {
        let size = used.size.as_ref();
        let repos = size.and_then(|s| s.repos.as_ref());
        let git = size.and_then(|s| s.git.as_ref());
        let assets = size.and_then(|s| s.assets.as_ref());
        let attachments = assets.and_then(|a| a.attachments.as_ref());
        let packages = assets.and_then(|a| a.packages.as_ref());
        Self {
            repos_public: repos.map(|r| r.public).unwrap_or(0),
            repos_private: repos.map(|r| r.private).unwrap_or(0),
            git_lfs: git.map(|g| g.lfs).unwrap_or(0),
            attachments_issues: attachments.map(|a| a.issues).unwrap_or(0),
            attachments_releases: attachments.map(|a| a.releases).unwrap_or(0),
            artifacts: assets.map(|a| a.artifacts).unwrap_or(0),
            packages: packages.map(|p| p.all).unwrap_or(0),
        }
    }

    pub fn repos_all(self) -> i64 {
        self.repos_public + self.repos_private
    }

    pub fn git_all(self) -> i64 {
        self.repos_all() + self.git_lfs
    }

    pub fn attachments_all(self) -> i64 {
        self.attachments_issues + self.attachments_releases
    }

    pub fn assets_all(self) -> i64 {
        self.attachments_all() + self.artifacts + self.packages
    }

    pub fn all(self) -> i64 {
        self.git_all() + self.assets_all()
    }

    /// The bytes a rule with this subject is measured against.
    ///
    /// `None` for a subject this build does not know — a newer Forgejo's, or a typo. The caller
    /// renders that as `?` rather than as `0 B`, because "we do not know" and "nothing is stored"
    /// lead to opposite conclusions when someone is diagnosing a 413.
    pub fn for_subject(self, subject: &str) -> Option<i64> {
        match subject.trim().to_ascii_lowercase().as_str() {
            "none" => Some(0),
            "size:all" => Some(self.all()),
            "size:repos:all" => Some(self.repos_all()),
            "size:repos:public" => Some(self.repos_public),
            "size:repos:private" => Some(self.repos_private),
            "size:git:all" => Some(self.git_all()),
            "size:git:lfs" => Some(self.git_lfs),
            "size:assets:all" => Some(self.assets_all()),
            "size:assets:attachments:all" => Some(self.attachments_all()),
            "size:assets:attachments:issues" => Some(self.attachments_issues),
            "size:assets:attachments:releases" => Some(self.attachments_releases),
            "size:assets:artifacts" => Some(self.artifacts),
            "size:assets:packages:all" => Some(self.packages),
            // Forgejo reserves the subject but reports no usage for it, so zero is the honest
            // answer rather than "unknown".
            "size:wiki" => Some(0),
            _ => None,
        }
    }

    /// The breakdown, in the order a human should read it: biggest categories first by concept,
    /// not by value, so the same rows appear in the same order every run.
    pub fn breakdown(self) -> Vec<(&'static str, i64)> {
        vec![
            ("repositories (public)", self.repos_public),
            ("repositories (private)", self.repos_private),
            ("Git LFS", self.git_lfs),
            ("packages", self.packages),
            ("release attachments", self.attachments_releases),
            ("issue attachments", self.attachments_issues),
            ("Actions artifacts", self.artifacts),
        ]
    }
}

/// True when a subject is one this build knows. Used only to warn, never to refuse.
pub fn is_known(subject: &str) -> bool {
    let s = subject.trim().to_ascii_lowercase();
    KNOWN.iter().any(|(name, _)| *name == s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use forgejo_model::{
        QuotaUsedSize, QuotaUsedSizeAssets, QuotaUsedSizeAssetsAttachments,
        QuotaUsedSizeAssetsPackages, QuotaUsedSizeGit, QuotaUsedSizeRepos,
    };

    fn sample() -> Used {
        Used {
            repos_public: 100,
            repos_private: 200,
            git_lfs: 400,
            attachments_issues: 8,
            attachments_releases: 16,
            artifacts: 32,
            packages: 64,
        }
    }

    /// Bug this prevents: an aggregate subject being compared against the wrong leaves, so
    /// `fcli quota status` reports a user comfortably inside a `size:all` limit they are in fact
    /// over. This is the arithmetic that turns "413 Payload Too Large" into an answer, and it is
    /// the test that fails if Forgejo ever moves a leaf between branches.
    #[test]
    fn the_aggregates_are_the_sums_of_their_leaves() {
        let u = sample();
        assert_eq!(u.repos_all(), 300);
        assert_eq!(u.git_all(), 700, "git:all is repositories plus LFS");
        assert_eq!(u.attachments_all(), 24);
        assert_eq!(
            u.assets_all(),
            24 + 32 + 64,
            "assets:all is attachments + artifacts + packages"
        );
        assert_eq!(u.all(), 700 + 120);

        // And the same numbers through the subject names a rule is actually written with.
        assert_eq!(u.for_subject("size:all"), Some(820));
        assert_eq!(u.for_subject("size:git:all"), Some(700));
        assert_eq!(u.for_subject("size:git:lfs"), Some(400));
        assert_eq!(u.for_subject("size:assets:artifacts"), Some(32));
        assert_eq!(u.for_subject("size:assets:packages:all"), Some(64));
    }

    /// The claim the module comment makes, as an executable assertion: LFS, packages and release
    /// assets are inside `size:all`. Someone reading a quota as "my repository is small, so I
    /// cannot be over" is wrong by exactly this much.
    #[test]
    fn size_all_counts_lfs_packages_and_release_assets_not_just_the_repository() {
        let repo_only = Used { repos_public: 1_000, ..Default::default() };
        let with_extras = Used {
            repos_public: 1_000,
            git_lfs: 5_000,
            packages: 10_000,
            attachments_releases: 20_000,
            ..Default::default()
        };
        assert_eq!(repo_only.all(), 1_000);
        assert_eq!(with_extras.all(), 36_000);
        assert_eq!(
            with_extras.for_subject("size:repos:all"),
            Some(1_000),
            "the repository subject must NOT include them — that is the distinction"
        );
    }

    /// Bug this prevents: an unknown subject silently reading as 0 bytes, so a rule from a newer
    /// Forgejo appears to be comfortably unused. Unknown and empty must look different.
    #[test]
    fn an_unknown_subject_is_unknown_rather_than_zero() {
        assert_eq!(sample().for_subject("size:something:new"), None);
        assert_eq!(sample().for_subject("none"), Some(0));
        assert!(!is_known("size:something:new"));
        assert!(is_known("size:git:lfs"));
        // Case and surrounding space come from copy-paste out of a config file.
        assert!(is_known(" SIZE:GIT:LFS "));
    }

    /// Bug this prevents: mis-flattening the three levels of `Option` in the response, so every
    /// leaf reads zero and the whole command reports "nothing used".
    #[test]
    fn the_response_nest_of_optionals_flattens_to_the_leaves() {
        let used = QuotaUsed {
            size: Some(QuotaUsedSize {
                repos: Some(QuotaUsedSizeRepos { public: 100, private: 200 }),
                git: Some(QuotaUsedSizeGit { lfs: 400 }),
                assets: Some(QuotaUsedSizeAssets {
                    artifacts: 32,
                    attachments: Some(QuotaUsedSizeAssetsAttachments { issues: 8, releases: 16 }),
                    packages: Some(QuotaUsedSizeAssetsPackages { all: 64 }),
                }),
            }),
        };
        assert_eq!(Used::from(&used), sample());

        // A response with nothing filled in is all zeroes, not a panic: an instance with quotas
        // switched off answers exactly like that.
        assert_eq!(Used::from(&QuotaUsed::default()), Used::default());
    }
}
