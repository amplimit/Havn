//! `ask_choice` (issue #204): the agent asks the person to choose among a
//! few options. A chat app with buttons shows one per option (Telegram, the
//! web chat); one with polls shows a question of several answers as a poll;
//! any other gets the options as numbered text, answered by typing. A press
//! or a vote comes back as `/choose <id> <numbers>`, which the gateway turns
//! into the person's next message to the agent, marked as a choice.

use havn_core::{Button, Poll};
use havn_proto::GatewayToolSpec;
use serde_json::json;

pub const TOOL_NAME: &str = "ask_choice";

/// The options a question has, at least and at most (Telegram's poll
/// takes ten).
pub const MIN_OPTIONS: usize = 2;
pub const MAX_OPTIONS: usize = 10;
/// Characters of a question and of an option, as Telegram's poll takes
/// them.
const MAX_QUESTION_CHARS: usize = 300;
const MAX_OPTION_CHARS: usize = 100;
/// How long a question can be answered.
pub const KEPT: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

pub fn spec() -> GatewayToolSpec {
    GatewayToolSpec {
        name: TOOL_NAME.into(),
        description: "Ask the person you are talking to to choose among a few options, \
                      shown as buttons (or a poll when several may be chosen) in their app. \
                      Use it when a short, fixed choice settles what to do next (which of \
                      these, go ahead or not). Their choice comes as their next message, \
                      marked as one; end your turn after asking, without answering for them."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question, in at most 300 characters."
                },
                "options": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": MIN_OPTIONS,
                    "maxItems": MAX_OPTIONS,
                    "description": "2 to 10 short options, each at most 100 characters."
                },
                "multiple": {
                    "type": "boolean",
                    "description": "Whether several options may be chosen (default: one)."
                }
            },
            "required": ["question", "options"],
            "additionalProperties": false
        }),
    }
}

/// A question the agent asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub question: String,
    pub options: Vec<String>,
    pub multiple: bool,
}

/// The question in a tool call's `input`, checked.
pub fn read(input: &serde_json::Value) -> Result<Asked, String> {
    let question = input
        .get("question")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .ok_or("missing required field: question")?;
    if question.chars().count() > MAX_QUESTION_CHARS {
        return Err(format!(
            "the question is longer than {MAX_QUESTION_CHARS} characters"
        ));
    }
    let options: Vec<String> = input
        .get("options")
        .and_then(serde_json::Value::as_array)
        .ok_or("missing required field: options")?
        .iter()
        .map(|o| o.as_str().map(|o| o.trim().to_string()))
        .collect::<Option<_>>()
        .ok_or("options must be strings")?;
    if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&options.len()) {
        return Err(format!(
            "give {MIN_OPTIONS} to {MAX_OPTIONS} options, not {}",
            options.len()
        ));
    }
    if options.iter().any(String::is_empty) {
        return Err("an option is empty".into());
    }
    if options.iter().any(|o| o.chars().count() > MAX_OPTION_CHARS) {
        return Err(format!(
            "an option is longer than {MAX_OPTION_CHARS} characters"
        ));
    }
    let multiple = input
        .get("multiple")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok(Asked {
        question: question.to_string(),
        options,
        multiple,
    })
}

/// The question as text, its options numbered: what a channel without
/// buttons or polls shows, and the message the buttons go under.
pub fn text(asked: &Asked) -> String {
    let mut out = asked.question.clone();
    out.push('\n');
    for (n, option) in asked.options.iter().enumerate() {
        out.push_str(&format!("\n{}. {option}", n + 1));
    }
    out.push_str(if asked.multiple {
        "\n\n(Choose in the poll, or reply with the numbers.)"
    } else {
        "\n\n(Press one, or reply with its number.)"
    });
    out
}

/// What answers question `id`, followed by the chosen numbers.
fn answer(id: &str) -> String {
    format!("/choose {id}")
}

/// A button for each option of a question of one answer.
pub fn buttons(asked: &Asked, id: &str) -> Vec<Vec<Button>> {
    if asked.multiple {
        return Vec::new();
    }
    asked
        .options
        .iter()
        .enumerate()
        .map(|(n, option)| {
            vec![Button {
                label: option.clone(),
                data: format!("{} {}", answer(id), n + 1),
            }]
        })
        .collect()
}

/// A poll for a question of several answers.
pub fn poll(asked: &Asked, id: &str) -> Option<Poll> {
    asked.multiple.then(|| Poll {
        question: asked.question.clone(),
        options: asked.options.clone(),
        answer: answer(id),
    })
}

/// `/choose`'s arguments: the question's id and the numbers chosen (none,
/// for a poll answered with none).
pub fn read_answer(args: &str) -> Option<(&str, Vec<usize>)> {
    let mut words = args.split_whitespace();
    let id = words.next()?;
    let numbers = match words.next() {
        Some(list) => list
            .split(',')
            .map(|n| n.trim().parse::<usize>().ok())
            .collect::<Option<Vec<_>>>()?,
        None => Vec::new(),
    };
    if words.next().is_some() {
        return None;
    }
    Some((id, numbers))
}

/// The person's choice of `numbers` (from 1) among `asked`'s options, as
/// their message to the agent; why not, when the numbers do not answer it.
pub fn chosen(asked: &Asked, numbers: &[usize]) -> Result<String, String> {
    let mut picked: Vec<usize> = Vec::new();
    for &n in numbers {
        if n == 0 || n > asked.options.len() {
            return Err(format!(
                "(Choose by the options' numbers, 1 to {}.)",
                asked.options.len()
            ));
        }
        if !picked.contains(&n) {
            picked.push(n);
        }
    }
    picked.sort_unstable();
    let quote = |n: usize| serde_json::to_string(&asked.options[n - 1]).unwrap_or_default();
    if asked.multiple {
        if picked.is_empty() {
            return Ok(format!(
                "[Chosen in the poll {}: none of the options]",
                serde_json::to_string(&asked.question).unwrap_or_default()
            ));
        }
        return Ok(format!(
            "[Chosen in the poll {}: {}]",
            serde_json::to_string(&asked.question).unwrap_or_default(),
            picked
                .iter()
                .map(|&n| quote(n))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    match picked.as_slice() {
        [n] => Ok(format!(
            "[Chosen with a button, for {}: {}]",
            serde_json::to_string(&asked.question).unwrap_or_default(),
            quote(*n)
        )),
        _ => Err("(Choose one option, by its number.)".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(multiple: bool) -> Asked {
        read(&json!({
            "question": "Which report?", "options": ["Sales", " Costs "], "multiple": multiple
        }))
        .expect("asked")
    }

    #[test]
    fn a_question_is_checked() {
        assert_eq!(asked(false).options, ["Sales", "Costs"]);
        for bad in [
            json!({"options": ["a", "b"]}),
            json!({"question": "q", "options": ["a"]}),
            json!({"question": "q", "options": (0..11).map(|n| n.to_string()).collect::<Vec<_>>()}),
            json!({"question": "q", "options": ["a", ""]}),
            json!({"question": "q", "options": ["a", 2]}),
            json!({"question": "q", "options": ["a", "x".repeat(101)]}),
            json!({"question": "q".repeat(301), "options": ["a", "b"]}),
        ] {
            assert!(read(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn one_answer_gets_buttons_and_several_a_poll() {
        let one = asked(false);
        let rows = buttons(&one, "abc");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][0].label, "Costs");
        assert_eq!(rows[1][0].data, "/choose abc 2");
        assert!(poll(&one, "abc").is_none());
        let several = asked(true);
        assert!(buttons(&several, "abc").is_empty());
        let p = poll(&several, "abc").expect("poll");
        assert_eq!(p.answer, "/choose abc");
        assert_eq!(p.options, ["Sales", "Costs"]);
        assert!(text(&one).contains("1. Sales\n2. Costs"));
    }

    #[test]
    fn an_answer_is_read_and_checked() {
        assert_eq!(read_answer("abc 2"), Some(("abc", vec![2])));
        assert_eq!(read_answer("abc 1,3"), Some(("abc", vec![1, 3])));
        assert_eq!(read_answer("abc"), Some(("abc", vec![])));
        assert_eq!(read_answer("abc x"), None);
        assert_eq!(read_answer("abc 1 2"), None);
        assert_eq!(read_answer(""), None);
        assert_eq!(
            chosen(&asked(false), &[2]),
            Ok("[Chosen with a button, for \"Which report?\": \"Costs\"]".to_string())
        );
        assert!(chosen(&asked(false), &[1, 2]).is_err());
        assert!(chosen(&asked(false), &[]).is_err());
        assert!(chosen(&asked(false), &[3]).is_err());
        assert_eq!(
            chosen(&asked(true), &[2, 1, 2]),
            Ok("[Chosen in the poll \"Which report?\": \"Sales\", \"Costs\"]".to_string())
        );
        assert_eq!(
            chosen(&asked(true), &[]),
            Ok("[Chosen in the poll \"Which report?\": none of the options]".to_string())
        );
    }
}
