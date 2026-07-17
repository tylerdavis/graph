# Changelog

All notable changes to graph. Generated from conventional commits by git-cliff.

## v0.8.1 — 2026-07-17


### Added

- show plan metadata, input schema, and finish on the root node (#61)

### Fixed

- wheel over steps/tool list moves selection (#60)
- gh_pr_ticket default pattern requires a separator (#62)
- wheel scrolls the steps/tool list view (#63)
- hide list highlight when selection scrolls out of view (#64)

## v0.8.0 — 2026-07-17


### Added

- write the built-in system prompts into the config init starter (#44)
- steer check plans to explicit exits and list inference to map (#45)
- named models selectable from prompt tools and builtin__infer (#46)
- marker-keyed PR comments, ticket extraction, and file/grep at a ref (#47)
- catalog-aware tool resolution before any step runs (#49)
- incremental draft strategy — outline, then one validated step per inference (#50)
- edit input_schema, requires_servers, and silent finish via update_metadata (#51)
- mouse support — click to focus, switch tabs, select rows, wheel-scroll (#55)
- add data pack with builtin__reshape for shape projection (#58)
- optional per-gate model override on exit/decide infer (#59)

### Documentation

- add graph-github-actions-setup skill for coding agents (#42)

### Fixed

- render sub-text and borders with the terminal's dim modifier (#43)
- PR reviewer no longer emits absence false positives on truncated diffs (#40)
- paste literally, edit invalid drafts, repair bad drafts, fence agent-only tools (#48)
- separate outline and drafting phases in workbench trace (#52)
- show span start time on the left and duration on the right; surface outline call duration (#53)
- order trace chronologically so draft_plan brackets its phases; fix outline duration origin (#54)
- carry failing tool error into aborted run result (#56)
- default output_schema type + reset workbench iteration budget on progress (#57)

## v0.7.0 — 2026-07-15


### Added

- replace LadybugDB with file-based storage (#24)
- plan workbench — dual-pane TUI for drafting and test-running plans (#25)
- workbench debug logging to <data_dir>/workbench.log (#28)
- workbench step view shows body sub-steps and the finish stage (#30)
- workbench read_file/grep/glob tools for researching the project (#29)
- step ids are any unique identifier, not just E-numbers (#31)
- workbench tools for precise plan edits: update_metadata, add_step, update_step, delete_step (#33)
- [prompts] config overrides for the chat prompt and workbench addendum (#37)
- project-first config — config init and default search paths target ./.graph (#38)
- draft safety, control-step guidance, turn-failure recovery (#36)

### Documentation

- cookbook covers a custom bot identity for the CI reviewer (#23)
- add @emichy to special thanks (#26)
- require worktrees for all coding work in CLAUDE.md (#39)

### Fixed

- scrolling reaches wrapped content; PgUp/PgDn is the one scroll binding (#27)
- draft saves can no longer overwrite a different plan's file (#32)
- section-scoped bare keys are not roots in plan validation (#34)
- a broken plan file no longer takes down the whole catalog (#35)

## v0.6.0 — 2026-07-11


### Added

- decide steps fork plan execution into then/else branches (#15)
- decide gates read if/then/else — the logical gate keyword is now if (#17)
- map and reduce steps iterate a body over a list (#18)
- pr_review anchors findings as inline diff comments (#20)

## v0.5.0 — 2026-07-10


### Added

- grouped listing for graph tools list, tighter layout (#14)

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
