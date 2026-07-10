# Changelog

All notable changes to graph. Generated from conventional commits by git-cliff.

## v0.4.1 — 2026-07-10


### Added

- group graph mcp tools output by server (#11)

### Documentation

- rewrite README, add MIT license (#12)

### Fixed

- create releases atomically — assets can't be added after publish (#13)

## v0.4.0 — 2026-07-10


### Added

- publish a container image with each release (#4)
- vendor lbug fts/vector extensions into the binary (#9)
- builtin__ namespace for bundled tool packs, Built-ins docs page (#10)

### Documentation

- CI cookbook — the dogfooded plans and workflow, annotated (#7)
- cookbook as a section — pages by solution category (#8)

## v0.3.0 — 2026-07-10


### Added

- bundled tool packs and GitHub Actions failure annotations (#2)

### Fixed

- portable version bump in release.sh; align workspace version with v0.2.0

## v0.2.0 — 2026-07-10


### Added

- exit gates — end a plan early with success or error state
- plan composability — plans call plans

### Documentation

- bring CLAUDE.md current — composability, exit gates, storage, build story, conventions

## v0.1.0 — 2026-07-10


### Added

- ladybug spike (validated) + layered config crate
- clap command tree, tracing, working config show/init/path
- provider trait, Anthropic + OpenAI-compat providers, structured output with repair, role router
- rmcp manager — stdio + streamable-http transports, lazy connect, tool discovery with namespacing and overrides, ToolRegistry impl
- ReAct loop + ask/chat/tools commands
- thread persistence + observed-shape cache (phase 3)
- unify thread continuation under --thread
- strict typed template engine for the {{Ex.path}} dialect
- plan pipeline — planner/validation/execution/solver with bus-driven replanning
- YAML plan docs, plans-as-tools, plan_and_execute (phase 4 complete)
- JSON input documents for plan run and tools test
- nested tool display, pipeline progress, streamed solver
- optional solver — plans can render structured output or run silently
- backend abstraction — dyn Store everywhere, memory backend
- user-defined tools — exec, cypher, and prompt kinds
- schema defaults for plan/tool inputs; fmt fixes
- codify release process — semver bump, git-cliff changelog, tag-driven binaries
- run traces — tools_used in ask envelope, GRAPH_EVENTS=jsonl event stream

### Documentation

- Mintlify documentation site (25 pages) + CLAUDE.md
- point repository URLs at the real remote
- fix clone directory in installation
- touch content to trigger first build
- remove build-trigger scratch line
- plans-first framing of core concepts
- quickstart — freeze-into-a-plan step
- plan-first nav order and README framing; drop unverified heading anchors

### Fixed

- shut down servers before runtime teardown; silence child stderr
- read the shape cache at each planning attempt
- steps_executed excludes the input root
- replace RUSTFLAGS with per-target build.rs link directives
