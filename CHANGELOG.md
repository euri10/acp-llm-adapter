# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).


## [Unreleased]

## [0.7.4](https://github.com/euri10/acp-llm-adapter/compare/v0.7.3...v0.7.4) - 2026-08-23

### Fixed

- *(deps)* update rust crate sse-reqwest-client to 0.4.0 ([#48](https://github.com/euri10/acp-llm-adapter/pull/48))
- *(deps)* update rust crate tokio to ^1.53.1 ([#49](https://github.com/euri10/acp-llm-adapter/pull/49))
- *(deps)* update rust crate serde_json to ^1.0.151 ([#46](https://github.com/euri10/acp-llm-adapter/pull/46))
- *(deps)* update rust crate tokio-util to ^0.7.19 ([#47](https://github.com/euri10/acp-llm-adapter/pull/47))
- *(deps)* update rust crate serde to ^1.0.229 ([#44](https://github.com/euri10/acp-llm-adapter/pull/44))
- *(deps)* update rust crate futures-util to ^0.3.34 ([#43](https://github.com/euri10/acp-llm-adapter/pull/43))
- *(deps)* reset renovate dependency tracking

### Other

- restore renovate configuration
- re-enable renovate dashboard

## [0.7.3](https://github.com/euri10/acp-llm-adapter/compare/v0.7.2...v0.7.3) - 2026-08-22

### Added

- *(logsink)* add ACP_LOG_UNREDACTED opt-out for local debugging
- *(acp)* expose session log paths
- *(logging)* route tracing events by session
- *(tracing)* scope events to ACP sessions
- *(usage)* report DeepSeek session cost
- *(logging)* bound and redact shared sink
- *(logging)* log serve ACP frames
- *(logging)* add proxy and session logs

### Fixed

- *(deps)* update rust crate clap to ^4.6.6 ([#36](https://github.com/euri10/acp-llm-adapter/pull/36))
- *(deps)* update rust crate globset to ^0.4.20 ([#37](https://github.com/euri10/acp-llm-adapter/pull/37))
- *(deps)* update rust crate ignore to ^0.4.33 ([#38](https://github.com/euri10/acp-llm-adapter/pull/38))
- *(deps)* update rust crate thiserror to ^2.0.20 ([#39](https://github.com/euri10/acp-llm-adapter/pull/39))
- *(deps)* update rust crate uuid to ^1.25.0 ([#41](https://github.com/euri10/acp-llm-adapter/pull/41))
- *(test)* remove main_returns_successful_exit_code test
- *(session)* honor cwd in persisted listing
- *(session)* record cwd filter bug
- report accurate context windows from explicit model table (daa-qej)
- stop debug wrapper from logging LLM request bodies (daa-4hf)
- *(deps)* update rust crate thiserror to ^2.0.19 ([#33](https://github.com/euri10/acp-llm-adapter/pull/33))
- *(deps)* update rust crate ignore to ^0.4.32 ([#32](https://github.com/euri10/acp-llm-adapter/pull/32))
- *(deps)* update rust crate clap to ^4.6.5 ([#34](https://github.com/euri10/acp-llm-adapter/pull/34))
- *(deps)* update rust crate http to ^1.5.0 ([#35](https://github.com/euri10/acp-llm-adapter/pull/35))

### Other

- *(deps)* update dependency rust to 1.98 ([#40](https://github.com/euri10/acp-llm-adapter/pull/40))
- *(acp-proxy)* require -- separator before agent command
- *(beads)* close logging feature
- *(logging)* remove legacy wrapper
- *(beads)* close session filter bug
- ignore local artifacts
- add CodeCompanion repro (daa-5op)

### Added

- `ACP_LOG_UNREDACTED` opts out of the always-on redaction of prompts, messages, and tool arguments in `ACP_LOG`/`acp-proxy` logs, for local debugging where the real content is what you're chasing

### Fixed

- stop the legacy debug wrapper from enabling `llm=trace` request-body logging by default; default to `debug` and never write LLM request bodies to the log (only their serialized byte size at trace)
- report accurate context windows from an explicit model table (`deepseek-v4-pro`/`deepseek-v4-flash` 1M, `glm-4.6` 128K) instead of silently asserting 1M for every unknown model; unknown models no longer emit a misleading `usage_update`

## [0.7.2](https://github.com/euri10/acp-llm-adapter/compare/v0.7.1...v0.7.2) - 2026-07-17

### Added

- genericize LLM backend env vars (DEEPSEEK_API_KEY → LLM_API_KEY)

### Other

- update RUST_LOG module

## [0.7.1](https://github.com/euri10/acp-llm-adapter/compare/v0.7.0...v0.7.1) - 2026-07-17

### Added

- require backend and enable MCP SSE

### Fixed

- rename debug log dir from codecompanion-acp to acp-llm-adapter

## [0.7.0] - 2026-07-16

### Added
- Multi-provider support: `--backend deepseek|glm|mock` CLI flag
- Dynamic model discovery via `GET /models` endpoint
- `AdapterState::available_models` with startup fetch

### Changed
- **Breaking:** Renamed crate from `deepseek-acp-adapter` to `acp-llm-adapter`
- **Breaking:** Module `deepseek` → `llm`; `DeepSeekClient` → `ChatClient`; `DeepSeekConfig` → `ChatConfig`; `DeepSeekError` → `ChatError`
- **Breaking:** `AdapterError::DeepSeek` → `AdapterError::Llm`
- **Breaking:** Session persistence dir renamed from `deepseek-acp-adapter` to `acp-llm-adapter`
- `initial_model_from_env()` replaced by `initial_model(fallback)` that checks `DEEPSEEK_MODEL` + `GLM_MODEL`
- Model functions (`model_select_options`, `is_known_model`, `validate_session_model`) use dynamic lists instead of hardcoded constants

## [0.6.0](https://github.com/euri10/acp-llm-adapter/compare/v0.5.4...v0.6.0) - 2026-07-16

### Other

- update Cargo.toml dependencies

## [0.5.4](https://github.com/euri10/acp-llm-adapter/compare/v0.5.3...v0.5.4) - 2026-07-16

### Added

- switch to caret deps + add Renovate config for automated upgrades
- upgrade agent-client-protocol
- add plan exit transition (daa-h3s)
- enforce plan mode in turns
- add plan session mode

### Fixed

- fix rmcp 2.2 api break
- *(deps)* update rust crate http to ^1.4.2 ([#16](https://github.com/euri10/acp-llm-adapter/pull/16))
- *(deps)* update rust crate grep to ^0.4.1 ([#22](https://github.com/euri10/acp-llm-adapter/pull/22))
- *(deps)* update rust crate thiserror to ^2.0.18 ([#19](https://github.com/euri10/acp-llm-adapter/pull/19))
- *(deps)* update rust crate ignore to ^0.4.29 ([#17](https://github.com/euri10/acp-llm-adapter/pull/17))
- *(deps)* update rust crate uuid to ^1.24.0 ([#20](https://github.com/euri10/acp-llm-adapter/pull/20))
- *(deps)* update rust crate rmcp to ^1.8.0 ([#18](https://github.com/euri10/acp-llm-adapter/pull/18))
- *(deps)* update rust crate globset to ^0.4.19 ([#15](https://github.com/euri10/acp-llm-adapter/pull/15))
- *(deps)* update rust crate clap to ^4.6.2 ([#14](https://github.com/euri10/acp-llm-adapter/pull/14))

### Other

- fix reqwest upgrade
- *(ci)* fix upgrade
- *(deps)* update dependency rust to 1.97 ([#21](https://github.com/euri10/acp-llm-adapter/pull/21))
- *(deps)* update actions/checkout action to v7 ([#24](https://github.com/euri10/acp-llm-adapter/pull/24))
- *(deps)* update rust crate axum to ^0.8.9 ([#13](https://github.com/euri10/acp-llm-adapter/pull/13))
- cover plan mode transition

## [0.5.3](https://github.com/euri10/acp-llm-adapter/compare/v0.5.2...v0.5.3) - 2026-07-16

### Fixed

- byte-cap grep/read_file tool output to bound history (daa-8q1)
- sanitize conversation before DeepSeek send to avoid 400 (daa-rts)
- reduce message budget and improve size estimation to avoid 400 errors

### Other

- DeepSeek 400 Bad Request regression fixed (daa-gvo)
- DeepSeek API 400 Bad Request regression (daa-gvo)

## [0.5.2](https://github.com/euri10/acp-llm-adapter/compare/v0.5.1...v0.5.2) - 2026-07-15

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
- enable TRACE logging in the legacy debug wrapper for easier debugging
- clarify EventSource error response body limitation
- DeepSeek API returns 400 Bad Request on streaming chat completion (daa-gd9)

## [0.5.1](https://github.com/euri10/acp-llm-adapter/compare/v0.5.0...v0.5.1) - 2026-06-22

### Fixed

- *(acp)* classify invalid prompt blocks as Invalid params

## [0.5.0](https://github.com/euri10/acp-llm-adapter/compare/v0.4.1...v0.5.0) - 2026-06-16

### Added

- *(acp)* add ACP parity
- *(usage)* accumulate token usage across prompt turns in PromptResponse

### Fixed

- *(usage)* extract and apply context_length from model specifications

### Other

- backfill historical changelog ([#5](https://github.com/euri10/acp-llm-adapter/pull/5))

## [0.4.1](https://github.com/euri10/acp-llm-adapter/compare/v0.4.0...v0.4.1) - 2026-06-11

### Fixed

- *(serve)* exit on client disconnect and termination signals ([#3](https://github.com/euri10/acp-llm-adapter/pull/3))

### Other

- *(error)* add tests for error.rs, coverage 36% -> 100%

## [0.4.0](https://github.com/euri10/acp-llm-adapter/compare/v0.3.1...v0.4.0) - 2026-06-10

### Added

- *(deepseek)* add usage_update telemetry to track token consumption
- add usage_update telemetry to acp-llm-adapter (daa-ik5)
- populate ACP _meta with historyJsonlPath; replace debug script

### Other

- Update issues (daa-ik5 closed)
- fix broken architecture table links in README
- isolate test session state from real XDG_STATE_HOME
- replace manual publish workflow with release-plz

## [0.3.1](https://github.com/euri10/acp-llm-adapter/compare/v0.3.0...v0.3.1) - 2026-06-09

### Fixed

- derive session titles from the first prompt instead of empty history

### Other

- update Cargo.lock for the 0.3.0 release

## [0.3.0](https://github.com/euri10/acp-llm-adapter/compare/v0.2.0...v0.3.0) - 2026-06-09

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

## [0.2.0](https://github.com/euri10/acp-llm-adapter/compare/v0.1.1...v0.2.0) - 2026-06-07

### Added

- introduce a crate-level `AdapterError` and switch domain function signatures to it
- add targeted strictness lints for safer adapter development

### Changed

- extract session, development, ACP, DeepSeek, MCP, prompt-turn, and built-in tool logic into focused modules
- split large inline test modules into module-local test files

### Other

- expand MCP, ACP, tool routing, and requester wrapper test coverage
- remove stale dead-code suppressions and clarify boxed future aliases

## [0.1.1](https://github.com/euri10/acp-llm-adapter/compare/v0.1.0...v0.1.1) - 2026-06-05

### Other

- make the debug adapter launcher more generic
- update ACP coverage, installation, alpha-status, and debugging documentation

## [0.1.0](https://github.com/euri10/acp-llm-adapter/releases/tag/v0.1.0) - 2026-06-05

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
