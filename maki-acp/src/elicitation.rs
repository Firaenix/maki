//! Maps the `question` tool onto what the client can render: `elicitation/create`
//! when it declares form elicitation, one `session/request_permission` per
//! question otherwise. Clients with neither (Paseo, most ACP adapters) still get
//! clickable choices.

use std::collections::BTreeMap;

use agent_client_protocol_schema::v1::{
    ClientCapabilities, CreateElicitationRequest, ElicitationContentValue, ElicitationFormMode,
    ElicitationPropertySchema, ElicitationSchema, ElicitationSessionScope, EnumOption,
    MultiSelectPropertySchema, PermissionOption, PermissionOptionId, PermissionOptionKind,
    RequestPermissionRequest, SessionId, StringPropertySchema, ToolCallId, ToolCallUpdate,
    ToolCallUpdateFields,
};
use serde::Deserialize;
use serde_json::Value;

/// Mirrors the Lua question tool's dismiss output so the model sees the same
/// text regardless of frontend.
pub const DISMISSED: &str = "(question dismissed by user)";
const NO_ANSWER: &str = "(no answer)";

#[derive(Deserialize)]
struct Question {
    question: String,
    #[serde(default)]
    header: String,
    #[serde(default)]
    options: Vec<QuestionOption>,
    #[serde(default, rename = "multiSelect", alias = "multiple")]
    multi_select: bool,
}

#[derive(Deserialize)]
struct QuestionOption {
    label: String,
    #[serde(default)]
    description: String,
}

pub fn supports_form(caps: &ClientCapabilities) -> bool {
    caps.elicitation.as_ref().is_some_and(|e| e.form.is_some())
}

const DISMISS_OPTION: &str = "dismiss";

/// One `session/request_permission` per question: the option list is flat, so a
/// merged dialog could not say which answer belongs to which question, and a
/// `multiSelect` question degrades to a single choice here.
///
/// A question with no options has no free-text channel to answer it, so those
/// are refused rather than faked with an empty answer.
pub fn option_requests(
    session_id: &str,
    tool_call_id: &str,
    input: &Value,
) -> Result<Vec<RequestPermissionRequest>, String> {
    let questions = parse_questions(input)?;
    if questions.is_empty() {
        return Err("at least one question is required".to_owned());
    }
    if questions.iter().any(|question| question.options.is_empty()) {
        return Err(
            "this client has no form elicitation, so every question needs options; \
             ask free-text questions in prose instead"
                .to_owned(),
        );
    }
    Ok(questions
        .iter()
        .map(|question| {
            let fields = ToolCallUpdateFields::new()
                .title(question.question.clone())
                .raw_input(serde_json::json!({ "question": question.question }));
            let options = question
                .options
                .iter()
                .enumerate()
                .map(|(index, option)| {
                    let title = if option.description.is_empty() {
                        option.label.clone()
                    } else {
                        format!("{} - {}", option.label, option.description)
                    };
                    PermissionOption::new(
                        PermissionOptionId::from(format!("opt{}", index + 1)),
                        title,
                        PermissionOptionKind::AllowOnce,
                    )
                })
                .chain(std::iter::once(PermissionOption::new(
                    PermissionOptionId::from(DISMISS_OPTION),
                    "Dismiss",
                    PermissionOptionKind::RejectOnce,
                )))
                .collect();
            RequestPermissionRequest::new(
                SessionId::from(session_id.to_owned()),
                ToolCallUpdate::new(ToolCallId::from(tool_call_id.to_owned()), fields),
                options,
            )
        })
        .collect())
}

fn selected_option(raw: &str) -> Option<String> {
    let response: Value = serde_json::from_str(raw).ok()?;
    let outcome = response.get("outcome")?;
    if outcome["outcome"] != "selected" {
        return None;
    }
    outcome.get("optionId")?.as_str().map(str::to_owned)
}

/// One `header: label` line, mirroring `format_response`'s output shape.
pub fn format_option_response(input: &Value, index: usize, raw: &str) -> String {
    let questions = parse_questions(input).unwrap_or_default();
    let Some(question) = questions.get(index) else {
        return DISMISSED.to_owned();
    };
    let label = if question.header.is_empty() {
        format!("Q{}", index + 1)
    } else {
        question.header.clone()
    };
    let answer = match selected_option(raw) {
        None => NO_ANSWER.to_owned(),
        Some(option) if option == DISMISS_OPTION => NO_ANSWER.to_owned(),
        Some(option) => match option
            .strip_prefix("opt")
            .and_then(|n| n.parse::<usize>().ok())
        {
            Some(number) => number
                .checked_sub(1)
                .and_then(|index| question.options.get(index))
                .map(|choice| choice.label.clone())
                .unwrap_or(option),
            None => option,
        },
    };
    format!("{label}: {answer}")
}

fn parse_questions(input: &Value) -> Result<Vec<Question>, String> {
    let questions = input.get("questions").cloned().unwrap_or(Value::Null);
    serde_json::from_value(questions).map_err(|e| format!("invalid questions: {e}"))
}

fn enum_options(options: &[QuestionOption]) -> Vec<EnumOption> {
    options
        .iter()
        .map(|opt| {
            let title = if opt.description.is_empty() {
                opt.label.clone()
            } else {
                format!("{} - {}", opt.label, opt.description)
            };
            EnumOption::new(opt.label.clone(), title)
        })
        .collect()
}

fn property(q: &Question) -> ElicitationPropertySchema {
    let title = q.question.clone();
    if q.options.is_empty() {
        ElicitationPropertySchema::String(StringPropertySchema::new().title(title))
    } else if q.multi_select {
        ElicitationPropertySchema::Array(
            MultiSelectPropertySchema::titled(enum_options(&q.options)).title(title),
        )
    } else {
        ElicitationPropertySchema::String(
            StringPropertySchema::new()
                .title(title)
                .one_of(enum_options(&q.options)),
        )
    }
}

/// Property keys are positional (`q1`, `q2`, ...) so answers map back to
/// questions even when headers repeat or are missing.
fn key(index: usize) -> String {
    format!("q{}", index + 1)
}

pub fn form_request(
    session_id: &str,
    tool_call_id: Option<String>,
    input: &Value,
) -> Result<CreateElicitationRequest, String> {
    let questions = parse_questions(input)?;
    if questions.is_empty() {
        return Err("at least one question is required".to_owned());
    }

    let mut schema = ElicitationSchema::new();
    schema.properties = questions
        .iter()
        .enumerate()
        .map(|(i, q)| (key(i), property(q)))
        .collect();

    let scope = ElicitationSessionScope::new(SessionId::from(session_id.to_owned()))
        .tool_call_id(tool_call_id.map(ToolCallId::from));
    let message = match questions.as_slice() {
        [only] => only.question.clone(),
        many => format!("{} questions", many.len()),
    };
    Ok(CreateElicitationRequest::new(
        ElicitationFormMode::new(scope, schema),
        message,
    ))
}

fn answer_text(value: Option<&ElicitationContentValue>) -> Option<String> {
    match value? {
        ElicitationContentValue::String(s) if !s.is_empty() => Some(s.clone()),
        ElicitationContentValue::StringArray(items) if !items.is_empty() => Some(items.join(", ")),
        ElicitationContentValue::Integer(n) => Some(n.to_string()),
        ElicitationContentValue::Number(n) => Some(n.to_string()),
        ElicitationContentValue::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Turns the client's `elicitation/create` result into the same markdown the
/// Lua tool feeds the model: one `header: labels` line per question.
pub fn format_response(input: &Value, raw_result: &str) -> String {
    let Ok(response) = serde_json::from_str::<Value>(raw_result) else {
        return DISMISSED.to_owned();
    };
    if response["action"] != "accept" {
        return DISMISSED.to_owned();
    }
    // Values parse per key: nothing in the schema is `required`, so clients
    // may null out skipped fields, and one unreadable value should cost one
    // answer, not the whole form.
    let content: BTreeMap<String, ElicitationContentValue> = response["content"]
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| Some((k.clone(), serde_json::from_value(v.clone()).ok()?)))
                .collect()
        })
        .unwrap_or_default();
    let questions = parse_questions(input).unwrap_or_default();

    questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let label = if q.header.is_empty() {
                format!("Q{}", i + 1)
            } else {
                q.header.clone()
            };
            let answer = answer_text(content.get(&key(i))).unwrap_or_else(|| NO_ANSWER.to_owned());
            format!("{label}: {answer}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::ElicitationScope;
    use test_case::test_case;

    use super::*;

    fn questions_input() -> Value {
        serde_json::json!({
            "questions": [
                {
                    "question": "Pick a framework",
                    "header": "Framework",
                    "options": [
                        { "label": "axum", "description": "tokio based" },
                        { "label": "actix" }
                    ]
                },
                {
                    "question": "Which features?",
                    "header": "Features",
                    "multiSelect": true,
                    "options": [{ "label": "auth" }, { "label": "uploads" }]
                },
                { "question": "Anything else?" }
            ]
        })
    }

    #[test]
    fn form_request_maps_questions_to_schema() {
        let req = form_request("sess_1", Some("tool_1".to_owned()), &questions_input()).unwrap();
        assert_eq!(req.message, "3 questions");

        let ElicitationScope::Session(scope) = req.scope() else {
            panic!("expected session scope");
        };
        assert_eq!(scope.session_id.0.as_ref(), "sess_1");
        assert_eq!(
            scope.tool_call_id.as_ref().map(|t| t.0.as_ref()),
            Some("tool_1")
        );

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["mode"], "form");
        let props = &json["requestedSchema"]["properties"];
        assert_eq!(props["q1"]["type"], "string");
        assert_eq!(props["q1"]["oneOf"][0]["const"], "axum");
        assert_eq!(props["q2"]["type"], "array");
        assert_eq!(props["q3"]["type"], "string");
        assert!(props["q3"].get("oneOf").is_none());
    }

    #[test]
    fn single_question_is_the_message() {
        let input = serde_json::json!({ "questions": [{ "question": "Proceed?" }] });
        let req = form_request("sess_1", None, &input).unwrap();
        assert_eq!(req.message, "Proceed?");
    }

    #[test_case(serde_json::json!({}) ; "missing_questions")]
    #[test_case(serde_json::json!({ "questions": [] }) ; "empty_questions")]
    fn form_request_rejects_bad_input(input: Value) {
        assert!(form_request("sess_1", None, &input).is_err());
    }

    #[test]
    fn accept_response_formats_answers() {
        let raw = serde_json::json!({
            "action": "accept",
            "content": { "q1": "axum", "q2": ["auth", "uploads"] }
        })
        .to_string();
        assert_eq!(
            format_response(&questions_input(), &raw),
            "Framework: axum\nFeatures: auth, uploads\nQ3: (no answer)"
        );
    }

    #[test]
    fn nulled_out_field_costs_one_answer_not_the_form() {
        let raw = serde_json::json!({
            "action": "accept",
            "content": { "q1": "axum", "q2": null }
        })
        .to_string();
        assert_eq!(
            format_response(&questions_input(), &raw),
            "Framework: axum\nFeatures: (no answer)\nQ3: (no answer)"
        );
    }

    #[test_case(r#"{"action":"decline"}"# ; "decline")]
    #[test_case(r#"{"action":"cancel"}"# ; "cancel")]
    #[test_case("not json" ; "unparsable")]
    #[test_case("null" ; "jsonrpc_error_forwarded_as_null")]
    fn non_accept_is_dismissed(raw: &str) {
        assert_eq!(format_response(&questions_input(), raw), DISMISSED);
    }

    #[test]
    fn supports_form_requires_form_capability() {
        assert!(!supports_form(&ClientCapabilities::default()));
        let caps: ClientCapabilities = serde_json::from_value(serde_json::json!({
            "elicitation": { "form": {} }
        }))
        .unwrap();
        assert!(supports_form(&caps));
        let url_only: ClientCapabilities = serde_json::from_value(serde_json::json!({
            "elicitation": { "url": {} }
        }))
        .unwrap();
        assert!(!supports_form(&url_only));
    }

    fn all_optional_input() -> Value {
        serde_json::json!({
            "questions": [
                {
                    "question": "Pick a framework",
                    "header": "Framework",
                    "options": [
                        { "label": "axum", "description": "tokio based" },
                        { "label": "actix" }
                    ]
                },
                { "question": "Ship it?", "options": [{ "label": "yes" }, { "label": "no" }] }
            ]
        })
    }

    #[test]
    fn option_requests_ask_one_permission_per_question() {
        let requests = option_requests("sess_1", "tool_1", &all_optional_input()).unwrap();
        assert_eq!(requests.len(), 2);

        let json = serde_json::to_value(&requests[0]).unwrap();
        assert_eq!(json["sessionId"], "sess_1");
        assert_eq!(json["toolCall"]["toolCallId"], "tool_1");
        assert_eq!(json["toolCall"]["title"], "Pick a framework");

        let options = json["options"].as_array().unwrap();
        assert_eq!(options.len(), 3);
        assert_eq!(options[0]["optionId"], "opt1");
        assert_eq!(options[0]["name"], "axum - tokio based");
        assert_eq!(options[1]["name"], "actix");
        assert_eq!(options[2]["optionId"], DISMISS_OPTION);
        assert_eq!(options[2]["name"], "Dismiss");
    }

    #[test_case(serde_json::json!({ "questions": [] }) ; "no_questions")]
    #[test_case(questions_input() ; "a_question_without_options")]
    fn option_requests_refuse_unanswerable_questions(input: Value) {
        assert!(option_requests("sess_1", "tool_1", &input).is_err());
    }

    #[test_case(r#"{"outcome":{"outcome":"selected","optionId":"opt1"}}"# => "Framework: axum"; "first_option")]
    #[test_case(r#"{"outcome":{"outcome":"selected","optionId":"opt2"}}"# => "Framework: actix"; "last_option")]
    #[test_case(r#"{"outcome":{"outcome":"selected","optionId":"dismiss"}}"# => "Framework: (no answer)"; "dismissed")]
    #[test_case(r#"{"outcome":{"outcome":"cancelled"}}"# => "Framework: (no answer)"; "cancelled")]
    #[test_case("not json" => "Framework: (no answer)"; "unparsable")]
    #[test_case(r#"{"outcome":{"outcome":"selected","optionId":"opt0"}}"# => "Framework: opt0"; "opt0_is_not_a_valid_index")]
    #[test_case(r#"{"outcome":{"outcome":"selected","optionId":"sideways"}}"# => "Framework: sideways"; "unknown_option_is_passed_through")]
    fn option_answer_formats_one_line(raw: &str) -> String {
        format_option_response(&all_optional_input(), 0, raw)
    }

    #[test]
    fn headerless_questions_are_numbered() {
        let input = serde_json::json!({
            "questions": [{ "question": "Ship it?", "options": [{ "label": "yes" }] }]
        });
        assert_eq!(
            format_option_response(
                &input,
                0,
                r#"{"outcome":{"outcome":"selected","optionId":"opt1"}}"#
            ),
            "Q1: yes"
        );
    }

    #[test]
    fn out_of_range_index_is_dismissed() {
        assert_eq!(
            format_option_response(
                &all_optional_input(),
                9,
                r#"{"outcome":{"outcome":"selected","optionId":"opt1"}}"#
            ),
            DISMISSED
        );
    }
}
