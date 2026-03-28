//! Variable substitution for script step arguments.
//!
//! Replaces `$NAME` patterns in JSON template strings with values from
//! the script context (app-provided variables and step-stored results).

use crate::types::ScriptContext;

/// Substitute `$NAME` variables in a template string.
///
/// Resolution order:
/// 1. App-provided variables (`$CURRENT_FILE`, `$SERVICE_NAME`, etc.)
/// 2. Step-stored results (`$content` from `store = "content"`)
///
/// Unresolved variables are left as-is with a warning log.
/// Replacement values are JSON-escaped (quotes, backslashes, newlines).
pub fn substitute(template: &str, ctx: &ScriptContext) -> String {
    let mut result = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' {
            // Collect variable name (alphanumeric + underscore)
            let mut name = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_alphanumeric() || c == '_' {
                    name.push(c);
                    chars.next();
                } else {
                    break;
                }
            }

            if name.is_empty() {
                result.push('$');
                continue;
            }

            // Look up in app_vars first, then step_vars
            if let Some(val) = ctx.app_vars.get(&name).or_else(|| ctx.step_vars.get(&name)) {
                result.push_str(&json_escape(val));
            } else {
                tracing::warn!("Unresolved script variable: ${name}");
                result.push('$');
                result.push_str(&name);
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// Escape a string for safe embedding in a JSON string value.
///
/// Uses serde_json to guarantee correct escaping for all edge cases.
fn json_escape(s: &str) -> String {
    // serde_json::to_string produces "..." with proper escaping.
    // Strip the surrounding quotes since we're embedding in an existing JSON template.
    let quoted = serde_json::to_string(s).unwrap_or_else(|_| format!("\"{}\"", s));
    // Remove leading and trailing "
    quoted[1..quoted.len() - 1].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_basic_substitution() {
        let ctx = ScriptContext {
            app_vars: HashMap::from([("CURRENT_FILE".into(), "/tmp/test.md".into())]),
            step_vars: HashMap::new(),
            service_name: "edit".into(),
        };
        let result = substitute(r#"{"path": "$CURRENT_FILE"}"#, &ctx);
        assert_eq!(result, r#"{"path": "/tmp/test.md"}"#);
    }

    #[test]
    fn test_step_vars() {
        let ctx = ScriptContext {
            app_vars: HashMap::new(),
            step_vars: HashMap::from([("content".into(), "hello world".into())]),
            service_name: "edit".into(),
        };
        let result = substitute(r#"{"content": "$content"}"#, &ctx);
        assert_eq!(result, r#"{"content": "hello world"}"#);
    }

    #[test]
    fn test_json_escaping() {
        let ctx = ScriptContext {
            app_vars: HashMap::from([("TEXT".into(), "line1\nline2\"quoted\"".into())]),
            step_vars: HashMap::new(),
            service_name: "edit".into(),
        };
        let result = substitute(r#"{"text": "$TEXT"}"#, &ctx);
        assert_eq!(result, r#"{"text": "line1\nline2\"quoted\""}"#);
    }
}
