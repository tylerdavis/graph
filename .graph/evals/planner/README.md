# Planner evals

Compares plan drafters on a fixed goal set. Each goal in `goals.yaml` has run inputs, a hand-written reference plan in `plans/`, and a rubric. `compare.py` drafts each goal with every planner, runs the drafted plan on the goal's inputs against real data (this repo and Linear), runs the reference plan alongside it, and has `planner_eval_judge` decide whether the drafted plan's output answers the goal.

Run it from the repo root with a clean working tree, a configured `decider` role, and Linear and the `github` pack available:

```bash
python3 .graph/evals/planner/compare.py compose_plan --reps 3 --workers 4 --out /tmp/planner-eval/results.json
```

It rewrites the `[plans]` paths in the local `.graph/config.toml` while it runs: drafting sees only the repo's plans, so reference and candidate plans never leak into tool search, and running sees the reference plans and the drafted candidates in `/tmp/planner-eval/plans`.
