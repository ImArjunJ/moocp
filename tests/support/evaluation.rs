//! Outcome grading for synthetic student tasks, independent of the agent's chosen tool sequence.
#![allow(dead_code)]
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};

#[derive(Deserialize)]
pub struct Case {
    pub id: String,
    pub prompt: String,
    pub facts: BTreeMap<String, Rule>,
    pub citation_suffixes: Vec<String>,
    pub required_tools: Vec<String>,
    pub transports: Vec<String>,
}
#[derive(Deserialize)]
#[serde(untagged)]
pub enum Rule {
    Exact(String),
    Contains { contains: String },
}
pub fn cases() -> Vec<Case> {
    serde_json::from_str(include_str!("../fixtures/tasks.json")).expect("evaluation task manifest")
}
pub fn schema() -> Value {
    json!({"type":"object","properties":{"facts":{"type":"array","items":{"type":"object","properties":{"key":{"type":"string"},"value":{"type":"string"}},"required":["key","value"],"additionalProperties":false}},"citations":{"type":"array","items":{"type":"string"}}},"required":["facts","citations"],"additionalProperties":false})
}
#[derive(Default, Serialize)]
pub struct Trace {
    pub tool_calls: usize,
    pub tool_response_bytes: usize,
    pub tools: HashSet<String>,
    pub usage: Option<Value>,
    pub violations: Vec<String>,
    pub failures: Vec<String>,
}
impl Trace {
    pub fn observe(&mut self, event: &Value) {
        if event["type"] == "turn.completed" {
            self.usage = event.get("usage").cloned();
        }
        if matches!(event["type"].as_str(), Some("turn.failed" | "error")) {
            self.failures.push(event.to_string());
        }
        if event["type"] != "item.completed" {
            return;
        }
        let item = &event["item"];
        match item["type"].as_str() {
            Some("mcp_tool_call") => {
                self.tool_calls += 1;
                let tool = item["tool"].as_str().unwrap_or_default();
                self.tools.insert(tool.into());
                if item["server"] != "moocp_eval" {
                    self.violations
                        .push("Used an MCP server outside the evaluation fixture.".into());
                }
                if !allowed_tools().contains(&tool) {
                    self.violations
                        .push(format!("Used non-evaluation tool {tool}."));
                }
                if let Some(result) = item.get("result") {
                    self.tool_response_bytes += result.to_string().len();
                }
            }
            Some("command_execution" | "web_search") => self
                .violations
                .push("Used shell/web access instead of fixture tools.".into()),
            _ => (),
        }
    }
}
pub fn allowed_tools() -> &'static [&'static str] {
    &[
        "moodle_list_courses",
        "moodle_get_course",
        "moodle_list_materials",
        "moodle_get_coursework",
        "moodle_get_activity",
        "moodle_read",
        "moodle_search",
    ]
}
#[derive(Serialize)]
pub struct Grade {
    pub passed: bool,
    pub errors: Vec<String>,
}
pub fn grade(case: &Case, answer: &Value, site: &url::Url, trace: &Trace) -> Grade {
    let mut errors = trace.violations.clone();
    errors.extend(trace.failures.clone());
    let facts: BTreeMap<_, _> = answer["facts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| Some((f["key"].as_str()?, f["value"].as_str()?)))
        .collect();
    if facts.len() != case.facts.len()
        || answer["facts"].as_array().map(Vec::len) != Some(case.facts.len())
    {
        errors.push("Expected exactly the requested facts, without duplicate keys.".into());
    }
    for (key, rule) in &case.facts {
        let found = facts.get(key.as_str()).copied().unwrap_or_default().trim();
        let matched = match rule {
            Rule::Exact(expected) => {
                let dates = chrono::DateTime::parse_from_rfc3339(expected)
                    .ok()
                    .zip(chrono::DateTime::parse_from_rfc3339(found).ok());
                dates
                    .map(|(a, b)| a.timestamp() == b.timestamp())
                    .unwrap_or_else(|| found.eq_ignore_ascii_case(expected))
            }
            Rule::Contains { contains } => found.to_lowercase().contains(&contains.to_lowercase()),
        };
        if !matched {
            errors.push(format!("Incorrect or missing fact: {key}."));
        }
    }
    let citations: Vec<_> = answer["citations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    for suffix in &case.citation_suffixes {
        let expected = site.join(suffix).expect("synthetic source path");
        if !citations.iter().any(|citation| {
            url::Url::parse(citation).ok().is_some_and(|mut url| {
                url.set_fragment(None);
                url == expected
            })
        }) {
            errors.push(format!("Missing correct source citation: {suffix}."));
        }
    }
    if citations.iter().any(|citation| {
        url::Url::parse(citation).ok().is_none_or(|url| {
            url.origin() != site.origin()
                || !url.path().starts_with(site.path())
                || !url.username().is_empty()
                || url.password().is_some()
        })
    }) {
        errors.push("Citations must belong to the synthetic Moodle site.".into());
    }
    for tool in &case.required_tools {
        if !trace.tools.contains(tool) {
            errors.push(format!("Task evidence requires {tool}."));
        }
    }
    Grade {
        passed: errors.is_empty(),
        errors,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grader_rejects_wrong_personal_deadlines_uncited_answers_and_unrelated_tools() {
        let case = cases()
            .into_iter()
            .find(|c| c.id == "personal-deadline")
            .unwrap();
        let site = url::Url::parse("http://127.0.0.1:1234/moodle/").unwrap();
        let mut trace = Trace::default();
        trace.tools.insert("moodle_get_coursework".into());
        let mut answer = json!({"facts":[{"key":"effective_due","value":"2030-11-08T12:00:00Z"},{"key":"cutoff","value":"2030-11-09T12:00:00Z"}],"citations":["http://127.0.0.1:1234/moodle/mod/assign/view.php?id=20"]});
        assert!(grade(&case, &answer, &site, &trace).passed);
        answer["facts"][0]["value"] = json!("2030-11-07T12:00:00Z");
        assert!(!grade(&case, &answer, &site, &trace).passed);
        answer["facts"][0]["value"] = json!("2030-11-08T12:00:00Z");
        answer["citations"] = json!([]);
        assert!(!grade(&case, &answer, &site, &trace).passed);
        trace.violations.push("Used real account".into());
        assert!(!grade(&case, &answer, &site, &trace).passed);
    }
    #[test]
    fn trace_counts_actual_tool_calls_and_reports_token_usage() {
        let mut trace = Trace::default();
        trace.observe(&json!({"type":"item.completed","item":{"type":"mcp_tool_call","server":"moocp_eval","tool":"moodle_read","result":{"content":"source"}}}));
        trace.observe(
            &json!({"type":"turn.completed","usage":{"input_tokens":12,"output_tokens":4}}),
        );
        assert_eq!(trace.tool_calls, 1);
        assert!(trace.tool_response_bytes > 0);
        assert_eq!(trace.usage.unwrap()["input_tokens"], 12);
    }
}
