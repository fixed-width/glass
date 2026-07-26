//! The seam every check runs against. A real run drives a spawned `glass-mcp`
//! over stdio; tests and `--self-check` drive a scripted double. Because the
//! checks never touch a process directly, every assertion is unit-testable
//! without a display.

use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct CallResult {
    pub is_error: bool,
    /// The first text block, parsed as JSON — glass's `{ok,tool,result}` envelope.
    /// `None` when the block is absent or not JSON.
    pub envelope: Option<Value>,
    /// Text blocks after the first.
    pub siblings: Vec<String>,
    pub images: usize,
}

pub trait McpTransport {
    fn call(&mut self, tool: &str, args: Value) -> Result<CallResult, String>;
}

impl CallResult {
    /// A successful call carrying glass's `{ok,tool,result}` envelope. The one place that
    /// shape is built for a double, so a change to the frozen envelope shape cannot be
    /// honoured by one caller and missed by another.
    pub fn ok(tool: &str, result: Value, siblings: &[&str], images: usize) -> Self {
        Self {
            is_error: false,
            envelope: Some(serde_json::json!({ "ok": true, "tool": tool, "result": result })),
            siblings: siblings.iter().copied().map(String::from).collect(),
            images,
        }
    }

    /// Parse the `result` object of a `tools/call` response.
    pub fn from_mcp(raw: &Value) -> Self {
        let is_error = raw["isError"].as_bool().unwrap_or(false);
        let mut envelope = None;
        let mut siblings = Vec::new();
        let mut images = 0;
        let mut seen_text_block = false;
        for item in raw["content"].as_array().unwrap_or(&Vec::new()).iter() {
            match item["type"].as_str() {
                Some("text") => {
                    let text = item["text"].as_str().unwrap_or_default().to_string();
                    // An error result carries a message, never an envelope: parsing it as one
                    // would let a stray JSON-shaped error masquerade as a pass. The first TEXT
                    // block — not `content[0]` — is the envelope; later text blocks are
                    // siblings.
                    if !seen_text_block && !is_error {
                        seen_text_block = true;
                        envelope = serde_json::from_str::<Value>(&text).ok();
                        if envelope.is_none() {
                            siblings.push(text);
                        }
                    } else {
                        siblings.push(text);
                    }
                }
                Some("image") => images += 1,
                _ => {}
            }
        }
        Self {
            is_error,
            envelope,
            siblings,
            images,
        }
    }
}

/// Replays a fixed script. Used by the unit tests and by `--self-check`.
pub struct ScriptedTransport {
    queue: std::collections::VecDeque<(String, Result<CallResult, String>)>,
}

impl ScriptedTransport {
    pub fn new(script: Vec<(&str, Result<CallResult, String>)>) -> Self {
        Self {
            queue: script
                .into_iter()
                .map(|(t, r)| (t.to_string(), r))
                .collect(),
        }
    }
}

impl McpTransport for ScriptedTransport {
    fn call(&mut self, tool: &str, _args: Value) -> Result<CallResult, String> {
        match self.queue.pop_front() {
            None => Err(format!("script exhausted; unexpected call to {tool}")),
            Some((expected, r)) if expected == tool => r,
            Some((expected, _)) => Err(format!("expected {expected}, got {tool}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_envelope_siblings_and_images() {
        let raw = json!({
            "content": [
                { "type": "text", "text": "{\"ok\":true,\"tool\":\"glass_a11y_snapshot\",\"result\":{}}" },
                { "type": "text", "text": "untrusted body" },
                { "type": "image", "data": "iVBOR", "mimeType": "image/png" }
            ],
            "isError": false
        });
        let r = CallResult::from_mcp(&raw);
        assert!(!r.is_error);
        assert_eq!(
            r.envelope.as_ref().unwrap()["tool"],
            json!("glass_a11y_snapshot")
        );
        assert_eq!(r.siblings, vec!["untrusted body".to_string()]);
        assert_eq!(r.images, 1);
    }

    #[test]
    fn an_error_result_keeps_its_text_and_has_no_envelope() {
        let raw = json!({
            "content": [ { "type": "text", "text": "no active session — call glass_start first" } ],
            "isError": true
        });
        let r = CallResult::from_mcp(&raw);
        assert!(r.is_error);
        assert!(r.envelope.is_none(), "error text is not an envelope");
        assert_eq!(
            r.siblings,
            vec!["no active session — call glass_start first".to_string()]
        );
    }

    #[test]
    fn scripted_transport_replays_in_order_and_rejects_an_unexpected_tool() {
        let mut t = ScriptedTransport::new(vec![
            ("glass_start", Ok(CallResult::default())),
            ("glass_stop", Ok(CallResult::default())),
        ]);
        assert!(t.call("glass_start", json!({})).is_ok());
        let err = t.call("glass_screenshot", json!({})).unwrap_err();
        assert!(
            err.contains("glass_stop"),
            "must name what it expected: {err}"
        );
    }

    #[test]
    fn an_image_before_the_envelope_still_parses_the_envelope() {
        let raw = json!({
            "content": [
                { "type": "image", "data": "iVBOR", "mimeType": "image/png" },
                { "type": "text", "text": "{\"ok\":true,\"tool\":\"glass_screenshot\",\"result\":{}}" },
                { "type": "text", "text": "sibling text" }
            ],
            "isError": false
        });
        let r = CallResult::from_mcp(&raw);
        assert!(!r.is_error);
        assert_eq!(
            r.envelope.as_ref().unwrap()["tool"],
            json!("glass_screenshot")
        );
        assert_eq!(r.siblings, vec!["sibling text".to_string()]);
        assert_eq!(r.images, 1);
    }
}
