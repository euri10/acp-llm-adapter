# Usage and cost

`usage_update` notifications include cumulative session cost for the models with known published rates: `deepseek-v4-flash` and `deepseek-v4-pro` ([DeepSeek's pricing table](https://api-docs.deepseek.com/quick_start/pricing/)), and `openai/gpt-oss-120b` and `openai/gpt-oss-20b` (Groq's per-model pages, e.g. [gpt-oss-120b](https://console.groq.com/docs/model/openai/gpt-oss-120b)). Cost is calculated from cache-hit, cache-miss, and output tokens; both providers bill prompt-cache reads below the uncached input rate. A model with no known rates reports usage without a cost rather than an invented one.

For local testing or a provider price change, `LLM_PRICING` accepts JSON such as `{"deepseek-v4-pro":{"cache_hit":0.003625,"cache_miss":0.435,"output":0.87}}`, with values in USD per million tokens.

Usage counters are validated before they update telemetry, session cost, or
assistant/tool history. Input/output and cache sums must fit `u64`; a supplied
total must cover input plus output, reasoning tokens must fit within output,
and cache reads plus writes must fit within input. Larger provider totals are
preserved. Unrepresentable per-response or cumulative totals and costs fail the
prompt with a provider error; the session remains usable for the next prompt.
Costs use wide integer arithmetic before conversion to microdollars, avoiding
silent wrapping or saturation. Missing usage or unknown model prices remain
unknown.

Usage may arrive beside a completion or in a separate final accounting frame,
including Groq's `x_groq.usage` envelope. Duplicate envelopes count once;
matching counters are combined with optional details, and conflicting counters
are rejected. Accounting requires explicit input and output counts and never
substitutes for a completion's finish reason.

## Context-window reporting

The context gauge uses the streamed usage `context_length` when present.
Otherwise it uses the selected model's positive integer `context_window` from
the startup `GET /models` response, then the built-in model table as a fallback.
Discovery metadata is retained in memory and refreshed on adapter startup;
no extra request is made per turn. Missing or invalid sizes leave the fallback
intact. If all sources are unknown, no `usage_update` is emitted.

Optional startup discovery has a two-second deadline covering connection,
response headers, and the full body. Failure keeps the configured default model.
Unix termination handlers are registered before discovery starts, so signals
cancel it immediately. Editor disconnect is observed when this bounded startup
step finishes, within two seconds of starting discovery.
