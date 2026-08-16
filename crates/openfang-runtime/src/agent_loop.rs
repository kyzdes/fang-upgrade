//! Core agent execution loop.
//!
//! The agent loop handles receiving a user message, recalling relevant memories,
//! calling the LLM, executing tool calls, and saving the conversation.

use crate::auth_cooldown::{CooldownVerdict, ProviderCooldown};
use crate::context_budget::{apply_context_guard, truncate_tool_result_dynamic, ContextBudget};
use crate::context_overflow::{recover_from_overflow, RecoveryStage};
use crate::embedding::EmbeddingDriver;
use crate::kernel_handle::KernelHandle;
use crate::llm_driver::{
    CallReport, CompletionRequest, DriverConfig, LlmDriver, LlmError, StreamEvent,
};
use crate::llm_errors;
use crate::loop_guard::{LoopGuard, LoopGuardConfig, LoopGuardVerdict};
use crate::mcp::McpConnection;
use crate::tool_runner;
use crate::web_search::WebToolsContext;
use openfang_memory::session::Session;
use openfang_memory::MemorySubstrate;
use openfang_skills::registry::SkillRegistry;
use openfang_types::agent::{AgentManifest, FallbackModel};
use openfang_types::error::{OpenFangError, OpenFangResult};
use openfang_types::memory::{Memory, MemoryFilter, MemorySource};
use openfang_types::message::{
    ContentBlock, Message, MessageContent, Role, StopReason, TokenUsage,
};
use openfang_types::tool::{ToolCall, ToolDefinition};
use openfang_types::usage::LlmCall;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Maximum iterations in the agent loop before giving up.
const MAX_ITERATIONS: u32 = 50;

/// Maximum retries for rate-limited or overloaded API calls.
const MAX_RETRIES: u32 = 3;

/// Base delay for exponential backoff (milliseconds).
const BASE_RETRY_DELAY_MS: u64 = 1000;

/// Default timeout for individual tool executions (seconds).
/// Raised from 60s to 120s for browser automation and long-running builds.
/// Overridable via `OPENFANG_TOOL_TIMEOUT_SECS` env var. Set to `0` to disable
/// the timeout entirely (useful for slow local inference like vLLM on old GPUs).
const TOOL_TIMEOUT_SECS: u64 = 120;

/// Default timeout for inter-agent tool calls (seconds).
/// Agent delegation (agent_send, agent_spawn) can involve a full agent loop on the
/// target, so these need a significantly longer timeout than regular tools.
/// Overridable via `OPENFANG_AGENT_TOOL_TIMEOUT_SECS` env var. Set to `0` to
/// disable (issue #1125: slow vLLM rigs running Hands need unbounded waits).
const AGENT_TOOL_TIMEOUT_SECS: u64 = 600;

/// Parse a u64 env var, returning `None` when unset or unparseable so the
/// caller falls back to the compiled-in default.
fn env_timeout_secs(var: &str) -> Option<u64> {
    std::env::var(var).ok().and_then(|s| s.trim().parse().ok())
}

/// Returns the appropriate timeout duration for a given tool name.
/// Inter-agent calls get a longer timeout since they may trigger full agent loops.
///
/// Returns `None` when the operator opted out by setting the relevant env var
/// to `0`. In that case the tool runs with no upper bound, which is what users
/// on slow local inference (vLLM on old GPUs) want for Hands and inter-agent
/// delegation (issue #1125).
fn tool_timeout_for(tool_name: &str) -> Option<Duration> {
    let secs = match tool_name {
        "agent_send" | "agent_spawn" => {
            env_timeout_secs("OPENFANG_AGENT_TOOL_TIMEOUT_SECS").unwrap_or(AGENT_TOOL_TIMEOUT_SECS)
        }
        _ => env_timeout_secs("OPENFANG_TOOL_TIMEOUT_SECS").unwrap_or(TOOL_TIMEOUT_SECS),
    };
    if secs == 0 {
        None
    } else {
        Some(Duration::from_secs(secs))
    }
}

/// Maximum consecutive MaxTokens continuations before returning partial response.
/// Raised from 3 to 5 to allow longer-form generation.
const MAX_CONTINUATIONS: u32 = 5;

/// Default maximum message history size before auto-trimming to prevent context overflow.
/// Per-agent overrides come from `AgentManifest::max_history_messages` (issue #871).
#[allow(dead_code)]
const MAX_HISTORY_MESSAGES: usize = openfang_types::agent::DEFAULT_MAX_HISTORY_MESSAGES;

/// Detect when the LLM claims to have performed an action (sent, posted, emailed)
/// without actually calling any tools. Prevents hallucinated completions.
fn phantom_action_detected(text: &str) -> bool {
    let lower = text.to_lowercase();
    let action_verbs = ["sent ", "posted ", "emailed ", "delivered ", "forwarded "];
    let channel_refs = [
        "telegram",
        "whatsapp",
        "slack",
        "discord",
        "email",
        "channel",
        "message sent",
        "successfully sent",
        "has been sent",
    ];
    let has_action = action_verbs.iter().any(|v| lower.contains(v));
    let has_channel = channel_refs.iter().any(|c| lower.contains(c));
    has_action && has_channel
}

/// Returns true when the agent response text indicates an intentional silent completion.
/// Matches `NO_REPLY` (exact) and `[SILENT]` (case-insensitive).
fn is_silent_token(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed == "NO_REPLY" || trimmed.eq_ignore_ascii_case("[silent]")
}

/// Extra guidance injected after failed tool calls to prevent fabricated follow-up actions.
const TOOL_ERROR_GUIDANCE: &str =
    "[System: One or more tool calls failed. Failed tools did not produce usable data. Do NOT invent missing results, cite nonexistent search results, or pretend failed tools succeeded. If your next steps depend on a failed tool, either retry with a materially different approach or explain the failure to the user and stop. Do not write files, store memory, or take downstream actions based on failed tool outputs.]";

fn append_tool_error_guidance(tool_result_blocks: &mut Vec<ContentBlock>) {
    let has_tool_error = tool_result_blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolResult { is_error: true, .. }));
    if has_tool_error {
        tool_result_blocks.push(ContentBlock::Text {
            text: TOOL_ERROR_GUIDANCE.to_string(),
            provider_metadata: None,
        });
    }
}

/// Build an assistant message that preserves Thinking blocks alongside the
/// final visible text.
///
/// Issue #1098 — thinking-model state preservation.  When the LLM response
/// contains `ContentBlock::Thinking` (Anthropic extended thinking with
/// signatures, Gemini 2.5+ thoughts, OpenAI-compat reasoning_content,
/// MiniMax/Qwen inline `<think>` blocks), the prior code stored only the
/// final text via `Message::assistant(text)` — discarding all reasoning
/// state.  On the next turn the model re-derived its answer from scratch
/// and quality degraded.
///
/// This helper preserves the full block list whenever any Thinking block is
/// present, otherwise returns the legacy `Message::assistant(text)` form so
/// downstream consumers (channel formatters, JSONL mirrors, embeddings) keep
/// working without changes.
///
/// Note: we deliberately replace any visible Text blocks in `response_blocks`
/// with `final_text` so that any post-processing the agent loop applied
/// (phantom-action recovery, accumulated_text fallback, EmptyResponse guard
/// stub) is reflected in the persisted message.
fn build_assistant_message_preserving_thinking(
    response_blocks: &[ContentBlock],
    final_text: &str,
) -> Message {
    // Key on either Thinking or RedactedThinking — Anthropic/Bedrock both
    // reject extended-thinking history that drops the redacted variant, so a
    // turn that contains only RedactedThinking must still be preserved.
    let has_reasoning = response_blocks.iter().any(|b| {
        matches!(
            b,
            ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
        )
    });
    if !has_reasoning {
        return Message::assistant(final_text.to_string());
    }

    // Preserve order: Thinking / RedactedThinking blocks first (in original
    // order), then a single Text block carrying `final_text`. Tool blocks
    // aren't expected here (StopReason::EndTurn path), but copy them through
    // if present so we don't drop information.
    let mut blocks: Vec<ContentBlock> = Vec::with_capacity(response_blocks.len() + 1);
    let mut emitted_text = false;
    for b in response_blocks {
        match b {
            ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => {
                blocks.push(b.clone())
            }
            ContentBlock::Text { .. } if !emitted_text => {
                blocks.push(ContentBlock::Text {
                    text: final_text.to_string(),
                    provider_metadata: None,
                });
                emitted_text = true;
            }
            ContentBlock::Text { .. } => {
                // Drop additional text blocks — final_text already captures
                // the canonical visible message.
            }
            other => blocks.push(other.clone()),
        }
    }
    if !emitted_text && !final_text.is_empty() {
        blocks.push(ContentBlock::Text {
            text: final_text.to_string(),
            provider_metadata: None,
        });
    }

    Message::assistant_with_blocks(blocks)
}

/// Strip a provider prefix from a model ID before sending to the API.
///
/// Many models are stored as `provider/org/model` (e.g. `openrouter/google/gemini-2.5-flash`)
/// but the upstream API expects just `org/model` (e.g. `google/gemini-2.5-flash`).
pub fn strip_provider_prefix(model: &str, provider: &str) -> String {
    let slash_prefix = format!("{}/", provider);
    let colon_prefix = format!("{}:", provider);
    if model.starts_with(&slash_prefix) {
        model[slash_prefix.len()..].to_string()
    } else if model.starts_with(&colon_prefix) {
        model[colon_prefix.len()..].to_string()
    } else {
        model.to_string()
    }
}

/// Default context window size (tokens) for token-based trimming.
const DEFAULT_CONTEXT_WINDOW: usize = 200_000;

/// Agent lifecycle phase within the execution loop.
/// Used for UX indicators (typing, reactions) without coupling to channel types.
#[derive(Debug, Clone, PartialEq)]
pub enum LoopPhase {
    /// Agent is calling the LLM.
    Thinking,
    /// Agent is executing a tool.
    ToolUse { tool_name: String },
    /// Agent is streaming tokens.
    Streaming,
    /// Agent finished successfully.
    Done,
    /// Agent encountered an error.
    Error,
}

/// Callback for agent lifecycle phase changes.
/// Implementations should be non-blocking (fire-and-forget) to avoid slowing the loop.
pub type PhaseCallback = Arc<dyn Fn(LoopPhase) + Send + Sync>;

/// Result of an agent loop execution.
#[derive(Debug)]
pub struct AgentLoopResult {
    /// The final text response from the agent.
    pub response: String,
    /// Total token usage across all LLM calls.
    pub total_usage: TokenUsage,
    /// Number of iterations the loop ran.
    pub iterations: u32,
    /// Estimated cost in USD (populated by the kernel after the loop returns).
    pub cost_usd: Option<f64>,
    /// True when the agent intentionally chose not to reply (NO_REPLY token or [[silent]]).
    pub silent: bool,
    /// Reply directives extracted from the agent's response.
    pub directives: openfang_types::message::ReplyDirectives,
    /// One entry per LLM call of this turn, in order — who served it and what
    /// it consumed. The unit of accounting; every usage surface is a projection
    /// of this array.
    pub calls: Vec<LlmCall>,
}

/// Close out a turn's call log: attribute tool calls and hand the vector over.
///
/// `tool_calls` is set per call by `set_last_tool_calls` once the response is final, so
/// this only hands the accumulated rows over.
///
/// It used to attribute 1 to every call but the last, keeping the turn total at exactly
/// `iterations - 1` so no shipped number moved. That was accurate only when a model emits
/// one tool call per turn. Measured on a model emitting three in a single response: the
/// turn recorded 1, and nine executed calls across a turn recorded 3. A metric named
/// `tool_calls` that counts iterations is worse than one that counts nothing, because the
/// number looks plausible.
fn finish_calls(calls: &mut Vec<LlmCall>) -> Vec<LlmCall> {
    std::mem::take(calls)
}

/// How many tool calls each section of a max-iterations summary names one by
/// one before the rest are elided into a count. A runaway loop repeats itself;
/// twenty lines show its shape.
const MAX_ITER_SUMMARY_CALLS: usize = 20;

/// Per-call argument budget in that listing, in characters. Long enough to
/// carry the arguments that identify the call (a path, a key, a query),
/// short enough that fifty of them cannot blow up a response body.
const MAX_ITER_SUMMARY_ARG_CHARS: usize = 300;

/// Truncate on a character boundary, marking that something was cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// What became of one tool call the model asked for.
///
/// Recorded where the decision is taken, not reconstructed from the transcript
/// afterwards: a `ToolUse` block sits in the session whether or not the call
/// ever reached a tool, so reading the session back cannot tell the two apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolCallFate {
    /// The tool ran and reported success.
    Completed,
    /// The tool ran, or refused to, and reported an error — a failure, a
    /// timeout, a denied approval, a capability it does not hold.
    Errored,
    /// The call never reached the tool. The word is what stopped it.
    Blocked(&'static str),
}

/// One tool call of this turn and what became of it.
#[derive(Debug, Clone)]
struct TurnToolCall {
    name: String,
    args: String,
    fate: ToolCallFate,
}

impl TurnToolCall {
    fn new(name: &str, args: &serde_json::Value, fate: ToolCallFate) -> Self {
        Self {
            name: name.to_string(),
            args: args.to_string(),
            fate,
        }
    }
}

/// How many of a turn's recorded tool calls ran, returned an error and were
/// stopped before they ran, in that order. What the log line and the
/// AgentLoopEnd hook report, so neither has to say "tool calls" and leave the
/// reader to guess whether the blocked ones are in the number.
fn fate_counts(calls: &[TurnToolCall]) -> (usize, usize, usize) {
    let mut counts = (0usize, 0usize, 0usize);
    for call in calls {
        match call.fate {
            ToolCallFate::Completed => counts.0 += 1,
            ToolCallFate::Errored => counts.1 += 1,
            ToolCallFate::Blocked(_) => counts.2 += 1,
        }
    }
    counts
}

/// Render one section of the max-iterations listing, or nothing when no call
/// had that fate. The count in the heading is the number of calls in the
/// section, not the number of lines printed under it.
fn push_fate_section(out: &mut String, heading: &str, calls: &[&TurnToolCall]) {
    if calls.is_empty() {
        return;
    }
    out.push_str(&format!("\n\n{heading} ({}):", calls.len()));
    for (i, call) in calls.iter().take(MAX_ITER_SUMMARY_CALLS).enumerate() {
        let reason = match call.fate {
            ToolCallFate::Blocked(by) => format!(" — {by}"),
            _ => String::new(),
        };
        out.push_str(&format!(
            "\n  {}. {}{} {}",
            i + 1,
            call.name,
            reason,
            truncate_chars(&call.args, MAX_ITER_SUMMARY_ARG_CHARS)
        ));
    }
    if calls.len() > MAX_ITER_SUMMARY_CALLS {
        out.push_str(&format!(
            "\n  … and {} more",
            calls.len() - MAX_ITER_SUMMARY_CALLS
        ));
    }
}

/// The notice appended to a turn that ran out of iterations: which exit door
/// the loop left by, and what became of each tool call on the way.
///
/// Every line is built from a `TurnToolCall` recorded at the moment the loop
/// decided that call's fate, so a call the loop guard stopped is listed as
/// stopped and is never counted among the calls that ran. The notice makes no
/// statement about calls it has no record of.
///
/// It keeps the words "Max iterations exceeded (N)" because that names which
/// exit door the loop left by — the circuit breaker says something else — and
/// because the operator's next move (raise the limit) has to be spelled out
/// somewhere the caller can see.
fn max_iterations_notice(max_iterations: u32, tool_calls: &[TurnToolCall]) -> String {
    let mut out = format!(
        "[Turn incomplete — Max iterations exceeded ({max_iterations}). The agent \
         stopped here without finishing. Configure a higher limit in agent.toml \
         under [autonomous] max_iterations.]"
    );

    let completed: Vec<&TurnToolCall> = tool_calls
        .iter()
        .filter(|c| matches!(c.fate, ToolCallFate::Completed))
        .collect();
    let errored: Vec<&TurnToolCall> = tool_calls
        .iter()
        .filter(|c| matches!(c.fate, ToolCallFate::Errored))
        .collect();
    let blocked: Vec<&TurnToolCall> = tool_calls
        .iter()
        .filter(|c| matches!(c.fate, ToolCallFate::Blocked(_)))
        .collect();

    if completed.is_empty() && errored.is_empty() && blocked.is_empty() {
        out.push_str("\n\nNo tool calls were executed in this turn.");
        return out;
    }

    push_fate_section(&mut out, "Tool calls that ran and succeeded", &completed);
    push_fate_section(&mut out, "Tool calls that returned an error", &errored);
    push_fate_section(&mut out, "Tool calls stopped before they ran", &blocked);
    out
}

/// The full max-iterations response: the text the turn produced, then the
/// notice. The notice goes last so that the streaming loop — whose caller has
/// already received `accumulated_text` as deltas — can emit exactly the notice
/// without repeating a word of the text.
///
/// It does NOT make the two identical: the deltas went out one per iteration
/// with no separator, and this joins the same texts with "\n\n". Over 50
/// talkative iterations that measured 2 910 characters at the client against
/// 3 008 returned.
fn max_iterations_summary(accumulated_text: &str, notice: &str) -> String {
    let text = accumulated_text.trim();
    if text.is_empty() {
        return notice.to_string();
    }
    format!("{text}\n\n{notice}")
}

/// The prefix every "this turn produced no text" failure carries.
///
/// It exists so a consumer can recognise the failure without matching on the
/// rest of the sentence, which is assembled from what happened and therefore
/// varies. `openfang-api`'s WebSocket error path uses it to skip
/// `llm_errors::classify_error`: that classifier is a substring matcher over
/// provider error bodies, and a message it does not recognise comes back out
/// of it labelled `Format` — "Request failed: …". Nothing failed to be
/// requested here; the provider answered, with nothing.
pub const NO_TEXT_FAILURE_PREFIX: &str = "The turn produced no text:";

/// Why a turn reached the end with nothing to say. One variant per exit that
/// can get there, because the two are not the same event and the sentence the
/// caller reads is built from this, not chosen from a template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoText {
    /// The provider's final message came back with `finish_reason: stop` (or a
    /// stop sequence), carrying no text, no tool calls and no content of any
    /// kind; the one-shot retry did not change that; and no text had been
    /// accumulated during earlier tool_use iterations. The response was
    /// *structurally* valid, which is why it never became an `LlmError` in the
    /// driver the way an empty `choices` array does (`drivers/openai.rs`, "No
    /// choices in response"). It was simply empty.
    EmptyFinalMessage,
    /// The provider stopped at `finish_reason: length` `continuations` times in
    /// a row — the whole continuation budget (`MAX_CONTINUATIONS`) — and not
    /// one of those responses carried a character of text.
    TruncatedWithNoText { continuations: u32 },
}

/// FANG-13 — the turn ended with nothing to say, and that is a failed turn.
///
/// Both loops used to substitute a sentence of their own at each of the two
/// exits below and return `Ok`, so the caller got HTTP 200 with the runtime's
/// words sitting in the field where the model's answer belongs — and when tools
/// had run, those words asserted the task had COMPLETED on a turn where the
/// provider had said nothing.
///
/// What each caller-facing surface does with the resulting `Err` is not
/// uniform, and is measured rather than assumed: `tests/fang/FANG-13.sh`
/// drives this shape through all five of them on a live stand and prints what
/// each one did. Measured there, on this patch:
///
/// * `POST /api/agents/{id}/message` — HTTP 500 carrying this text.
/// * `POST /v1/chat/completions` (`stream:false`) — HTTP 500.
/// * `GET  /api/agents/{id}/ws` — `{"type":"error"}` carrying this text
///   verbatim (see `ws.rs`, `classify_streaming_error`).
/// * `POST /api/agents/{id}/message/stream` — no failure reported: the SSE
///   ends after its per-call `done` events, exactly as it ends a turn that
///   succeeded.
/// * `POST /v1/chat/completions` (`stream:true`) — no failure reported: the
///   stream ends with `finish_reason: "stop"` and `[DONE]`.
///
/// The last two both drop the loop's join handle (`routes.rs`
/// `send_message_stream`, `openai_compat.rs` `stream_response`: `let (rx,
/// _handle)`), so no loop failure of any kind has ever reached them —
/// max-iterations included. That gap is named here and by the repro; it is not
/// closed here.
///
/// One function for both loops on purpose: the streaming and non-streaming
/// paths are copies of each other, and this is precisely the kind of text that
/// drifts apart between them.
fn no_text_failure(
    agent: &str,
    iterations: u32,
    usage: &TokenUsage,
    any_tools_executed: bool,
    streaming: bool,
    cause: NoText,
) -> OpenFangError {
    let stream = if streaming { " streamed" } else { "" };
    // No guesses about *why* in this text. The sentence it replaces offered
    // three ("overloaded, the context is too large, or the API key lacks
    // credits") and the runtime knew none of them to be true.
    //
    // There is a second reason to keep the wording clear of those words, and it
    // is a hazard rather than something already observed: "overloaded" is one of
    // the patterns `llm_errors::classify_error` matches on, so a guess phrased
    // that way is liable to be re-read downstream as a confident "Provider
    // overloaded". The old sentence was never seen doing that — it did not reach
    // a WebSocket client at all — so this is a trap avoided, not a bug fixed.
    // "token limit" carries the same risk: that table reads it as a
    // context-window overflow.
    let what = match cause {
        NoText::EmptyFinalMessage => format!(
            "after {iterations} iteration(s) the final{stream} message carried no text, \
             no tool calls and no content"
        ),
        // Says what the caller counted, and stops there. An earlier wording
        // added "and none of those responses carried any text" — which nothing
        // here observes: the loop counts length-stops, it does not record
        // whether each truncated response was empty. That was a new unverified
        // sentence inside the very change that removes unverified sentences.
        NoText::TruncatedWithNoText { continuations } => format!(
            "the provider stopped at finish_reason=length {continuations} times in a row \
             across {iterations} iteration(s), and the turn ended with no{stream} text to \
             return"
        ),
    };
    let tools_note = if any_tools_executed {
        " Tools executed earlier in this turn did run and their effects stand, \
         but the provider never summarised them."
    } else {
        ""
    };
    OpenFangError::LlmDriver(format!(
        "{NO_TEXT_FAILURE_PREFIX} {what} ({input} in / {output} out tokens).{tools_note} \
         (agent: {agent})",
        input = usage.input_tokens,
        output = usage.output_tokens,
    ))
}

/// Record how many tool calls the just-finished LLM response actually asked for.
///
/// Called after text-based recovery, because a model that emits `<function=…>` in prose has
/// an empty `tool_calls` at the moment the call is recorded and a populated one afterwards.
fn set_last_tool_calls(calls: &mut [LlmCall], n: usize) {
    if let Some(last) = calls.last_mut() {
        last.tool_calls = n as u32;
    }
}

/// Build the accounting row for a finished LLM call.
///
/// When nothing was substituted the accounting name comes from the manifest —
/// unstripped, exactly as written today — so existing `by-model` rows keep
/// their key.
fn record_call(
    calls: &mut Vec<LlmCall>,
    iteration: u32,
    model: &openfang_types::agent::ModelConfig,
    report: &CallReport,
    usage: TokenUsage,
) {
    calls.push(LlmCall {
        n: iteration,
        provider: report
            .provider
            .clone()
            .unwrap_or_else(|| model.provider.clone()),
        model: report
            .substituted
            .clone()
            .unwrap_or_else(|| model.model.clone()),
        requested: report.substituted.as_ref().map(|_| model.model.clone()),
        reason: report.reason.clone(),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        tool_calls: 0,
        cost_usd: 0.0,
    });
}

/// Build the user-turn message, combining text with any image content blocks.
///
/// When the turn has both text and image blocks the text is emitted as the
/// first block followed by the images so the LLM sees the full multimodal
/// turn. When only one is present the single-mode representation is used.
fn build_user_turn_message(user_message: &str, blocks: Option<Vec<ContentBlock>>) -> Message {
    match blocks {
        Some(blocks) if !blocks.is_empty() => {
            if user_message.trim().is_empty() {
                Message::user_with_blocks(blocks)
            } else {
                let mut combined = Vec::with_capacity(blocks.len() + 1);
                combined.push(ContentBlock::Text {
                    text: user_message.to_string(),
                    provider_metadata: None,
                });
                combined.extend(blocks);
                Message::user_with_blocks(combined)
            }
        }
        _ => Message::user(user_message),
    }
}

/// Run the agent execution loop for a single user message.
///
/// This is the core of OpenFang: it loads session context, recalls memories,
/// runs the LLM in a tool-use loop, and saves the updated session.
#[allow(clippy::too_many_arguments)]
pub async fn run_agent_loop(
    manifest: &AgentManifest,
    user_message: &str,
    session: &mut Session,
    memory: &MemorySubstrate,
    driver: Arc<dyn LlmDriver>,
    available_tools: &[ToolDefinition],
    kernel: Option<Arc<dyn KernelHandle>>,
    skill_registry: Option<&SkillRegistry>,
    mcp_connections: Option<&tokio::sync::Mutex<Vec<McpConnection>>>,
    web_ctx: Option<&WebToolsContext>,
    browser_ctx: Option<&crate::browser::BrowserManager>,
    embedding_driver: Option<&(dyn EmbeddingDriver + Send + Sync)>,
    workspace_root: Option<&Path>,
    on_phase: Option<&PhaseCallback>,
    media_engine: Option<&crate::media_understanding::MediaEngine>,
    tts_engine: Option<&crate::tts::TtsEngine>,
    docker_config: Option<&openfang_types::config::DockerSandboxConfig>,
    hooks: Option<&crate::hooks::HookRegistry>,
    context_window_tokens: Option<usize>,
    process_manager: Option<&crate::process_manager::ProcessManager>,
    user_content_blocks: Option<Vec<ContentBlock>>,
) -> OpenFangResult<AgentLoopResult> {
    info!(agent = %manifest.name, "Starting agent loop");

    // Extract hand-allowed env vars from manifest metadata (set by kernel for hand settings)
    let hand_allowed_env: Vec<String> = manifest
        .metadata
        .get("hand_allowed_env")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    // Recall relevant memories — prefer vector similarity search when embedding driver is available
    let memories = if let Some(emb) = embedding_driver {
        match emb.embed_one(user_message).await {
            Ok(query_vec) => {
                debug!("Using vector recall (dims={})", query_vec.len());
                memory
                    .recall_with_embedding_async(
                        user_message,
                        5,
                        Some(MemoryFilter {
                            agent_id: Some(session.agent_id),
                            ..Default::default()
                        }),
                        Some(&query_vec),
                    )
                    .await
                    .unwrap_or_default()
            }
            Err(e) => {
                warn!("Embedding recall failed, falling back to text search: {e}");
                memory
                    .recall(
                        user_message,
                        5,
                        Some(MemoryFilter {
                            agent_id: Some(session.agent_id),
                            ..Default::default()
                        }),
                    )
                    .await
                    .unwrap_or_default()
            }
        }
    } else {
        memory
            .recall(
                user_message,
                5,
                Some(MemoryFilter {
                    agent_id: Some(session.agent_id),
                    ..Default::default()
                }),
            )
            .await
            .unwrap_or_default()
    };

    // Fire BeforePromptBuild hook
    let agent_id_str = session.agent_id.0.to_string();
    if let Some(hook_reg) = hooks {
        let ctx = crate::hooks::HookContext {
            agent_name: &manifest.name,
            agent_id: agent_id_str.as_str(),
            event: openfang_types::agent::HookEvent::BeforePromptBuild,
            data: serde_json::json!({
                "system_prompt": &manifest.model.system_prompt,
                "user_message": user_message,
            }),
        };
        let _ = hook_reg.fire(&ctx);
    }

    // Build the system prompt — base prompt comes from kernel (prompt_builder),
    // we append recalled memories here since they are resolved at loop time.
    let mut system_prompt = manifest.model.system_prompt.clone();
    if !memories.is_empty() {
        let mem_pairs: Vec<(String, String)> = memories
            .iter()
            .map(|m| (String::new(), m.content.clone()))
            .collect();
        system_prompt.push_str("\n\n");
        system_prompt.push_str(&crate::prompt_builder::build_memory_section(&mem_pairs));
    }

    // Add the user message to session history.
    // When content blocks are provided (e.g. text + image from a channel),
    // combine them with the user text so the LLM sees the full multimodal turn.
    session
        .messages
        .push(build_user_turn_message(user_message, user_content_blocks));

    // Build the messages for the LLM, filtering system messages
    // System prompt goes into the separate `system` field.
    // NOTE: We build llm_messages BEFORE stripping images so the LLM
    // sees the full image data for the current turn.
    let llm_messages: Vec<Message> = session
        .messages
        .iter()
        .filter(|m| m.role != Role::System)
        .cloned()
        .collect();

    // Strip Image blocks from session to prevent base64 bloat.
    // The LLM already received them via llm_messages above.
    for msg in session.messages.iter_mut() {
        if let MessageContent::Blocks(blocks) = &mut msg.content {
            let had_images = blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. }));
            if had_images {
                blocks.retain(|b| !matches!(b, ContentBlock::Image { .. }));
                if blocks.is_empty() {
                    blocks.push(ContentBlock::Text {
                        text: "[Image processed]".to_string(),
                        provider_metadata: None,
                    });
                }
            }
        }
    }

    // Validate and repair session history (drop orphans, merge consecutive)
    let mut messages = crate::session_repair::validate_and_repair(&llm_messages);

    // Inject canonical context as the first user message (not in system prompt)
    // to keep the system prompt stable across turns for provider prompt caching.
    if let Some(cc_msg) = manifest
        .metadata
        .get("canonical_context_msg")
        .and_then(|v| v.as_str())
    {
        if !cc_msg.is_empty() {
            messages.insert(0, Message::user(cc_msg));
        }
    }

    let mut total_usage = TokenUsage::default();
    // One row per LLM call of this turn — the unit of accounting.
    let mut calls: Vec<LlmCall> = Vec::new();
    let final_response;
    // Accumulate text from intermediate iterations (tool_use turns may include text
    // alongside tool calls — this text would otherwise be lost when the final
    // EndTurn iteration has empty text).
    let mut accumulated_text = String::new();

    // Safety valve: trim excessively long message histories to prevent context overflow.
    // The full compaction system handles sophisticated summarization, but this prevents
    // the catastrophic case where 200+ messages cause instant context overflow.
    // Per-agent cap: manifest override -> runtime default (issue #871).
    let max_history = manifest.effective_max_history_messages();
    if messages.len() > max_history {
        let trim_count = messages.len() - max_history;
        warn!(
            agent = %manifest.name,
            total_messages = messages.len(),
            trimming = trim_count,
            max_history = max_history,
            "Trimming old messages to prevent context overflow"
        );
        messages.drain(..trim_count);
        // Re-validate after trimming: the drain may have split a ToolUse/ToolResult
        // pair across the cut boundary, leaving orphaned blocks that cause the LLM
        // to return empty responses (input_tokens=0).
        messages = crate::session_repair::validate_and_repair(&messages);
        // Ensure history starts with a user turn: trimming may have left an
        // assistant turn at position 0, which strict providers (e.g. Gemini)
        // reject with INVALID_ARGUMENT on function-call turns.
        messages = crate::session_repair::ensure_starts_with_user(messages);
    }

    // Use autonomous config max_iterations if set, else default
    let max_iterations = manifest
        .autonomous
        .as_ref()
        .map(|a| a.max_iterations)
        .unwrap_or(MAX_ITERATIONS);

    // Initialize loop guard — scale circuit breaker for autonomous agents
    let loop_guard_config = {
        let mut cfg = LoopGuardConfig::default();
        if max_iterations > cfg.global_circuit_breaker {
            cfg.global_circuit_breaker = max_iterations * 3;
        }
        cfg
    };
    let mut loop_guard = LoopGuard::new(loop_guard_config);
    let mut consecutive_max_tokens: u32 = 0;

    // Build context budget from model's actual context window (or fallback to default)
    let ctx_window = context_window_tokens.unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let context_budget = ContextBudget::new(ctx_window);
    let mut any_tools_executed = false;
    // What became of each tool call this turn, recorded as each fate is
    // decided. Read at the max-iterations exit; nothing else consumes it.
    let mut turn_tool_calls: Vec<TurnToolCall> = Vec::new();

    for iteration in 0..max_iterations {
        debug!(iteration, "Agent loop iteration");

        // Context overflow recovery pipeline (replaces emergency_trim_messages)
        let recovery =
            recover_from_overflow(&mut messages, &system_prompt, available_tools, ctx_window);
        if recovery == RecoveryStage::FinalError {
            warn!("Context overflow unrecoverable — suggest /reset or /compact");
        }

        // Re-validate tool_call/tool_result pairing after overflow drains
        // which may have broken assistant→tool ordering invariants.
        if recovery != RecoveryStage::None {
            messages = crate::session_repair::validate_and_repair(&messages);
            // Ensure history starts with a user turn after overflow recovery.
            messages = crate::session_repair::ensure_starts_with_user(messages);
        }

        // Context guard: compact oversized tool results before LLM call
        apply_context_guard(&mut messages, &context_budget, available_tools);

        // Strip provider prefix: "openrouter/google/gemini-2.5-flash" → "google/gemini-2.5-flash"
        let api_model = strip_provider_prefix(&manifest.model.model, &manifest.model.provider);

        let request = CompletionRequest {
            model: api_model,
            messages: messages.clone(),
            tools: available_tools.to_vec(),
            max_tokens: manifest.model.max_tokens,
            temperature: manifest.model.temperature,
            system: Some(system_prompt.clone()),
            thinking: None,
        };

        // Notify phase: Thinking
        if let Some(cb) = on_phase {
            cb(LoopPhase::Thinking);
        }

        // Stamp last_active before the (potentially long) LLM call so the
        // heartbeat monitor doesn't flag us as unresponsive mid-iteration.
        if let Some(k) = &kernel {
            k.touch_agent(&agent_id_str);
        }

        // Call LLM with retry, error classification, and circuit breaker
        let provider_name = manifest.model.provider.as_str();
        let (mut response, report) = call_with_retry(
            &*driver,
            request,
            Some(provider_name),
            None,
            &manifest.fallback_models,
        )
        .await?;

        total_usage.input_tokens += response.usage.input_tokens;
        total_usage.output_tokens += response.usage.output_tokens;
        record_call(
            &mut calls,
            iteration,
            &manifest.model,
            &report,
            response.usage,
        );

        // Recover tool calls output as text by models that don't use the tool_calls API field
        // (e.g. Groq/Llama, DeepSeek emit `<function=name>{json}</function>` in text)
        if matches!(
            response.stop_reason,
            StopReason::EndTurn | StopReason::StopSequence
        ) && response.tool_calls.is_empty()
        {
            let recovered = recover_text_tool_calls(&response.text(), available_tools);
            if !recovered.is_empty() {
                info!(
                    count = recovered.len(),
                    "Recovered text-based tool calls → promoting to ToolUse"
                );
                response.tool_calls = recovered;
                response.stop_reason = StopReason::ToolUse;
                // Build ToolUse content blocks from recovered calls
                let mut new_blocks: Vec<ContentBlock> = Vec::new();
                for tc in &response.tool_calls {
                    new_blocks.push(ContentBlock::ToolUse {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        input: tc.input.clone(),
                        provider_metadata: None,
                    });
                }
                response.content = new_blocks;
            }
        }
        set_last_tool_calls(&mut calls, response.tool_calls.len());

        match response.stop_reason {
            StopReason::EndTurn | StopReason::StopSequence => {
                // LLM is done — extract text and save
                let text = response.text();

                // Parse reply directives from the response text
                let (cleaned_text, parsed_directives) =
                    crate::reply_directives::parse_directives(&text);
                let text = cleaned_text;

                // NO_REPLY / [SILENT]: agent intentionally chose not to reply.
                // [SILENT] must not be stored literally — it reinforces silence in future turns.
                if is_silent_token(&text) || parsed_directives.silent {
                    debug!(agent = %manifest.name, "Agent chose NO_REPLY/silent — silent completion");
                    session
                        .messages
                        .push(Message::assistant("[no reply needed]".to_string()));
                    memory
                        .save_session_async(session)
                        .await
                        .map_err(|e| OpenFangError::Memory(e.to_string()))?;
                    return Ok(AgentLoopResult {
                        response: String::new(),
                        total_usage,
                        iterations: iteration + 1,
                        cost_usd: None,
                        silent: true,
                        directives: openfang_types::message::ReplyDirectives {
                            reply_to: parsed_directives.reply_to,
                            current_thread: parsed_directives.current_thread,
                            silent: true,
                        },
                        calls: finish_calls(&mut calls),
                    });
                }

                // One-shot retry: if the LLM returns empty text with no tool use,
                // try once more before accepting the empty result.
                // Triggers on first call OR when input_tokens=0 (silently failed request).
                if text.trim().is_empty()
                    && response.tool_calls.is_empty()
                    && !response.has_any_content()
                {
                    let is_silent_failure =
                        response.usage.input_tokens == 0 && response.usage.output_tokens == 0;
                    if iteration == 0 || is_silent_failure {
                        warn!(
                            agent = %manifest.name,
                            iteration,
                            input_tokens = response.usage.input_tokens,
                            output_tokens = response.usage.output_tokens,
                            silent_failure = is_silent_failure,
                            "Empty response, retrying once"
                        );
                        // Re-validate messages before retry — the history may have
                        // broken tool_use/tool_result pairs that caused the failure.
                        if is_silent_failure {
                            messages = crate::session_repair::validate_and_repair(&messages);
                        }
                        messages.push(Message::assistant("[no response]".to_string()));
                        messages.push(Message::user("Please provide your response.".to_string()));
                        continue;
                    }
                }

                // Guard against empty response — covers both iteration 0 and post-tool cycles.
                // Use accumulated_text from intermediate tool_use iterations as fallback.
                let text = if text.trim().is_empty() {
                    if !accumulated_text.is_empty() {
                        debug!(
                            agent = %manifest.name,
                            accumulated_len = accumulated_text.len(),
                            "Using accumulated text from intermediate tool_use iterations"
                        );
                        accumulated_text.clone()
                    } else {
                        // FANG-13: nothing was said, by the provider or by any
                        // earlier iteration of this turn. Fail the turn instead
                        // of writing an answer on the model's behalf — see
                        // `no_text_failure`. Same exit shape as the
                        // max-iterations failure below: persist what the turn
                        // did accomplish (the tool_use/tool_result pairs are
                        // already in `session`), close the loop out through the
                        // hook, then return the error.
                        warn!(
                            agent = %manifest.name,
                            iteration,
                            input_tokens = total_usage.input_tokens,
                            output_tokens = total_usage.output_tokens,
                            messages_count = messages.len(),
                            any_tools_executed,
                            "Empty response from LLM — failing the turn"
                        );
                        if let Err(e) = memory.save_session_async(session).await {
                            warn!("Failed to save session on empty response: {e}");
                        }
                        if let Some(hook_reg) = hooks {
                            let ctx = crate::hooks::HookContext {
                                agent_name: &manifest.name,
                                agent_id: agent_id_str.as_str(),
                                event: openfang_types::agent::HookEvent::AgentLoopEnd,
                                data: serde_json::json!({
                                    "reason": "empty_response",
                                    "iterations": iteration + 1,
                                    "any_tools_executed": any_tools_executed,
                                }),
                            };
                            let _ = hook_reg.fire(&ctx);
                        }
                        return Err(no_text_failure(
                            &manifest.name,
                            iteration + 1,
                            &total_usage,
                            any_tools_executed,
                            false,
                            NoText::EmptyFinalMessage,
                        ));
                    }
                } else {
                    text
                };
                // Phantom action detection: if the LLM claims it performed a
                // channel action (send, post, email, etc.) but never actually
                // called the corresponding tool, re-prompt once to force real
                // tool usage instead of hallucinated completion.
                let text = if !any_tools_executed
                    && iteration == 0
                    && phantom_action_detected(&text)
                {
                    warn!(agent = %manifest.name, "Phantom action detected — re-prompting for real tool use");
                    messages.push(Message::assistant(text));
                    messages.push(Message::user(
                        "[System: You claimed to perform an action but did not call any tools. \
                         You must use the appropriate tool (e.g., channel_send, web_fetch, file_write) \
                         to actually perform the action. Do not claim completion without executing tools.]"
                    ));
                    continue;
                } else {
                    text
                };

                final_response = text.clone();
                // Issue #1098: persist Thinking blocks alongside the text so
                // reasoning models retain state across turns.  When the
                // response carries any Thinking content (Anthropic extended
                // thinking, Gemini 2.5 thought signatures, DeepSeek-R1/Qwen3
                // `reasoning_content`, MiniMax inline `<think>`), save the
                // full content blocks; otherwise fall back to the legacy
                // Text shape so existing sessions/snapshots stay readable.
                let assistant_msg =
                    build_assistant_message_preserving_thinking(&response.content, &text);
                session.messages.push(assistant_msg);

                // Prune NO_REPLY heartbeat turns to save context budget
                crate::session_repair::prune_heartbeat_turns(&mut session.messages, 10);

                // Save session
                memory
                    .save_session_async(session)
                    .await
                    .map_err(|e| OpenFangError::Memory(e.to_string()))?;

                // Remember this interaction (with embedding if available)
                let interaction_text = format!(
                    "User asked: {}\nI responded: {}",
                    user_message, final_response
                );
                if let Some(emb) = embedding_driver {
                    match emb.embed_one(&interaction_text).await {
                        Ok(vec) => {
                            let _ = memory
                                .remember_with_embedding_async(
                                    session.agent_id,
                                    &interaction_text,
                                    MemorySource::Conversation,
                                    "episodic",
                                    HashMap::new(),
                                    Some(&vec),
                                )
                                .await;
                        }
                        Err(e) => {
                            warn!("Embedding for remember failed: {e}");
                            let _ = memory
                                .remember(
                                    session.agent_id,
                                    &interaction_text,
                                    MemorySource::Conversation,
                                    "episodic",
                                    HashMap::new(),
                                )
                                .await;
                        }
                    }
                } else {
                    let _ = memory
                        .remember(
                            session.agent_id,
                            &interaction_text,
                            MemorySource::Conversation,
                            "episodic",
                            HashMap::new(),
                        )
                        .await;
                }

                // Notify phase: Done
                if let Some(cb) = on_phase {
                    cb(LoopPhase::Done);
                }

                info!(
                    agent = %manifest.name,
                    iterations = iteration + 1,
                    tokens = total_usage.total(),
                    "Agent loop completed"
                );

                // Fire AgentLoopEnd hook
                if let Some(hook_reg) = hooks {
                    let ctx = crate::hooks::HookContext {
                        agent_name: &manifest.name,
                        agent_id: agent_id_str.as_str(),
                        event: openfang_types::agent::HookEvent::AgentLoopEnd,
                        data: serde_json::json!({
                            "iterations": iteration + 1,
                            "response_length": final_response.len(),
                        }),
                    };
                    let _ = hook_reg.fire(&ctx);
                }

                return Ok(AgentLoopResult {
                    response: final_response,
                    total_usage,
                    iterations: iteration + 1,
                    cost_usd: None,
                    silent: false,
                    directives: Default::default(),
                    calls: finish_calls(&mut calls),
                });
            }
            StopReason::ToolUse => {
                // Reset MaxTokens continuation counter on tool use
                consecutive_max_tokens = 0;
                any_tools_executed = true;

                // Capture any text content from this tool_use turn — the LLM may
                // produce text alongside tool calls (e.g., a message to the user
                // before calling memory_store). Without this, the text is lost if
                // the next iteration returns EndTurn with empty text.
                let intermediate_text = response.text();
                if !intermediate_text.trim().is_empty() {
                    if !accumulated_text.is_empty() {
                        accumulated_text.push_str("\n\n");
                    }
                    accumulated_text.push_str(intermediate_text.trim());
                }

                // Execute tool calls
                let assistant_blocks = response.content.clone();

                // Add assistant message with tool use blocks
                session.messages.push(Message {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(assistant_blocks.clone()),
                    ..Default::default()
                });
                messages.push(Message {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(assistant_blocks),
                    ..Default::default()
                });

                // Build allowed tool names list for capability enforcement
                let allowed_tool_names: Vec<String> =
                    available_tools.iter().map(|t| t.name.clone()).collect();
                let caller_id_str = session.agent_id.to_string();

                // Execute each tool call with loop guard, timeout, and truncation
                let mut tool_result_blocks = Vec::new();
                for tool_call in deduplicate_tool_calls(&response) {
                    // Loop guard check
                    let verdict = loop_guard.check(&tool_call.name, &tool_call.input);
                    match &verdict {
                        LoopGuardVerdict::CircuitBreak(msg) => {
                            warn!(tool = %tool_call.name, "Circuit breaker triggered");
                            // Save session before bailing
                            if let Err(e) = memory.save_session_async(session).await {
                                warn!("Failed to save session on circuit break: {e}");
                            }
                            // Fire AgentLoopEnd hook on circuit break
                            if let Some(hook_reg) = hooks {
                                let ctx = crate::hooks::HookContext {
                                    agent_name: &manifest.name,
                                    agent_id: agent_id_str.as_str(),
                                    event: openfang_types::agent::HookEvent::AgentLoopEnd,
                                    data: serde_json::json!({
                                        "reason": "circuit_break",
                                        "error": msg.as_str(),
                                    }),
                                };
                                let _ = hook_reg.fire(&ctx);
                            }
                            return Err(OpenFangError::Internal(msg.clone()));
                        }
                        LoopGuardVerdict::Block(msg) => {
                            warn!(tool = %tool_call.name, "Tool call blocked by loop guard");
                            turn_tool_calls.push(TurnToolCall::new(
                                &tool_call.name,
                                &tool_call.input,
                                ToolCallFate::Blocked("stopped by the loop guard"),
                            ));
                            tool_result_blocks.push(ContentBlock::ToolResult {
                                tool_use_id: tool_call.id.clone(),
                                tool_name: tool_call.name.clone(),
                                content: msg.clone(),
                                is_error: true,
                            });
                            continue;
                        }
                        _ => {} // Allow or Warn — proceed with execution
                    }

                    debug!(tool = %tool_call.name, id = %tool_call.id, "Executing tool");

                    // Notify phase: ToolUse
                    if let Some(cb) = on_phase {
                        let sanitized: String = tool_call
                            .name
                            .chars()
                            .filter(|c| !c.is_control())
                            .take(64)
                            .collect();
                        cb(LoopPhase::ToolUse {
                            tool_name: sanitized,
                        });
                    }

                    // Fire BeforeToolCall hook (can block execution)
                    if let Some(hook_reg) = hooks {
                        let ctx = crate::hooks::HookContext {
                            agent_name: &manifest.name,
                            agent_id: &caller_id_str,
                            event: openfang_types::agent::HookEvent::BeforeToolCall,
                            data: serde_json::json!({
                                "tool_name": &tool_call.name,
                                "input": &tool_call.input,
                            }),
                        };
                        if let Err(reason) = hook_reg.fire(&ctx) {
                            turn_tool_calls.push(TurnToolCall::new(
                                &tool_call.name,
                                &tool_call.input,
                                ToolCallFate::Blocked("stopped by a BeforeToolCall hook"),
                            ));
                            tool_result_blocks.push(ContentBlock::ToolResult {
                                tool_use_id: tool_call.id.clone(),
                                tool_name: tool_call.name.clone(),
                                content: format!(
                                    "Hook blocked tool '{}': {}",
                                    tool_call.name, reason
                                ),
                                is_error: true,
                            });
                            continue;
                        }
                    }

                    // Resolve effective exec policy (per-agent override or global)
                    let effective_exec_policy = manifest.exec_policy.as_ref();

                    // Timeout-wrapped execution. `tool_timeout_for` returns None
                    // when the operator disabled the timeout (issue #1125).
                    let timeout_opt = tool_timeout_for(&tool_call.name);
                    let exec_fut = tool_runner::execute_tool(
                        &tool_call.id,
                        &tool_call.name,
                        &tool_call.input,
                        kernel.as_ref(),
                        Some(&allowed_tool_names),
                        Some(&caller_id_str),
                        skill_registry,
                        mcp_connections,
                        web_ctx,
                        browser_ctx,
                        if hand_allowed_env.is_empty() {
                            None
                        } else {
                            Some(&hand_allowed_env)
                        },
                        workspace_root,
                        media_engine,
                        effective_exec_policy,
                        tts_engine,
                        docker_config,
                        process_manager,
                    );
                    let result = match timeout_opt {
                        Some(timeout) => {
                            let timeout_secs = timeout.as_secs();
                            match tokio::time::timeout(timeout, exec_fut).await {
                                Ok(result) => result,
                                Err(_) => {
                                    warn!(tool = %tool_call.name, "Tool execution timed out after {}s", timeout_secs);
                                    openfang_types::tool::ToolResult {
                                        tool_use_id: tool_call.id.clone(),
                                        content: format!(
                                            "Tool '{}' timed out after {}s.",
                                            tool_call.name, timeout_secs
                                        ),
                                        is_error: true,
                                    }
                                }
                            }
                        }
                        None => exec_fut.await,
                    };

                    // Fire AfterToolCall hook
                    if let Some(hook_reg) = hooks {
                        let ctx = crate::hooks::HookContext {
                            agent_name: &manifest.name,
                            agent_id: caller_id_str.as_str(),
                            event: openfang_types::agent::HookEvent::AfterToolCall,
                            data: serde_json::json!({
                                "tool_name": &tool_call.name,
                                "result": &result.content,
                                "is_error": result.is_error,
                            }),
                        };
                        let _ = hook_reg.fire(&ctx);
                    }

                    // Dynamic truncation based on context budget (replaces flat MAX_TOOL_RESULT_CHARS)
                    let content = truncate_tool_result_dynamic(&result.content, &context_budget);

                    // Append warning if verdict was Warn
                    let final_content = if let LoopGuardVerdict::Warn(ref warn_msg) = verdict {
                        format!("{content}\n\n[LOOP GUARD] {warn_msg}")
                    } else {
                        content
                    };

                    turn_tool_calls.push(TurnToolCall::new(
                        &tool_call.name,
                        &tool_call.input,
                        if result.is_error {
                            ToolCallFate::Errored
                        } else {
                            ToolCallFate::Completed
                        },
                    ));
                    tool_result_blocks.push(ContentBlock::ToolResult {
                        tool_use_id: result.tool_use_id,
                        tool_name: tool_call.name.clone(),
                        content: final_content,
                        is_error: result.is_error,
                    });
                }

                append_tool_error_guidance(&mut tool_result_blocks);

                // Detect approval denials and inject guidance to prevent infinite retry loops
                let denial_count = tool_result_blocks
                    .iter()
                    .filter(|b| {
                        matches!(b, ContentBlock::ToolResult { content, is_error: true, .. }
                        if content.contains("requires human approval and was denied"))
                    })
                    .count();
                if denial_count > 0 {
                    tool_result_blocks.push(ContentBlock::Text {
                        text: format!(
                            "[System: {} tool call(s) were denied by approval policy. \
                             Do NOT retry denied tools. Explain to the user what you \
                             wanted to do and that it requires their approval. \
                             Hint: set auto_approve = true in [approval] section of \
                             config.toml, or start with --yolo flag, to auto-approve \
                             all tool calls.]",
                            denial_count
                        ),
                        provider_metadata: None,
                    });
                }

                // Detect tool errors and inject guidance to prevent fabrication
                let error_count = tool_result_blocks
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ToolResult { is_error: true, .. }))
                    .count();
                let non_denial_errors = error_count.saturating_sub(denial_count);
                if non_denial_errors > 0 {
                    tool_result_blocks.push(ContentBlock::Text {
                        text: format!(
                            "[System: {} tool(s) returned errors. Report the error honestly \
                             to the user. Do NOT fabricate results or pretend the tool succeeded. \
                             If a search or fetch failed, tell the user it failed and suggest \
                             alternatives instead of making up data.]",
                            non_denial_errors
                        ),
                        provider_metadata: None,
                    });
                }

                // Add tool results as a user message (Anthropic API requirement)
                let tool_results_msg = Message {
                    role: Role::User,
                    content: MessageContent::Blocks(tool_result_blocks.clone()),
                    ..Default::default()
                };
                session.messages.push(tool_results_msg.clone());
                messages.push(tool_results_msg);

                // Interim save after tool execution to prevent data loss on crash
                if let Err(e) = memory.save_session_async(session).await {
                    warn!("Failed to interim-save session: {e}");
                }
            }
            StopReason::MaxTokens => {
                consecutive_max_tokens += 1;
                if consecutive_max_tokens >= MAX_CONTINUATIONS {
                    // Return the partial response instead of continuing forever.
                    let text = response.text();
                    // FANG-13, second half. "Partial" is only true when there
                    // is a part. The continuation budget can run out without a
                    // single character having arrived — `MAX_CONTINUATIONS`
                    // truncations in a row that each carried nothing — and this
                    // branch used to answer that with a sentence of the
                    // runtime's own and HTTP 200, the same fabrication the exit
                    // above stopped making. Text accumulated during earlier
                    // tool_use iterations still counts as an answer: that
                    // fallback is unchanged from the EndTurn branch.
                    let text = if text.trim().is_empty() {
                        if !accumulated_text.is_empty() {
                            accumulated_text.clone()
                        } else {
                            warn!(
                                agent = %manifest.name,
                                iteration,
                                consecutive_max_tokens,
                                input_tokens = total_usage.input_tokens,
                                output_tokens = total_usage.output_tokens,
                                any_tools_executed,
                                "Continuation budget exhausted with no text — failing the turn"
                            );
                            if let Err(e) = memory.save_session_async(session).await {
                                warn!("Failed to save session on max continuations: {e}");
                            }
                            if let Some(hook_reg) = hooks {
                                let ctx = crate::hooks::HookContext {
                                    agent_name: &manifest.name,
                                    agent_id: agent_id_str.as_str(),
                                    event: openfang_types::agent::HookEvent::AgentLoopEnd,
                                    data: serde_json::json!({
                                        "reason": "max_continuations_no_text",
                                        "iterations": iteration + 1,
                                        "any_tools_executed": any_tools_executed,
                                    }),
                                };
                                let _ = hook_reg.fire(&ctx);
                            }
                            return Err(no_text_failure(
                                &manifest.name,
                                iteration + 1,
                                &total_usage,
                                any_tools_executed,
                                false,
                                NoText::TruncatedWithNoText {
                                    continuations: consecutive_max_tokens,
                                },
                            ));
                        }
                    } else {
                        text
                    };
                    // Issue #1148: preserve Thinking / RedactedThinking blocks
                    // present in the response so reasoning state survives
                    // MaxTokens truncation — same as the EndTurn branch.
                    let assistant_msg =
                        build_assistant_message_preserving_thinking(&response.content, &text);
                    session.messages.push(assistant_msg);
                    if let Err(e) = memory.save_session_async(session).await {
                        warn!("Failed to save session on max continuations: {e}");
                    }
                    warn!(
                        iteration,
                        consecutive_max_tokens,
                        "Max continuations reached, returning partial response"
                    );
                    // Fire AgentLoopEnd hook
                    if let Some(hook_reg) = hooks {
                        let ctx = crate::hooks::HookContext {
                            agent_name: &manifest.name,
                            agent_id: agent_id_str.as_str(),
                            event: openfang_types::agent::HookEvent::AgentLoopEnd,
                            data: serde_json::json!({
                                "iterations": iteration + 1,
                                "reason": "max_continuations",
                            }),
                        };
                        let _ = hook_reg.fire(&ctx);
                    }
                    return Ok(AgentLoopResult {
                        response: text,
                        total_usage,
                        iterations: iteration + 1,
                        cost_usd: None,
                        silent: false,
                        directives: Default::default(),
                        calls: finish_calls(&mut calls),
                    });
                }
                // Model hit token limit — add partial response and continue.
                // Issue #1148: preserve full response content (Thinking,
                // RedactedThinking, etc.) so reasoning state is not dropped
                // when continuing across the token-limit boundary.
                let text = response.text();
                let assistant_msg =
                    build_assistant_message_preserving_thinking(&response.content, &text);
                session.messages.push(assistant_msg.clone());
                messages.push(assistant_msg);
                session.messages.push(Message::user("Please continue."));
                messages.push(Message::user("Please continue."));
                warn!(iteration, "Max tokens hit, continuing");
            }
        }
    }

    // Iterations exhausted. This is a TRUNCATED turn, not a failed one: the
    // assistant text accumulated above is real, the tool calls in
    // `turn_tool_calls` really reached their fates, and the provider has really
    // billed every token in `total_usage`.
    //
    // Returning `Err(MaxIterationsExceeded)` here threw all of that away twice
    // over. The caller got a bare HTTP 500 carrying neither the partial text nor
    // the list of calls (FANG-10), and the kernel — which books usage only on
    // the Ok arm, `record_usage` / `record_tool_calls` / `record_turn_usage` —
    // never saw `calls`, so `usage_events`, `/api/usage/by-model` and the quota
    // counters all moved by zero for a turn the provider charged for (FANG-47).
    // The session was already being saved right here, so the runtime kept the
    // work for itself and told the caller nothing.
    let notice = max_iterations_notice(max_iterations, &turn_tool_calls);
    let summary = max_iterations_summary(&accumulated_text, &notice);
    session.messages.push(Message::assistant(summary.clone()));

    // Save session so conversation history is preserved
    if let Err(e) = memory.save_session_async(session).await {
        warn!("Failed to save session on max iterations: {e}");
    }

    let (n_ran, n_errored, n_blocked) = fate_counts(&turn_tool_calls);
    warn!(
        agent = %manifest.name,
        iterations = max_iterations,
        tool_calls_ran = n_ran,
        tool_calls_errored = n_errored,
        tool_calls_blocked = n_blocked,
        tokens = total_usage.total(),
        "Max iterations reached — returning the partial result"
    );

    // Fire AgentLoopEnd hook on max iterations exceeded
    if let Some(hook_reg) = hooks {
        let ctx = crate::hooks::HookContext {
            agent_name: &manifest.name,
            agent_id: agent_id_str.as_str(),
            event: openfang_types::agent::HookEvent::AgentLoopEnd,
            data: serde_json::json!({
                "reason": "max_iterations_exceeded",
                "iterations": max_iterations,
                "partial": true,
                "tool_calls_ran": n_ran,
                "tool_calls_errored": n_errored,
                "tool_calls_blocked": n_blocked,
            }),
        };
        let _ = hook_reg.fire(&ctx);
    }

    Ok(AgentLoopResult {
        response: summary,
        total_usage,
        iterations: max_iterations,
        cost_usd: None,
        silent: false,
        directives: Default::default(),
        calls: finish_calls(&mut calls),
    })
}

/// Call an LLM driver with automatic retry on rate-limit and overload errors.
///
/// Uses the `llm_errors` classifier for smart error handling and the
/// `ProviderCooldown` circuit breaker to prevent request storms.
///
/// When the primary model returns a `ModelNotFound` error and `fallback_models`
/// is non-empty, each fallback is tried in order before propagating the error.
async fn call_with_retry(
    driver: &dyn LlmDriver,
    request: CompletionRequest,
    provider: Option<&str>,
    cooldown: Option<&ProviderCooldown>,
    fallback_models: &[FallbackModel],
) -> OpenFangResult<(crate::llm_driver::CompletionResponse, CallReport)> {
    // Check circuit breaker before calling
    if let (Some(provider), Some(cooldown)) = (provider, cooldown) {
        match cooldown.check(provider) {
            CooldownVerdict::Reject {
                reason,
                retry_after_secs,
            } => {
                return Err(OpenFangError::LlmDriver(format!(
                    "Provider '{provider}' is in cooldown ({reason}). Retry in {retry_after_secs}s."
                )));
            }
            CooldownVerdict::AllowProbe => {
                debug!(provider, "Allowing probe request through circuit breaker");
            }
            CooldownVerdict::Allow => {}
        }
    }

    let mut last_error = None;

    for attempt in 0..=MAX_RETRIES {
        match driver.complete_reported(request.clone()).await {
            Ok((response, report)) => {
                // Record success with circuit breaker
                if let (Some(provider), Some(cooldown)) = (provider, cooldown) {
                    cooldown.record_success(provider);
                }
                return Ok((response, report));
            }
            Err(LlmError::RateLimited { retry_after_ms }) => {
                if attempt == MAX_RETRIES {
                    if let (Some(provider), Some(cooldown)) = (provider, cooldown) {
                        cooldown.record_failure(provider, false);
                    }
                    return Err(OpenFangError::LlmDriver(format!(
                        "Rate limited after {} retries",
                        MAX_RETRIES
                    )));
                }
                let delay = std::cmp::max(retry_after_ms, BASE_RETRY_DELAY_MS * 2u64.pow(attempt));
                warn!(
                    attempt,
                    delay_ms = delay,
                    "Rate limited, retrying after delay"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                last_error = Some("Rate limited".to_string());
            }
            Err(LlmError::Overloaded { retry_after_ms }) => {
                if attempt == MAX_RETRIES {
                    if let (Some(provider), Some(cooldown)) = (provider, cooldown) {
                        cooldown.record_failure(provider, false);
                    }
                    return Err(OpenFangError::LlmDriver(format!(
                        "Model overloaded after {} retries",
                        MAX_RETRIES
                    )));
                }
                let delay = std::cmp::max(retry_after_ms, BASE_RETRY_DELAY_MS * 2u64.pow(attempt));
                warn!(
                    attempt,
                    delay_ms = delay,
                    "Model overloaded, retrying after delay"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                last_error = Some("Overloaded".to_string());
            }
            Err(e) => {
                // Use classifier for smarter error handling
                let raw_error = e.to_string();
                let status = match &e {
                    LlmError::Api { status, .. } => Some(*status),
                    _ => None,
                };
                let classified = llm_errors::classify_error(&raw_error, status);
                warn!(
                    category = ?classified.category,
                    retryable = classified.is_retryable,
                    raw = %raw_error,
                    "LLM error classified: {}",
                    classified.sanitized_message
                );

                if let (Some(provider), Some(cooldown)) = (provider, cooldown) {
                    cooldown.record_failure(provider, classified.is_billing);
                }

                // --- ModelNotFound fallback chain (issue #845) ---
                // If the primary model was not found and fallback models are
                // configured, try each fallback before giving up.
                if classified.category == llm_errors::LlmErrorCategory::ModelNotFound
                    && !fallback_models.is_empty()
                {
                    warn!(
                        "Primary model not found, trying {} fallback model(s)",
                        fallback_models.len()
                    );
                    for (fb_idx, fb) in fallback_models.iter().enumerate() {
                        let api_key = fb
                            .api_key_env
                            .as_deref()
                            .and_then(|env_name| std::env::var(env_name).ok());
                        let fb_config = DriverConfig {
                            provider: fb.provider.clone(),
                            api_key,
                            base_url: fb.base_url.clone(),
                            skip_permissions: true,
                            subprocess_timeout_secs: None,
                        };
                        let fb_driver = match crate::drivers::create_driver(&fb_config) {
                            Ok(d) => d,
                            Err(driver_err) => {
                                warn!(
                                    fallback_index = fb_idx,
                                    provider = %fb.provider,
                                    model = %fb.model,
                                    error = %driver_err,
                                    "Failed to create fallback driver, skipping"
                                );
                                continue;
                            }
                        };
                        let mut fb_request = request.clone();
                        // FANG-36: send the same wire name `resolve_driver`'s
                        // chain sends. Sending `fb.model` unstripped made one
                        // configured fallback reach the provider under two
                        // different names depending on which failover path ran.
                        fb_request.model = strip_provider_prefix(&fb.model, &fb.provider);
                        warn!(
                            fallback_index = fb_idx,
                            provider = %fb.provider,
                            model = %fb.model,
                            "Trying fallback model"
                        );
                        match fb_driver.complete(fb_request).await {
                            Ok(response) => {
                                info!(
                                    fallback_index = fb_idx,
                                    provider = %fb.provider,
                                    model = %fb.model,
                                    "Fallback model succeeded"
                                );
                                // Booked under the configured name, so both
                                // failover paths produce one accounting key.
                                return Ok((
                                    response,
                                    CallReport {
                                        substituted: Some(fb.model.clone()),
                                        provider: Some(fb.provider.clone()),
                                        // sanitized, not raw: this reaches the caller in
                                        // calls[].reason and onward to SSE/WS/openai-compat.
                                        reason: Some(classified.sanitized_message.clone()),
                                    },
                                ));
                            }
                            Err(fb_err) => {
                                warn!(
                                    fallback_index = fb_idx,
                                    provider = %fb.provider,
                                    model = %fb.model,
                                    error = %fb_err,
                                    "Fallback model failed"
                                );
                            }
                        }
                    }
                    // All fallbacks exhausted — fall through to return the
                    // original ModelNotFound error below.
                }

                // Include raw error detail so dashboard users can debug
                let user_msg = if classified.category == llm_errors::LlmErrorCategory::Format {
                    format!("{} — raw: {}", classified.sanitized_message, raw_error)
                } else {
                    classified.sanitized_message
                };
                return Err(OpenFangError::LlmDriver(user_msg));
            }
        }
    }

    Err(OpenFangError::LlmDriver(
        last_error.unwrap_or_else(|| "Unknown error".to_string()),
    ))
}

/// Call an LLM driver in streaming mode with automatic retry on rate-limit and overload errors.
///
/// Uses the `llm_errors` classifier and `ProviderCooldown` circuit breaker.
///
/// When the primary model returns a `ModelNotFound` error and `fallback_models`
/// is non-empty, each fallback is tried in order before propagating the error.
async fn stream_with_retry(
    driver: &dyn LlmDriver,
    request: CompletionRequest,
    tx: mpsc::Sender<StreamEvent>,
    provider: Option<&str>,
    cooldown: Option<&ProviderCooldown>,
    fallback_models: &[FallbackModel],
) -> OpenFangResult<(crate::llm_driver::CompletionResponse, CallReport)> {
    // Check circuit breaker before calling
    if let (Some(provider), Some(cooldown)) = (provider, cooldown) {
        match cooldown.check(provider) {
            CooldownVerdict::Reject {
                reason,
                retry_after_secs,
            } => {
                return Err(OpenFangError::LlmDriver(format!(
                    "Provider '{provider}' is in cooldown ({reason}). Retry in {retry_after_secs}s."
                )));
            }
            CooldownVerdict::AllowProbe => {
                debug!(
                    provider,
                    "Allowing probe request through circuit breaker (stream)"
                );
            }
            CooldownVerdict::Allow => {}
        }
    }

    let mut last_error = None;

    for attempt in 0..=MAX_RETRIES {
        match driver.stream_reported(request.clone(), tx.clone()).await {
            Ok((response, report)) => {
                if let (Some(provider), Some(cooldown)) = (provider, cooldown) {
                    cooldown.record_success(provider);
                }
                return Ok((response, report));
            }
            Err(LlmError::RateLimited { retry_after_ms }) => {
                if attempt == MAX_RETRIES {
                    if let (Some(provider), Some(cooldown)) = (provider, cooldown) {
                        cooldown.record_failure(provider, false);
                    }
                    return Err(OpenFangError::LlmDriver(format!(
                        "Rate limited after {} retries",
                        MAX_RETRIES
                    )));
                }
                let delay = std::cmp::max(retry_after_ms, BASE_RETRY_DELAY_MS * 2u64.pow(attempt));
                warn!(
                    attempt,
                    delay_ms = delay,
                    "Rate limited (stream), retrying after delay"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                last_error = Some("Rate limited".to_string());
            }
            Err(LlmError::Overloaded { retry_after_ms }) => {
                if attempt == MAX_RETRIES {
                    if let (Some(provider), Some(cooldown)) = (provider, cooldown) {
                        cooldown.record_failure(provider, false);
                    }
                    return Err(OpenFangError::LlmDriver(format!(
                        "Model overloaded after {} retries",
                        MAX_RETRIES
                    )));
                }
                let delay = std::cmp::max(retry_after_ms, BASE_RETRY_DELAY_MS * 2u64.pow(attempt));
                warn!(
                    attempt,
                    delay_ms = delay,
                    "Model overloaded (stream), retrying after delay"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                last_error = Some("Overloaded".to_string());
            }
            Err(e) => {
                let raw_error = e.to_string();
                let status = match &e {
                    LlmError::Api { status, .. } => Some(*status),
                    _ => None,
                };
                let classified = llm_errors::classify_error(&raw_error, status);
                warn!(
                    category = ?classified.category,
                    retryable = classified.is_retryable,
                    raw = %raw_error,
                    "LLM stream error classified: {}",
                    classified.sanitized_message
                );

                if let (Some(provider), Some(cooldown)) = (provider, cooldown) {
                    cooldown.record_failure(provider, classified.is_billing);
                }

                // --- ModelNotFound fallback chain (issue #845) ---
                if classified.category == llm_errors::LlmErrorCategory::ModelNotFound
                    && !fallback_models.is_empty()
                {
                    warn!(
                        "Primary model not found (stream), trying {} fallback model(s)",
                        fallback_models.len()
                    );
                    for (fb_idx, fb) in fallback_models.iter().enumerate() {
                        let api_key = fb
                            .api_key_env
                            .as_deref()
                            .and_then(|env_name| std::env::var(env_name).ok());
                        let fb_config = DriverConfig {
                            provider: fb.provider.clone(),
                            api_key,
                            base_url: fb.base_url.clone(),
                            skip_permissions: true,
                            subprocess_timeout_secs: None,
                        };
                        let fb_driver = match crate::drivers::create_driver(&fb_config) {
                            Ok(d) => d,
                            Err(driver_err) => {
                                warn!(
                                    fallback_index = fb_idx,
                                    provider = %fb.provider,
                                    model = %fb.model,
                                    error = %driver_err,
                                    "Failed to create fallback stream driver, skipping"
                                );
                                continue;
                            }
                        };
                        let mut fb_request = request.clone();
                        // FANG-36 (stream path): same wire name as the
                        // `resolve_driver` chain sends.
                        fb_request.model = strip_provider_prefix(&fb.model, &fb.provider);
                        warn!(
                            fallback_index = fb_idx,
                            provider = %fb.provider,
                            model = %fb.model,
                            "Trying fallback model (stream)"
                        );
                        match fb_driver.stream(fb_request, tx.clone()).await {
                            Ok(response) => {
                                info!(
                                    fallback_index = fb_idx,
                                    provider = %fb.provider,
                                    model = %fb.model,
                                    "Fallback model succeeded (stream)"
                                );
                                return Ok((
                                    response,
                                    CallReport {
                                        substituted: Some(fb.model.clone()),
                                        provider: Some(fb.provider.clone()),
                                        // sanitized, not raw: this reaches the caller in
                                        // calls[].reason and onward to SSE/WS/openai-compat.
                                        reason: Some(classified.sanitized_message.clone()),
                                    },
                                ));
                            }
                            Err(fb_err) => {
                                warn!(
                                    fallback_index = fb_idx,
                                    provider = %fb.provider,
                                    model = %fb.model,
                                    error = %fb_err,
                                    "Fallback model failed (stream)"
                                );
                            }
                        }
                    }
                }

                let user_msg = if classified.category == llm_errors::LlmErrorCategory::Format {
                    format!("{} — raw: {}", classified.sanitized_message, raw_error)
                } else {
                    classified.sanitized_message
                };
                return Err(OpenFangError::LlmDriver(user_msg));
            }
        }
    }

    Err(OpenFangError::LlmDriver(
        last_error.unwrap_or_else(|| "Unknown error".to_string()),
    ))
}

/// Run the agent execution loop with streaming support.
///
/// Like `run_agent_loop`, but sends `StreamEvent`s to the provided channel
/// as tokens arrive from the LLM. Tool execution happens between LLM calls
/// and is not streamed.
#[allow(clippy::too_many_arguments)]
pub async fn run_agent_loop_streaming(
    manifest: &AgentManifest,
    user_message: &str,
    session: &mut Session,
    memory: &MemorySubstrate,
    driver: Arc<dyn LlmDriver>,
    available_tools: &[ToolDefinition],
    kernel: Option<Arc<dyn KernelHandle>>,
    stream_tx: mpsc::Sender<StreamEvent>,
    skill_registry: Option<&SkillRegistry>,
    mcp_connections: Option<&tokio::sync::Mutex<Vec<McpConnection>>>,
    web_ctx: Option<&WebToolsContext>,
    browser_ctx: Option<&crate::browser::BrowserManager>,
    embedding_driver: Option<&(dyn EmbeddingDriver + Send + Sync)>,
    workspace_root: Option<&Path>,
    on_phase: Option<&PhaseCallback>,
    media_engine: Option<&crate::media_understanding::MediaEngine>,
    tts_engine: Option<&crate::tts::TtsEngine>,
    docker_config: Option<&openfang_types::config::DockerSandboxConfig>,
    hooks: Option<&crate::hooks::HookRegistry>,
    context_window_tokens: Option<usize>,
    process_manager: Option<&crate::process_manager::ProcessManager>,
    user_content_blocks: Option<Vec<ContentBlock>>,
) -> OpenFangResult<AgentLoopResult> {
    info!(agent = %manifest.name, "Starting streaming agent loop");

    // Extract hand-allowed env vars from manifest metadata (set by kernel for hand settings)
    let hand_allowed_env: Vec<String> = manifest
        .metadata
        .get("hand_allowed_env")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    // Recall relevant memories — prefer vector similarity search when embedding driver is available
    let memories = if let Some(emb) = embedding_driver {
        match emb.embed_one(user_message).await {
            Ok(query_vec) => {
                debug!("Using vector recall (streaming, dims={})", query_vec.len());
                memory
                    .recall_with_embedding_async(
                        user_message,
                        5,
                        Some(MemoryFilter {
                            agent_id: Some(session.agent_id),
                            ..Default::default()
                        }),
                        Some(&query_vec),
                    )
                    .await
                    .unwrap_or_default()
            }
            Err(e) => {
                warn!("Embedding recall failed (streaming), falling back to text search: {e}");
                memory
                    .recall(
                        user_message,
                        5,
                        Some(MemoryFilter {
                            agent_id: Some(session.agent_id),
                            ..Default::default()
                        }),
                    )
                    .await
                    .unwrap_or_default()
            }
        }
    } else {
        memory
            .recall(
                user_message,
                5,
                Some(MemoryFilter {
                    agent_id: Some(session.agent_id),
                    ..Default::default()
                }),
            )
            .await
            .unwrap_or_default()
    };

    // Fire BeforePromptBuild hook
    let agent_id_str = session.agent_id.0.to_string();
    if let Some(hook_reg) = hooks {
        let ctx = crate::hooks::HookContext {
            agent_name: &manifest.name,
            agent_id: agent_id_str.as_str(),
            event: openfang_types::agent::HookEvent::BeforePromptBuild,
            data: serde_json::json!({
                "system_prompt": &manifest.model.system_prompt,
                "user_message": user_message,
            }),
        };
        let _ = hook_reg.fire(&ctx);
    }

    // Build the system prompt — base prompt comes from kernel (prompt_builder),
    // we append recalled memories here since they are resolved at loop time.
    let mut system_prompt = manifest.model.system_prompt.clone();
    if !memories.is_empty() {
        let mem_pairs: Vec<(String, String)> = memories
            .iter()
            .map(|m| (String::new(), m.content.clone()))
            .collect();
        system_prompt.push_str("\n\n");
        system_prompt.push_str(&crate::prompt_builder::build_memory_section(&mem_pairs));
    }

    // Add the user message to session history.
    // When content blocks are provided (e.g. text + image from a channel),
    // combine them with the user text so the LLM sees the full multimodal turn.
    session
        .messages
        .push(build_user_turn_message(user_message, user_content_blocks));

    let llm_messages: Vec<Message> = session
        .messages
        .iter()
        .filter(|m| m.role != Role::System)
        .cloned()
        .collect();

    // Strip Image blocks from session to prevent base64 bloat.
    // The LLM already received them via llm_messages above.
    for msg in session.messages.iter_mut() {
        if let MessageContent::Blocks(blocks) = &mut msg.content {
            let had_images = blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. }));
            if had_images {
                blocks.retain(|b| !matches!(b, ContentBlock::Image { .. }));
                if blocks.is_empty() {
                    blocks.push(ContentBlock::Text {
                        text: "[Image processed]".to_string(),
                        provider_metadata: None,
                    });
                }
            }
        }
    }

    // Validate and repair session history (drop orphans, merge consecutive)
    let mut messages = crate::session_repair::validate_and_repair(&llm_messages);

    // Inject canonical context as the first user message (not in system prompt)
    // to keep the system prompt stable across turns for provider prompt caching.
    if let Some(cc_msg) = manifest
        .metadata
        .get("canonical_context_msg")
        .and_then(|v| v.as_str())
    {
        if !cc_msg.is_empty() {
            messages.insert(0, Message::user(cc_msg));
        }
    }

    let mut total_usage = TokenUsage::default();
    // One row per LLM call of this turn — the unit of accounting.
    let mut calls: Vec<LlmCall> = Vec::new();
    let final_response;
    let mut accumulated_text = String::new();

    // Safety valve: trim excessively long message histories to prevent context overflow.
    // Per-agent cap: manifest override -> runtime default (issue #871).
    let max_history = manifest.effective_max_history_messages();
    if messages.len() > max_history {
        let trim_count = messages.len() - max_history;
        warn!(
            agent = %manifest.name,
            total_messages = messages.len(),
            trimming = trim_count,
            max_history = max_history,
            "Trimming old messages to prevent context overflow (streaming)"
        );
        messages.drain(..trim_count);
        // Re-validate after trimming: the drain may have split a ToolUse/ToolResult
        // pair across the cut boundary, leaving orphaned blocks that cause the LLM
        // to return empty responses (input_tokens=0).
        messages = crate::session_repair::validate_and_repair(&messages);
        // Ensure history starts with a user turn: trimming may have left an
        // assistant turn at position 0, which strict providers (e.g. Gemini)
        // reject with INVALID_ARGUMENT on function-call turns.
        messages = crate::session_repair::ensure_starts_with_user(messages);
    }

    // Use autonomous config max_iterations if set, else default
    let max_iterations = manifest
        .autonomous
        .as_ref()
        .map(|a| a.max_iterations)
        .unwrap_or(MAX_ITERATIONS);

    // Initialize loop guard — scale circuit breaker for autonomous agents
    let loop_guard_config = {
        let mut cfg = LoopGuardConfig::default();
        if max_iterations > cfg.global_circuit_breaker {
            cfg.global_circuit_breaker = max_iterations * 3;
        }
        cfg
    };
    let mut loop_guard = LoopGuard::new(loop_guard_config);
    let mut consecutive_max_tokens: u32 = 0;

    // Build context budget from model's actual context window (or fallback to default)
    let ctx_window = context_window_tokens.unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let context_budget = ContextBudget::new(ctx_window);
    let mut any_tools_executed = false;
    // What became of each tool call this turn, recorded as each fate is
    // decided. Read at the max-iterations exit; nothing else consumes it.
    let mut turn_tool_calls: Vec<TurnToolCall> = Vec::new();

    for iteration in 0..max_iterations {
        debug!(iteration, "Streaming agent loop iteration");

        // Context overflow recovery pipeline (replaces emergency_trim_messages)
        let recovery =
            recover_from_overflow(&mut messages, &system_prompt, available_tools, ctx_window);
        match &recovery {
            RecoveryStage::None => {}
            RecoveryStage::FinalError => {
                if stream_tx.send(StreamEvent::PhaseChange {
                    phase: "context_warning".to_string(),
                    detail: Some("Context overflow unrecoverable. Use /reset or /compact.".to_string()),
                }).await.is_err() {
                    warn!("Stream consumer disconnected while sending context overflow warning");
                }
            }
            _ => {
                if stream_tx.send(StreamEvent::PhaseChange {
                    phase: "context_warning".to_string(),
                    detail: Some("Older messages trimmed to stay within context limits. Use /compact for smarter summarization.".to_string()),
                }).await.is_err() {
                    warn!("Stream consumer disconnected while sending context trim warning");
                }
            }
        }

        // Re-validate tool_call/tool_result pairing after overflow drains
        // which may have broken assistant→tool ordering invariants.
        // (Matches the non-streaming loop; fixes Qwen3.5-plus "tool_calls must
        // be followed by tool messages" errors after context overflow recovery.)
        if recovery != RecoveryStage::None {
            messages = crate::session_repair::validate_and_repair(&messages);
            // Ensure history starts with a user turn after overflow recovery.
            messages = crate::session_repair::ensure_starts_with_user(messages);
        }

        // Context guard: compact oversized tool results before LLM call
        apply_context_guard(&mut messages, &context_budget, available_tools);

        // Strip provider prefix: "openrouter/google/gemini-2.5-flash" → "google/gemini-2.5-flash"
        let api_model = strip_provider_prefix(&manifest.model.model, &manifest.model.provider);

        let request = CompletionRequest {
            model: api_model,
            messages: messages.clone(),
            tools: available_tools.to_vec(),
            max_tokens: manifest.model.max_tokens,
            temperature: manifest.model.temperature,
            system: Some(system_prompt.clone()),
            thinking: None,
        };

        // Notify phase: on first iteration emit Streaming; on subsequent
        // iterations (after tool execution) emit Thinking so the UI shows
        // "Thinking..." instead of overwriting streamed text with "streaming".
        if let Some(cb) = on_phase {
            if iteration == 0 {
                cb(LoopPhase::Streaming);
            } else {
                cb(LoopPhase::Thinking);
            }
        }

        // Stamp last_active before the (potentially long) LLM call so the
        // heartbeat monitor doesn't flag us as unresponsive mid-iteration.
        if let Some(k) = &kernel {
            k.touch_agent(&agent_id_str);
        }

        // Stream LLM call with retry, error classification, and circuit breaker
        let provider_name = manifest.model.provider.as_str();
        let (mut response, report) = stream_with_retry(
            &*driver,
            request,
            stream_tx.clone(),
            Some(provider_name),
            None,
            &manifest.fallback_models,
        )
        .await?;

        total_usage.input_tokens += response.usage.input_tokens;
        total_usage.output_tokens += response.usage.output_tokens;
        record_call(
            &mut calls,
            iteration,
            &manifest.model,
            &report,
            response.usage,
        );
        // Disclose the call on the stream itself: the dashboard and the SSE
        // clients build their view from stream events and never see
        // `AgentLoopResult`. The driver already sent `ContentComplete` on this
        // same channel, so a client sees `done` and then `call`.
        if let Some(c) = calls.last() {
            let _ = stream_tx
                .send(StreamEvent::CallReported {
                    n: c.n,
                    provider: c.provider.clone(),
                    model: c.model.clone(),
                    requested: c.requested.clone(),
                    reason: c.reason.clone(),
                    usage: response.usage,
                })
                .await;
        }

        // Recover tool calls output as text (streaming path)
        if matches!(
            response.stop_reason,
            StopReason::EndTurn | StopReason::StopSequence
        ) && response.tool_calls.is_empty()
        {
            let recovered = recover_text_tool_calls(&response.text(), available_tools);
            if !recovered.is_empty() {
                info!(
                    count = recovered.len(),
                    "Recovered text-based tool calls (streaming) → promoting to ToolUse"
                );
                response.tool_calls = recovered;
                response.stop_reason = StopReason::ToolUse;
                let mut new_blocks: Vec<ContentBlock> = Vec::new();
                for tc in &response.tool_calls {
                    new_blocks.push(ContentBlock::ToolUse {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        input: tc.input.clone(),
                        provider_metadata: None,
                    });
                }
                response.content = new_blocks;
            }
        }
        set_last_tool_calls(&mut calls, response.tool_calls.len());

        match response.stop_reason {
            StopReason::EndTurn | StopReason::StopSequence => {
                let text = response.text();

                // Parse reply directives from the streaming response text
                let (cleaned_text_s, parsed_directives_s) =
                    crate::reply_directives::parse_directives(&text);
                let text = cleaned_text_s;

                // NO_REPLY / [SILENT]: agent intentionally chose not to reply.
                // [SILENT] must not be stored literally — it reinforces silence in future turns.
                if is_silent_token(&text) || parsed_directives_s.silent {
                    debug!(agent = %manifest.name, "Agent chose NO_REPLY/silent (streaming) — silent completion");
                    session
                        .messages
                        .push(Message::assistant("[no reply needed]".to_string()));
                    memory
                        .save_session_async(session)
                        .await
                        .map_err(|e| OpenFangError::Memory(e.to_string()))?;
                    return Ok(AgentLoopResult {
                        response: String::new(),
                        total_usage,
                        iterations: iteration + 1,
                        cost_usd: None,
                        silent: true,
                        directives: openfang_types::message::ReplyDirectives {
                            reply_to: parsed_directives_s.reply_to,
                            current_thread: parsed_directives_s.current_thread,
                            silent: true,
                        },
                        calls: finish_calls(&mut calls),
                    });
                }

                // One-shot retry: if the LLM returns empty text with no tool use,
                // try once more before accepting the empty result.
                // Triggers on first call OR when input_tokens=0 (silently failed request).
                if text.trim().is_empty()
                    && response.tool_calls.is_empty()
                    && !response.has_any_content()
                {
                    let is_silent_failure =
                        response.usage.input_tokens == 0 && response.usage.output_tokens == 0;
                    if iteration == 0 || is_silent_failure {
                        warn!(
                            agent = %manifest.name,
                            iteration,
                            input_tokens = response.usage.input_tokens,
                            output_tokens = response.usage.output_tokens,
                            silent_failure = is_silent_failure,
                            "Empty response (streaming), retrying once"
                        );
                        // Re-validate messages before retry — the history may have
                        // broken tool_use/tool_result pairs that caused the failure.
                        if is_silent_failure {
                            messages = crate::session_repair::validate_and_repair(&messages);
                        }
                        messages.push(Message::assistant("[no response]".to_string()));
                        messages.push(Message::user("Please provide your response.".to_string()));
                        continue;
                    }
                }

                // Guard against empty response — use accumulated text as fallback (streaming).
                let text = if text.trim().is_empty() {
                    if !accumulated_text.is_empty() {
                        debug!(
                            agent = %manifest.name,
                            accumulated_len = accumulated_text.len(),
                            "Using accumulated text from intermediate tool_use iterations (streaming)"
                        );
                        accumulated_text.clone()
                    } else {
                        // FANG-13, streaming half. Kept byte-for-byte parallel
                        // with the non-streaming guard: a difference here is a
                        // difference between what the dashboard/SSE sees and
                        // what REST sees, which is how these two drifted apart
                        // before.
                        warn!(
                            agent = %manifest.name,
                            iteration,
                            input_tokens = total_usage.input_tokens,
                            output_tokens = total_usage.output_tokens,
                            messages_count = messages.len(),
                            any_tools_executed,
                            "Empty response from LLM (streaming) — failing the turn"
                        );
                        if let Err(e) = memory.save_session_async(session).await {
                            warn!("Failed to save session on empty response (streaming): {e}");
                        }
                        if let Some(hook_reg) = hooks {
                            let ctx = crate::hooks::HookContext {
                                agent_name: &manifest.name,
                                agent_id: agent_id_str.as_str(),
                                event: openfang_types::agent::HookEvent::AgentLoopEnd,
                                data: serde_json::json!({
                                    "reason": "empty_response",
                                    "iterations": iteration + 1,
                                    "any_tools_executed": any_tools_executed,
                                }),
                            };
                            let _ = hook_reg.fire(&ctx);
                        }
                        return Err(no_text_failure(
                            &manifest.name,
                            iteration + 1,
                            &total_usage,
                            any_tools_executed,
                            true,
                            NoText::EmptyFinalMessage,
                        ));
                    }
                } else {
                    text
                };
                final_response = text.clone();
                // Issue #1098: preserve Thinking blocks (with Anthropic
                // signatures / Gemini thought signatures / inline-think /
                // reasoning_content) on the persisted assistant turn.  See
                // build_assistant_message_preserving_thinking for details.
                let assistant_msg =
                    build_assistant_message_preserving_thinking(&response.content, &text);
                session.messages.push(assistant_msg);

                // Prune NO_REPLY heartbeat turns to save context budget
                crate::session_repair::prune_heartbeat_turns(&mut session.messages, 10);

                memory
                    .save_session_async(session)
                    .await
                    .map_err(|e| OpenFangError::Memory(e.to_string()))?;

                // Remember this interaction (with embedding if available)
                let interaction_text = format!(
                    "User asked: {}\nI responded: {}",
                    user_message, final_response
                );
                if let Some(emb) = embedding_driver {
                    match emb.embed_one(&interaction_text).await {
                        Ok(vec) => {
                            let _ = memory
                                .remember_with_embedding_async(
                                    session.agent_id,
                                    &interaction_text,
                                    MemorySource::Conversation,
                                    "episodic",
                                    HashMap::new(),
                                    Some(&vec),
                                )
                                .await;
                        }
                        Err(e) => {
                            warn!("Embedding for remember failed (streaming): {e}");
                            let _ = memory
                                .remember(
                                    session.agent_id,
                                    &interaction_text,
                                    MemorySource::Conversation,
                                    "episodic",
                                    HashMap::new(),
                                )
                                .await;
                        }
                    }
                } else {
                    let _ = memory
                        .remember(
                            session.agent_id,
                            &interaction_text,
                            MemorySource::Conversation,
                            "episodic",
                            HashMap::new(),
                        )
                        .await;
                }

                // Notify phase: Done
                if let Some(cb) = on_phase {
                    cb(LoopPhase::Done);
                }

                info!(
                    agent = %manifest.name,
                    iterations = iteration + 1,
                    tokens = total_usage.total(),
                    "Streaming agent loop completed"
                );

                // Fire AgentLoopEnd hook
                if let Some(hook_reg) = hooks {
                    let ctx = crate::hooks::HookContext {
                        agent_name: &manifest.name,
                        agent_id: agent_id_str.as_str(),
                        event: openfang_types::agent::HookEvent::AgentLoopEnd,
                        data: serde_json::json!({
                            "iterations": iteration + 1,
                            "response_length": final_response.len(),
                        }),
                    };
                    let _ = hook_reg.fire(&ctx);
                }

                return Ok(AgentLoopResult {
                    response: final_response,
                    total_usage,
                    iterations: iteration + 1,
                    cost_usd: None,
                    silent: false,
                    directives: Default::default(),
                    calls: finish_calls(&mut calls),
                });
            }
            StopReason::ToolUse => {
                // Reset MaxTokens continuation counter on tool use
                consecutive_max_tokens = 0;
                any_tools_executed = true;

                // Capture text from intermediate tool_use turns (streaming path).
                let intermediate_text = response.text();
                if !intermediate_text.trim().is_empty() {
                    if !accumulated_text.is_empty() {
                        accumulated_text.push_str("\n\n");
                    }
                    accumulated_text.push_str(intermediate_text.trim());
                }

                let assistant_blocks = response.content.clone();

                session.messages.push(Message {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(assistant_blocks.clone()),
                    ..Default::default()
                });
                messages.push(Message {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(assistant_blocks),
                    ..Default::default()
                });

                let allowed_tool_names: Vec<String> =
                    available_tools.iter().map(|t| t.name.clone()).collect();
                let caller_id_str = session.agent_id.to_string();

                // Execute each tool call with loop guard, timeout, and truncation
                let mut tool_result_blocks = Vec::new();
                for tool_call in deduplicate_tool_calls(&response) {
                    // Loop guard check
                    let verdict = loop_guard.check(&tool_call.name, &tool_call.input);
                    match &verdict {
                        LoopGuardVerdict::CircuitBreak(msg) => {
                            warn!(tool = %tool_call.name, "Circuit breaker triggered (streaming)");
                            if let Err(e) = memory.save_session_async(session).await {
                                warn!("Failed to save session on circuit break: {e}");
                            }
                            // Fire AgentLoopEnd hook on circuit break
                            if let Some(hook_reg) = hooks {
                                let ctx = crate::hooks::HookContext {
                                    agent_name: &manifest.name,
                                    agent_id: agent_id_str.as_str(),
                                    event: openfang_types::agent::HookEvent::AgentLoopEnd,
                                    data: serde_json::json!({
                                        "reason": "circuit_break",
                                        "error": msg.as_str(),
                                    }),
                                };
                                let _ = hook_reg.fire(&ctx);
                            }
                            return Err(OpenFangError::Internal(msg.clone()));
                        }
                        LoopGuardVerdict::Block(msg) => {
                            warn!(tool = %tool_call.name, "Tool call blocked by loop guard (streaming)");
                            turn_tool_calls.push(TurnToolCall::new(
                                &tool_call.name,
                                &tool_call.input,
                                ToolCallFate::Blocked("stopped by the loop guard"),
                            ));
                            tool_result_blocks.push(ContentBlock::ToolResult {
                                tool_use_id: tool_call.id.clone(),
                                tool_name: tool_call.name.clone(),
                                content: msg.clone(),
                                is_error: true,
                            });
                            continue;
                        }
                        _ => {} // Allow or Warn — proceed with execution
                    }

                    debug!(tool = %tool_call.name, id = %tool_call.id, "Executing tool (streaming)");

                    // Notify phase: ToolUse
                    if let Some(cb) = on_phase {
                        let sanitized: String = tool_call
                            .name
                            .chars()
                            .filter(|c| !c.is_control())
                            .take(64)
                            .collect();
                        cb(LoopPhase::ToolUse {
                            tool_name: sanitized,
                        });
                    }

                    // Fire BeforeToolCall hook (can block execution)
                    if let Some(hook_reg) = hooks {
                        let ctx = crate::hooks::HookContext {
                            agent_name: &manifest.name,
                            agent_id: &caller_id_str,
                            event: openfang_types::agent::HookEvent::BeforeToolCall,
                            data: serde_json::json!({
                                "tool_name": &tool_call.name,
                                "input": &tool_call.input,
                            }),
                        };
                        if let Err(reason) = hook_reg.fire(&ctx) {
                            turn_tool_calls.push(TurnToolCall::new(
                                &tool_call.name,
                                &tool_call.input,
                                ToolCallFate::Blocked("stopped by a BeforeToolCall hook"),
                            ));
                            tool_result_blocks.push(ContentBlock::ToolResult {
                                tool_use_id: tool_call.id.clone(),
                                tool_name: tool_call.name.clone(),
                                content: format!(
                                    "Hook blocked tool '{}': {}",
                                    tool_call.name, reason
                                ),
                                is_error: true,
                            });
                            continue;
                        }
                    }

                    // Resolve effective exec policy (per-agent override or global)
                    let effective_exec_policy = manifest.exec_policy.as_ref();

                    // Timeout-wrapped execution. `tool_timeout_for` returns None
                    // when the operator disabled the timeout (issue #1125).
                    let timeout_opt = tool_timeout_for(&tool_call.name);
                    let exec_fut = tool_runner::execute_tool(
                        &tool_call.id,
                        &tool_call.name,
                        &tool_call.input,
                        kernel.as_ref(),
                        Some(&allowed_tool_names),
                        Some(&caller_id_str),
                        skill_registry,
                        mcp_connections,
                        web_ctx,
                        browser_ctx,
                        if hand_allowed_env.is_empty() {
                            None
                        } else {
                            Some(&hand_allowed_env)
                        },
                        workspace_root,
                        media_engine,
                        effective_exec_policy,
                        tts_engine,
                        docker_config,
                        process_manager,
                    );
                    let result = match timeout_opt {
                        Some(timeout) => {
                            let timeout_secs = timeout.as_secs();
                            match tokio::time::timeout(timeout, exec_fut).await {
                                Ok(result) => result,
                                Err(_) => {
                                    warn!(tool = %tool_call.name, "Tool execution timed out after {}s (streaming)", timeout_secs);
                                    openfang_types::tool::ToolResult {
                                        tool_use_id: tool_call.id.clone(),
                                        content: format!(
                                            "Tool '{}' timed out after {}s.",
                                            tool_call.name, timeout_secs
                                        ),
                                        is_error: true,
                                    }
                                }
                            }
                        }
                        None => exec_fut.await,
                    };

                    // Fire AfterToolCall hook
                    if let Some(hook_reg) = hooks {
                        let ctx = crate::hooks::HookContext {
                            agent_name: &manifest.name,
                            agent_id: caller_id_str.as_str(),
                            event: openfang_types::agent::HookEvent::AfterToolCall,
                            data: serde_json::json!({
                                "tool_name": &tool_call.name,
                                "result": &result.content,
                                "is_error": result.is_error,
                            }),
                        };
                        let _ = hook_reg.fire(&ctx);
                    }

                    // Dynamic truncation based on context budget (replaces flat MAX_TOOL_RESULT_CHARS)
                    let content = truncate_tool_result_dynamic(&result.content, &context_budget);

                    // Append warning if verdict was Warn
                    let final_content = if let LoopGuardVerdict::Warn(ref warn_msg) = verdict {
                        format!("{content}\n\n[LOOP GUARD] {warn_msg}")
                    } else {
                        content
                    };

                    // Notify client of tool execution result (detect dead consumer)
                    let preview: String = final_content.chars().take(300).collect();
                    if stream_tx
                        .send(StreamEvent::ToolExecutionResult {
                            id: tool_call.id.clone(),
                            name: tool_call.name.clone(),
                            result_preview: preview,
                            is_error: result.is_error,
                        })
                        .await
                        .is_err()
                    {
                        warn!(agent = %manifest.name, "Stream consumer disconnected — continuing tool loop but will not stream further");
                    }

                    turn_tool_calls.push(TurnToolCall::new(
                        &tool_call.name,
                        &tool_call.input,
                        if result.is_error {
                            ToolCallFate::Errored
                        } else {
                            ToolCallFate::Completed
                        },
                    ));
                    tool_result_blocks.push(ContentBlock::ToolResult {
                        tool_use_id: result.tool_use_id,
                        tool_name: tool_call.name.clone(),
                        content: final_content,
                        is_error: result.is_error,
                    });
                }

                append_tool_error_guidance(&mut tool_result_blocks);

                // Detect approval denials and inject guidance to prevent infinite retry loops
                let denial_count = tool_result_blocks
                    .iter()
                    .filter(|b| {
                        matches!(b, ContentBlock::ToolResult { content, is_error: true, .. }
                        if content.contains("requires human approval and was denied"))
                    })
                    .count();
                if denial_count > 0 {
                    tool_result_blocks.push(ContentBlock::Text {
                        text: format!(
                            "[System: {} tool call(s) were denied by approval policy. \
                             Do NOT retry denied tools. Explain to the user what you \
                             wanted to do and that it requires their approval. \
                             Hint: set auto_approve = true in [approval] section of \
                             config.toml, or start with --yolo flag, to auto-approve \
                             all tool calls.]",
                            denial_count
                        ),
                        provider_metadata: None,
                    });
                }

                // Detect tool errors and inject guidance to prevent fabrication
                let error_count = tool_result_blocks
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ToolResult { is_error: true, .. }))
                    .count();
                let non_denial_errors = error_count.saturating_sub(denial_count);
                if non_denial_errors > 0 {
                    tool_result_blocks.push(ContentBlock::Text {
                        text: format!(
                            "[System: {} tool(s) returned errors. Report the error honestly \
                             to the user. Do NOT fabricate results or pretend the tool succeeded. \
                             If a search or fetch failed, tell the user it failed and suggest \
                             alternatives instead of making up data.]",
                            non_denial_errors
                        ),
                        provider_metadata: None,
                    });
                }

                let tool_results_msg = Message {
                    role: Role::User,
                    content: MessageContent::Blocks(tool_result_blocks.clone()),
                    ..Default::default()
                };
                session.messages.push(tool_results_msg.clone());
                messages.push(tool_results_msg);

                if let Err(e) = memory.save_session_async(session).await {
                    warn!("Failed to interim-save session: {e}");
                }
            }
            StopReason::MaxTokens => {
                consecutive_max_tokens += 1;
                if consecutive_max_tokens >= MAX_CONTINUATIONS {
                    let text = response.text();
                    // FANG-13, second half — see the non-streaming twin of this
                    // branch for why a continuation budget that ran out without
                    // producing a character is a failed turn and not a partial
                    // answer.
                    let text = if text.trim().is_empty() {
                        if !accumulated_text.is_empty() {
                            accumulated_text.clone()
                        } else {
                            warn!(
                                agent = %manifest.name,
                                iteration,
                                consecutive_max_tokens,
                                input_tokens = total_usage.input_tokens,
                                output_tokens = total_usage.output_tokens,
                                any_tools_executed,
                                "Continuation budget exhausted with no text (streaming) — failing the turn"
                            );
                            if let Err(e) = memory.save_session_async(session).await {
                                warn!("Failed to save session on max continuations: {e}");
                            }
                            if let Some(hook_reg) = hooks {
                                let ctx = crate::hooks::HookContext {
                                    agent_name: &manifest.name,
                                    agent_id: agent_id_str.as_str(),
                                    event: openfang_types::agent::HookEvent::AgentLoopEnd,
                                    data: serde_json::json!({
                                        "reason": "max_continuations_no_text",
                                        "iterations": iteration + 1,
                                        "any_tools_executed": any_tools_executed,
                                    }),
                                };
                                let _ = hook_reg.fire(&ctx);
                            }
                            return Err(no_text_failure(
                                &manifest.name,
                                iteration + 1,
                                &total_usage,
                                any_tools_executed,
                                true,
                                NoText::TruncatedWithNoText {
                                    continuations: consecutive_max_tokens,
                                },
                            ));
                        }
                    } else {
                        text
                    };
                    // Issue #1148: preserve Thinking / RedactedThinking blocks
                    // present in the response so reasoning state survives
                    // MaxTokens truncation — same as the EndTurn branch.
                    let assistant_msg =
                        build_assistant_message_preserving_thinking(&response.content, &text);
                    session.messages.push(assistant_msg);
                    if let Err(e) = memory.save_session_async(session).await {
                        warn!("Failed to save session on max continuations: {e}");
                    }
                    warn!(
                        iteration,
                        consecutive_max_tokens,
                        "Max continuations reached (streaming), returning partial response"
                    );
                    // Fire AgentLoopEnd hook
                    if let Some(hook_reg) = hooks {
                        let ctx = crate::hooks::HookContext {
                            agent_name: &manifest.name,
                            agent_id: agent_id_str.as_str(),
                            event: openfang_types::agent::HookEvent::AgentLoopEnd,
                            data: serde_json::json!({
                                "iterations": iteration + 1,
                                "reason": "max_continuations",
                            }),
                        };
                        let _ = hook_reg.fire(&ctx);
                    }
                    return Ok(AgentLoopResult {
                        response: text,
                        total_usage,
                        iterations: iteration + 1,
                        cost_usd: None,
                        silent: false,
                        directives: Default::default(),
                        calls: finish_calls(&mut calls),
                    });
                }
                // Issue #1148: preserve full response content (Thinking,
                // RedactedThinking, etc.) so reasoning state is not dropped
                // when continuing across the token-limit boundary.
                let text = response.text();
                let assistant_msg =
                    build_assistant_message_preserving_thinking(&response.content, &text);
                session.messages.push(assistant_msg.clone());
                messages.push(assistant_msg);
                session.messages.push(Message::user("Please continue."));
                messages.push(Message::user("Please continue."));
                warn!(iteration, "Max tokens hit (streaming), continuing");
            }
        }
    }

    // Same exit, same reasoning as the non-streaming loop above: exhaustion is
    // a truncated turn, so hand back the partial result instead of an Err that
    // costs the caller the work (FANG-10) and the ledger the tokens (FANG-47).
    let notice = max_iterations_notice(max_iterations, &turn_tool_calls);
    let summary = max_iterations_summary(&accumulated_text, &notice);
    session.messages.push(Message::assistant(summary.clone()));

    // The WS/SSE client's transcript is built from TextDelta events, not from
    // the returned result (ws.rs concatenates the deltas and sends the result as
    // the `response` event). Without a delta here, a streaming caller would see
    // a turn that ends with nothing said about why it ended.
    //
    // Only the notice is sent, because `accumulated_text` already reached the
    // client as deltas — emitting the whole summary here would print the partial
    // text to the client twice.
    //
    // The two are NOT the same string, and nothing here should claim they are.
    // `accumulated_text` is the iterations' texts joined with "\n\n" and trimmed
    // (see the accumulation site below); the deltas went out one per iteration
    // with no separator between them. Any turn that spoke in more than one
    // iteration therefore ends with a client transcript shorter than the
    // returned `response` — measured at 2 910 against 3 008 characters over 50
    // talkative iterations. What IS guaranteed here is only that the partial
    // text is not duplicated.
    let delta = if accumulated_text.trim().is_empty() {
        notice.clone()
    } else {
        format!("\n\n{notice}")
    };
    let _ = stream_tx.send(StreamEvent::TextDelta { text: delta }).await;

    if let Err(e) = memory.save_session_async(session).await {
        warn!("Failed to save session on max iterations: {e}");
    }

    let (n_ran, n_errored, n_blocked) = fate_counts(&turn_tool_calls);
    warn!(
        agent = %manifest.name,
        iterations = max_iterations,
        tool_calls_ran = n_ran,
        tool_calls_errored = n_errored,
        tool_calls_blocked = n_blocked,
        tokens = total_usage.total(),
        "Max iterations reached (streaming) — returning the partial result"
    );

    // Fire AgentLoopEnd hook on max iterations exceeded
    if let Some(hook_reg) = hooks {
        let ctx = crate::hooks::HookContext {
            agent_name: &manifest.name,
            agent_id: agent_id_str.as_str(),
            event: openfang_types::agent::HookEvent::AgentLoopEnd,
            data: serde_json::json!({
                "reason": "max_iterations_exceeded",
                "iterations": max_iterations,
                "partial": true,
                "tool_calls_ran": n_ran,
                "tool_calls_errored": n_errored,
                "tool_calls_blocked": n_blocked,
            }),
        };
        let _ = hook_reg.fire(&ctx);
    }

    Ok(AgentLoopResult {
        response: summary,
        total_usage,
        iterations: max_iterations,
        cost_usd: None,
        silent: false,
        directives: Default::default(),
        calls: finish_calls(&mut calls),
    })
}

/// Recover tool calls that LLMs output as plain text instead of the proper
/// `tool_calls` API field. Covers Groq/Llama, DeepSeek, Qwen, and Ollama models.
///
/// Supported patterns:
/// 1. `<function=tool_name>{"key":"value"}</function>`
/// 2. `<function>tool_name{"key":"value"}</function>`
/// 3. `<tool>tool_name{"key":"value"}</tool>`
/// 4. Markdown code blocks containing `tool_name {"key":"value"}`
/// 5. Backtick-wrapped `tool_name {"key":"value"}`
/// 6. `[TOOL_CALL]...[/TOOL_CALL]` blocks (JSON or arrow syntax) — issue #354
/// 7. `<tool_call>{"name":"tool","arguments":{...}}</tool_call>` — Qwen3, issue #332
/// 8. Bare JSON `{"name":"tool","arguments":{...}}` objects (last resort, only if no tags found)
/// 9. `<function name="tool" parameters="{...}" />` — XML attribute style (Groq/Llama)
/// 10. `<|plugin|>...<|endofblock|>` — Qwen/ChatGLM thinking-model format
/// 11. `Action: tool\nAction Input: {"key":"value"}` — ReAct-style (LM Studio, GPT-OSS)
/// 12. `tool_name\n{"key":"value"}` — bare name + JSON on next line (Llama 4 Scout)
/// 13. `<tool_use>{"name":"tool","arguments":{...}}</tool_use>` — Llama 3.1+ variant
/// 14. `<function=tool><parameter=name>value</parameter></function>` — nested XML parameter style
///
/// Validates tool names against available tools and returns synthetic `ToolCall` entries.
fn recover_text_tool_calls(text: &str, available_tools: &[ToolDefinition]) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    let tool_names: Vec<&str> = available_tools.iter().map(|t| t.name.as_str()).collect();

    // Pattern 1: <function=TOOL_NAME>JSON_BODY</function>
    let mut search_from = 0;
    while let Some(start) = text[search_from..].find("<function=") {
        let abs_start = search_from + start;
        let after_prefix = abs_start + "<function=".len();

        // Extract tool name (ends at '>')
        let Some(name_end) = text[after_prefix..].find('>') else {
            search_from = after_prefix;
            continue;
        };
        let tool_name = &text[after_prefix..after_prefix + name_end];
        let json_start = after_prefix + name_end + 1;

        // Find closing </function>
        let Some(close_offset) = text[json_start..].find("</function>") else {
            search_from = json_start;
            continue;
        };
        let json_body = text[json_start..json_start + close_offset].trim();
        search_from = json_start + close_offset + "</function>".len();

        // Validate: tool name must be in available_tools
        if !tool_names.contains(&tool_name) {
            warn!(
                tool = tool_name,
                "Text-based tool call for unknown tool — skipping"
            );
            continue;
        }

        // Parse JSON input, or fall back to nested XML parameter blocks.
        let input: serde_json::Value = match serde_json::from_str(json_body) {
            Ok(v) => v,
            Err(json_err) => match parse_xml_parameter_blocks(json_body) {
                Some(v) => v,
                None => {
                    warn!(tool = tool_name, error = %json_err, "Failed to parse text-based tool call payload — skipping");
                    continue;
                }
            },
        };

        info!(
            tool = tool_name,
            "Recovered text-based tool call → synthetic ToolUse"
        );
        calls.push(ToolCall {
            id: format!("recovered_{}", uuid::Uuid::new_v4()),
            name: tool_name.to_string(),
            input,
        });
    }

    // Pattern 2: <function>TOOL_NAME{JSON_BODY}</function>
    // (Groq/Llama variant — tool name immediately followed by JSON object)
    search_from = 0;
    while let Some(start) = text[search_from..].find("<function>") {
        let abs_start = search_from + start;
        let after_tag = abs_start + "<function>".len();

        // Find closing </function>
        let Some(close_offset) = text[after_tag..].find("</function>") else {
            search_from = after_tag;
            continue;
        };
        let inner = &text[after_tag..after_tag + close_offset];
        search_from = after_tag + close_offset + "</function>".len();

        // The inner content is "tool_name{json}" — find the first '{' to split
        let Some(brace_pos) = inner.find('{') else {
            continue;
        };
        let tool_name = inner[..brace_pos].trim();
        let json_body = inner[brace_pos..].trim();

        if tool_name.is_empty() {
            continue;
        }

        // Validate: tool name must be in available_tools
        if !tool_names.contains(&tool_name) {
            warn!(
                tool = tool_name,
                "Text-based tool call (variant 2) for unknown tool — skipping"
            );
            continue;
        }

        // Parse JSON input
        let input: serde_json::Value = match serde_json::from_str(json_body) {
            Ok(v) => v,
            Err(e) => {
                warn!(tool = tool_name, error = %e, "Failed to parse text-based tool call JSON (variant 2) — skipping");
                continue;
            }
        };

        // Avoid duplicates if pattern 1 already captured this call
        if calls
            .iter()
            .any(|c| c.name == tool_name && c.input == input)
        {
            continue;
        }

        info!(
            tool = tool_name,
            "Recovered text-based tool call (variant 2) → synthetic ToolUse"
        );
        calls.push(ToolCall {
            id: format!("recovered_{}", uuid::Uuid::new_v4()),
            name: tool_name.to_string(),
            input,
        });
    }

    // Pattern 3: <tool>TOOL_NAME{JSON}</tool>  (Qwen / DeepSeek variant)
    search_from = 0;
    while let Some(start) = text[search_from..].find("<tool>") {
        let abs_start = search_from + start;
        let after_tag = abs_start + "<tool>".len();

        let Some(close_offset) = text[after_tag..].find("</tool>") else {
            search_from = after_tag;
            continue;
        };
        let inner = &text[after_tag..after_tag + close_offset];
        search_from = after_tag + close_offset + "</tool>".len();

        let Some(brace_pos) = inner.find('{') else {
            continue;
        };
        let tool_name = inner[..brace_pos].trim();
        let json_body = inner[brace_pos..].trim();

        if tool_name.is_empty() || !tool_names.contains(&tool_name) {
            continue;
        }

        let input: serde_json::Value = match serde_json::from_str(json_body) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if calls
            .iter()
            .any(|c| c.name == tool_name && c.input == input)
        {
            continue;
        }

        info!(
            tool = tool_name,
            "Recovered text-based tool call (<tool> variant) → synthetic ToolUse"
        );
        calls.push(ToolCall {
            id: format!("recovered_{}", uuid::Uuid::new_v4()),
            name: tool_name.to_string(),
            input,
        });
    }

    // Pattern 4: Markdown code blocks containing tool_name {JSON}
    // Matches: ```\nexec {"command":"ls"}\n``` or ```bash\nexec {"command":"ls"}\n```
    {
        let mut in_block = false;
        let mut block_content = String::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("```") {
                if in_block {
                    // End of block — try to extract tool call from content
                    let content = block_content.trim();
                    if let Some(brace_pos) = content.find('{') {
                        let potential_tool = content[..brace_pos].trim();
                        if tool_names.contains(&potential_tool) {
                            if let Ok(input) = serde_json::from_str::<serde_json::Value>(
                                content[brace_pos..].trim(),
                            ) {
                                if !calls
                                    .iter()
                                    .any(|c| c.name == potential_tool && c.input == input)
                                {
                                    info!(
                                        tool = potential_tool,
                                        "Recovered tool call from markdown code block"
                                    );
                                    calls.push(ToolCall {
                                        id: format!("recovered_{}", uuid::Uuid::new_v4()),
                                        name: potential_tool.to_string(),
                                        input,
                                    });
                                }
                            }
                        }
                    }
                    block_content.clear();
                    in_block = false;
                } else {
                    in_block = true;
                    block_content.clear();
                }
            } else if in_block {
                if !block_content.is_empty() {
                    block_content.push('\n');
                }
                block_content.push_str(trimmed);
            }
        }
    }

    // Pattern 5: Backtick-wrapped tool call: `tool_name {"key":"value"}`
    {
        let parts: Vec<&str> = text.split('`').collect();
        // Every odd-indexed element is inside backticks
        for chunk in parts.iter().skip(1).step_by(2) {
            let trimmed = chunk.trim();
            if let Some(brace_pos) = trimmed.find('{') {
                let potential_tool = trimmed[..brace_pos].trim();
                if !potential_tool.is_empty()
                    && !potential_tool.contains(' ')
                    && tool_names.contains(&potential_tool)
                {
                    if let Ok(input) =
                        serde_json::from_str::<serde_json::Value>(trimmed[brace_pos..].trim())
                    {
                        if !calls
                            .iter()
                            .any(|c| c.name == potential_tool && c.input == input)
                        {
                            info!(
                                tool = potential_tool,
                                "Recovered tool call from backtick-wrapped text"
                            );
                            calls.push(ToolCall {
                                id: format!("recovered_{}", uuid::Uuid::new_v4()),
                                name: potential_tool.to_string(),
                                input,
                            });
                        }
                    }
                }
            }
        }
    }

    // Pattern 6: [TOOL_CALL]...[/TOOL_CALL] blocks (Ollama models like Qwen, issue #354)
    // Handles both JSON args and custom `{tool => "name", args => {--key "value"}}` syntax.
    search_from = 0;
    while let Some(start) = text[search_from..].find("[TOOL_CALL]") {
        let abs_start = search_from + start;
        let after_tag = abs_start + "[TOOL_CALL]".len();

        let Some(close_offset) = text[after_tag..].find("[/TOOL_CALL]") else {
            search_from = after_tag;
            continue;
        };
        let inner = text[after_tag..after_tag + close_offset].trim();
        search_from = after_tag + close_offset + "[/TOOL_CALL]".len();

        // Try standard JSON first: {"name":"tool","arguments":{...}}
        if let Some((tool_name, input)) = parse_json_tool_call_object(inner, &tool_names) {
            if !calls
                .iter()
                .any(|c| c.name == tool_name && c.input == input)
            {
                info!(
                    tool = tool_name.as_str(),
                    "Recovered tool call from [TOOL_CALL] block (JSON)"
                );
                calls.push(ToolCall {
                    id: format!("recovered_{}", uuid::Uuid::new_v4()),
                    name: tool_name,
                    input,
                });
            }
            continue;
        }

        // Custom arrow syntax: {tool => "name", args => {--key "value"}}
        if let Some((tool_name, input)) = parse_arrow_syntax_tool_call(inner, &tool_names) {
            if !calls
                .iter()
                .any(|c| c.name == tool_name && c.input == input)
            {
                info!(
                    tool = tool_name.as_str(),
                    "Recovered tool call from [TOOL_CALL] block (arrow syntax)"
                );
                calls.push(ToolCall {
                    id: format!("recovered_{}", uuid::Uuid::new_v4()),
                    name: tool_name,
                    input,
                });
            }
        }
    }

    // Pattern 7: <tool_call>JSON</tool_call> (Qwen3 models on Ollama, issue #332)
    search_from = 0;
    while let Some(start) = text[search_from..].find("<tool_call>") {
        let abs_start = search_from + start;
        let after_tag = abs_start + "<tool_call>".len();

        let Some(close_offset) = text[after_tag..].find("</tool_call>") else {
            search_from = after_tag;
            continue;
        };
        let inner = text[after_tag..after_tag + close_offset].trim();
        search_from = after_tag + close_offset + "</tool_call>".len();

        if let Some((tool_name, input)) = parse_json_tool_call_object(inner, &tool_names) {
            if !calls
                .iter()
                .any(|c| c.name == tool_name && c.input == input)
            {
                info!(
                    tool = tool_name.as_str(),
                    "Recovered tool call from <tool_call> block"
                );
                calls.push(ToolCall {
                    id: format!("recovered_{}", uuid::Uuid::new_v4()),
                    name: tool_name,
                    input,
                });
            }
        }
    }

    // Pattern 9: <function name="tool" parameters="{...}" /> — XML attribute style
    // Groq/Llama sometimes emit self-closing XML with name/parameters attributes.
    // The parameters value is HTML-entity-escaped JSON (&quot; etc.).
    {
        use regex_lite::Regex;
        // Match both self-closing <function ... /> and <function ...></function>
        let re =
            Regex::new(r#"<function\s+name="([^"]+)"\s+parameters="([^"]*)"[^/]*/?>"#).unwrap();
        for caps in re.captures_iter(text) {
            let tool_name = caps.get(1).unwrap().as_str();
            let raw_params = caps.get(2).unwrap().as_str();

            if !tool_names.contains(&tool_name) {
                warn!(
                    tool = tool_name,
                    "XML-attribute tool call for unknown tool — skipping"
                );
                continue;
            }

            // Unescape HTML entities (&quot; &amp; &lt; &gt; &apos;)
            let unescaped = raw_params
                .replace("&quot;", "\"")
                .replace("&amp;", "&")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&apos;", "'");

            let input: serde_json::Value = match serde_json::from_str(&unescaped) {
                Ok(v) => v,
                Err(e) => {
                    warn!(tool = tool_name, error = %e, "Failed to parse XML-attribute tool call params — skipping");
                    continue;
                }
            };

            if calls
                .iter()
                .any(|c| c.name == tool_name && c.input == input)
            {
                continue;
            }

            info!(
                tool = tool_name,
                "Recovered XML-attribute tool call → synthetic ToolUse"
            );
            calls.push(ToolCall {
                id: format!("recovered_{}", uuid::Uuid::new_v4()),
                name: tool_name.to_string(),
                input,
            });
        }
    }

    // Pattern 10: <|plugin|>...<|endofblock|> (Qwen/ChatGLM thinking-model format)
    search_from = 0;
    while let Some(start) = text[search_from..].find("<|plugin|>") {
        let abs_start = search_from + start;
        let after_tag = abs_start + "<|plugin|>".len();

        let close_tag = "<|endofblock|>";
        let Some(close_offset) = text[after_tag..].find(close_tag) else {
            search_from = after_tag;
            continue;
        };
        let inner = text[after_tag..after_tag + close_offset].trim();
        search_from = after_tag + close_offset + close_tag.len();

        if let Some((tool_name, input)) = parse_json_tool_call_object(inner, &tool_names) {
            if !calls
                .iter()
                .any(|c| c.name == tool_name && c.input == input)
            {
                info!(
                    tool = tool_name.as_str(),
                    "Recovered tool call from <|plugin|> block"
                );
                calls.push(ToolCall {
                    id: format!("recovered_{}", uuid::Uuid::new_v4()),
                    name: tool_name,
                    input,
                });
            }
        }
    }

    // Pattern 11: Action: tool_name\nAction Input: {JSON} (ReAct-style, LM Studio / GPT-OSS)
    {
        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i].trim();
            if let Some(tool_part) = line
                .strip_prefix("Action:")
                .or_else(|| line.strip_prefix("action:"))
            {
                let tool_name = tool_part.trim();
                if tool_names.contains(&tool_name) {
                    // Look for "Action Input:" on the next line(s)
                    if i + 1 < lines.len() {
                        let next = lines[i + 1].trim();
                        if let Some(json_part) = next
                            .strip_prefix("Action Input:")
                            .or_else(|| next.strip_prefix("action input:"))
                            .or_else(|| next.strip_prefix("action_input:"))
                        {
                            let json_str = json_part.trim();
                            if let Ok(input) = serde_json::from_str::<serde_json::Value>(json_str) {
                                if !calls
                                    .iter()
                                    .any(|c| c.name == tool_name && c.input == input)
                                {
                                    info!(
                                        tool = tool_name,
                                        "Recovered tool call from Action/Action Input pattern"
                                    );
                                    calls.push(ToolCall {
                                        id: format!("recovered_{}", uuid::Uuid::new_v4()),
                                        name: tool_name.to_string(),
                                        input,
                                    });
                                }
                            }
                            i += 2;
                            continue;
                        }
                    }
                }
            }
            i += 1;
        }
    }

    // Pattern 12: tool_name\n{"key":"value"} — bare name + JSON on next line (Llama 4 Scout)
    {
        let lines: Vec<&str> = text.lines().collect();
        for i in 0..lines.len().saturating_sub(1) {
            let name_line = lines[i].trim();
            // Tool name must be a single word matching a known tool
            if name_line.contains(' ') || name_line.contains('{') || name_line.is_empty() {
                continue;
            }
            if !tool_names.contains(&name_line) {
                continue;
            }
            // Next line must be valid JSON
            let json_line = lines[i + 1].trim();
            if !json_line.starts_with('{') {
                continue;
            }
            if let Ok(input) = serde_json::from_str::<serde_json::Value>(json_line) {
                if !calls
                    .iter()
                    .any(|c| c.name == name_line && c.input == input)
                {
                    info!(
                        tool = name_line,
                        "Recovered tool call from name+JSON line pair"
                    );
                    calls.push(ToolCall {
                        id: format!("recovered_{}", uuid::Uuid::new_v4()),
                        name: name_line.to_string(),
                        input,
                    });
                }
            }
        }
    }

    // Pattern 13: <tool_use>JSON</tool_use> (Llama 3.1+ variant)
    search_from = 0;
    while let Some(start) = text[search_from..].find("<tool_use>") {
        let abs_start = search_from + start;
        let after_tag = abs_start + "<tool_use>".len();

        let Some(close_offset) = text[after_tag..].find("</tool_use>") else {
            search_from = after_tag;
            continue;
        };
        let inner = text[after_tag..after_tag + close_offset].trim();
        search_from = after_tag + close_offset + "</tool_use>".len();

        if let Some((tool_name, input)) = parse_json_tool_call_object(inner, &tool_names) {
            if !calls
                .iter()
                .any(|c| c.name == tool_name && c.input == input)
            {
                info!(
                    tool = tool_name.as_str(),
                    "Recovered tool call from <tool_use> block"
                );
                calls.push(ToolCall {
                    id: format!("recovered_{}", uuid::Uuid::new_v4()),
                    name: tool_name,
                    input,
                });
            }
        }
    }

    // Pattern 8: Bare JSON tool call objects in text (common Ollama fallback)
    // Matches: {"name":"tool_name","arguments":{"key":"value"}} not already inside tags
    // Only try this if no calls were found by tag-based patterns, to avoid false positives.
    if calls.is_empty() {
        // Scan for JSON objects that look like tool calls
        let mut scan_from = 0;
        while let Some(brace_start) = text[scan_from..].find('{') {
            let abs_brace = scan_from + brace_start;
            // Try to parse a JSON object starting here
            if let Some((tool_name, input)) =
                try_parse_bare_json_tool_call(&text[abs_brace..], &tool_names)
            {
                if !calls
                    .iter()
                    .any(|c| c.name == tool_name && c.input == input)
                {
                    info!(
                        tool = tool_name.as_str(),
                        "Recovered tool call from bare JSON object in text"
                    );
                    calls.push(ToolCall {
                        id: format!("recovered_{}", uuid::Uuid::new_v4()),
                        name: tool_name,
                        input,
                    });
                }
            }
            scan_from = abs_brace + 1;
        }
    }

    calls
}

/// Parse a JSON object that represents a tool call.
/// Supports formats:
/// - `{"name":"tool","arguments":{"key":"value"}}`
/// - `{"name":"tool","parameters":{"key":"value"}}`
/// - `{"function":"tool","arguments":{"key":"value"}}`
/// - `{"tool":"tool_name","args":{"key":"value"}}`
fn parse_json_tool_call_object(
    text: &str,
    tool_names: &[&str],
) -> Option<(String, serde_json::Value)> {
    let obj: serde_json::Value = serde_json::from_str(text).ok()?;
    let obj = obj.as_object()?;

    // Extract tool name from various field names
    let name = obj
        .get("name")
        .or_else(|| obj.get("function"))
        .or_else(|| obj.get("tool"))
        .and_then(|v| v.as_str())?;

    if !tool_names.contains(&name) {
        return None;
    }

    // Extract arguments from various field names
    let args = obj
        .get("arguments")
        .or_else(|| obj.get("parameters"))
        .or_else(|| obj.get("args"))
        .or_else(|| obj.get("input"))
        .cloned()
        .unwrap_or(serde_json::json!({}));

    // If arguments is a string (some models stringify it), try to parse it
    let args = if let Some(s) = args.as_str() {
        serde_json::from_str(s).unwrap_or(serde_json::json!({}))
    } else {
        args
    };

    Some((name.to_string(), args))
}

fn unescape_xml_entities(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&apos;", "'")
}

fn parse_xml_parameter_blocks(text: &str) -> Option<serde_json::Value> {
    use regex_lite::Regex;

    let re = Regex::new(r#"(?s)<parameter=([A-Za-z0-9_.:-]+)>\s*(.*?)\s*</parameter>"#).unwrap();
    let mut params = serde_json::Map::new();

    for caps in re.captures_iter(text) {
        let Some(name) = caps.get(1).map(|m| m.as_str().trim()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }

        let raw_value = caps.get(2).map(|m| m.as_str()).unwrap_or_default();
        let value_text = unescape_xml_entities(raw_value).trim().to_string();
        let value =
            serde_json::from_str(&value_text).unwrap_or(serde_json::Value::String(value_text));
        params.insert(name.to_string(), value);
    }

    if params.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(params))
    }
}

/// Parse the custom arrow syntax used by some Ollama models:
/// `{tool => "name", args => {--key "value"}}` or `{tool => "name", args => {"key":"value"}}`
fn parse_arrow_syntax_tool_call(
    text: &str,
    tool_names: &[&str],
) -> Option<(String, serde_json::Value)> {
    // Extract tool name: look for `tool => "name"` or `tool=>"name"`
    let tool_marker_pos = text.find("tool")?;
    let after_tool = &text[tool_marker_pos + 4..];
    // Skip whitespace and `=>`
    let after_arrow = after_tool.trim_start();
    let after_arrow = after_arrow.strip_prefix("=>")?;
    let after_arrow = after_arrow.trim_start();

    // Extract quoted tool name
    let tool_name = if let Some(stripped) = after_arrow.strip_prefix('"') {
        let end_quote = stripped.find('"')?;
        &stripped[..end_quote]
    } else {
        // Unquoted: take until comma, whitespace, or '}'
        let end = after_arrow
            .find(|c: char| c == ',' || c == '}' || c.is_whitespace())
            .unwrap_or(after_arrow.len());
        &after_arrow[..end]
    };

    if tool_name.is_empty() || !tool_names.contains(&tool_name) {
        return None;
    }

    // Extract args: look for `args => {` or `args=>{`
    let args_value = if let Some(args_pos) = text.find("args") {
        let after_args = &text[args_pos + 4..];
        let after_args = after_args.trim_start();
        let after_args = after_args.strip_prefix("=>")?;
        let after_args = after_args.trim_start();

        if after_args.starts_with('{') {
            // Try standard JSON parse first
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(after_args) {
                v
            } else {
                // Parse `--key "value"` / `--key value` style args
                parse_dash_dash_args(after_args)
            }
        } else {
            serde_json::json!({})
        }
    } else {
        serde_json::json!({})
    };

    Some((tool_name.to_string(), args_value))
}

/// Parse `{--key "value", --flag}` or `{--command "ls -F /"}` style arguments
/// into a JSON object.
fn parse_dash_dash_args(text: &str) -> serde_json::Value {
    let mut map = serde_json::Map::new();

    // Strip outer braces — find matching close brace
    let inner = if text.starts_with('{') {
        let mut depth = 0;
        let mut end = text.len();
        for (i, c) in text.char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        text[1..end].trim()
    } else {
        text.trim()
    };

    // Parse --key "value" or --key value pairs
    let mut remaining = inner;
    while let Some(dash_pos) = remaining.find("--") {
        remaining = &remaining[dash_pos + 2..];

        // Extract key: runs until whitespace, '=', '"', or end
        let key_end = remaining
            .find(|c: char| c.is_whitespace() || c == '=' || c == '"')
            .unwrap_or(remaining.len());
        let key = &remaining[..key_end];
        if key.is_empty() {
            continue;
        }
        remaining = &remaining[key_end..];
        remaining = remaining.trim_start();

        // Skip optional '='
        if remaining.starts_with('=') {
            remaining = remaining[1..].trim_start();
        }

        // Extract value
        if remaining.starts_with('"') {
            // Quoted value — find closing quote
            if let Some(end_quote) = remaining[1..].find('"') {
                let value = &remaining[1..1 + end_quote];
                map.insert(
                    key.to_string(),
                    serde_json::Value::String(value.to_string()),
                );
                remaining = &remaining[2 + end_quote..];
            } else {
                // Unclosed quote — take rest
                let value = &remaining[1..];
                map.insert(
                    key.to_string(),
                    serde_json::Value::String(value.to_string()),
                );
                break;
            }
        } else {
            // Unquoted value — take until next --, comma, }, or end
            let val_end = remaining
                .find([',', '}'])
                .or_else(|| remaining.find("--"))
                .unwrap_or(remaining.len());
            let value = remaining[..val_end].trim();
            if !value.is_empty() {
                map.insert(
                    key.to_string(),
                    serde_json::Value::String(value.to_string()),
                );
            } else {
                // Flag with no value — set to true
                map.insert(key.to_string(), serde_json::Value::Bool(true));
            }
            remaining = &remaining[val_end..];
        }

        // Skip comma separator
        remaining = remaining.trim_start();
        if remaining.starts_with(',') {
            remaining = remaining[1..].trim_start();
        }
    }

    serde_json::Value::Object(map)
}

/// Try to parse a bare JSON object as a tool call.
/// The JSON must have a "name"/"function"/"tool" field matching a known tool.
fn try_parse_bare_json_tool_call(
    text: &str,
    tool_names: &[&str],
) -> Option<(String, serde_json::Value)> {
    // Find the end of this JSON object by counting braces
    let mut depth = 0;
    let mut end = 0;
    for (i, c) in text.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    if end == 0 {
        return None;
    }

    parse_json_tool_call_object(&text[..end], tool_names)
}

/// Deduplicate tool calls from the response.
/// Returns a reference to the deduplicated tool calls.
pub fn deduplicate_tool_calls(response: &crate::llm_driver::CompletionResponse) -> Vec<&ToolCall> {
    let mut hash_set = std::collections::HashSet::new();
    let mut deduplicated = Vec::new();
    for tool_call in &response.tool_calls {
        let hash = LoopGuard::compute_hash(&tool_call.name, &tool_call.input);
        if hash_set.insert(hash) {
            deduplicated.push(tool_call);
        }
    }
    deduplicated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm_driver::{CompletionResponse, LlmError};
    use async_trait::async_trait;
    use openfang_types::tool::ToolCall;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn test_max_iterations_constant() {
        assert_eq!(MAX_ITERATIONS, 50);
    }

    /// Issue #1098: when a response carries Thinking blocks, the persisted
    /// assistant turn must keep them so the next turn round-trips reasoning
    /// state to the model.
    #[test]
    fn test_build_assistant_message_preserves_thinking() {
        let response_blocks = vec![
            ContentBlock::Thinking {
                thinking: "Let me reason carefully...".to_string(),
                signature: Some("sig_anthropic_xyz".to_string()),
                provider_metadata: Some(serde_json::json!({
                    "format": "anthropic_extended_thinking"
                })),
            },
            ContentBlock::Text {
                text: "Initial response text".to_string(),
                provider_metadata: None,
            },
        ];
        // Final text might differ from the original Text block (phantom-action
        // recovery / synthesis fallback rewrites it). The helper should adopt
        // final_text into the persisted Text block.
        let final_text = "Initial response text";
        let msg = build_assistant_message_preserving_thinking(&response_blocks, final_text);
        assert_eq!(msg.role, Role::Assistant);
        let blocks = match &msg.content {
            MessageContent::Blocks(b) => b,
            other => panic!("expected blocks, got {other:?}"),
        };
        assert_eq!(blocks.len(), 2, "must preserve thinking + text");
        match &blocks[0] {
            ContentBlock::Thinking {
                thinking,
                signature,
                ..
            } => {
                assert_eq!(thinking, "Let me reason carefully...");
                assert_eq!(signature.as_deref(), Some("sig_anthropic_xyz"));
            }
            _ => panic!("expected Thinking first"),
        }
        match &blocks[1] {
            ContentBlock::Text { text, .. } => assert_eq!(text, "Initial response text"),
            _ => panic!("expected Text second"),
        }
    }

    /// Without thinking, fall back to the legacy `Message::assistant(text)`
    /// shape so existing JSONL mirrors and embeddings keep working.
    #[test]
    fn test_build_assistant_message_no_thinking_is_plain_text() {
        let response_blocks = vec![ContentBlock::Text {
            text: "Hi.".to_string(),
            provider_metadata: None,
        }];
        let msg = build_assistant_message_preserving_thinking(&response_blocks, "Hi.");
        match msg.content {
            MessageContent::Text(t) => assert_eq!(t, "Hi."),
            _ => panic!("expected plain text content for non-thinking responses"),
        }
    }

    /// Final text supplied by the loop (e.g. recovery stub) must replace
    /// the original text part — the persisted message reflects what was
    /// actually returned to the user, not the raw LLM output.
    #[test]
    fn test_build_assistant_message_final_text_replaces_original_text() {
        let response_blocks = vec![
            ContentBlock::Thinking {
                thinking: "deliberation".to_string(),
                signature: None,
                provider_metadata: Some(serde_json::json!({"format": "inline_think"})),
            },
            ContentBlock::Text {
                text: "raw LLM output".to_string(),
                provider_metadata: None,
            },
        ];
        let final_text = "[Task completed — recovered after empty response.]";
        let msg = build_assistant_message_preserving_thinking(&response_blocks, final_text);
        let blocks = match &msg.content {
            MessageContent::Blocks(b) => b,
            _ => panic!("expected blocks"),
        };
        let saved_text = blocks.iter().find_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        });
        assert_eq!(saved_text, Some(final_text));
    }

    /// Issue #1148 — when the LLM hits MaxTokens, the persisted assistant
    /// turn must keep `Thinking` and `RedactedThinking` blocks so reasoning
    /// state survives across the token-limit boundary. The helper used by
    /// the MaxTokens branches is the same `build_assistant_message_preserving_thinking`
    /// that EndTurn uses; this test pins that contract for both block types
    /// so the four MaxTokens persistence sites stay correct.
    #[test]
    fn test_build_assistant_message_preserves_redacted_thinking_for_max_tokens() {
        let response_blocks = vec![
            ContentBlock::Thinking {
                thinking: "Mid-stream reasoning".to_string(),
                signature: Some("sig_xyz".to_string()),
                provider_metadata: Some(serde_json::json!({
                    "format": "anthropic_extended_thinking"
                })),
            },
            ContentBlock::RedactedThinking {
                data: "encrypted_blob_abc".to_string(),
            },
            ContentBlock::Text {
                text: "Partial answer before token limit".to_string(),
                provider_metadata: None,
            },
        ];
        let final_text = "Partial answer before token limit";
        let msg = build_assistant_message_preserving_thinking(&response_blocks, final_text);
        let blocks = match &msg.content {
            MessageContent::Blocks(b) => b,
            other => panic!("expected Blocks content for MaxTokens persistence, got {other:?}"),
        };

        // All reasoning blocks must survive the persistence step so the
        // follow-up "Please continue." turn carries them back to the model.
        let has_thinking = blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::Thinking { .. }));
        let has_redacted = blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::RedactedThinking { .. }));
        assert!(
            has_thinking,
            "Thinking block must be preserved on MaxTokens"
        );
        assert!(
            has_redacted,
            "RedactedThinking block must be preserved on MaxTokens"
        );

        // Verify the opaque blob is byte-identical (Anthropic rejects altered data).
        for b in blocks {
            if let ContentBlock::RedactedThinking { data } = b {
                assert_eq!(data, "encrypted_blob_abc");
            }
        }

        // Final text reflects what the user will see.
        let saved_text = blocks.iter().find_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        });
        assert_eq!(saved_text, Some(final_text));
    }

    /// Issue #1187 — a turn that contains only `RedactedThinking` (no
    /// `Thinking` block) must still trigger the block-preserving path. The
    /// previous gate keyed solely on `Thinking`, so redacted-only turns were
    /// downgraded to plain text and the encrypted blob was lost on the next
    /// request, which Anthropic/Bedrock reject.
    #[test]
    fn test_build_assistant_message_preserves_redacted_only() {
        let response_blocks = vec![
            ContentBlock::RedactedThinking {
                data: "encrypted_only".to_string(),
            },
            ContentBlock::Text {
                text: "Answer".to_string(),
                provider_metadata: None,
            },
        ];
        let msg = build_assistant_message_preserving_thinking(&response_blocks, "Answer");
        let blocks = match &msg.content {
            MessageContent::Blocks(b) => b,
            other => panic!("expected Blocks content for redacted-only turn, got {other:?}"),
        };
        let has_redacted = blocks.iter().any(
            |b| matches!(b, ContentBlock::RedactedThinking { data } if data == "encrypted_only"),
        );
        assert!(
            has_redacted,
            "RedactedThinking-only turn must be preserved as Blocks"
        );
    }

    #[test]
    fn test_retry_constants() {
        assert_eq!(MAX_RETRIES, 3);
        assert_eq!(BASE_RETRY_DELAY_MS, 1000);
    }

    #[test]
    fn test_dynamic_truncate_short_unchanged() {
        use crate::context_budget::{truncate_tool_result_dynamic, ContextBudget};
        let budget = ContextBudget::new(200_000);
        let short = "Hello, world!";
        assert_eq!(truncate_tool_result_dynamic(short, &budget), short);
    }

    #[test]
    fn test_dynamic_truncate_over_limit() {
        use crate::context_budget::{truncate_tool_result_dynamic, ContextBudget};
        let budget = ContextBudget::new(200_000);
        let long = "x".repeat(budget.per_result_cap() + 10_000);
        let result = truncate_tool_result_dynamic(&long, &budget);
        assert!(result.len() <= budget.per_result_cap() + 200);
        assert!(result.contains("[TRUNCATED:"));
    }

    #[test]
    fn test_dynamic_truncate_newline_boundary() {
        use crate::context_budget::{truncate_tool_result_dynamic, ContextBudget};
        // Small budget to force truncation
        let budget = ContextBudget::new(1_000);
        let content = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_tool_result_dynamic(&content, &budget);
        // Should break at a newline, not mid-line
        let before_marker = result.split("[TRUNCATED:").next().unwrap();
        let trimmed = before_marker.trim_end();
        assert!(!trimmed.is_empty());
    }

    #[test]
    fn test_max_continuations_constant() {
        assert_eq!(MAX_CONTINUATIONS, 5);
    }

    #[test]
    fn test_tool_timeout_constant() {
        assert_eq!(TOOL_TIMEOUT_SECS, 120);
        assert_eq!(AGENT_TOOL_TIMEOUT_SECS, 600);
    }

    /// All `tool_timeout_for` cases live in one test (defaults plus env
    /// overrides) to avoid env-var races between parallel test threads.
    /// Issue #1125: operators on slow local inference (vLLM on old GPUs) need
    /// to disable or extend the inter-agent timeout via env var.
    #[test]
    fn test_tool_timeout_for_agent_tools() {
        // Baseline: no env overrides → compiled-in defaults.
        std::env::remove_var("OPENFANG_AGENT_TOOL_TIMEOUT_SECS");
        std::env::remove_var("OPENFANG_TOOL_TIMEOUT_SECS");
        assert_eq!(
            tool_timeout_for("agent_send"),
            Some(Duration::from_secs(600))
        );
        assert_eq!(
            tool_timeout_for("agent_spawn"),
            Some(Duration::from_secs(600))
        );
        assert_eq!(
            tool_timeout_for("file_read"),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            tool_timeout_for("shell_exec"),
            Some(Duration::from_secs(120))
        );

        // Override: set to 0 → timeout disabled.
        std::env::set_var("OPENFANG_AGENT_TOOL_TIMEOUT_SECS", "0");
        std::env::set_var("OPENFANG_TOOL_TIMEOUT_SECS", "0");
        assert_eq!(tool_timeout_for("agent_send"), None);
        assert_eq!(tool_timeout_for("agent_spawn"), None);
        assert_eq!(tool_timeout_for("file_read"), None);

        // Override: custom positive values are honored verbatim.
        std::env::set_var("OPENFANG_AGENT_TOOL_TIMEOUT_SECS", "1800");
        std::env::set_var("OPENFANG_TOOL_TIMEOUT_SECS", "300");
        assert_eq!(
            tool_timeout_for("agent_send"),
            Some(Duration::from_secs(1800))
        );
        assert_eq!(
            tool_timeout_for("file_read"),
            Some(Duration::from_secs(300))
        );

        // Override: unparseable values fall back to compiled-in defaults.
        std::env::set_var("OPENFANG_AGENT_TOOL_TIMEOUT_SECS", "not-a-number");
        std::env::set_var("OPENFANG_TOOL_TIMEOUT_SECS", "");
        assert_eq!(
            tool_timeout_for("agent_send"),
            Some(Duration::from_secs(600))
        );
        assert_eq!(
            tool_timeout_for("file_read"),
            Some(Duration::from_secs(120))
        );

        std::env::remove_var("OPENFANG_AGENT_TOOL_TIMEOUT_SECS");
        std::env::remove_var("OPENFANG_TOOL_TIMEOUT_SECS");
    }

    #[test]
    fn test_max_history_messages() {
        assert_eq!(MAX_HISTORY_MESSAGES, 20);
        assert_eq!(
            openfang_types::agent::DEFAULT_MAX_HISTORY_MESSAGES,
            MAX_HISTORY_MESSAGES
        );
    }

    /// Issue #871: an agent with a manifest override uses that value.
    #[test]
    fn test_effective_max_history_uses_manifest_override() {
        let mut manifest = openfang_types::agent::AgentManifest {
            max_history_messages: Some(40),
            ..Default::default()
        };
        assert_eq!(manifest.effective_max_history_messages(), 40);

        manifest.max_history_messages = Some(6);
        assert_eq!(manifest.effective_max_history_messages(), 6);
    }

    /// Issue #871: an agent without an override falls back to the runtime
    /// default. `Some(0)` is also treated as the default to avoid an agent
    /// accidentally disabling history entirely.
    #[test]
    fn test_effective_max_history_falls_back_to_default() {
        let mut manifest = openfang_types::agent::AgentManifest {
            max_history_messages: None,
            ..Default::default()
        };
        assert_eq!(
            manifest.effective_max_history_messages(),
            MAX_HISTORY_MESSAGES
        );

        manifest.max_history_messages = Some(0);
        assert_eq!(
            manifest.effective_max_history_messages(),
            MAX_HISTORY_MESSAGES
        );
    }

    /// Issue #871: `max_history_messages` round-trips through serde with
    /// `#[serde(default)]`, so manifests without the field still deserialize.
    #[test]
    fn test_manifest_max_history_round_trip_json() {
        let json_no_override = r#"{"name":"worker","module":"builtin:chat"}"#;
        let manifest: openfang_types::agent::AgentManifest =
            serde_json::from_str(json_no_override).unwrap();
        assert_eq!(manifest.max_history_messages, None);
        assert_eq!(
            manifest.effective_max_history_messages(),
            MAX_HISTORY_MESSAGES
        );

        let json_with_override =
            r#"{"name":"orchestrator","module":"builtin:chat","max_history_messages":40}"#;
        let manifest: openfang_types::agent::AgentManifest =
            serde_json::from_str(json_with_override).unwrap();
        assert_eq!(manifest.max_history_messages, Some(40));
        assert_eq!(manifest.effective_max_history_messages(), 40);
    }

    fn sample_image_block() -> ContentBlock {
        ContentBlock::Image {
            media_type: "image/png".to_string(),
            data: "aGVsbG8=".to_string(),
        }
    }

    #[test]
    fn test_build_user_turn_text_only() {
        let msg = build_user_turn_message("hello", None);
        assert_eq!(msg.role, Role::User);
        match msg.content {
            MessageContent::Text(text) => assert_eq!(text, "hello"),
            MessageContent::Blocks(_) => panic!("expected Text content for text-only turn"),
        }
    }

    #[test]
    fn test_build_user_turn_images_only() {
        let msg = build_user_turn_message("", Some(vec![sample_image_block()]));
        assert_eq!(msg.role, Role::User);
        match msg.content {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                assert!(matches!(blocks[0], ContentBlock::Image { .. }));
            }
            MessageContent::Text(_) => panic!("expected Blocks content for images-only turn"),
        }
    }

    #[test]
    fn test_build_user_turn_text_and_images_combined() {
        let msg =
            build_user_turn_message("what is in this image?", Some(vec![sample_image_block()]));
        assert_eq!(msg.role, Role::User);
        match msg.content {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2, "text must be combined with images");
                match &blocks[0] {
                    ContentBlock::Text { text, .. } => {
                        assert_eq!(text, "what is in this image?");
                    }
                    _ => panic!("expected first block to be user text"),
                }
                assert!(matches!(blocks[1], ContentBlock::Image { .. }));
            }
            MessageContent::Text(_) => panic!("expected Blocks content for multimodal turn"),
        }
    }

    #[test]
    fn test_build_user_turn_whitespace_text_treated_as_empty() {
        let msg = build_user_turn_message("   \n", Some(vec![sample_image_block()]));
        match msg.content {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                assert!(matches!(blocks[0], ContentBlock::Image { .. }));
            }
            MessageContent::Text(_) => panic!("expected Blocks content"),
        }
    }

    #[test]
    fn test_build_user_turn_empty_blocks_falls_back_to_text() {
        let msg = build_user_turn_message("hi", Some(Vec::new()));
        match msg.content {
            MessageContent::Text(text) => assert_eq!(text, "hi"),
            MessageContent::Blocks(_) => panic!("expected Text content when blocks are empty"),
        }
    }

    // --- Integration tests for empty response guards ---

    fn test_manifest() -> AgentManifest {
        AgentManifest {
            name: "test-agent".to_string(),
            model: openfang_types::agent::ModelConfig {
                system_prompt: "You are a test agent.".to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Mock driver that simulates: first call returns ToolUse with no text,
    /// second call returns EndTurn with empty text. This reproduces the bug
    /// where the LLM ends with no text after a tool-use cycle.
    struct EmptyAfterToolUseDriver {
        call_count: AtomicU32,
    }

    impl EmptyAfterToolUseDriver {
        fn new() -> Self {
            Self {
                call_count: AtomicU32::new(0),
            }
        }
    }

    #[async_trait]
    impl LlmDriver for EmptyAfterToolUseDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            let call = self.call_count.fetch_add(1, Ordering::Relaxed);
            if call == 0 {
                // First call: LLM wants to use a tool (with no text block)
                Ok(CompletionResponse {
                    content: vec![ContentBlock::ToolUse {
                        id: "tool_1".to_string(),
                        name: "fake_tool".to_string(),
                        input: serde_json::json!({"query": "test"}),
                        provider_metadata: None,
                    }],
                    stop_reason: StopReason::ToolUse,
                    tool_calls: vec![ToolCall {
                        id: "tool_1".to_string(),
                        name: "fake_tool".to_string(),
                        input: serde_json::json!({"query": "test"}),
                    }],
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                })
            } else {
                // Second call: LLM returns EndTurn with EMPTY text (the bug)
                Ok(CompletionResponse {
                    content: vec![],
                    stop_reason: StopReason::EndTurn,
                    tool_calls: vec![],
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 0,
                    },
                })
            }
        }
    }

    /// Mock driver that returns empty text with MaxTokens stop reason,
    /// repeated MAX_CONTINUATIONS times to trigger the max continuations path.
    struct EmptyMaxTokensDriver;

    #[async_trait]
    impl LlmDriver for EmptyMaxTokensDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            Ok(CompletionResponse {
                content: vec![],
                stop_reason: StopReason::MaxTokens,
                tool_calls: vec![],
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 0,
                },
            })
        }
    }

    /// Mock driver that returns normal text (sanity check).
    struct NormalDriver;

    #[async_trait]
    impl LlmDriver for NormalDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: "Hello from the agent!".to_string(),
                    provider_metadata: None,
                }],
                stop_reason: StopReason::EndTurn,
                tool_calls: vec![],
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 8,
                },
            })
        }
    }

    /// FANG-13. The driver runs a tool, then ends the turn with an empty
    /// message. Until this fix the loop answered `Ok("[Task completed — the
    /// agent executed tools but did not produce a text summary.]")`, i.e. the
    /// runtime asserted completion on a turn the provider never answered. The
    /// turn now fails, and the failure names what happened — including that the
    /// tools did run, because their side effects are real.
    #[tokio::test]
    async fn test_empty_response_after_tool_use_fails_the_turn() {
        let memory = openfang_memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = openfang_types::agent::AgentId::new();
        let mut session = openfang_memory::session::Session {
            id: openfang_types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(EmptyAfterToolUseDriver::new());

        let result = run_agent_loop(
            &manifest,
            "Do something with tools",
            &mut session,
            &memory,
            driver,
            &[], // no tools registered — the tool call will fail, which is fine
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // on_phase
            None, // media_engine
            None, // tts_engine
            None, // docker_config
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
        )
        .await;

        let err = result.expect_err("an empty final message must fail the turn");
        assert!(
            matches!(err, OpenFangError::LlmDriver(_)),
            "empty response is a provider failure, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains(NO_TEXT_FAILURE_PREFIX) && msg.contains("carried no text"),
            "error should name the cause, got: {msg}"
        );
        assert!(
            !msg.contains("Task completed"),
            "no completion may be claimed for a turn with no answer, got: {msg}"
        );
        assert!(
            msg.contains("Tools executed earlier in this turn did run"),
            "the tool side effects must be disclosed, got: {msg}"
        );
    }

    #[tokio::test]
    async fn test_tool_error_injects_no_fabrication_guidance() {
        let memory = openfang_memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = openfang_types::agent::AgentId::new();
        let mut session = openfang_memory::session::Session {
            id: openfang_types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(EmptyAfterToolUseDriver::new());

        run_agent_loop(
            &manifest,
            "Do something with tools",
            &mut session,
            &memory,
            driver,
            &[], // no tools registered — the tool call will fail, which is fine
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // on_phase
            None, // media_engine
            None, // tts_engine
            None, // docker_config
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
        )
        .await
        // This driver ends the turn with an empty message, which since FANG-13
        // is a failure. The subject here is what landed in `session` on the way
        // — the guidance injected after the failed tool call — and `session` is
        // borrowed mutably, so it carries the turn's messages either way.
        .expect_err("this driver's empty final message must fail the turn");

        let guidance_seen = session.messages.iter().any(|msg| {
            match &msg.content {
            MessageContent::Blocks(blocks) => blocks.iter().any(|block| {
                matches!(block, ContentBlock::Text { text, .. } if text == TOOL_ERROR_GUIDANCE)
            }),
            _ => false,
        }
        });

        assert!(
            guidance_seen,
            "Expected tool error guidance in session messages after failed tool call"
        );
    }

    #[tokio::test]
    async fn test_empty_response_max_tokens_fails_the_turn() {
        let memory = openfang_memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = openfang_types::agent::AgentId::new();
        let mut session = openfang_memory::session::Session {
            id: openfang_types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(EmptyMaxTokensDriver);

        let result = run_agent_loop(
            &manifest,
            "Tell me something long",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // on_phase
            None, // media_engine
            None, // tts_engine
            None, // docker_config
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
        )
        .await
        // FANG-13. This driver truncates at the token limit MAX_CONTINUATIONS
        // times and carries no text on any of them. The loop used to answer
        // that with "[Partial response — token limit reached with no text
        // output.]" and Ok — a sentence of the runtime's own in the field
        // where the model's answer belongs, on a turn where nothing was said.
        // It is a failed turn.
        .expect_err("a spent continuation budget with no text must fail the turn");

        let msg = format!("{result}");
        assert!(
            msg.contains(NO_TEXT_FAILURE_PREFIX),
            "expected the no-text failure, got: {msg:?}"
        );
        assert!(
            msg.contains("finish_reason=length"),
            "the message must say what was actually observed, got: {msg:?}"
        );
        assert!(
            !msg.contains("token limit"),
            "'token limit' is a context-overflow pattern in llm_errors::classify_error \
             and would come back to the user as 'Context is full'; got: {msg:?}"
        );
    }

    #[tokio::test]
    async fn test_normal_response_not_replaced_by_fallback() {
        let memory = openfang_memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = openfang_types::agent::AgentId::new();
        let mut session = openfang_memory::session::Session {
            id: openfang_types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(NormalDriver);

        let result = run_agent_loop(
            &manifest,
            "Say hello",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // on_phase
            None, // media_engine
            None, // tts_engine
            None, // docker_config
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
        )
        .await
        .expect("Loop should complete without error");

        // Normal response should pass through unchanged
        assert_eq!(result.response, "Hello from the agent!");
    }

    /// FANG-13, streaming half — the surface the dashboard and SSE read. It
    /// must fail exactly as the non-streaming loop does; a difference here is
    /// a difference between what the web UI is told and what REST is told.
    #[tokio::test]
    async fn test_streaming_empty_response_after_tool_use_fails_the_turn() {
        let memory = openfang_memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = openfang_types::agent::AgentId::new();
        let mut session = openfang_memory::session::Session {
            id: openfang_types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(EmptyAfterToolUseDriver::new());
        let (tx, _rx) = mpsc::channel(64);

        let result = run_agent_loop_streaming(
            &manifest,
            "Do something with tools",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            tx,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // on_phase
            None, // media_engine
            None, // tts_engine
            None, // docker_config
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
        )
        .await;

        let err = result.expect_err("an empty final streamed message must fail the turn");
        assert!(
            matches!(err, OpenFangError::LlmDriver(_)),
            "empty response is a provider failure, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains(NO_TEXT_FAILURE_PREFIX) && msg.contains("streamed"),
            "error should name the cause and the path, got: {msg}"
        );
        assert!(
            !msg.contains("Task completed"),
            "no completion may be claimed for a turn with no answer, got: {msg}"
        );
    }

    /// Mock driver that returns empty text on first call (EndTurn), then normal text on second.
    /// This tests the one-shot retry logic for iteration 0 empty responses.
    struct EmptyThenNormalDriver {
        call_count: AtomicU32,
    }

    impl EmptyThenNormalDriver {
        fn new() -> Self {
            Self {
                call_count: AtomicU32::new(0),
            }
        }
    }

    #[async_trait]
    impl LlmDriver for EmptyThenNormalDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            let call = self.call_count.fetch_add(1, Ordering::Relaxed);
            if call == 0 {
                // First call: empty EndTurn (triggers retry)
                Ok(CompletionResponse {
                    content: vec![],
                    stop_reason: StopReason::EndTurn,
                    tool_calls: vec![],
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 0,
                    },
                })
            } else {
                // Second call (retry): normal response
                Ok(CompletionResponse {
                    content: vec![ContentBlock::Text {
                        text: "Recovered after retry!".to_string(),
                        provider_metadata: None,
                    }],
                    stop_reason: StopReason::EndTurn,
                    tool_calls: vec![],
                    usage: TokenUsage {
                        input_tokens: 15,
                        output_tokens: 8,
                    },
                })
            }
        }
    }

    /// Mock driver that always returns empty EndTurn (no recovery on retry).
    /// Tests that the fallback message appears when retry also fails.
    struct AlwaysEmptyDriver;

    #[async_trait]
    impl LlmDriver for AlwaysEmptyDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            Ok(CompletionResponse {
                content: vec![],
                stop_reason: StopReason::EndTurn,
                tool_calls: vec![],
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 0,
                },
            })
        }
    }

    #[tokio::test]
    async fn test_empty_first_response_retries_and_recovers() {
        let memory = openfang_memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = openfang_types::agent::AgentId::new();
        let mut session = openfang_memory::session::Session {
            id: openfang_types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(EmptyThenNormalDriver::new());

        let result = run_agent_loop(
            &manifest,
            "Hello",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
        )
        .await
        .expect("Loop should recover via retry");

        assert_eq!(result.response, "Recovered after retry!");
        assert_eq!(
            result.iterations, 2,
            "Should have taken 2 iterations (retry)"
        );
    }

    /// FANG-13, the no-tools half of the guard: the one-shot retry fires on
    /// iteration 0 and the second answer is empty too. Two LLM calls, no
    /// answer — previously HTTP 200 carrying "[The model returned an empty
    /// response …]", a sentence the runtime wrote itself.
    #[tokio::test]
    async fn test_empty_first_response_fails_when_retry_also_empty() {
        let memory = openfang_memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = openfang_types::agent::AgentId::new();
        let mut session = openfang_memory::session::Session {
            id: openfang_types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(AlwaysEmptyDriver);

        let result = run_agent_loop(
            &manifest,
            "Hello",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
        )
        .await;

        let err = result.expect_err("two empty answers in a row must fail the turn");
        let msg = err.to_string();
        assert!(
            matches!(err, OpenFangError::LlmDriver(_)) && msg.contains(NO_TEXT_FAILURE_PREFIX),
            "expected a provider failure naming the empty response, got: {msg}"
        );
        // No tool ran, so nothing may be claimed about tool side effects.
        assert!(
            !msg.contains("Tools executed earlier"),
            "no tools ran in this turn, got: {msg}"
        );
        assert!(
            msg.contains("2 iteration(s)"),
            "the retry must be counted, got: {msg}"
        );
    }

    #[tokio::test]
    async fn test_max_history_messages_constant() {
        assert_eq!(MAX_HISTORY_MESSAGES, 20);
    }

    #[tokio::test]
    async fn test_streaming_empty_response_max_tokens_fails_the_turn() {
        let memory = openfang_memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = openfang_types::agent::AgentId::new();
        let mut session = openfang_memory::session::Session {
            id: openfang_types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(EmptyMaxTokensDriver);
        let (tx, _rx) = mpsc::channel(64);

        let result = run_agent_loop_streaming(
            &manifest,
            "Tell me something long",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            tx,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // on_phase
            None, // media_engine
            None, // tts_engine
            None, // docker_config
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
        )
        .await
        // FANG-13 — the streaming twin of
        // `test_empty_response_max_tokens_fails_the_turn`.
        .expect_err("a spent continuation budget with no text must fail the turn");

        let msg = format!("{result}");
        assert!(
            msg.contains(NO_TEXT_FAILURE_PREFIX),
            "expected the no-text failure, got: {msg:?}"
        );
        assert!(
            msg.contains("finish_reason=length"),
            "the message must say what was actually observed, got: {msg:?}"
        );
    }

    #[test]
    fn test_recover_text_tool_calls_basic() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({}),
        }];
        let text =
            r#"Let me search for that. <function=web_search>{"query":"rust async"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].input["query"], "rust async");
        assert!(calls[0].id.starts_with("recovered_"));
    }

    #[test]
    fn test_recover_text_tool_calls_xml_parameters() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function=shell_exec><parameter=command>python3 "/tmp/run.py" --flag value</parameter></function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(
            calls[0].input["command"],
            r#"python3 "/tmp/run.py" --flag value"#
        );
    }

    #[test]
    fn test_recover_text_tool_calls_xml_parameters_with_wrapper() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<tool_call>
<function=shell_exec>
<parameter=command>python3 "/tmp/poll.py" --job-id "abc123"</parameter>
</function>
</tool_call>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(
            calls[0].input["command"],
            r#"python3 "/tmp/poll.py" --job-id "abc123""#
        );
    }

    #[test]
    fn test_recover_text_tool_calls_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function=hack_system>{"cmd":"rm -rf /"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty(), "Unknown tools should be rejected");
    }

    #[test]
    fn test_recover_text_tool_calls_invalid_json() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function=web_search>not valid json</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty(), "Invalid JSON should be skipped");
    }

    #[test]
    fn test_recover_text_tool_calls_multiple() {
        let tools = vec![
            ToolDefinition {
                name: "web_search".into(),
                description: "Search".into(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "read_file".into(),
                description: "Read a file".into(),
                input_schema: serde_json::json!({}),
            },
        ];
        let text = r#"<function=web_search>{"query":"hello"}</function> then <function=read_file>{"path":"a.txt"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[1].name, "read_file");
    }

    #[test]
    fn test_recover_text_tool_calls_no_pattern() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "Just a normal response with no tool calls.";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_recover_text_tool_calls_empty_tools() {
        let text = r#"<function=web_search>{"query":"hello"}</function>"#;
        let calls = recover_text_tool_calls(text, &[]);
        assert!(calls.is_empty(), "No tools = no recovery");
    }

    // --- Deep edge-case tests for text-to-tool recovery ---

    #[test]
    fn test_recover_text_tool_calls_nested_json() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function=web_search>{"query":"rust","filters":{"lang":"en","year":2024}}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input["filters"]["lang"], "en");
    }

    #[test]
    fn test_recover_text_tool_calls_with_surrounding_text() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "Sure, let me search that for you.\n\n<function=web_search>{\"query\":\"rust async programming\"}</function>\n\nI'll get back to you with results.";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input["query"], "rust async programming");
    }

    #[test]
    fn test_recover_text_tool_calls_whitespace_in_json() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        // Some models emit pretty-printed JSON
        let text = "<function=web_search>\n  {\"query\": \"hello world\"}\n</function>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input["query"], "hello world");
    }

    #[test]
    fn test_recover_text_tool_calls_unclosed_tag() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        // Missing </function> — should gracefully skip
        let text = r#"<function=web_search>{"query":"test"}"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty(), "Unclosed tag should be skipped");
    }

    #[test]
    fn test_recover_text_tool_calls_missing_closing_bracket() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        // Missing > after tool name
        let text = r#"<function=web_search{"query":"test"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        // The parser finds > inside JSON, will likely produce invalid tool name
        // or invalid JSON — either way, should not panic
        // (just verifying no panic / no bad behavior)
        let _ = calls;
    }

    #[test]
    fn test_recover_text_tool_calls_empty_json_object() {
        let tools = vec![ToolDefinition {
            name: "list_files".into(),
            description: "List".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function=list_files>{}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "list_files");
        assert_eq!(calls[0].input, serde_json::json!({}));
    }

    #[test]
    fn test_recover_text_tool_calls_mixed_valid_invalid() {
        let tools = vec![
            ToolDefinition {
                name: "web_search".into(),
                description: "Search".into(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "read_file".into(),
                description: "Read".into(),
                input_schema: serde_json::json!({}),
            },
        ];
        // First: valid, second: unknown tool, third: valid
        let text = r#"<function=web_search>{"q":"a"}</function> <function=unknown>{"x":1}</function> <function=read_file>{"path":"b"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 2, "Should recover 2 valid, skip 1 unknown");
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[1].name, "read_file");
    }

    // --- Variant 2 pattern tests: <function>NAME{JSON}</function> ---

    #[test]
    fn test_recover_variant2_basic() {
        let tools = vec![ToolDefinition {
            name: "web_fetch".into(),
            description: "Fetch".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function>web_fetch{"url":"https://example.com"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_fetch");
        assert_eq!(calls[0].input["url"], "https://example.com");
    }

    #[test]
    fn test_recover_variant2_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function>unknown_tool{"q":"test"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn test_recover_variant2_with_surrounding_text() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"Let me search for that. <function>web_search{"query":"rust lang"}</function> I'll find the answer."#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
    }

    #[test]
    fn test_recover_both_variants_mixed() {
        let tools = vec![
            ToolDefinition {
                name: "web_search".into(),
                description: "Search".into(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "web_fetch".into(),
                description: "Fetch".into(),
                input_schema: serde_json::json!({}),
            },
        ];
        // Mix of variant 1 and variant 2
        let text = r#"<function=web_search>{"q":"a"}</function> <function>web_fetch{"url":"https://x.com"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[1].name, "web_fetch");
    }

    #[test]
    fn test_recover_tool_tag_variant() {
        let tools = vec![ToolDefinition {
            name: "exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"I'll run that for you. <tool>exec{"command":"ls -la"}</tool>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "exec");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn test_recover_markdown_code_block() {
        let tools = vec![ToolDefinition {
            name: "exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "I'll execute that command:\n```\nexec {\"command\": \"ls -la\"}\n```";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "exec");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn test_recover_markdown_code_block_with_lang() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "```json\nweb_search {\"query\": \"rust\"}\n```";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
    }

    #[test]
    fn test_recover_backtick_wrapped() {
        let tools = vec![ToolDefinition {
            name: "exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"Let me run `exec {"command":"pwd"}` for you."#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "exec");
        assert_eq!(calls[0].input["command"], "pwd");
    }

    #[test]
    fn test_recover_backtick_ignores_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"Try `unknown_tool {"key":"val"}` instead."#;
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_recover_no_duplicates_across_patterns() {
        let tools = vec![ToolDefinition {
            name: "exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        // Same call in both function tag and tool tag — should only appear once
        let text =
            r#"<function=exec>{"command":"ls"}</function> <tool>exec{"command":"ls"}</tool>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
    }

    // --- Pattern 6: [TOOL_CALL]...[/TOOL_CALL] tests (issue #354) ---

    #[test]
    fn test_recover_tool_call_block_json() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute shell command".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "[TOOL_CALL]\n{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls -la\"}}\n[/TOOL_CALL]";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn test_recover_tool_call_block_arrow_syntax() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute shell command".into(),
            input_schema: serde_json::json!({}),
        }];
        // Exact format from issue #354
        let text = "[TOOL_CALL]\n{tool => \"shell_exec\", args => {\n--command \"ls -F /\"\n}}\n[/TOOL_CALL]";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input["command"], "ls -F /");
    }

    #[test]
    fn test_recover_tool_call_block_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "[TOOL_CALL]\n{\"name\": \"hack_system\", \"arguments\": {\"cmd\": \"rm -rf /\"}}\n[/TOOL_CALL]";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_recover_tool_call_block_multiple() {
        let tools = vec![
            ToolDefinition {
                name: "shell_exec".into(),
                description: "Execute".into(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "file_read".into(),
                description: "Read".into(),
                input_schema: serde_json::json!({}),
            },
        ];
        let text = "[TOOL_CALL]\n{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls\"}}\n[/TOOL_CALL]\nSome text.\n[TOOL_CALL]\n{\"name\": \"file_read\", \"arguments\": {\"path\": \"/tmp/test.txt\"}}\n[/TOOL_CALL]";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[1].name, "file_read");
    }

    #[test]
    fn test_recover_tool_call_block_unclosed() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        // Unclosed [TOOL_CALL] — pattern 6 skips it, but pattern 8 (bare JSON)
        // still finds the valid JSON tool call object.
        let text = "[TOOL_CALL]\n{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls\"}}";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1, "Bare JSON fallback should recover this");
        assert_eq!(calls[0].name, "shell_exec");
    }

    // --- Pattern 7: <tool_call>JSON</tool_call> tests (Qwen3, issue #332) ---

    #[test]
    fn test_recover_tool_call_xml_basic() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<tool_call>\n{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls -la\"}}\n</tool_call>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn test_recover_tool_call_xml_with_surrounding_text() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "I'll search for that.\n\n<tool_call>\n{\"name\": \"web_search\", \"arguments\": {\"query\": \"rust async\"}}\n</tool_call>\n\nLet me get results.";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].input["query"], "rust async");
    }

    #[test]
    fn test_recover_tool_call_xml_function_field() {
        let tools = vec![ToolDefinition {
            name: "file_read".into(),
            description: "Read".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<tool_call>{\"function\": \"file_read\", \"arguments\": {\"path\": \"/etc/hosts\"}}</tool_call>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_read");
    }

    #[test]
    fn test_recover_tool_call_xml_parameters_field() {
        let tools = vec![ToolDefinition {
            name: "web_fetch".into(),
            description: "Fetch".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<tool_call>{\"name\": \"web_fetch\", \"parameters\": {\"url\": \"https://example.com\"}}</tool_call>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_fetch");
        assert_eq!(calls[0].input["url"], "https://example.com");
    }

    #[test]
    fn test_recover_tool_call_xml_stringified_args() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<tool_call>{\"name\": \"shell_exec\", \"arguments\": \"{\\\"command\\\": \\\"pwd\\\"}\"}</tool_call>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input["command"], "pwd");
    }

    #[test]
    fn test_recover_tool_call_xml_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<tool_call>{\"name\": \"hack_system\", \"arguments\": {\"cmd\": \"rm -rf /\"}}</tool_call>";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_recover_tool_call_xml_multiple() {
        let tools = vec![
            ToolDefinition {
                name: "shell_exec".into(),
                description: "Execute".into(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "web_search".into(),
                description: "Search".into(),
                input_schema: serde_json::json!({}),
            },
        ];
        let text = "<tool_call>{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls\"}}</tool_call>\n<tool_call>{\"name\": \"web_search\", \"arguments\": {\"query\": \"rust\"}}</tool_call>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[1].name, "web_search");
    }

    // --- Pattern 8: Bare JSON tool call object tests ---

    #[test]
    fn test_recover_bare_json_tool_call() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text =
            "I'll run that: {\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls -la\"}}";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn test_recover_bare_json_no_false_positive() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "The config looks like {\"debug\": true, \"level\": \"info\"}";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_recover_bare_json_skipped_when_tags_found() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<function=shell_exec>{\"command\":\"ls\"}</function> {\"name\": \"shell_exec\", \"arguments\": {\"command\": \"pwd\"}}";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input["command"], "ls");
    }

    // --- Pattern 9: XML-attribute style <function name="..." parameters="..." /> ---

    #[test]
    fn test_recover_xml_attribute_basic() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function name="web_search" parameters="{&quot;query&quot;: &quot;best crypto 2024&quot;}" />"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].input["query"], "best crypto 2024");
    }

    #[test]
    fn test_recover_xml_attribute_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function name="unknown_tool" parameters="{&quot;x&quot;: 1}" />"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_recover_xml_attribute_non_selfclosing() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function name="shell_exec" parameters="{&quot;command&quot;: &quot;ls&quot;}"></function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
    }

    // --- Pattern 10: <|plugin|>...<|endofblock|> tests ---

    #[test]
    fn test_recover_plugin_block() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<|plugin|>\n{\"name\": \"web_search\", \"arguments\": {\"query\": \"rust\"}}\n<|endofblock|>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].input["query"], "rust");
    }

    #[test]
    fn test_recover_plugin_block_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text =
            "<|plugin|>\n{\"name\": \"hack\", \"arguments\": {\"cmd\": \"rm\"}}\n<|endofblock|>";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    // --- Pattern 11: Action/Action Input tests ---

    #[test]
    fn test_recover_action_input() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "Action: web_search\nAction Input: {\"query\": \"rust programming\"}";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].input["query"], "rust programming");
    }

    #[test]
    fn test_recover_action_input_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "Action: unknown_tool\nAction Input: {\"key\": \"value\"}";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    // --- Pattern 12: name + JSON on next line tests ---

    #[test]
    fn test_recover_name_json_nextline() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "shell_exec\n{\"command\": \"ls -la\"}";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn test_recover_name_json_nextline_unknown() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "unknown_tool\n{\"command\": \"ls\"}";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    // --- Pattern 13: <tool_use> tests ---

    #[test]
    fn test_recover_tool_use_block() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text =
            "<tool_use>{\"name\": \"web_search\", \"arguments\": {\"query\": \"test\"}}</tool_use>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
    }

    #[test]
    fn test_recover_tool_use_block_unknown() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<tool_use>{\"name\": \"hack\", \"arguments\": {\"cmd\": \"rm\"}}</tool_use>";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    // --- Helper function tests ---

    #[test]
    fn test_parse_dash_dash_args_basic() {
        let result = parse_dash_dash_args("{--command \"ls -F /\"}");
        assert_eq!(result["command"], "ls -F /");
    }

    #[test]
    fn test_parse_dash_dash_args_multiple() {
        let result = parse_dash_dash_args("{--file \"test.txt\", --verbose}");
        assert_eq!(result["file"], "test.txt");
        assert_eq!(result["verbose"], true);
    }

    #[test]
    fn test_parse_dash_dash_args_unquoted_value() {
        let result = parse_dash_dash_args("{--count 5}");
        assert_eq!(result["count"], "5");
    }

    #[test]
    fn test_parse_json_tool_call_object_standard() {
        let tool_names = vec!["shell_exec"];
        let result = parse_json_tool_call_object(
            "{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls\"}}",
            &tool_names,
        );
        assert!(result.is_some());
        let (name, args) = result.unwrap();
        assert_eq!(name, "shell_exec");
        assert_eq!(args["command"], "ls");
    }

    #[test]
    fn test_parse_json_tool_call_object_function_field() {
        let tool_names = vec!["web_fetch"];
        let result = parse_json_tool_call_object(
            "{\"function\": \"web_fetch\", \"parameters\": {\"url\": \"https://x.com\"}}",
            &tool_names,
        );
        assert!(result.is_some());
        let (name, args) = result.unwrap();
        assert_eq!(name, "web_fetch");
        assert_eq!(args["url"], "https://x.com");
    }

    #[test]
    fn test_parse_json_tool_call_object_unknown_tool() {
        let tool_names = vec!["shell_exec"];
        let result =
            parse_json_tool_call_object("{\"name\": \"unknown\", \"arguments\": {}}", &tool_names);
        assert!(result.is_none());
    }

    // --- End-to-end integration test: text-as-tool-call recovery through agent loop ---

    /// Mock driver that simulates a Groq/Llama model outputting tool calls as text.
    /// Call 1: Returns text with `<function=web_search>...</function>` (EndTurn, no tool_calls)
    /// Call 2: Returns a normal text response (after tool result is provided)
    struct TextToolCallDriver {
        call_count: AtomicU32,
    }

    impl TextToolCallDriver {
        fn new() -> Self {
            Self {
                call_count: AtomicU32::new(0),
            }
        }
    }

    /// Mock driver that emits nested XML parameter-style tool calls as plain text.
    struct NestedXmlTextToolCallDriver {
        call_count: AtomicU32,
    }

    impl NestedXmlTextToolCallDriver {
        fn new() -> Self {
            Self {
                call_count: AtomicU32::new(0),
            }
        }
    }

    #[async_trait]
    impl LlmDriver for NestedXmlTextToolCallDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            let call = self.call_count.fetch_add(1, Ordering::Relaxed);
            if call == 0 {
                Ok(CompletionResponse {
                    content: vec![ContentBlock::Text {
                        text: "<tool_call><function=web_search><parameter=query>rust async</parameter></function></tool_call>".to_string(),
                        provider_metadata: None,
                    }],
                    stop_reason: StopReason::EndTurn,
                    tool_calls: vec![],
                    usage: TokenUsage {
                        input_tokens: 18,
                        output_tokens: 10,
                    },
                })
            } else {
                Ok(CompletionResponse {
                    content: vec![ContentBlock::Text {
                        text: "Recovered nested XML tool call successfully.".to_string(),
                        provider_metadata: None,
                    }],
                    stop_reason: StopReason::EndTurn,
                    tool_calls: vec![],
                    usage: TokenUsage {
                        input_tokens: 24,
                        output_tokens: 8,
                    },
                })
            }
        }
    }

    #[async_trait]
    impl LlmDriver for TextToolCallDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            let call = self.call_count.fetch_add(1, Ordering::Relaxed);
            if call == 0 {
                // Simulate Groq/Llama: tool call as text, not in tool_calls field
                Ok(CompletionResponse {
                    content: vec![ContentBlock::Text {
                        text: r#"Let me search for that. <function=web_search>{"query":"rust async"}</function>"#.to_string(),
                        provider_metadata: None,
                    }],
                    stop_reason: StopReason::EndTurn,
                    tool_calls: vec![], // BUG: no tool_calls!
                    usage: TokenUsage {
                        input_tokens: 20,
                        output_tokens: 15,
                    },
                })
            } else {
                // After tool result, return normal response
                Ok(CompletionResponse {
                    content: vec![ContentBlock::Text {
                        text: "Based on the search results, Rust async is great!".to_string(),
                        provider_metadata: None,
                    }],
                    stop_reason: StopReason::EndTurn,
                    tool_calls: vec![],
                    usage: TokenUsage {
                        input_tokens: 30,
                        output_tokens: 12,
                    },
                })
            }
        }
    }

    #[tokio::test]
    async fn test_text_tool_call_recovery_e2e() {
        // This is THE critical test: a model outputs a tool call as text,
        // the recovery code detects it, promotes it to ToolUse, executes the tool,
        // and the agent loop continues to produce a final response.
        let memory = openfang_memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = openfang_types::agent::AgentId::new();
        let mut session = openfang_memory::session::Session {
            id: openfang_types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(TextToolCallDriver::new());

        // Provide web_search as an available tool so recovery can match it
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                }
            }),
        }];

        let result = run_agent_loop(
            &manifest,
            "Search for rust async programming",
            &mut session,
            &memory,
            driver,
            &tools,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // on_phase
            None, // media_engine
            None, // tts_engine
            None, // docker_config
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
        )
        .await
        .expect("Agent loop should complete");

        // The response should contain the second call's output, NOT the raw function tag
        assert!(
            !result.response.contains("<function="),
            "Response should not contain raw function tags, got: {:?}",
            result.response
        );
        assert!(
            result.iterations >= 2,
            "Should have at least 2 iterations (tool call + final response), got: {}",
            result.iterations
        );
        // Verify the final text response came through
        assert!(
            result.response.contains("search results") || result.response.contains("Rust async"),
            "Expected final response text, got: {:?}",
            result.response
        );
    }

    #[tokio::test]
    async fn test_nested_xml_text_tool_call_recovery_e2e() {
        let memory = openfang_memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = openfang_types::agent::AgentId::new();
        let mut session = openfang_memory::session::Session {
            id: openfang_types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(NestedXmlTextToolCallDriver::new());

        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                }
            }),
        }];

        let result = run_agent_loop(
            &manifest,
            "Search for rust async programming",
            &mut session,
            &memory,
            driver,
            &tools,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("Agent loop should recover nested XML tool calls");

        assert!(
            !result.response.contains("<tool_call>"),
            "Response should not contain raw tool_call tags, got: {:?}",
            result.response
        );
        assert!(
            !result.response.contains("<function="),
            "Response should not contain raw function tags, got: {:?}",
            result.response
        );
        assert!(
            result
                .response
                .contains("Recovered nested XML tool call successfully."),
            "Expected final response text, got: {:?}",
            result.response
        );
        assert!(
            result.iterations >= 2,
            "Should have at least 2 iterations (tool call + final response), got: {}",
            result.iterations
        );
    }

    /// Mock driver that returns NO text-based tool calls — just normal text.
    /// Verifies recovery does NOT interfere with normal flow.
    #[tokio::test]
    async fn test_normal_flow_unaffected_by_recovery() {
        let memory = openfang_memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = openfang_types::agent::AgentId::new();
        let mut session = openfang_memory::session::Session {
            id: openfang_types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(NormalDriver);

        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({}),
        }];

        let result = run_agent_loop(
            &manifest,
            "Say hello",
            &mut session,
            &memory,
            driver,
            &tools, // tools available but not used
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // user_content_blocks
        )
        .await
        .expect("Normal loop should complete");

        assert_eq!(result.response, "Hello from the agent!");
        assert_eq!(
            result.iterations, 1,
            "Normal response should complete in 1 iteration"
        );
    }

    // --- Streaming path: text-as-tool-call recovery ---

    #[tokio::test]
    async fn test_text_tool_call_recovery_streaming_e2e() {
        let memory = openfang_memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = openfang_types::agent::AgentId::new();
        let mut session = openfang_memory::session::Session {
            id: openfang_types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(TextToolCallDriver::new());

        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                }
            }),
        }];

        let (tx, mut rx) = mpsc::channel(64);

        let result = run_agent_loop_streaming(
            &manifest,
            "Search for rust async programming",
            &mut session,
            &memory,
            driver,
            &tools,
            None,
            tx,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // on_phase
            None, // media_engine
            None, // tts_engine
            None, // docker_config
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
        )
        .await
        .expect("Streaming loop should complete");

        // Same assertions as non-streaming
        assert!(
            !result.response.contains("<function="),
            "Streaming: response should not contain raw function tags, got: {:?}",
            result.response
        );
        assert!(
            result.iterations >= 2,
            "Streaming: should have at least 2 iterations, got: {}",
            result.iterations
        );

        // Drain the stream channel to verify events were sent
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(!events.is_empty(), "Should have received stream events");
    }

    #[test]
    fn test_silent_detection_uppercase() {
        assert!(is_silent_token("[SILENT]"));
    }

    #[test]
    fn test_silent_detection_lowercase() {
        assert!(is_silent_token("[silent]"));
    }

    #[test]
    fn test_silent_detection_mixed_case() {
        assert!(is_silent_token("[Silent]"));
    }

    #[test]
    fn test_silent_detection_with_whitespace() {
        assert!(is_silent_token("  [SILENT]  "));
    }

    #[test]
    fn test_silent_detection_no_reply() {
        assert!(is_silent_token("NO_REPLY"));
    }

    #[test]
    fn test_silent_detection_rejects_normal_text() {
        assert!(!is_silent_token("Hello, how can I help?"));
        assert!(!is_silent_token("SILENT"));
        assert!(!is_silent_token(""));
    }

    // =====================================================================
    // Per-call accounting
    // =====================================================================

    fn usage(input: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
        }
    }

    fn adv_model() -> openfang_types::agent::ModelConfig {
        openfang_types::agent::ModelConfig {
            provider: "hyperfusion".to_string(),
            model: "adv-primary".to_string(),
            ..Default::default()
        }
    }

    /// The turn total must stay `iterations - 1`, which is the number written
    /// before accounting moved to per-call rows.
    #[test]
    fn test_tool_calls_count_what_the_response_asked_for() {
        // The previous version of this test asserted `total == iterations - 1`, which is what
        // the code did — so it locked in the undercount instead of guarding against it. A
        // response asking for three tool calls made the turn report one. The assertion now
        // follows the responses, not the iteration count.
        let per_response = [3usize, 1, 0];
        let mut calls: Vec<LlmCall> = Vec::new();
        for (i, n) in per_response.iter().enumerate() {
            record_call(
                &mut calls,
                i as u32,
                &adv_model(),
                &CallReport::default(),
                usage(10, 5),
            );
            set_last_tool_calls(&mut calls, *n);
        }
        let finished = finish_calls(&mut calls);
        assert_eq!(
            finished.iter().map(|c| c.tool_calls).collect::<Vec<_>>(),
            vec![3u32, 1, 0],
            "each row carries the count from its own response"
        );
        assert_eq!(
            finished
                .iter()
                .map(|c| u64::from(c.tool_calls))
                .sum::<u64>(),
            4,
            "turn total is the sum of real calls, not iterations - 1 (which would be 2)"
        );
        assert!(calls.is_empty(), "finish_calls hands the vector over");
    }

    #[test]
    fn test_finish_calls_leaves_recorded_counts_alone() {
        let mut calls: Vec<LlmCall> = Vec::new();
        record_call(
            &mut calls,
            0,
            &adv_model(),
            &CallReport::default(),
            usage(10, 5),
        );
        set_last_tool_calls(&mut calls, 7);
        let finished = finish_calls(&mut calls);
        assert_eq!(
            finished[0].tool_calls, 7,
            "finish_calls must not rewrite it"
        );
    }

    /// A mixed turn: the substitute served call 0, the primary came back for
    /// call 1. Under per-turn accounting this produced "fell back from
    /// adv-primary to adv-primary" with the substitute named nowhere.
    #[test]
    fn test_mixed_turn_rows_name_both_models() {
        let mut calls: Vec<LlmCall> = Vec::new();
        record_call(
            &mut calls,
            0,
            &adv_model(),
            &CallReport {
                substituted: Some("adv-fallback".to_string()),
                provider: Some("hyperfusion".to_string()),
                reason: Some("HTTP error: connection refused".to_string()),
            },
            usage(202, 22),
        );
        record_call(
            &mut calls,
            1,
            &adv_model(),
            &CallReport::default(),
            usage(101, 11),
        );
        let calls = finish_calls(&mut calls);

        assert_eq!(calls[0].model, "adv-fallback");
        assert_eq!(calls[0].requested.as_deref(), Some("adv-primary"));
        assert_eq!(calls[0].provider, "hyperfusion");
        assert!(calls[0].substituted());
        assert_eq!(calls[1].model, "adv-primary");
        assert_eq!(calls[1].requested, None, "the primary is not a substitute");
        assert_eq!(
            calls[1].provider, "hyperfusion",
            "an unsubstituted call takes the manifest's provider"
        );

        let s = openfang_types::usage::fallback_summary(&calls).expect("substitution");
        assert_eq!(s.requested, "adv-primary");
        assert_eq!(s.served_by, vec!["adv-fallback".to_string()]);
        assert_eq!((s.calls, s.of), (1, 2));
        assert!(
            !s.served_by.contains(&s.requested),
            "the disclosure must never claim a model fell back to itself"
        );
    }

    /// Primary down for iteration 0, back for iteration 1 — driven through the
    /// real loop and a real `FallbackDriver`.
    struct FlakyPrimaryDriver {
        calls: AtomicU32,
    }

    #[async_trait]
    impl LlmDriver for FlakyPrimaryDriver {
        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            assert_eq!(request.model, "adv-primary", "wire name of the primary");
            if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
                return Err(LlmError::Http("primary is down".to_string()));
            }
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: "ADV-PRIMARY-WROTE-THE-FINAL-ANSWER".to_string(),
                    provider_metadata: None,
                }],
                stop_reason: StopReason::EndTurn,
                tool_calls: vec![],
                usage: usage(101, 11),
            })
        }
    }

    /// The substitute answers with a tool call, so the turn needs a second
    /// iteration — by which time the primary is back.
    struct ToolCallSubstituteDriver;

    #[async_trait]
    impl LlmDriver for ToolCallSubstituteDriver {
        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            assert_eq!(request.model, "adv-fallback", "wire name of the substitute");
            Ok(CompletionResponse {
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "fake_tool".to_string(),
                    input: serde_json::json!({"q": "x"}),
                    provider_metadata: None,
                }],
                stop_reason: StopReason::ToolUse,
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "fake_tool".to_string(),
                    input: serde_json::json!({"q": "x"}),
                }],
                usage: usage(202, 22),
            })
        }
    }

    fn mixed_turn_driver() -> Arc<dyn LlmDriver> {
        use crate::drivers::fallback::{FallbackDriver, FallbackTarget};
        Arc::new(FallbackDriver::with_targets(vec![
            FallbackTarget {
                driver: Arc::new(FlakyPrimaryDriver {
                    calls: AtomicU32::new(0),
                }),
                model: String::new(),
                model_id: String::new(),
                provider: "hyperfusion".to_string(),
            },
            FallbackTarget {
                driver: Arc::new(ToolCallSubstituteDriver),
                model: "adv-fallback".to_string(),
                model_id: "adv-fallback".to_string(),
                provider: "hyperfusion".to_string(),
            },
        ]))
    }

    fn adv_manifest() -> AgentManifest {
        AgentManifest {
            name: "test-adv-mixed".to_string(),
            model: openfang_types::agent::ModelConfig {
                system_prompt: "You are a test agent.".to_string(),
                ..adv_model()
            },
            ..Default::default()
        }
    }

    async fn run_adv_turn(driver: Arc<dyn LlmDriver>) -> AgentLoopResult {
        let memory = openfang_memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = openfang_types::agent::AgentId::new();
        let mut session = openfang_memory::session::Session {
            id: openfang_types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
        };
        run_agent_loop(
            &adv_manifest(),
            "probe",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("the turn completes: the substitute covers the dead primary")
    }

    #[tokio::test]
    async fn test_mixed_turn_end_to_end_splits_the_turn_between_two_models() {
        let result = run_adv_turn(mixed_turn_driver()).await;

        assert_eq!(result.iterations, 2);
        assert_eq!(
            result.calls.len(),
            result.iterations as usize,
            "one accounting row per LLM call"
        );

        assert_eq!(result.calls[0].model, "adv-fallback");
        assert_eq!(result.calls[0].requested.as_deref(), Some("adv-primary"));
        assert!(result.calls[0]
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("primary is down"));
        assert_eq!(
            (
                result.calls[0].input_tokens,
                result.calls[0].output_tokens,
                result.calls[0].tool_calls
            ),
            (202, 22, 1)
        );

        assert_eq!(result.calls[1].model, "adv-primary");
        assert_eq!(result.calls[1].requested, None);
        assert_eq!(
            (
                result.calls[1].input_tokens,
                result.calls[1].output_tokens,
                result.calls[1].tool_calls
            ),
            (101, 11, 0)
        );

        // Token conservation: the rows must add up to the turn's own totals.
        assert_eq!(
            result.calls.iter().map(|c| c.input_tokens).sum::<u64>(),
            result.total_usage.input_tokens
        );
        assert_eq!(
            result.calls.iter().map(|c| c.output_tokens).sum::<u64>(),
            result.total_usage.output_tokens
        );
        assert_eq!(result.total_usage.input_tokens, 303);
        assert_eq!(result.total_usage.output_tokens, 33);

        let s = openfang_types::usage::fallback_summary(&result.calls).expect("substitution");
        assert_eq!(s.served_by, vec!["adv-fallback".to_string()]);
        assert_eq!(s.requested, "adv-primary");
        assert!(!s.served_by.contains(&s.requested));
        assert_eq!(
            openfang_types::usage::last_served(&result.calls),
            Some(("hyperfusion", "adv-primary")),
            "the last call was the primary's — and that is all model_used claims"
        );
    }

    // ── FANG-10 / FANG-47: the max-iterations exit ────────────────────────
    //
    // Both defects live on one line. `Err(MaxIterationsExceeded)` gave the
    // caller a bare 500 with none of the turn in it (FANG-10) and gave the
    // kernel — which books usage on the Ok arm only — nothing to book, so the
    // ledger moved by zero for a turn the provider had already billed
    // (FANG-47). Before the fix this test failed on its first assertion:
    // `run_agent_loop` returned Err.

    /// The tool this section drives the loop with. `system_time` takes no
    /// arguments, needs no kernel and no workspace, and always succeeds — so
    /// "the call completed" in these tests is the tool really having run, not
    /// a stub saying so.
    fn system_time_tools() -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "system_time".to_string(),
            description: "Current time".to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }]
    }

    fn system_time_response(
        n: u32,
        input: serde_json::Value,
        text: Option<String>,
    ) -> CompletionResponse {
        let mut content = Vec::new();
        if let Some(t) = &text {
            content.push(ContentBlock::Text {
                text: t.clone(),
                provider_metadata: None,
            });
        }
        content.push(ContentBlock::ToolUse {
            id: format!("call_{n}"),
            name: "system_time".to_string(),
            input: input.clone(),
            provider_metadata: None,
        });
        CompletionResponse {
            content,
            stop_reason: StopReason::ToolUse,
            tool_calls: vec![ToolCall {
                id: format!("call_{n}"),
                name: "system_time".to_string(),
                input,
            }],
            usage: usage(7919, 7907),
        }
    }

    /// Asks for a fresh tool call forever. The only response shape that can
    /// reach the max-iterations exit — a text default ends the turn instead.
    /// Arguments differ per call so the loop guard's repeat block (5 identical
    /// calls) never fires and every iteration is a real, distinct call that
    /// really executes.
    struct NeverStopsDriver {
        calls: AtomicU32,
        /// Text emitted alongside each tool call. `{n}` is replaced with the
        /// call index. None means tool call only.
        text: Option<&'static str>,
        /// When true every call carries identical arguments, which is what the
        /// loop guard blocks after `block_threshold` repeats.
        identical_args: bool,
    }

    impl NeverStopsDriver {
        fn distinct() -> Self {
            Self {
                calls: AtomicU32::new(0),
                text: None,
                identical_args: false,
            }
        }
        fn repeating() -> Self {
            Self {
                calls: AtomicU32::new(0),
                text: None,
                identical_args: true,
            }
        }
        fn talking(text: &'static str) -> Self {
            Self {
                calls: AtomicU32::new(0),
                text: Some(text),
                identical_args: false,
            }
        }
    }

    #[async_trait]
    impl LlmDriver for NeverStopsDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            let n = self.calls.fetch_add(1, Ordering::Relaxed);
            let input = if self.identical_args {
                serde_json::json!({ "note": "MAXITER-PARTIAL-CANARY" })
            } else {
                serde_json::json!({ "note": "MAXITER-PARTIAL-CANARY", "step": n })
            };
            let text = self.text.map(|t| t.replace("{n}", &n.to_string()));
            Ok(system_time_response(n, input, text))
        }
    }

    fn never_stops_manifest(max_iterations: u32) -> AgentManifest {
        AgentManifest {
            name: "test-max-iterations".to_string(),
            model: openfang_types::agent::ModelConfig {
                system_prompt: "You are a test agent.".to_string(),
                ..adv_model()
            },
            autonomous: Some(openfang_types::agent::AutonomousConfig {
                max_iterations,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn blank_session() -> openfang_memory::session::Session {
        openfang_memory::session::Session {
            id: openfang_types::agent::SessionId::new(),
            agent_id: openfang_types::agent::AgentId::new(),
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
        }
    }

    #[tokio::test]
    async fn test_max_iterations_hands_back_the_partial_turn_not_an_error() {
        let memory = openfang_memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let mut session = blank_session();
        let result = run_agent_loop(
            &never_stops_manifest(3),
            "start the unbounded task",
            &mut session,
            &memory,
            Arc::new(NeverStopsDriver::distinct()),
            &system_time_tools(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("exhausting the iteration budget truncates a turn; it does not fail one");

        // FANG-47: every call the provider served is in the turn's accounting,
        // which is the only thing the kernel can book. Losing the Err lost all
        // three rows and both totals.
        assert_eq!(result.iterations, 3);
        assert_eq!(result.calls.len(), 3, "one accounting row per LLM call");
        assert_eq!(result.total_usage.input_tokens, 3 * 7919);
        assert_eq!(result.total_usage.output_tokens, 3 * 7907);
        assert_eq!(
            result.calls.iter().map(|c| c.input_tokens).sum::<u64>(),
            result.total_usage.input_tokens,
            "the rows must add up to the turn's own totals"
        );
        assert!(
            result.calls.iter().all(|c| c.model == "adv-primary"),
            "booked to the model that served the calls"
        );

        // FANG-10: the caller is told what ran, and by which door the loop left.
        assert!(!result.silent);
        assert!(
            result.response.contains("Max iterations exceeded (3)"),
            "the exit door has to be named, and it is not the circuit breaker: {}",
            result.response
        );
        // Three distinct calls, all allowed by the loop guard, all executed by
        // the real tool runner: the section that says they ran says three.
        assert!(
            result
                .response
                .contains("Tool calls that ran and succeeded (3)"),
            "{}",
            result.response
        );
        assert!(
            !result.response.contains("stopped before they ran"),
            "nothing was stopped in this turn, so nothing is listed as stopped: {}",
            result.response
        );
        assert!(
            result.response.contains("system_time"),
            "the tool calls the turn made are part of the result: {}",
            result.response
        );
        assert!(
            result.response.contains("MAXITER-PARTIAL-CANARY"),
            "and named concretely enough to identify the work: {}",
            result.response
        );

        // What the caller was told is what the history records.
        assert_eq!(
            session.messages.last().map(|m| m.content.text_content()),
            Some(result.response.clone()),
        );
    }

    /// The defect the first patch introduced: it read the turn's tool calls
    /// back off the session, where a `ToolUse` block sits whether or not the
    /// call ever reached a tool, and announced all of them as executed. A
    /// runaway loop is exactly the case where that is false — the loop guard
    /// blocks a call repeated `block_threshold` (5) times, so on a 50-iteration
    /// runaway most of the "executed" calls never ran at all.
    #[tokio::test]
    async fn test_max_iterations_does_not_report_loop_guard_blocked_calls_as_executed() {
        let memory = openfang_memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let mut session = blank_session();
        // 9 iterations of one identical call. LoopGuardConfig::default() warns
        // at 3 and blocks from the 5th occurrence on, so 4 run and 5 do not.
        let result = run_agent_loop(
            &never_stops_manifest(9),
            "start the unbounded task",
            &mut session,
            &memory,
            Arc::new(NeverStopsDriver::repeating()),
            &system_time_tools(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("the loop still leaves by the max-iterations door");

        assert_eq!(result.iterations, 9);
        assert_eq!(result.calls.len(), 9, "the provider served nine calls");

        assert!(
            result
                .response
                .contains("Tool calls that ran and succeeded (4)"),
            "only the four the loop guard let through ran: {}",
            result.response
        );
        assert!(
            result
                .response
                .contains("Tool calls stopped before they ran (5)"),
            "the five the loop guard blocked are named as blocked: {}",
            result.response
        );
        assert!(
            result
                .response
                .contains("system_time — stopped by the loop guard"),
            "and what stopped them is named: {}",
            result.response
        );
        // The claim the reviewer caught: nine calls asked for, nine announced
        // as executed.
        assert!(
            !result.response.contains("ran and succeeded (9)"),
            "all nine announced as executed — the defect this test exists for: {}",
            result.response
        );
    }

    /// The streaming half, which the first patch shipped with no test at all.
    ///
    /// A WS/SSE caller's transcript is the concatenation of the `TextDelta`
    /// events (ws.rs:781-782), so the max-iterations notice has to arrive as a
    /// delta or the turn ends saying nothing. Sending the whole summary as that
    /// delta — which is what the first patch did — repeats every word of the
    /// partial text the caller already received chunk by chunk.
    #[tokio::test]
    async fn test_streaming_max_iterations_notice_does_not_repeat_the_streamed_text() {
        let memory = openfang_memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let mut session = blank_session();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamEvent>(4096);

        let result = run_agent_loop_streaming(
            &never_stops_manifest(3),
            "start the unbounded task",
            &mut session,
            &memory,
            Arc::new(NeverStopsDriver::talking("STREAM-PARTIAL-{n}")),
            &system_time_tools(),
            None,
            tx,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("the streaming loop truncates a turn too; it does not fail one");

        rx.close();
        let mut streamed = String::new();
        while let Some(ev) = rx.recv().await {
            if let StreamEvent::TextDelta { text } = ev {
                streamed.push_str(&text);
            }
        }

        // FANG-47, streaming side: the accounting survives the exit.
        assert_eq!(result.calls.len(), 3);
        assert_eq!(result.total_usage.input_tokens, 3 * 7919);

        // The notice reached the client exactly once — a streaming caller is
        // told why the turn ended.
        assert_eq!(
            streamed.matches("Max iterations exceeded (3)").count(),
            1,
            "streamed transcript: {streamed}"
        );
        assert_eq!(
            streamed
                .matches("Tool calls that ran and succeeded (3)")
                .count(),
            1,
            "streamed transcript: {streamed}"
        );

        // And each chunk of partial text reached it exactly once — not twice,
        // which is what embedding the text in the final delta produced.
        for n in 0..3 {
            assert_eq!(
                streamed.matches(&format!("STREAM-PARTIAL-{n}")).count(),
                1,
                "chunk {n} duplicated in the streamed transcript: {streamed}"
            );
        }

        // The returned response says the same thing the deltas did, once each.
        for n in 0..3 {
            assert_eq!(
                result
                    .response
                    .matches(&format!("STREAM-PARTIAL-{n}"))
                    .count(),
                1,
                "{}",
                result.response
            );
        }
        assert!(result.response.contains("Max iterations exceeded (3)"));
    }

    fn turn_call(n: u32, fate: ToolCallFate) -> TurnToolCall {
        TurnToolCall::new("file_write", &serde_json::json!({ "n": n }), fate)
    }

    #[test]
    fn test_max_iterations_notice_elides_a_long_tool_list_without_losing_the_count() {
        let calls: Vec<TurnToolCall> = (0..50)
            .map(|n| turn_call(n, ToolCallFate::Completed))
            .collect();
        let s = max_iterations_summary("partial answer so far", &max_iterations_notice(50, &calls));
        assert!(s.contains("Max iterations exceeded (50)"));
        assert!(s.contains("partial answer so far"));
        assert!(s.contains("Tool calls that ran and succeeded (50)"), "{s}");
        assert!(s.contains("… and 30 more"), "{s}");
        assert!(
            s.contains("{\"n\":0}") && !s.contains("{\"n\":49}"),
            "the head of the list is what is shown"
        );
    }

    #[test]
    fn test_max_iterations_notice_says_so_when_nothing_ran() {
        let s = max_iterations_notice(2, &[]);
        assert!(s.contains("No tool calls were executed in this turn."));
    }

    /// The notice must not count a call that never reached a tool among the
    /// calls that ran: that is exactly the claim FANG-9 is open for.
    #[test]
    fn test_max_iterations_notice_counts_blocked_calls_apart_from_calls_that_ran() {
        let calls = vec![
            turn_call(0, ToolCallFate::Completed),
            turn_call(1, ToolCallFate::Errored),
            turn_call(2, ToolCallFate::Blocked("stopped by the loop guard")),
            turn_call(3, ToolCallFate::Blocked("stopped by the loop guard")),
            turn_call(4, ToolCallFate::Blocked("stopped by a BeforeToolCall hook")),
        ];
        let s = max_iterations_notice(5, &calls);
        assert!(s.contains("Tool calls that ran and succeeded (1)"), "{s}");
        assert!(s.contains("Tool calls that returned an error (1)"), "{s}");
        assert!(s.contains("Tool calls stopped before they ran (3)"), "{s}");
        assert!(s.contains("file_write — stopped by the loop guard"), "{s}");
        assert!(
            s.contains("file_write — stopped by a BeforeToolCall hook"),
            "{s}"
        );
        // The three blocked calls appear once each, under the stopped heading —
        // never under a heading that says they ran.
        let ran_section = s
            .split("Tool calls stopped before they ran")
            .next()
            .unwrap();
        for n in [2, 3, 4] {
            assert!(
                !ran_section.contains(&format!("{{\"n\":{n}}}")),
                "blocked call {n} listed as having run: {s}"
            );
        }
    }

    /// `fate_counts` is what the max-iterations log line and the AgentLoopEnd
    /// hook report, so a blocked call landing in the "ran" number would put the
    /// same false claim into the operator's logs as into the response body.
    #[test]
    fn test_fate_counts_keeps_the_three_groups_apart() {
        let calls = vec![
            turn_call(0, ToolCallFate::Completed),
            turn_call(1, ToolCallFate::Completed),
            turn_call(2, ToolCallFate::Errored),
            turn_call(3, ToolCallFate::Blocked("stopped by the loop guard")),
            turn_call(4, ToolCallFate::Blocked("stopped by a BeforeToolCall hook")),
            turn_call(5, ToolCallFate::Blocked("stopped by the loop guard")),
        ];
        assert_eq!(fate_counts(&calls), (2, 1, 3));
        assert_eq!(fate_counts(&[]), (0, 0, 0));
    }

    /// A section with no members prints nothing at all — the notice never says
    /// "(0)" about a fate no call had.
    #[test]
    fn test_max_iterations_notice_omits_the_sections_it_has_no_calls_for() {
        let s = max_iterations_notice(
            4,
            &[turn_call(
                0,
                ToolCallFate::Blocked("stopped by the loop guard"),
            )],
        );
        assert!(s.contains("Tool calls stopped before they ran (1)"), "{s}");
        assert!(!s.contains("ran and succeeded"), "{s}");
        assert!(!s.contains("returned an error"), "{s}");
        assert!(!s.contains("No tool calls were executed"), "{s}");
    }

    /// `max_iterations_summary` appends the notice to the partial text exactly
    /// once. That is the property the streaming path depends on: the caller has
    /// already received `accumulated_text` chunk by chunk, so the loop sends only
    /// the notice as a delta, and a summary that repeated the partial text would
    /// show it to a streaming caller twice.
    ///
    /// This asserts the no-duplication property and nothing wider. It does NOT
    /// assert that the client's concatenated deltas equal the returned
    /// `response` — they do not, and an earlier version of this test claimed so
    /// in its name while checking only what is below. The deltas go out one per
    /// iteration with no separator; `accumulated_text` joins the same texts with
    /// "\n\n" and trims. Over 50 talkative iterations that measured 2 910
    /// characters at the client against 3 008 returned.
    #[test]
    fn test_summary_appends_the_notice_once_and_never_repeats_the_partial_text() {
        let notice = max_iterations_notice(3, &[turn_call(0, ToolCallFate::Completed)]);
        let streamed = "partial answer so far";
        let response = max_iterations_summary(streamed, &notice);
        let delta = format!("\n\n{notice}");
        assert_eq!(format!("{streamed}{delta}"), response);
        assert_eq!(
            response.matches(streamed).count(),
            1,
            "the partial text appears once, not twice: {response}"
        );

        // And with nothing streamed, the delta is the whole response.
        let response = max_iterations_summary("", &notice);
        assert_eq!(response, notice);
    }

    /// A retry inside `call_with_retry` is the same call, not a new one.
    struct RateLimitOnceDriver {
        calls: AtomicU32,
    }

    #[async_trait]
    impl LlmDriver for RateLimitOnceDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
                return Err(LlmError::RateLimited { retry_after_ms: 1 });
            }
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: "answered after a retry".to_string(),
                    provider_metadata: None,
                }],
                stop_reason: StopReason::EndTurn,
                tool_calls: vec![],
                usage: usage(50, 5),
            })
        }
    }

    #[tokio::test]
    async fn test_retry_does_not_add_an_accounting_row() {
        let result = run_adv_turn(Arc::new(RateLimitOnceDriver {
            calls: AtomicU32::new(0),
        }))
        .await;
        assert_eq!(result.iterations, 1);
        assert_eq!(result.calls.len(), 1, "the retry is the same call");
        assert_eq!(result.calls[0].input_tokens, 50);
        assert_eq!(
            result.calls[0].model, "adv-primary",
            "a single-model driver reports no substitution"
        );
        assert!(openfang_types::usage::fallback_summary(&result.calls).is_none());
    }

    /// Minimal OpenAI-compatible server that answers exactly one request and
    /// reports the `model` it was asked for.
    async fn fake_openai_once() -> (u16, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut header_end: Option<usize> = None;
            let mut content_length: Option<usize> = None;
            loop {
                let n = socket.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if header_end.is_none() {
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = Some(pos + 4);
                        let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                        for line in head.lines() {
                            if let Some(v) = line.strip_prefix("content-length:") {
                                content_length = v.trim().parse().ok();
                            }
                        }
                    }
                }
                match (header_end, content_length) {
                    (Some(h), Some(cl)) if buf.len() >= h + cl => break,
                    _ => {}
                }
            }
            let body: serde_json::Value = header_end
                .and_then(|h| serde_json::from_slice(&buf[h..]).ok())
                .unwrap_or(serde_json::Value::Null);
            let asked_for = body
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string();
            let payload = serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "created": 0,
                "model": asked_for,
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "FALLBACK-ANSWERED"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                payload.len(),
                payload
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
            let _ = tx.send(asked_for);
        });
        (port, rx)
    }

    struct ModelNotFoundDriver;

    #[async_trait]
    impl LlmDriver for ModelNotFoundDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            Err(LlmError::Api {
                status: 404,
                message: "model not found".to_string(),
            })
        }
    }

    /// FANG-36: one configured fallback, one wire name and one accounting name.
    ///
    /// The `ModelNotFound` chain used to send `fb.model` unstripped while
    /// `resolve_driver`'s `FallbackDriver` chain sent it stripped, so the same
    /// configured entry reached the provider as two different models and landed
    /// in two `by-model` rows.
    #[tokio::test]
    async fn test_model_not_found_chain_sends_the_stripped_name_and_books_the_configured_one() {
        let (port, asked_for) = fake_openai_once().await;
        let fallbacks = vec![FallbackModel {
            provider: "y7router".to_string(),
            model: "y7router/kimi/k3".to_string(),
            api_key_env: None,
            base_url: Some(format!("http://127.0.0.1:{port}/v1")),
        }];
        let request = CompletionRequest {
            model: "dead-primary".to_string(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: 64,
            temperature: 0.0,
            system: None,
            thinking: None,
        };

        let (response, report) = tokio::time::timeout(
            Duration::from_secs(10),
            call_with_retry(&ModelNotFoundDriver, request, None, None, &fallbacks),
        )
        .await
        .expect("the fallback answers well inside the timeout")
        .expect("the manifest fallback covers a missing primary model");

        assert_eq!(response.text(), "FALLBACK-ANSWERED");
        let wire_name = tokio::time::timeout(Duration::from_secs(5), asked_for)
            .await
            .expect("the fake server reported the model")
            .unwrap();
        assert_eq!(
            wire_name, "kimi/k3",
            "the provider prefix must be stripped on the wire, exactly as the \
             FallbackDriver chain does it"
        );
        assert_eq!(
            report.substituted.as_deref(),
            Some("y7router/kimi/k3"),
            "accounting keeps the configured spelling, so both failover paths \
             book one key"
        );
        assert_eq!(report.provider.as_deref(), Some("y7router"));
        assert!(report.reason.is_some(), "the disclosure names the failure");
    }
}
