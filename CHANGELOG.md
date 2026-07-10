# Changelog

All notable changes to graph. Generated from conventional commits by git-cliff.

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
