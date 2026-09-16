# Changelog

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
