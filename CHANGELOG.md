# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.0](https://github.com/euri10/deepseek-acp-adapter/compare/v0.5.4...v0.6.0) - 2026-07-16

### Other

- update Cargo.toml dependencies

## [0.5.4](https://github.com/euri10/deepseek-acp-adapter/compare/v0.5.3...v0.5.4) - 2026-07-16

### Added

- switch to caret deps + add Renovate config for automated upgrades
- upgrade agent-client-protocol
- add plan exit transition (daa-h3s)
- enforce plan mode in turns
- add plan session mode

### Fixed

- fix rmcp 2.2 api break
- *(deps)* update rust crate http to ^1.4.2 ([#16](https://github.com/euri10/deepseek-acp-adapter/pull/16))
- *(deps)* update rust crate grep to ^0.4.1 ([#22](https://github.com/euri10/deepseek-acp-adapter/pull/22))
- *(deps)* update rust crate thiserror to ^2.0.18 ([#19](https://github.com/euri10/deepseek-acp-adapter/pull/19))
- *(deps)* update rust crate ignore to ^0.4.29 ([#17](https://github.com/euri10/deepseek-acp-adapter/pull/17))
- *(deps)* update rust crate uuid to ^1.24.0 ([#20](https://github.com/euri10/deepseek-acp-adapter/pull/20))
- *(deps)* update rust crate rmcp to ^1.8.0 ([#18](https://github.com/euri10/deepseek-acp-adapter/pull/18))
- *(deps)* update rust crate globset to ^0.4.19 ([#15](https://github.com/euri10/deepseek-acp-adapter/pull/15))
- *(deps)* update rust crate clap to ^4.6.2 ([#14](https://github.com/euri10/deepseek-acp-adapter/pull/14))

### Other

- fix reqwest upgrade
- *(ci)* fix upgrade
- *(deps)* update dependency rust to 1.97 ([#21](https://github.com/euri10/deepseek-acp-adapter/pull/21))
- *(deps)* update actions/checkout action to v7 ([#24](https://github.com/euri10/deepseek-acp-adapter/pull/24))
- *(deps)* update rust crate axum to ^0.8.9 ([#13](https://github.com/euri10/deepseek-acp-adapter/pull/13))
- cover plan mode transition

## [0.5.3](https://github.com/euri10/deepseek-acp-adapter/compare/v0.5.2...v0.5.3) - 2026-07-16

### Fixed

- byte-cap grep/read_file tool output to bound history (daa-8q1)
- sanitize conversation before DeepSeek send to avoid 400 (daa-rts)
- reduce message budget and improve size estimation to avoid 400 errors

### Other

- DeepSeek 400 Bad Request regression fixed (daa-gvo)
- DeepSeek API 400 Bad Request regression (daa-gvo)

## [0.5.2](https://github.com/euri10/deepseek-acp-adapter/compare/v0.5.1...v0.5.2) - 2026-07-15

### Added

- expose max_tokens as a session config option (daa-hna)
- implement message filtering to respect CloudFront payload limit

### Fixed

- resolve cargo audit vulnerabilities
- stop history truncation collapsing to a single message (daa-wx1)
- validate tool result messages when filtering conversation history
- validate MCP tool schemas to ensure DeepSeek API compatibility (daa-fj0)
- root cause of 400 Bad Request identified - payload size limit (daa-gd9)

### Other

- cargo audit failures fixed (daa-nnu)
- cargo audit fails in CI (daa-nnu)
- close daa-fj0 - root cause identified and fixed (daa-fj0)
- investigate root cause of 400 errors (daa-fj0)
- enable TRACE logging in acp-debug.sh for easier debugging
- clarify EventSource error response body limitation
- DeepSeek API returns 400 Bad Request on streaming chat completion (daa-gd9)

## [0.5.1](https://github.com/euri10/deepseek-acp-adapter/compare/v0.5.0...v0.5.1) - 2026-06-22

### Fixed

- *(acp)* classify invalid prompt blocks as Invalid params

## [0.5.0](https://github.com/euri10/deepseek-acp-adapter/compare/v0.4.1...v0.5.0) - 2026-06-16

### Added

- *(acp)* add ACP parity
- *(usage)* accumulate token usage across prompt turns in PromptResponse

### Fixed

- *(usage)* extract and apply context_length from model specifications

### Other

- backfill historical changelog ([#5](https://github.com/euri10/deepseek-acp-adapter/pull/5))

## [0.4.1](https://github.com/euri10/deepseek-acp-adapter/compare/v0.4.0...v0.4.1) - 2026-06-11

### Fixed

- *(serve)* exit on client disconnect and termination signals ([#3](https://github.com/euri10/deepseek-acp-adapter/pull/3))

### Other

- *(error)* add tests for error.rs, coverage 36% -> 100%

## [0.4.0](https://github.com/euri10/deepseek-acp-adapter/compare/v0.3.1...v0.4.0) - 2026-06-10

### Added

- *(deepseek)* add usage_update telemetry to track token consumption
- add usage_update telemetry to deepseek-acp-adapter (daa-ik5)
- populate ACP _meta with historyJsonlPath; replace debug script

### Other

- Update issues (daa-ik5 closed)
- fix broken architecture table links in README
- isolate test session state from real XDG_STATE_HOME
- replace manual publish workflow with release-plz

## [0.3.1](https://github.com/euri10/deepseek-acp-adapter/compare/v0.3.0...v0.3.1) - 2026-06-09

### Fixed

- derive session titles from the first prompt instead of empty history

### Other

- update Cargo.lock for the 0.3.0 release

## [0.3.0](https://github.com/euri10/deepseek-acp-adapter/compare/v0.2.0...v0.3.0) - 2026-06-09

### Added

- populate session titles and update timestamps in ACP session metadata
- list all persisted sessions sorted by recency
- add detailed request logging for DeepSeek API debugging

### Fixed

- resolve DeepSeek API 400 Bad Request failures
- persist session history after each prompt turn

### Other

- update the README module map to reflect the current architecture
- apply clippy and formatting cleanup required by the project lint policy

## [0.2.0](https://github.com/euri10/deepseek-acp-adapter/compare/v0.1.1...v0.2.0) - 2026-06-07

### Added

- introduce a crate-level `AdapterError` and switch domain function signatures to it
- add targeted strictness lints for safer adapter development

### Changed

- extract session, development, ACP, DeepSeek, MCP, prompt-turn, and built-in tool logic into focused modules
- split large inline test modules into module-local test files

### Other

- expand MCP, ACP, tool routing, and requester wrapper test coverage
- remove stale dead-code suppressions and clarify boxed future aliases

## [0.1.1](https://github.com/euri10/deepseek-acp-adapter/compare/v0.1.0...v0.1.1) - 2026-06-05

### Other

- make the debug adapter launcher more generic
- update ACP coverage, installation, alpha-status, and debugging documentation

## [0.1.0](https://github.com/euri10/deepseek-acp-adapter/releases/tag/v0.1.0) - 2026-06-05

### Added

- bootstrap the ACP adapter server with DeepSeek streaming prompt sessions and initialize handshake support
- add prompt cancellation, local tool-call handling, permission modes, and prompt-turn request limits
- add read, write, edit, shell, and local navigation tool support through ACP client capabilities
- support stdio and HTTP MCP servers
- persist, load, list, and resume sessions, including embedded text context and session setting notifications
- emit optional ACP plan and slash-command updates
- add the hidden development smoke-test flow, setup guide, GitHub Actions CI, and crates.io metadata

### Fixed

- handle non-UTF-8 `read_file` errors
- expose ACP model session options and additional directories
- route write and edit operations through the client filesystem
- route terminal commands through ACP terminal methods when available
- retry DeepSeek SSE streams on transport errors before the first event
- handle null `write_text_file` responses from the client

### Other

- add architecture documentation and design principles
- raise test coverage above 90%
- bump the MSRV to Rust 1.95
