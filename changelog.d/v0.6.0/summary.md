#### Branch execution with decide steps

Plans can now include `decide` steps that fork execution into `then` and `else` branches based on a gate, letting you author plans with conditional logic instead of separate plans per outcome. The gate keyword for a `decide` step is `if`.

#### Iterate over lists with map and reduce

`map` and `reduce` steps run a body of steps over each item in a list, so repetitive per-item work no longer needs to be unrolled manually in the plan.

#### Inline PR review comments

The `pr_review` tool now anchors its findings as inline diff comments on the pull request, rather than only surfacing them elsewhere.
