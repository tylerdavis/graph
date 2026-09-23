//! Per-run token and cost accounting.
//!
//! [`UsageLedger`] implements [`graph_llm::UsageMeter`], so every call the
//! router serves lands here with its provider, model, and token counts. What
//! the meter cannot know is *which step* spent them — and a total without a
//! breakdown says "the run cost $6" when the useful answer is "the scouts
//! cost $4.20 of the $6".
//!
//! That context arrives through [`CALL_SITE`], a task-local each inference
//! site scopes around its own await. A task-local rather than a parameter
//! threaded through `graph-llm` because `map` runs items through ordered
//! `buffered(n)`, so several plan bodies share one task: a field on the
//! ledger would race, while `LocalKey::scope` wraps the *future* and is
//! entered and exited around each poll.

use graph_config::ModelPrice;
use graph_llm::types::Usage;
use graph_llm::{LlmCall, UsageMeter};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Mutex;

tokio::task_local! {
    /// Who is making the current inference. Unset outside a scoped site.
    static CALL_SITE: CallSite;
}

/// The plan-level context of one inference.
#[derive(Debug, Clone, Default)]
pub struct CallSite {
    /// Model role, standard or custom — `chat`, `solver`, `repair`, `judge`, …
    pub role: String,
    /// Step path in bus syntax: `E5`, `E4f/do.2/agent`. `None` for calls
    /// that belong to no step (the planner, a chat turn).
    pub path: Option<String>,
    /// Plan-call nesting, outermost first. Empty at the top level.
    pub plan_stack: Vec<String>,
}

impl CallSite {
    pub fn role(role: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            ..Default::default()
        }
    }

    pub fn at(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn in_plans(mut self, stack: &[String]) -> Self {
        self.plan_stack = stack.to_vec();
        self
    }

    /// Run `future` with this call site in scope, so any inference it makes
    /// is attributed here.
    pub async fn scope<F: std::future::Future>(self, future: F) -> F::Output {
        CALL_SITE.scope(self, future).await
    }

    /// The current call site, or a bare `unknown` role outside any scope —
    /// a bare test pipeline meters fine, it just cannot say where from.
    fn current() -> CallSite {
        CALL_SITE
            .try_with(Clone::clone)
            .unwrap_or_else(|_| CallSite::role("unknown"))
    }

    /// How this call groups in `by_step`. Nested plans qualify their step
    /// paths so an inner plan's `E0` is distinct from the outer plan's.
    fn group(&self) -> String {
        let path = match &self.path {
            Some(path) => path.clone(),
            None => return self.role.clone(),
        };
        if self.plan_stack.is_empty() {
            path
        } else {
            format!("{}/{path}", self.plan_stack.join("/"))
        }
    }
}

/// One metered call, with the attribution the meter could not see.
#[derive(Debug, Clone)]
struct CallRecord {
    provider: String,
    model: String,
    site: CallSite,
    usage: Usage,
}

/// Accumulates every model call of a run.
///
/// Shared by `Arc` across a `Pipeline` and every plan it nests into, so a
/// composed plan's spend rolls up without extra plumbing.
pub struct UsageLedger {
    calls: Mutex<Vec<CallRecord>>,
    prices: BTreeMap<String, ModelPrice>,
    /// Set per command by [`UsageLedger::attach_events`]. Interior mutability
    /// because the ledger outlives any one run — it is built with the router,
    /// while the sink is chosen per command — and `record` only ever has
    /// `&self`.
    events: Mutex<Option<std::sync::Arc<dyn crate::EventSink>>>,
}

impl UsageLedger {
    pub fn new(prices: BTreeMap<String, ModelPrice>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            prices,
            events: Mutex::new(None),
        }
    }

    /// Stream each call to `sink` as it lands, in addition to tallying it.
    /// Replaces any previously attached sink.
    pub fn attach_events(&self, sink: std::sync::Arc<dyn crate::EventSink>) {
        *self.events.lock().unwrap() = Some(sink);
    }

    /// A ledger that meters tokens but reports no cost.
    pub fn unpriced() -> Self {
        Self::new(BTreeMap::new())
    }

    /// Drain everything recorded so far into a report.
    ///
    /// Draining rather than snapshotting is what makes one long-lived router
    /// compatible with per-run reporting: `plan run` takes once at the end,
    /// the `chat` REPL takes once per turn.
    pub fn take(&self) -> UsageReport {
        let calls = std::mem::take(&mut *self.calls.lock().unwrap());
        self.report(calls)
    }

    fn report(&self, calls: Vec<CallRecord>) -> UsageReport {
        let mut total = Usage::default();
        let mut by_step: BTreeMap<String, Usage> = BTreeMap::new();
        // Keyed by provider *and* model: failover can serve one role from a
        // second provider, and "which model actually answered" is the whole
        // point of metering each candidate separately.
        let mut by_model: BTreeMap<(String, String), (Usage, usize)> = BTreeMap::new();

        for call in &calls {
            total.add(&call.usage);
            by_step
                .entry(call.site.group())
                .or_default()
                .add(&call.usage);
            let entry = by_model
                .entry((call.provider.clone(), call.model.clone()))
                .or_default();
            entry.0.add(&call.usage);
            entry.1 += 1;
        }

        let mut steps: Vec<StepUsage> = by_step
            .into_iter()
            .map(|(path, usage)| StepUsage {
                calls: calls.iter().filter(|c| c.site.group() == path).count(),
                cost_usd: self.cost_of_group(&calls, &path),
                path,
                usage,
            })
            .collect();
        steps.sort_by(|a, b| rank(b).total_cmp(&rank(a)));

        let mut models: Vec<ModelUsage> = by_model
            .into_iter()
            .map(|((provider, model), (usage, calls))| ModelUsage {
                cost_usd: self.cost(&model, &usage),
                provider,
                model,
                calls,
                usage,
            })
            .collect();
        models.sort_by(|a, b| rank_model(b).total_cmp(&rank_model(a)));

        // Sum the per-model costs rather than pricing the grand total: a run
        // that mixes a strong chat model with a cheap repair model has no
        // single rate to apply. `None` only when nothing at all was priced.
        let priced: Vec<f64> = models.iter().filter_map(|m| m.cost_usd).collect();
        let cost_usd = (!priced.is_empty()).then(|| priced.iter().sum());

        UsageReport {
            calls: calls.len(),
            total,
            cost_usd,
            by_step: steps,
            by_model: models,
        }
    }

    fn cost_of_group(&self, calls: &[CallRecord], group: &str) -> Option<f64> {
        let costs: Vec<f64> = calls
            .iter()
            .filter(|c| c.site.group() == group)
            .filter_map(|c| self.cost(&c.model, &c.usage))
            .collect();
        (!costs.is_empty()).then(|| costs.iter().sum())
    }

    /// Price one model's usage, or `None` when that model has no configured
    /// price. Never guesses — an unpriced model reports tokens only.
    fn cost(&self, model: &str, usage: &Usage) -> Option<f64> {
        let price = self.prices.get(model)?;
        let per_million = |tokens: u64, rate: f64| (tokens as f64) * rate / 1_000_000.0;
        Some(
            per_million(usage.input_tokens, price.input)
                + per_million(usage.output_tokens, price.output)
                + per_million(usage.cache_creation_input_tokens, price.cache_write_rate())
                + per_million(usage.cache_read_input_tokens, price.cache_read_rate()),
        )
    }
}

/// Sort key: cost where known, else total tokens, so an unpriced run still
/// ranks the expensive sites first.
fn rank(step: &StepUsage) -> f64 {
    step.cost_usd
        .unwrap_or(step.usage.total_input_tokens() as f64 + step.usage.output_tokens as f64)
}

fn rank_model(model: &ModelUsage) -> f64 {
    model
        .cost_usd
        .unwrap_or(model.usage.total_input_tokens() as f64 + model.usage.output_tokens as f64)
}

impl UsageMeter for UsageLedger {
    fn record(&self, call: LlmCall) {
        let site = CallSite::current();
        let group = site.group();
        self.calls.lock().unwrap().push(CallRecord {
            provider: call.provider,
            model: call.model.clone(),
            site,
            usage: call.usage,
        });
        // Emitted with no lock held: a sink is arbitrary caller code, and
        // one that touched the ledger would deadlock on `calls`.
        let sink = self.events.lock().unwrap().clone();
        if let Some(sink) = sink {
            sink.llm_call(&group, &call.model, &call.usage, call.elapsed);
        }
    }
}

/// What a run spent.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageReport {
    pub calls: usize,
    #[serde(flatten)]
    pub total: Usage,
    /// `None` when no model involved had a configured price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub by_step: Vec<StepUsage>,
    pub by_model: Vec<ModelUsage>,
}

impl UsageReport {
    pub fn is_empty(&self) -> bool {
        self.calls == 0
    }

    /// One-line summary for a terminal: `47 calls · 1.9M in / 38k out · $6.41`.
    pub fn summary(&self) -> String {
        let mut line = format!(
            "{} call{} · {} in / {} out",
            self.calls,
            if self.calls == 1 { "" } else { "s" },
            compact(self.total.total_input_tokens()),
            compact(self.total.output_tokens),
        );
        if self.total.cache_read_input_tokens > 0 {
            line.push_str(&format!(
                " ({} cached)",
                compact(self.total.cache_read_input_tokens)
            ));
        }
        if let Some(cost) = self.cost_usd {
            line.push_str(&format!(" · ${cost:.2}"));
        }
        line
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StepUsage {
    /// Step path, plan-qualified when nested.
    pub path: String,
    pub calls: usize,
    #[serde(flatten)]
    pub usage: Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelUsage {
    /// Config name of the provider that served these calls.
    pub provider: String,
    pub model: String,
    pub calls: usize,
    #[serde(flatten)]
    pub usage: Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// `1943210` → `1.9M`. Token counts are read at a glance, not audited.
///
/// Public so every surface that shows usage — the CLI summary, MCP progress
/// notifications, the workbench run log — renders the same figure the same
/// way.
pub fn compact_tokens(tokens: u64) -> String {
    compact(tokens)
}

fn compact(tokens: u64) -> String {
    match tokens {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
        n if n >= 1_000 => format!("{:.0}k", n as f64 / 1_000.0),
        n => n.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    fn price(input: f64, output: f64) -> ModelPrice {
        ModelPrice {
            input,
            output,
            cache_write: None,
            cache_read: None,
        }
    }

    fn priced_ledger() -> UsageLedger {
        UsageLedger::new(BTreeMap::from([
            ("sonnet".to_string(), price(3.0, 15.0)),
            ("haiku".to_string(), price(1.0, 5.0)),
        ]))
    }

    fn call(model: &str, usage: Usage) -> LlmCall {
        LlmCall {
            provider: "anthropic".into(),
            model: model.into(),
            usage,
            elapsed: Duration::from_millis(1),
        }
    }

    fn tokens(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn prices_input_output_and_both_cache_rates() {
        let ledger = priced_ledger();
        ledger.record(call(
            "sonnet",
            Usage {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                cache_creation_input_tokens: 1_000_000,
                cache_read_input_tokens: 1_000_000,
            },
        ));
        // 3 (input) + 15 (output) + 3.75 (write, 1.25x) + 0.30 (read, 0.1x)
        let report = ledger.take();
        assert_eq!(report.cost_usd, Some(22.05));
    }

    #[tokio::test]
    async fn an_unpriced_model_reports_tokens_but_no_cost() {
        let ledger = UsageLedger::unpriced();
        ledger.record(call("mystery", tokens(500, 100)));
        let report = ledger.take();
        assert_eq!(report.total.input_tokens, 500);
        assert_eq!(
            report.cost_usd, None,
            "a missing price must never be guessed"
        );
        assert_eq!(report.by_model[0].cost_usd, None);
    }

    #[tokio::test]
    async fn a_partially_priced_run_still_reports_what_it_can() {
        let ledger = priced_ledger();
        ledger.record(call("sonnet", tokens(1_000_000, 0)));
        ledger.record(call("mystery", tokens(9_000_000, 0)));
        let report = ledger.take();
        assert_eq!(report.cost_usd, Some(3.0));
        assert_eq!(report.calls, 2);
    }

    #[tokio::test]
    async fn take_drains_so_the_next_run_starts_clean() {
        let ledger = priced_ledger();
        ledger.record(call("sonnet", tokens(100, 10)));
        assert_eq!(ledger.take().calls, 1);
        assert_eq!(ledger.take().calls, 0, "a second take sees nothing");
    }

    #[tokio::test]
    async fn attributes_calls_to_the_scoped_step() {
        let ledger = Arc::new(priced_ledger());

        CallSite::role("chat")
            .at("E4f/do.1/agent")
            .scope(async { ledger.record(call("sonnet", tokens(1_000_000, 0))) })
            .await;
        CallSite::role("judge")
            .at("E5")
            .scope(async { ledger.record(call("haiku", tokens(1_000_000, 0))) })
            .await;

        let report = ledger.take();
        // Sorted by cost: sonnet's $3 outranks haiku's $1.
        assert_eq!(report.by_step[0].path, "E4f/do.1/agent");
        assert_eq!(report.by_step[0].cost_usd, Some(3.0));
        assert_eq!(report.by_step[1].path, "E5");
        assert_eq!(report.by_step[1].cost_usd, Some(1.0));
    }

    #[tokio::test]
    async fn an_unscoped_call_is_recorded_as_unknown() {
        let ledger = priced_ledger();
        ledger.record(call("sonnet", tokens(100, 10)));
        let report = ledger.take();
        // Metering must not depend on attribution — a bare pipeline still
        // reports its spend, it just cannot say which step.
        assert_eq!(report.by_step[0].path, "unknown");
        assert_eq!(report.calls, 1);
    }

    #[tokio::test]
    async fn nested_plans_qualify_their_step_paths() {
        let ledger = priced_ledger();
        CallSite::role("chat")
            .at("E0")
            .in_plans(&["graph_review_9000".to_string()])
            .scope(async { ledger.record(call("sonnet", tokens(10, 1))) })
            .await;
        CallSite::role("chat")
            .at("E0")
            .scope(async { ledger.record(call("sonnet", tokens(10, 1))) })
            .await;

        let report = ledger.take();
        let paths: Vec<&str> = report.by_step.iter().map(|s| s.path.as_str()).collect();
        // An inner plan's E0 must not merge with the outer plan's.
        assert!(paths.contains(&"graph_review_9000/E0"));
        assert!(paths.contains(&"E0"));
    }

    /// The test the task-local design exists for: `map` polls concurrent
    /// bodies on one task via `buffered(n)`, so attribution has to ride the
    /// future rather than any shared cursor.
    #[tokio::test]
    async fn concurrent_map_items_stay_separately_attributed() {
        use futures::stream::{self, StreamExt};

        let ledger = Arc::new(priced_ledger());
        let items = 0..6;

        stream::iter(items)
            .map(|index| {
                let ledger = ledger.clone();
                async move {
                    CallSite::role("chat")
                        .at(format!("E4f/do.{index}/agent"))
                        .scope(async {
                            // Interleave: every item yields mid-scope, so a
                            // shared-cursor implementation would cross-wire.
                            tokio::task::yield_now().await;
                            ledger.record(call("sonnet", tokens(1_000_000, 0)));
                            tokio::task::yield_now().await;
                            ledger.record(call("haiku", tokens(1_000_000, 0)));
                        })
                        .await
                }
            })
            .buffered(6)
            .collect::<Vec<_>>()
            .await;

        let report = ledger.take();
        assert_eq!(report.calls, 12);
        assert_eq!(report.by_step.len(), 6, "one group per map item");
        for step in &report.by_step {
            assert_eq!(step.calls, 2, "{} got the wrong calls", step.path);
            // $3 sonnet + $1 haiku, never two of one model.
            assert_eq!(step.cost_usd, Some(4.0), "{} priced wrong", step.path);
        }
    }

    #[test]
    fn summary_reads_at_a_glance() {
        let report = UsageReport {
            calls: 47,
            total: Usage {
                input_tokens: 1_943_210,
                output_tokens: 38_402,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
            cost_usd: Some(6.4142),
            ..Default::default()
        };
        assert_eq!(report.summary(), "47 calls · 1.9M in / 38k out · $6.41");
    }

    #[test]
    fn summary_surfaces_cache_reads_when_caching_is_working() {
        let report = UsageReport {
            calls: 1,
            total: Usage {
                input_tokens: 1_000,
                output_tokens: 50,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 40_000,
            },
            cost_usd: None,
            ..Default::default()
        };
        assert_eq!(report.summary(), "1 call · 41k in / 50 out (40k cached)");
    }
}
