//! Claude Code's `--output-format stream-json` events, one per line. Parsed loosely: unknown
//! event types, subtypes and content blocks become `Other`, unknown fields are ignored.

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    System(System),
    Assistant {
        message: Message,
    },
    /// Carries tool results.
    User {
        #[serde(default)]
        message: Value,
    },
    RateLimitEvent,
    Result(RunResult),
    /// Not Claude's: the line the worker logs when it nudges a stalled or looping run.
    Nudge {
        reason: String,
    },
    /// Not Claude's: the line the worker logs when it starts a fresh session.
    Handoff {
        reason: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum System {
    Init {
        session_id: String,
        model: String,
        tools: Vec<String>,
        #[serde(default)]
        mcp_servers: Vec<McpServer>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct McpServer {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub id: String,
    pub content: Vec<Content>,
    pub usage: Usage,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    /// Tokens in context, for an assistant message's usage. A message's events repeat it, so
    /// the latest message's is the context; a result's sums every call and isn't.
    pub fn context(&self) -> u64 {
        self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens
    }
}

#[derive(Debug, Deserialize)]
pub struct RunResult {
    pub subtype: String,
    pub is_error: bool,
    pub total_cost_usd: f64,
    pub usage: Usage,
    pub permission_denials: Vec<Denial>,
    /// Claude's final message.
    #[serde(default)]
    pub result: String,
    /// The `--json-schema` answer.
    #[serde(default)]
    pub structured_output: Option<Value>,
    /// e.g. `No conversation found with session ID: …` for a `--resume` whose transcript is gone.
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Denial {
    pub tool_name: String,
    pub tool_input: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_run() {
        // claude 2.1.295: -p --output-format stream-json --verbose --allowedTools Read; Write denied
        let events: Vec<Event> = include_str!("../tests/fixtures/stream.jsonl")
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        let Event::System(System::Init {
            session_id,
            model,
            tools,
            mcp_servers,
        }) = &events[0]
        else {
            panic!("first event is init: {:?}", events[0]);
        };
        assert_eq!(session_id, "b98a1af4-0d10-40db-a505-3f1141bfa52d");
        assert_eq!(model, "claude-haiku-5-5");
        assert!(tools.contains(&"Read".to_string()));
        assert!(mcp_servers.is_empty());

        let tool_calls: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                Event::Assistant { message } => Some(&message.content),
                _ => None,
            })
            .flatten()
            .filter_map(|c| match c {
                Content::ToolUse { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_calls, ["Read", "Write"]);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::System(System::Other)))
        );
        assert!(events.iter().any(|e| matches!(e, Event::RateLimitEvent)));
        assert!(events.iter().any(|e| matches!(e, Event::User { .. })));

        let Some(Event::Result(r)) = events.last() else {
            panic!("last event is result");
        };
        assert_eq!((r.subtype.as_str(), r.is_error), ("success", false));
        assert!(r.total_cost_usd > 0.0);
        assert!(r.usage.cache_read_input_tokens > 0);
        assert_eq!(r.permission_denials[0].tool_name, "Write");
        assert_eq!(r.permission_denials[0].tool_input["content"], "x");

        // context is the latest assistant message's input, not the result's sum of calls
        let context: Vec<u64> = events
            .iter()
            .filter_map(|e| match e {
                Event::Assistant { message } => Some(message.usage.context()),
                _ => None,
            })
            .collect();
        assert_eq!(context.first(), Some(&(2 + 4012 + 13801)));
        assert_eq!(context.last(), Some(&(2 + 318 + 18009)));
        assert_eq!(r.usage.context(), 6 + 4526 + 49623);
    }

    #[test]
    fn a_missing_transcript_is_reported_in_errors() {
        // claude 2.1.295: -p --output-format stream-json --verbose --resume <unknown id>
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"total_cost_usd":0,"usage":{"input_tokens":0},"permission_denials":[],"errors":["No conversation found with session ID: 7d1b1d6a-0000-4000-8000-000000000000"]}"#;
        let Event::Result(r) = serde_json::from_str(line).unwrap() else {
            panic!("a result");
        };
        assert!(r.errors[0].starts_with("No conversation found"));
    }

    #[test]
    fn unknown_shapes_are_other() {
        let e: Event = serde_json::from_str(r#"{"type":"brand_new","x":1}"#).unwrap();
        assert!(matches!(e, Event::Other));
        let e: Event = serde_json::from_str(r#"{"type":"system","subtype":"new_thing"}"#).unwrap();
        assert!(matches!(e, Event::System(System::Other)));
    }
}
