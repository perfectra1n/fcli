# Changelog

## [0.2.1](https://github.com/perfectra1n/fjo/compare/v0.2.0...v0.2.1) (2026-09-16)


### Features

* **ci:** watch upstream Forgejo's spec and open bump PRs on drift ([bfb6a34](https://github.com/perfectra1n/fjo/commit/bfb6a3432a1fda5fa2f3fd92f4d3f5fcc711c896))
* **deps:** update rust (1.95.0 → 1.98.1) ([1ef2004](https://github.com/perfectra1n/fjo/commit/1ef200458aa47bc696e896964fd554c25ca44dbb))
* **xtask:** add spec-diff, a semantic report of upstream spec drift ([efb40de](https://github.com/perfectra1n/fjo/commit/efb40de329ce4666fb3f9694aad4689471e6666d))


### Bug Fixes

* **clippy:** drop redundant borrows flagged by rust 1.98 ([0705864](https://github.com/perfectra1n/fjo/commit/0705864ca00cf83e7bab69eb7b9b90e3a1042b06))
* **rust:** update crate clap (4.6.6 → 4.6.7) ([fdd3154](https://github.com/perfectra1n/fjo/commit/fdd3154749892de57aa7fe0b035e2068da3a8e74))
* **rust:** update crate clap_complete (4.6.9 → 4.6.11) ([d4e6dbc](https://github.com/perfectra1n/fjo/commit/d4e6dbca92c9cddd73856867e57edaeb1748c8c1))


### Documentation

* point toolchain comments at the 1.98.1 pin ([1fde351](https://github.com/perfectra1n/fjo/commit/1fde351ffdc7c51ab209fc090bc9355a5962043c))


### Miscellaneous Chores

* **github-action:** update github-actions ([c208eda](https://github.com/perfectra1n/fjo/commit/c208eda7d1af5f6e6385347daccd19296fff0b9a))
* land the open Renovate bumps and add upstream spec-drift automation ([ea200a0](https://github.com/perfectra1n/fjo/commit/ea200a0c1a23082c9151ee921955826a0657afa8))
* **rust:** lock file maintenance crate (cargo) ([b75c235](https://github.com/perfectra1n/fjo/commit/b75c2356d43dbd66124030bc66385c415dd9ffc1))

## [0.2.0](https://github.com/perfectra1n/fjo/compare/v0.1.0...v0.2.0) (2026-09-16)


### ⚠ BREAKING CHANGES

* four pieces of persisted user state move with no migration path, so an existing install will appear logged out and will lose its default-repo resolution until reconfigured.

### Features

* **api:** trim down API calls to Forgejo as much as possible ([06c2f08](https://github.com/perfectra1n/fjo/commit/06c2f0861f2df0a5974a741bcf46572af12564f1))
* **ci:** cut releases with release-please ([d881210](https://github.com/perfectra1n/fjo/commit/d8812106d4819a94f1e1ce9d9ad8d0883a0b0110))
* **license:** add license ([3f33bef](https://github.com/perfectra1n/fjo/commit/3f33bef74ee0eb3f76e82b5c5063eaad34e0a97e))
* **renovate:** implement renovate config ([60c41de](https://github.com/perfectra1n/fjo/commit/60c41dea26d103457b8b43b45bbcf693ee71ab9d))


### Bug Fixes

* **docstring:** update docstring to make cargo happy ([12fa9f1](https://github.com/perfectra1n/fjo/commit/12fa9f1dd9d68de344ec51785f11220cec5b179d))


### Code Refactoring

* rename fcli to fjo ([ef33082](https://github.com/perfectra1n/fjo/commit/ef33082e1eba55f9f2d9919509d9baa600348d9d))
