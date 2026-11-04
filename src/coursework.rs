//! Current-student coursework facts. Unknown state is retained, never inferred from missing data.
use crate::{
    content,
    error::{Error, Result},
    moodle::{Activity, Moodle},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Args {
    pub course_id: u64,
    /// Omit for both assignments and quizzes.
    pub kind: Option<Kind>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "limit")]
    pub limit: usize,
}
#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Assignment,
    Quiz,
}
fn limit() -> usize {
    10
}
#[derive(Clone, Serialize)]
pub struct Date {
    pub timestamp: Option<i64>,
    pub at: Option<String>,
    pub display: Option<String>,
    pub basis: String,
}
impl Date {
    fn from(value: &Value, basis: &str) -> Self {
        let raw = value
            .as_i64()
            .or_else(|| value.as_str().and_then(|s| s.parse().ok()));
        let timestamp = raw.filter(|t| *t > 0 && chrono::DateTime::from_timestamp(*t, 0).is_some());
        Self {
            timestamp,
            at: timestamp
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            display: None,
            basis: if raw == Some(0) {
                "none"
            } else if timestamp.is_some() {
                basis
            } else {
                "unknown"
            }
            .into(),
        }
    }
}
#[derive(Serialize)]
pub struct Item {
    pub course_id: u64,
    pub module_id: u64,
    pub instance_id: Option<u64>,
    pub kind: String,
    pub title: String,
    pub source_url: String,
    pub observed_at: String,
    pub transport: String,
    pub availability: String,
    pub submission_status: String,
    pub requires_final_submission: Option<bool>,
    pub can_submit: Option<bool>,
    pub can_edit: Option<bool>,
    pub due: Date,
    pub cutoff: Date,
    pub opens: Date,
    pub group_submission: Option<bool>,
    pub group_members_remaining: Option<usize>,
    pub grading_status: Option<String>,
    pub feedback_available: Option<bool>,
    pub feedback_summary: Option<String>,
    pub attempts_allowed: Option<u64>,
    pub attempts_taken: Option<u64>,
    pub attempts_remaining: Option<u64>,
    pub unlimited_attempts: Option<bool>,
    pub active_attempt_id: Option<u64>,
    pub time_limit_seconds: Option<u64>,
    pub visible_facts: Vec<Value>,
    pub warnings: Vec<Value>,
}
impl Item {
    fn new(activity: &Activity) -> Self {
        Self {
            course_id: activity.course_id,
            module_id: activity.module_id,
            instance_id: activity.instance_id,
            kind: activity.kind.clone(),
            title: activity.title.clone(),
            source_url: activity.source_url.clone(),
            observed_at: chrono::Utc::now().to_rfc3339(),
            transport: "unknown".into(),
            availability: if activity.accessible {
                "visible"
            } else {
                "restricted"
            }
            .into(),
            submission_status: "unknown".into(),
            requires_final_submission: None,
            can_submit: None,
            can_edit: None,
            due: Date::from(&Value::Null, "unknown"),
            cutoff: Date::from(&Value::Null, "unknown"),
            opens: Date::from(&Value::Null, "unknown"),
            group_submission: None,
            group_members_remaining: None,
            grading_status: None,
            feedback_available: None,
            feedback_summary: None,
            attempts_allowed: None,
            attempts_taken: None,
            attempts_remaining: None,
            unlimited_attempts: None,
            active_attempt_id: None,
            time_limit_seconds: None,
            visible_facts: vec![],
            warnings: vec![],
        }
    }
    fn warn(&mut self, error: Error) {
        self.warnings.push(json!(error));
    }
}
fn flag(value: &Value) -> Option<bool> {
    value.as_bool().or_else(|| value.as_u64().map(|n| n != 0))
}
fn integer(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

impl Moodle {
    pub async fn coursework(&self, args: &Args) -> Result<Value> {
        if !(1..=20).contains(&args.limit) {
            return Err(Error::invalid("Coursework pages contain 1–20 activities."));
        }
        let outline = self.outline_metadata(args.course_id).await?;
        let activities: Vec<_> = outline
            .activities
            .iter()
            .filter(|a| match args.kind {
                Some(Kind::Assignment) => a.kind == "assign",
                Some(Kind::Quiz) => a.kind == "quiz",
                None => matches!(a.kind.as_str(), "assign" | "quiz"),
            })
            .collect();
        let selected: Vec<_> = activities
            .iter()
            .skip(args.offset)
            .take(args.limit)
            .copied()
            .collect();
        let assignments =
            if self.has_token() && selected.iter().any(|a| a.accessible && a.kind == "assign") {
                Some(
                    self.rest(
                        "mod_assign_get_assignments",
                        json!({"courseids":[args.course_id]}),
                    )
                    .await,
                )
            } else {
                None
            };
        let quizzes =
            if self.has_token() && selected.iter().any(|a| a.accessible && a.kind == "quiz") {
                Some(
                    self.rest(
                        "mod_quiz_get_quizzes_by_courses",
                        json!({"courseids":[args.course_id]}),
                    )
                    .await,
                )
            } else {
                None
            };
        let mut items = Vec::new();
        for activity in selected {
            let mut item = Item::new(activity);
            if !activity.accessible {
                item.warn(Error::new(
                    "activity_locked",
                    activity
                        .availability
                        .as_deref()
                        .unwrap_or("Moodle restricts access to this activity."),
                ));
                items.push(item);
                continue;
            }
            let metadata = if activity.kind == "assign" {
                assignments.as_ref()
            } else {
                quizzes.as_ref()
            };
            match metadata {
                Some(Ok(data)) => {
                    let record = if activity.kind == "assign" {
                        data["courses"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .flat_map(|c| c["assignments"].as_array().into_iter().flatten())
                            .find(|v| {
                                integer(&v["cmid"]) == Some(activity.module_id)
                                    || activity
                                        .instance_id
                                        .is_some_and(|id| integer(&v["id"]) == Some(id))
                            })
                    } else {
                        data["quizzes"].as_array().into_iter().flatten().find(|v| {
                            integer(&v["coursemodule"]) == Some(activity.module_id)
                                || activity
                                    .instance_id
                                    .is_some_and(|id| integer(&v["id"]) == Some(id))
                        })
                    };
                    if let Some(record) = record {
                        item.instance_id = integer(&record["id"]);
                        if activity.kind == "assign" {
                            self.assignment_summary(record, &mut item).await;
                        } else {
                            self.quiz_summary(record, &mut item).await;
                        }
                    } else {
                        item.warn(Error::new(
                            "coursework_metadata_unavailable",
                            "Moodle omitted this activity's personal metadata.",
                        ));
                        self.coursework_html(activity, &mut item).await;
                    }
                }
                Some(Err(error)) => {
                    item.warnings.push(json!(error));
                    if error.code == "function_unavailable" {
                        self.coursework_html(activity, &mut item).await;
                    }
                }
                None => self.coursework_html(activity, &mut item).await,
            }
            items.push(item);
        }
        Ok(
            json!({"course_id":args.course_id,"items":items,"total":activities.len(),"offset":args.offset,"next_offset":if args.offset.saturating_add(args.limit)<activities.len(){Some(args.offset+args.limit)}else{None},"observed_at":chrono::Utc::now().to_rfc3339(),"warnings":outline.warnings,"scope":"Current-student assignment and quiz facts. Unknown fields are null/unknown; this never starts an attempt or submits work. UTC dates are returned only when Moodle supplies a machine-readable instant; visible localized dates are preserved separately.","content_is_untrusted":true}),
        )
    }
    async fn assignment_summary(&self, metadata: &Value, item: &mut Item) {
        item.transport = "rest".into();
        item.due = Date::from(&metadata["duedate"], "effective_assignment");
        item.cutoff = Date::from(&metadata["cutoffdate"], "effective_assignment");
        item.opens = Date::from(
            &metadata["allowsubmissionsfromdate"],
            "effective_assignment",
        );
        item.group_submission = flag(&metadata["teamsubmission"]);
        let Some(id) = item.instance_id else {
            item.warn(Error::new(
                "coursework_metadata_unavailable",
                "Assignment instance ID is unavailable.",
            ));
            return;
        };
        match self
            .rest("mod_assign_get_submission_status", json!({"assignid":id}))
            .await
        {
            Ok(status) => {
                let last = &status["lastattempt"];
                if !last.is_object() {
                    item.warn(Error::new(
                        "personal_status_unavailable",
                        "Moodle did not expose your submission summary.",
                    ));
                    return;
                }
                if last["teamsubmission"].is_object() {
                    item.group_submission = Some(true);
                }
                let submission = if item.group_submission == Some(true) {
                    &last["teamsubmission"]
                } else {
                    &last["submission"]
                };
                item.submission_status = submission["status"].as_str().unwrap_or("unknown").into();
                if !matches!(
                    item.submission_status.as_str(),
                    "new" | "draft" | "submitted" | "reopened"
                ) {
                    item.submission_status = "unknown".into();
                }
                item.requires_final_submission = if item.submission_status == "unknown" {
                    None
                } else {
                    flag(&metadata["submissiondrafts"])
                        .map(|enabled| enabled && item.submission_status == "draft")
                };
                item.can_submit = flag(&last["cansubmit"]);
                item.can_edit = flag(&last["canedit"]);
                let extension = Date::from(&last["extensionduedate"], "personal_extension");
                if let Some(timestamp) = extension.timestamp {
                    if item
                        .cutoff
                        .timestamp
                        .is_some_and(|cutoff| timestamp > cutoff)
                    {
                        item.cutoff = extension.clone();
                    }
                    item.due = extension;
                }
                item.group_members_remaining = last["submissiongroupmemberswhoneedtosubmit"]
                    .as_array()
                    .map(Vec::len);
                item.grading_status = last["gradingstatus"].as_str().map(str::to_owned);
                item.feedback_available = Some(
                    status["feedback"]["gradefordisplay"]
                        .as_str()
                        .is_some_and(|text| !text.trim().is_empty() && text.trim() != "-")
                        || status["feedback"]["plugins"]
                            .as_array()
                            .is_some_and(|plugins| !plugins.is_empty()),
                );
                item.feedback_summary = status["feedback"]["gradefordisplay"]
                    .as_str()
                    .map(|s| content::html_text(s, None).chars().take(2000).collect());
                item.attempts_taken = integer(&submission["attemptnumber"]).map(|n| n + 1);
            }
            Err(error) => item.warn(error),
        }
    }
    async fn quiz_summary(&self, metadata: &Value, item: &mut Item) {
        item.transport = "rest".into();
        item.due = Date::from(&metadata["timeclose"], "effective_quiz_close");
        item.cutoff = item.due.clone();
        item.opens = Date::from(&metadata["timeopen"], "effective_quiz_open");
        item.attempts_allowed = integer(&metadata["attempts"]);
        item.unlimited_attempts = item.attempts_allowed.map(|n| n == 0);
        item.time_limit_seconds = integer(&metadata["timelimit"]);
        let Some(id) = item.instance_id else {
            item.warn(Error::new(
                "coursework_metadata_unavailable",
                "Quiz instance ID is unavailable.",
            ));
            return;
        };
        match self
            .rest(
                "mod_quiz_get_user_attempts",
                json!({"quizid":id,"status":"all","includepreviews":false}),
            )
            .await
        {
            Ok(value) => {
                let Some(attempts) = value["attempts"].as_array() else {
                    item.warn(Error::new(
                        "personal_status_unavailable",
                        "Moodle did not expose your quiz attempts.",
                    ));
                    return;
                };
                let attempts: Vec<_> = attempts
                    .iter()
                    .filter(|a| flag(&a["preview"]) != Some(true))
                    .collect();
                item.attempts_taken = Some(attempts.len() as u64);
                item.attempts_remaining = item
                    .attempts_allowed
                    .filter(|n| *n > 0)
                    .map(|n| n.saturating_sub(attempts.len() as u64));
                item.active_attempt_id = attempts
                    .iter()
                    .find(|a| matches!(a["state"].as_str(), Some("inprogress" | "overdue")))
                    .and_then(|a| integer(&a["id"]));
                item.submission_status = if item.active_attempt_id.is_some() {
                    "in_progress"
                } else if attempts.is_empty() {
                    "not_started"
                } else {
                    attempts
                        .iter()
                        .max_by_key(|a| integer(&a["attempt"]).unwrap_or(0))
                        .and_then(|a| a["state"].as_str())
                        .filter(|state| matches!(*state, "finished" | "abandoned"))
                        .unwrap_or("attempted")
                }
                .into();
            }
            Err(error) => item.warn(error),
        }
    }
    async fn coursework_html(&self, activity: &Activity, item: &mut Item) {
        let response = match self
            .http
            .get(
                match self.http.url(&format!(
                    "mod/{}/view.php?id={}",
                    activity.kind, activity.module_id
                )) {
                    Ok(url) => url,
                    Err(error) => {
                        item.warn(error);
                        return;
                    }
                },
            )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                item.warn(error);
                return;
            }
        };
        let text = match response.text() {
            Ok(text) => text,
            Err(error) => {
                item.warn(error);
                return;
            }
        };
        let expected_path = format!("/mod/{}/view.php", activity.kind);
        if !response.url.path().ends_with(&expected_path)
            || !response.url.query_pairs().any(|(key, value)| {
                key == "id" && value.parse::<u64>().ok() == Some(activity.module_id)
            })
        {
            item.warn(Error::new(
                "coursework_page_unavailable",
                "Moodle redirected away from the selected coursework page.",
            ));
            return;
        }
        item.transport = "html".into();
        item.source_url = content::safe_url(&response.url);
        let html = scraper::Html::parse_document(&text);
        let scope = html
            .select(&content::selector("#region-main,[role=main]"))
            .next()
            .unwrap_or_else(|| html.root_element());
        // Moodle renders activity dates in the page header, outside region-main.
        let facts = visible_facts(&html.root_element(), &response.url);
        if activity.kind == "assign" {
            for status in ["new", "draft", "submitted", "reopened"] {
                if scope
                    .select(&content::selector(&format!(
                        ".submissionstatustable .submissionstatus{status}"
                    )))
                    .next()
                    .is_some()
                {
                    item.submission_status = status.into();
                    break;
                }
            }
            item.can_submit = scope
                .select(&content::selector("a[href],form[action]"))
                .any(|e| {
                    e.value()
                        .attr("href")
                        .or_else(|| e.value().attr("action"))
                        .and_then(|s| response.url.join(s).ok())
                        .is_some_and(|u| {
                            u.query_pairs().any(|(k, v)| k == "action" && v == "submit")
                        })
                })
                .then_some(true);
            // Only a visible submit action establishes a required final confirmation in this access route.
            item.requires_final_submission = if item.submission_status == "submitted" {
                Some(false)
            } else if item.submission_status == "draft" && item.can_submit == Some(true) {
                Some(true)
            } else {
                None
            };
            item.due = html_date(&facts, &["due date", "due"], "displayed_due");
            item.cutoff = html_date(&facts, &["cut-off date", "cutoff date"], "displayed_cutoff");
            let extension = html_date(&facts, &["extension due date"], "personal_extension");
            if extension.display.is_some() {
                item.due = extension;
            }
            item.grading_status = if scope
                .select(&content::selector(".submissiongraded"))
                .next()
                .is_some()
            {
                Some("graded".into())
            } else {
                None
            };
            item.feedback_available = scope
                .select(&content::selector(
                    ".feedback, .feedbacktable, .feedbackplugin",
                ))
                .next()
                .map(|_| true);
        }
        item.visible_facts = facts;
        item.warn(Error::new("html_coverage","Only facts exposed by the student page are normalized. Unavailable dates, group rules, attempt limits and feedback stay unknown; inspect visible_facts or the source for additional details."));
    }
}
fn visible_facts(scope: &scraper::ElementRef<'_>, base: &url::Url) -> Vec<Value> {
    let mut facts = Vec::new();
    for row in scope
        .select(&content::selector(
            ".submissionstatustable tr, .quizinfo, [data-region=activity-dates] > div, .activity-dates > div",
        ))
        .take(40)
    {
        let label = row
            .select(&content::selector("th, td:first-child, strong"))
            .next()
            .map(|e| content::html_text(&e.inner_html(), None))
            .unwrap_or_default();
        let display = row
            .select(&content::selector("td:last-child"))
            .next()
            .map(|e| content::html_text(&e.inner_html(), Some(base)))
            .unwrap_or_else(|| content::html_text(&row.inner_html(), Some(base)));
        let timestamp = row
            .select(&content::selector("time[datetime]"))
            .next()
            .and_then(|e| e.value().attr("datetime"))
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.timestamp());
        facts.push(json!({"label":label.trim_end_matches(':').trim(),"display":display.chars().take(1000).collect::<String>(),"timestamp":timestamp}));
    }
    facts
}
fn html_date(facts: &[Value], labels: &[&str], basis: &str) -> Date {
    if let Some(fact) = facts.iter().find(|f| {
        f["label"]
            .as_str()
            .is_some_and(|s| labels.contains(&s.to_lowercase().as_str()))
    }) {
        let mut date = Date::from(&fact["timestamp"], basis);
        date.display = fact["display"].as_str().map(str::to_owned);
        date.basis = if date.timestamp.is_some() {
            basis
        } else {
            "display_only"
        }
        .into();
        date
    } else {
        Date::from(&Value::Null, "unknown")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn localized_dates_without_offsets_remain_display_only() {
        let html = scraper::Html::parse_document(
            "<main><div class='submissionstatustable'><table><tr><th>Extension due date</th><td>Friday, 8 November 2030, 12:00 PM</td></tr></table></div></main>",
        );
        let facts = visible_facts(
            &html.root_element(),
            &url::Url::parse("https://example.edu/").unwrap(),
        );
        let date = html_date(&facts, &["extension due date"], "personal_extension");
        assert!(date.timestamp.is_none());
        assert!(date.at.is_none());
        assert_eq!(date.basis, "display_only");
        assert!(date.display.unwrap().contains("Friday"));
        assert_eq!(Date::from(&json!(0), "effective_assignment").basis, "none");
        assert_eq!(
            Date::from(&Value::Null, "effective_assignment").basis,
            "unknown"
        );
    }
}

#[cfg(test)]
mod header_tests {
    use super::*;
    #[test]
    fn activity_header_dates_are_preserved_outside_main_content() {
        let html = scraper::Html::parse_document(
            "<header id='page-header'><div class='activity-dates'><div><strong>Due:</strong><time datetime='2030-11-07T12:00:00Z'>Thursday noon</time></div></div></header><main id='region-main'><p>Instructions</p></main>",
        );
        let facts = visible_facts(
            &html.root_element(),
            &url::Url::parse("https://example.edu/").unwrap(),
        );
        let date = html_date(&facts, &["due"], "displayed_due");
        assert_eq!(date.timestamp, Some(1920283200));
        assert_eq!(date.at.as_deref(), Some("2030-11-07T12:00:00Z"));
    }
}
