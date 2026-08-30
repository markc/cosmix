pub const EVAL_SYSTEM: &str = "\
You evaluate agent task transcripts to determine if they contain learnable patterns.
You respond ONLY with valid JSON, no markdown fences, no commentary.";

pub const EVAL_USER_TEMPLATE: &str = "\
Rate this completed task on two dimensions (1-5 each):

1. **success** — Was the task completed correctly? (1=failed, 5=perfect)
2. **novelty** — Was the approach non-trivial or worth remembering? (1=routine/obvious, 5=complex multi-step solution)

Task description: {task_description}

Final output (truncated to 2000 chars):
{final_output}

Tools used: {tools_used}

Duration: {duration_ms}ms | Tokens: {token_count}

Respond as JSON: {{\"success\": N, \"novelty\": N, \"reasoning\": \"...\"}}";

pub const EXTRACT_SYSTEM: &str = "\
You extract reusable skill documents from successful agent task transcripts.
A skill captures WHEN to apply an approach, WHAT steps to take, and WHAT can go wrong.
You respond ONLY with valid JSON, no markdown fences, no commentary.";

pub const EXTRACT_USER_TEMPLATE: &str = "\
Extract a reusable skill from this successful task.

Task description: {task_description}

Approach taken (tool calls):
{tool_calls}

Final output (truncated to 2000 chars):
{final_output}

Respond as JSON matching this schema:
{{
  \"name\": \"short-kebab-case-name\",
  \"trigger\": \"when the task involves...\",
  \"approach\": \"1. First...\\n2. Then...\\n3. Finally...\",
  \"tools_required\": [\"tool1\", \"tool2\"],
  \"failure_modes\": [\"if X happens, then Y\"]
}}";

pub const REFINE_SYSTEM: &str = "\
You refine existing skill documents based on new usage outcomes.
Adjust confidence, improve approach text, and add newly discovered failure modes.
You respond ONLY with valid JSON, no markdown fences, no commentary.";

pub const REFINE_USER_TEMPLATE: &str = "\
Update this skill based on a new outcome.

Existing skill:
{existing_skill}

Outcome: {outcome}
Success: {success}
Notes: {notes}

Produce an updated skill JSON with the same schema. Adjust:
- confidence (increase on success, decrease on failure, range 0.0-1.0)
- approach (refine steps if the outcome revealed improvements)
- failure_modes (add new ones if the outcome revealed a new failure)

Respond as JSON:
{{
  \"name\": \"...\",
  \"trigger\": \"...\",
  \"approach\": \"...\",
  \"tools_required\": [...],
  \"failure_modes\": [...],
  \"confidence\": N.N
}}";

/// Fill a template by replacing {key} placeholders.
pub fn fill_template(template: &str, vars: &[(&str, &str)]) -> String {
    let mut result = template.to_string();
    for (key, value) in vars {
        result = result.replace(&format!("{{{key}}}"), value);
    }
    result
}
