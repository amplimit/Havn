//! What a turn said and did on the way to its reply (issues #212, #213):
//! kept with the reply, and given back to the model in later turns so it
//! knows what it read, ran and sent instead of doing it again.
//!
//! A turn's steps are its assistant messages (text and tool calls) and the
//! tool results between them, as the model saw them, without reasoning.
//! Turns after the latest few keep their tool results short.

use serde_json::{Value, json};

/// Characters of one tool result kept with a turn.
const MAX_RESULT_CHARS: usize = 20_000;
/// Replies, counted back from the newest, whose tool results are given
/// whole; older ones are cut to [`OLD_RESULT_CHARS`].
const RECENT_REPLIES: usize = 3;
const OLD_RESULT_CHARS: usize = 1_000;
/// Characters of a string in an older turn's tool call input.
const OLD_INPUT_CHARS: usize = 1_000;

/// Characters of steps given back in all, counted from the newest reply:
/// replies past it come back as their text alone.
const STEPS_BUDGET: usize = 200_000;
/// What ends steps whose turn wrote nothing after its last tool call.
const ENDED: &str = "(The turn ended here.)";

/// The steps of a turn, from its `messages` (those the tool loop added
/// after the history), as JSON to keep; `None` for a turn that called no
/// tool, whose reply is all there is. `reply` is the reply kept, which can
/// say more than the model did (that it was stopped, an error).
pub fn of_turn(messages: &[Value], reply: &str) -> Option<String> {
    let calls_a_tool = messages.iter().any(|m| has_block(m, "tool_use"));
    if !calls_a_tool {
        return None;
    }
    let mut kept: Vec<Value> = messages.iter().map(without_reasoning).collect();
    for message in &mut kept {
        cut_results(message, MAX_RESULT_CHARS);
    }
    // A call left without its result (the turn was stopped, or ran out of
    // rounds) cannot be given back: its text stays.
    if let Some(last) = kept.last_mut()
        && role(last) == Some("assistant")
        && let Some(blocks) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        blocks.retain(|b| block_type(b) != Some("tool_use"));
    }
    // A message with nothing in it (a call that ended with no text, or
    // with reasoning alone) is refused by models.
    kept.retain(|m| !is_empty(m));
    // What the reply says beyond the model's own text ends the steps, so
    // later turns know; steps end with the model's text either way.
    let said = kept
        .iter()
        .filter(|m| role(m) == Some("assistant"))
        .filter_map(|m| m.get("content").and_then(crate::tool_loop::extract_text))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(crate::tool_loop::SEGMENT_BREAK);
    let reply = reply.trim();
    let extra = reply.strip_prefix(said.as_str()).unwrap_or(reply).trim();
    let ends_in_text = kept
        .last()
        .is_some_and(|m| role(m) == Some("assistant") && !has_block(m, "tool_use"));
    let extra = match (extra.is_empty(), ends_in_text) {
        (false, _) => Some(extra),
        (true, false) => Some(ENDED),
        (true, true) => None,
    };
    if let Some(extra) = extra {
        let text = json!({"type": "text", "text": extra});
        match kept.last_mut() {
            Some(last) if role(last) == Some("assistant") && !has_block(last, "tool_use") => {
                if let Some(blocks) = last.get_mut("content").and_then(Value::as_array_mut) {
                    blocks.push(text);
                }
            }
            _ => kept.push(json!({ "role": "assistant", "content": [text] })),
        }
    }
    with_own_ids(&mut kept);
    serde_json::to_string(&kept).ok()
}

/// The messages of the `replies` (text and steps, oldest first) for the
/// model, one list each: the newest with their steps, cut down with age,
/// until [`STEPS_BUDGET`] is spent; the rest as their text.
pub fn messages_of_replies(replies: &[(&str, Option<&str>)]) -> Vec<Vec<Value>> {
    let mut left = STEPS_BUDGET;
    let mut out: Vec<Vec<Value>> = replies
        .iter()
        .rev()
        .enumerate()
        .map(|(age, (content, steps))| {
            let messages = messages_of_reply(content, *steps, age);
            if steps.is_none() || left == 0 {
                return plain(content);
            }
            let size = serde_json::to_string(&messages).map_or(usize::MAX, |s| s.len());
            if size > left {
                left = 0;
                return plain(content);
            }
            left -= size;
            messages
        })
        .collect();
    out.reverse();
    out
}

/// A reply's messages for the model: its steps when it has them (cut down
/// when it is `age` replies back from the newest), else its text.
fn messages_of_reply(content: &str, steps: Option<&str>, age: usize) -> Vec<Value> {
    let parsed = steps
        .and_then(|s| serde_json::from_str::<Vec<Value>>(s).ok())
        .filter(|m| !m.is_empty());
    let Some(mut messages) = parsed else {
        return plain(content);
    };
    if age >= RECENT_REPLIES {
        for message in &mut messages {
            cut_results(message, OLD_RESULT_CHARS);
            cut_inputs(message);
        }
    }
    messages
}

/// A reply as its text alone.
pub fn plain(content: &str) -> Vec<Value> {
    vec![json!({ "role": "assistant", "content": content })]
}

/// Give each tool call of `messages` an id of its own, and its results the
/// same: ids a provider made up repeat, across turns and from one round to
/// the next (`call_0`), or do not suit another provider the chat moves to.
/// A result answers the latest call with its id.
fn with_own_ids(messages: &mut [Value]) {
    let mut ids = std::collections::HashMap::new();
    for message in messages.iter_mut() {
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in blocks {
            match block_type(block) {
                Some("tool_use") => {
                    if let Some(Value::String(id)) = block.get_mut("id") {
                        let own = format!("toolu_{}", uuid::Uuid::now_v7().simple());
                        ids.insert(id.clone(), own.clone());
                        *id = own;
                    }
                }
                Some("tool_result") => {
                    if let Some(Value::String(id)) = block.get_mut("tool_use_id")
                        && let Some(own) = ids.get(id.as_str())
                    {
                        id.clone_from(own);
                    }
                }
                _ => {}
            }
        }
    }
}

fn is_empty(message: &Value) -> bool {
    match message.get("content") {
        Some(Value::Array(blocks)) => blocks.is_empty(),
        Some(Value::String(text)) => text.trim().is_empty(),
        _ => true,
    }
}

fn role(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

fn block_type(block: &Value) -> Option<&str> {
    block.get("type").and_then(Value::as_str)
}

fn has_block(message: &Value, kind: &str) -> bool {
    message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| blocks.iter().any(|b| block_type(b) == Some(kind)))
}

/// `message` without the model's reasoning, which is not given back in
/// later turns.
fn without_reasoning(message: &Value) -> Value {
    let mut message = message.clone();
    if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
        blocks.retain(|b| !matches!(block_type(b), Some("thinking" | "redacted_thinking")));
    }
    message
}

/// Cut each tool result of `message` to `max` characters, saying so.
fn cut_results(message: &mut Value, max: usize) {
    let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for block in blocks {
        if block_type(block) != Some("tool_result") {
            continue;
        }
        if let Some(Value::String(text)) = block.get_mut("content") {
            let total = text.chars().count();
            if total > max {
                // Its start and its end: a command's errors come last.
                let tail = max / 4;
                let head: String = text.chars().take(max - tail).collect();
                let end: String = text.chars().skip(total - tail).collect();
                *text = format!(
                    "{head}\n[{} more characters of this result are not kept; run the tool \
                     again if they are needed]\n{end}",
                    total - max
                );
            }
        }
    }
}

/// Cut long strings in the inputs of `message`'s tool calls (a file
/// written, say) to [`OLD_INPUT_CHARS`].
fn cut_inputs(message: &mut Value) {
    fn cut(value: &mut Value) {
        match value {
            Value::String(text) if text.chars().count() > OLD_INPUT_CHARS => {
                let kept: String = text.chars().take(OLD_INPUT_CHARS).collect();
                *text = format!("{kept}\u{2026}");
            }
            Value::Array(items) => items.iter_mut().for_each(cut),
            Value::Object(map) => map.values_mut().for_each(cut),
            _ => {}
        }
    }
    let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for block in blocks {
        if block_type(block) == Some("tool_use")
            && let Some(input) = block.get_mut("input")
        {
            cut(input);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn turn() -> Vec<Value> {
        vec![
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "let me look", "signature": "s"},
                {"type": "text", "text": "Checking the file."},
                {"type": "tool_use", "id": "t1", "name": "file_read", "input": {"path": "a.txt"}},
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "x".repeat(30_000), "is_error": false},
            ]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "It says x."}]}),
        ]
    }

    #[test]
    fn a_turn_without_tools_keeps_no_steps() {
        let plain = [json!({"role": "assistant", "content": [{"type": "text", "text": "hi"}]})];
        assert_eq!(of_turn(&plain, "hi"), None);
    }

    #[test]
    fn steps_keep_text_and_calls_but_no_reasoning() {
        let steps: Vec<Value> =
            serde_json::from_str(&of_turn(&turn(), "Checking the file.\n\nIt says x.").unwrap())
                .unwrap();
        assert_eq!(steps.len(), 3);
        let first = steps[0]["content"].as_array().unwrap();
        assert!(first.iter().all(|b| b["type"] != "thinking"));
        assert_eq!(first[0]["text"], "Checking the file.");
        let result = steps[1]["content"][0]["content"].as_str().unwrap();
        assert!(result.starts_with(&"x".repeat(MAX_RESULT_CHARS - MAX_RESULT_CHARS / 4)));
        assert!(result.contains("10000 more characters"));
    }

    #[test]
    fn a_stopped_turn_ends_with_its_text_not_a_dangling_call() {
        let mut stopped = turn();
        stopped.truncate(1);
        let steps: Vec<Value> =
            serde_json::from_str(&of_turn(&stopped, "Checking the file.\n\n(stopped)").unwrap())
                .unwrap();
        // The call has no result, so it goes; its text stays, and what the
        // reply adds ends the steps.
        assert_eq!(steps.len(), 1, "{steps:?}");
        let blocks = steps[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2, "{blocks:?}");
        assert_eq!(blocks[1]["text"], "(stopped)");
        let mut only_call = turn();
        only_call.truncate(1);
        only_call[0]["content"] =
            json!([{"type": "tool_use", "id": "t1", "name": "x", "input": {}}]);
        let steps: Vec<Value> =
            serde_json::from_str(&of_turn(&only_call, "(stopped)").unwrap()).unwrap();
        assert_eq!(
            steps,
            [json!({"role": "assistant", "content": [{"type": "text", "text": "(stopped)"}]})]
        );
    }

    #[test]
    fn a_turn_ending_in_results_ends_with_the_reply() {
        let mut capped = turn();
        capped.truncate(2);
        let steps: Vec<Value> =
            serde_json::from_str(&of_turn(&capped, "(reached the cap)").unwrap()).unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[2]["content"][0]["text"], "(reached the cap)");
    }

    #[test]
    fn a_turn_ending_with_no_text_ends_with_its_own_text() {
        let mut quiet = turn();
        quiet[2] = json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "done", "signature": "s"},
        ]});
        let steps: Vec<Value> =
            serde_json::from_str(&of_turn(&quiet, "Checking the file.").unwrap()).unwrap();
        // No empty message is kept, and the steps still end with text.
        assert_eq!(steps.len(), 3, "{steps:?}");
        assert_eq!(
            steps[2]["content"],
            json!([{"type": "text", "text": ENDED}])
        );
        assert!(steps.iter().all(|m| !is_empty(m)));
    }

    #[test]
    fn calls_get_ids_of_their_own_matched_in_their_results() {
        let steps: Vec<Value> = serde_json::from_str(
            &of_turn(
                &turn(),
                "Checking the file.

It says x.",
            )
            .unwrap(),
        )
        .unwrap();
        let call = steps[0]["content"][1]["id"].as_str().unwrap();
        assert_ne!(call, "t1");
        assert!(call.starts_with("toolu_"));
        assert_eq!(steps[1]["content"][0]["tool_use_id"], call);
        let again: Vec<Value> =
            serde_json::from_str(&of_turn(&turn(), "It says x.").unwrap()).unwrap();
        assert_ne!(again[0]["content"][1]["id"], call, "unique across turns");

        // The same made-up id in two rounds of one turn.
        let mut twice = turn();
        twice.insert(
            2,
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "file_read", "input": {"path": "b.txt"}},
            ]}),
        );
        twice.insert(
            3,
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "y", "is_error": false},
            ]}),
        );
        let steps: Vec<Value> =
            serde_json::from_str(&of_turn(&twice, "It says x.").unwrap()).unwrap();
        let first = steps[0]["content"][1]["id"].as_str().unwrap();
        let second = steps[2]["content"][0]["id"].as_str().unwrap();
        assert_ne!(first, second, "unique within a turn");
        assert_eq!(steps[1]["content"][0]["tool_use_id"], first);
        assert_eq!(steps[3]["content"][0]["tool_use_id"], second);
    }

    #[test]
    fn a_reply_in_several_blocks_is_not_kept_twice() {
        let mut split = turn();
        split[2] = json!({"role": "assistant", "content": [
            {"type": "text", "text": "It says "},
            {"type": "thinking", "thinking": "hm", "signature": "s"},
            {"type": "text", "text": "x."},
        ]});
        let steps: Vec<Value> = serde_json::from_str(
            &of_turn(
                &split,
                "Checking the file.

It says x.",
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            steps[2]["content"].as_array().unwrap().len(),
            2,
            "{steps:?}"
        );
    }

    #[test]
    fn steps_past_the_budget_come_back_as_text() {
        let steps = of_turn(&turn(), "It says x.").unwrap();
        let replies: Vec<(&str, Option<&str>)> = (0..200)
            .map(|_| ("It says x.", Some(steps.as_str())))
            .collect();
        let out = messages_of_replies(&replies);
        assert_eq!(out.len(), 200);
        let newest = out.last().unwrap();
        assert_eq!(newest.len(), 3, "the newest keep their steps");
        assert_eq!(out[0], plain("It says x."), "the oldest are text");
        let given: usize = out
            .iter()
            .filter(|m| m.len() > 1)
            .map(|m| serde_json::to_string(m).unwrap().len())
            .sum();
        assert!(given <= STEPS_BUDGET, "{given}");
    }

    #[test]
    fn older_replies_are_cut_down() {
        let steps = of_turn(&turn(), "It says x.").unwrap();
        let recent = messages_of_reply("It says x.", Some(&steps), 0);
        let result = recent[1]["content"][0]["content"].as_str().unwrap();
        assert!(result.chars().count() > MAX_RESULT_CHARS);
        let old = messages_of_reply("It says x.", Some(&steps), RECENT_REPLIES);
        let result = old[1]["content"][0]["content"].as_str().unwrap();
        assert!(
            result.chars().count() < OLD_RESULT_CHARS + 200,
            "{}",
            result.len()
        );
        assert_eq!(
            messages_of_reply("plain", None, 0),
            [json!({"role": "assistant", "content": "plain"})]
        );
        assert_eq!(
            messages_of_reply("bad", Some("not json"), 0),
            [json!({"role": "assistant", "content": "bad"})]
        );
    }
}
