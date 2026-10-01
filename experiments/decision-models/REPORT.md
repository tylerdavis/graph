# Drift-gate experiment: `infer` vs `decide`

Run 2026-09-30 on `dev/decision-models`. The PR set is the last 10 merged PRs that touched `crates/`, listed in `prs.txt`.

- **`infer`:** the gate is answered by the repo's LLM roles. docs uses `judge` (claude-haiku-4-5); format uses `reviewer` (claude-sonnet-5).
- **`decide`:** the gate is answered by `decider` (TypeSafe `jev-latest`, served as jev-1.13.0), with `min_confidence` 0.5.
- **What differs between the two:** only the gate. Each pair of plans has identical diff steps (see Setup).
- **Raw data:** per-run envelopes are in `results/` (gitignored), and the aggregates are in `summary.json`.

## Results

| Gate | PRs judged | Agreement | Cost `infer` | Cost `decide` | Cost ratio | Mean wall time `infer` | Mean wall time `decide` | Speedup |
|---|---|---|---|---|---|---|---|---|
| docs_drift | 10 | 10/10 | $0.198 | $0.0062 | 32× | 2.43 s | 0.41 s | 5.9× |
| format_drift | 6 | 4/6 | $0.207 | $0.0014 | 144× | 3.09 s | 0.33 s | 9.4× |

- Wall time covers the whole plan run, including git steps of about 0.1 s.
- format_drift judged only 6 of the 10 PRs. The other 4 changed no model file, so both variants exited early, before reaching the gate.

### Disagreements (format_drift)

| PR | `infer` | `decide` probability | Change |
|---|---|---|---|
| #121 | pass | 0.57 → fired | introduced file versioning (the constants themselves) |
| #125 | pass | 0.60 → fired | `feat(config)!` with a `CONFIG_FORMAT` bump |

- Both PRs bumped their format, so `infer` passing is the correct answer.
- `decide` was unsure on both, with probabilities just above the 0.5 default.
- The other four `decide` verdicts sat at 0.21, 0.89, 0.91, and 0.94.
- At `min_confidence: 0.7`, all six agree. That threshold was picked after seeing these six PRs, so it needs confirming on fresh ones.

## Caveats

- **docs_drift had no negative cases.** Both variants fired on every PR, so 10/10 shows they agree on "docs needed" and says nothing about false positives. A no-docs-needed set is required for that: refactor-only or test-only PRs.
- **The format_drift sample is small (6).** The two disagreements are the hardest kind of case, where a schema change is accounted for by a bump.
- **Diff budgets were cut to fit Jev's 32k state window**, for both variants:
  - docs: 150 KB → 90 KB
  - format: model diff 150 KB → 60 KB, format diff 40 KB → 30 KB

  At 120 KB, docs failed live with `max_tokens_exceeded`; code measured about 3.6 bytes per token. The production plans see more of large diffs than either variant did here.
- **Experiment-only changes to both variants:**
  - Repo-root pathspecs (`:/crates/`), so the plans run from this directory.
  - docs_drift skips its "docs were touched" shortcut, so the gate always judges.
- **One harness bug was fixed during the run.** An all-digit short SHA (`2018554`) was coerced to a number by `--input`, so `prs.txt` now carries full SHAs.

## Conclusion

- **docs_drift:** `decide` matches `infer` on every PR tested, at about 1/32 of the cost and about 6× the speed. It hasn't been tested on PRs that don't need docs.
- **format_drift:** `decide` is about 144× cheaper and 9× faster. At the default threshold it is over-eager on bumped-schema PRs. A `min_confidence` around 0.7 removes both disagreements here, but needs confirming on fresh PRs.
