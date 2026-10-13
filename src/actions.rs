use base64::Engine;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    content,
    error::{Error, Result},
    moodle::{Activity, Moodle},
    store::Store,
};

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnswerField {
    /// Exact answer input name returned by moodle_read_quiz_attempt.
    pub name: String,
    pub value: String,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    SetCompletion {
        course_id: u64,
        module_id: u64,
        completed: bool,
    },
    SendMessage {
        recipient_id: u64,
        message: String,
    },
    ReplyForum {
        course_id: u64,
        module_id: u64,
        post_id: u64,
        subject: String,
        message: String,
    },
    CreateDiscussion {
        course_id: u64,
        module_id: u64,
        subject: String,
        message: String,
    },
    SaveAssignment {
        course_id: u64,
        module_id: u64,
        /// Online submission text. Plain text is safely escaped into Moodle HTML.
        text: Option<String>,
        /// Absolute paths to coursework files. Their bytes are snapshotted at preparation.
        files: Vec<String>,
    },
    SubmitAssignment {
        course_id: u64,
        module_id: u64,
        /// Whether the student accepts the submission statement shown in the preview.
        accept_submission_statement: bool,
    },
    StartQuiz {
        course_id: u64,
        module_id: u64,
    },
    SaveQuizAnswers {
        course_id: u64,
        module_id: u64,
        attempt_id: u64,
        page: u64,
        fields: Vec<AnswerField>,
    },
    FinishQuiz {
        course_id: u64,
        module_id: u64,
        attempt_id: u64,
    },
    CreateCalendarEvent {
        title: String,
        description: String,
        /// Unix timestamp in seconds, interpreted as an instant.
        starts_at: i64,
        duration_seconds: u64,
    },
    RespondChoice {
        course_id: u64,
        module_id: u64,
        option_ids: Vec<u64>,
    },
}

#[derive(Serialize, Deserialize)]
struct FileSnapshot {
    name: String,
    sha256: String,
    bytes: String,
}

fn hash(value: &Value) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).unwrap_or_default())
    )
}
fn nonempty(value: &str, label: &str) -> Result<()> {
    if value.trim().is_empty() || value.chars().count() > 50_000 {
        Err(Error::invalid(format!(
            "{label} must contain 1–50000 characters."
        )))
    } else {
        Ok(())
    }
}
fn text_html(text: &str) -> String {
    html_escape::encode_text(text).replace('\n', "<br>\n")
}

impl Action {
    pub fn target(&self) -> Option<(u64, u64, &'static str)> {
        match self {
            Self::SetCompletion {
                course_id,
                module_id,
                ..
            } => Some((*course_id, *module_id, "any")),
            Self::ReplyForum {
                course_id,
                module_id,
                ..
            }
            | Self::CreateDiscussion {
                course_id,
                module_id,
                ..
            } => Some((*course_id, *module_id, "forum")),
            Self::SaveAssignment {
                course_id,
                module_id,
                ..
            }
            | Self::SubmitAssignment {
                course_id,
                module_id,
                ..
            } => Some((*course_id, *module_id, "assign")),
            Self::StartQuiz {
                course_id,
                module_id,
            }
            | Self::SaveQuizAnswers {
                course_id,
                module_id,
                ..
            }
            | Self::FinishQuiz {
                course_id,
                module_id,
                ..
            } => Some((*course_id, *module_id, "quiz")),
            Self::RespondChoice {
                course_id,
                module_id,
                ..
            } => Some((*course_id, *module_id, "choice")),
            _ => None,
        }
    }
    pub fn effect(&self) -> &'static str {
        match self {
            Self::SetCompletion { .. } => "Change your manual activity completion state.",
            Self::SendMessage { .. } => "Send a Moodle message to the specified user.",
            Self::ReplyForum { .. } | Self::CreateDiscussion { .. } => {
                "Publish a forum post visible to the forum audience."
            }
            Self::SaveAssignment { .. } => {
                "Save coursework in Moodle. Some assignment configurations submit immediately when saved; inspect the preview and submission settings."
            }
            Self::SubmitAssignment { .. } => {
                "Submit the existing coursework for grading; it may become locked."
            }
            Self::StartQuiz { .. } => {
                "Start a quiz attempt; this may consume an attempt and begin a timer."
            }
            Self::SaveQuizAnswers { .. } => {
                "Save responses to an existing quiz attempt without finishing it."
            }
            Self::FinishQuiz { .. } => "Finish and submit this quiz attempt for grading.",
            Self::CreateCalendarEvent { .. } => "Create a personal Moodle calendar event.",
            Self::RespondChoice { .. } => {
                "Save your choice response; the activity may limit subsequent changes."
            }
        }
    }
    fn validate(&self) -> Result<()> {
        if let Some((course, module, _)) = self.target()
            && (course == 0 || module == 0)
        {
            return Err(Error::invalid("Course and module IDs must be positive."));
        }
        match self {
            Self::SendMessage {
                recipient_id,
                message,
            } => {
                if *recipient_id == 0 {
                    return Err(Error::invalid("Recipient ID must be positive."));
                }
                nonempty(message, "Message")?;
            }
            Self::ReplyForum {
                subject, message, ..
            }
            | Self::CreateDiscussion {
                subject, message, ..
            } => {
                nonempty(subject, "Subject")?;
                nonempty(message, "Message")?;
            }
            Self::SaveAssignment { text, files, .. } => {
                if text.as_ref().is_none_or(|s| s.trim().is_empty()) && files.is_empty() {
                    return Err(Error::invalid(
                        "Provide submission text or at least one file.",
                    ));
                }
                if files.len() > 20 {
                    return Err(Error::invalid("At most 20 files per proposal."));
                }
                if let Some(text) = text {
                    nonempty(text, "Submission text")?;
                }
            }
            Self::SaveQuizAnswers { fields, .. } => {
                if fields.is_empty() || fields.len() > 100 {
                    return Err(Error::invalid("Provide 1–100 answer fields."));
                }
                let pattern =
                    regex::Regex::new(r"^q\d+:\d+_[A-Za-z0-9_:]+$").expect("static regex");
                for f in fields {
                    if !pattern.is_match(&f.name)
                        || f.name.ends_with("_-submit")
                        || f.name.ends_with("_-check")
                        || f.name.ends_with("_:sequencecheck")
                    {
                        return Err(Error::invalid(
                            "Use answer fields from the current quiz page. Check/submit/sequence fields are controlled by the adapter.",
                        ));
                    }
                    if f.value.len() > 100_000 {
                        return Err(Error::invalid("Answer value is too long."));
                    }
                }
            }
            Self::CreateCalendarEvent {
                title,
                description,
                starts_at,
                duration_seconds,
            } => {
                nonempty(title, "Event title")?;
                if description.len() > 50_000 || *starts_at <= 0 || *duration_seconds > 366 * 86400
                {
                    return Err(Error::invalid(
                        "Invalid calendar event time, duration, or description.",
                    ));
                }
            }
            Self::RespondChoice { option_ids, .. }
                if (option_ids.is_empty() || option_ids.len() > 100) =>
            {
                return Err(Error::invalid("Provide 1–100 choice option IDs."));
            }
            _ => (),
        }
        Ok(())
    }
}

pub async fn prepare(moodle: &Moodle, home: &crate::config::Home, action: Action) -> Result<Value> {
    action.validate()?;
    let activity = if let Some((course, module, kind)) = action.target() {
        let a = moodle.activity(course, module).await?;
        if kind != "any" && a.kind != kind {
            return Err(Error::invalid(format!(
                "This action requires a {kind} activity."
            )));
        }
        Some(a)
    } else {
        None
    };
    if matches!(
        action,
        Action::StartQuiz { .. }
            | Action::SaveQuizAnswers { .. }
            | Action::FinishQuiz { .. }
            | Action::CreateDiscussion { .. }
            | Action::RespondChoice { .. }
    ) && !moodle.has_token()
    {
        return Err(Error::unsupported(
            "This action currently requires Moodle mobile service access. The session adapter can read the activity but cannot safely execute this action yet.",
        ));
    }
    if let Action::SaveAssignment { files, .. } = &action
        && !files.is_empty()
        && !moodle.has_token()
    {
        return Err(Error::unsupported(
            "Assignment file uploads currently require Moodle mobile service access.",
        ));
    }
    let state = current_state(moodle, &action, activity.as_ref()).await?;
    let mut snapshots = Vec::new();
    if let Action::SaveAssignment { files, .. } = &action {
        let mut total = 0;
        for path in files {
            let path = std::path::Path::new(path);
            if !path.is_absolute() {
                return Err(Error::invalid("Upload file paths must be absolute."));
            }
            let metadata = std::fs::metadata(path).map_err(|_| {
                Error::invalid("Could not read an upload file. Check its path and permissions.")
            })?;
            if !metadata.is_file() || metadata.len() > crate::network::MAX_DOWNLOAD_BYTES as u64 {
                return Err(Error::invalid(
                    "Uploads must be ordinary files up to 25 MB each.",
                ));
            }
            total += metadata.len();
            if total > 50_000_000 {
                return Err(Error::new(
                    "size_limit",
                    "Proposal exceeds the 50 MB total upload limit.",
                ));
            }
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| Error::invalid("Upload filename must be valid Unicode."))?
                .to_string();
            use std::io::Read;
            let mut bytes = Vec::new();
            std::fs::File::open(path)?
                .take(crate::network::MAX_DOWNLOAD_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > crate::network::MAX_DOWNLOAD_BYTES {
                return Err(Error::new(
                    "size_limit",
                    "An upload file grew beyond the size limit during preparation.",
                ));
            }
            snapshots.push(FileSnapshot {
                name,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                bytes: base64::engine::general_purpose::STANDARD.encode(bytes),
            });
        }
    }
    let mut display = serde_json::to_value(&action)?;
    if let Some(object) = display.as_object_mut() {
        object.remove("files");
    }
    let preview = json!({"account":moodle.profile.display_name,"site":moodle.profile.url,"action":display,"activity":activity.as_ref().map(|a|json!({"title":a.title,"module_id":a.module_id,"instance_id":a.instance_id,"source_url":a.source_url})),"effect":action.effect(),"current_state":state,"files":snapshots.iter().map(|f|json!({"name":f.name,"sha256":f.sha256,"bytes":f.bytes.len()*3/4})).collect::<Vec<_>>()});
    let payload = json!({"action":action,"files":snapshots});
    let preconditions = json!({"state_hash":hash(&state),"state":state});
    Store::open(home)?.prepare(&moodle.profile.id, &payload, &preview, &preconditions)
}

async fn current_state(
    moodle: &Moodle,
    action: &Action,
    activity: Option<&Activity>,
) -> Result<Value> {
    match action {
        Action::ReplyForum {
            course_id, post_id, ..
        } => {
            let target = moodle.forum_post_target(*post_id).await?;
            if target["course_id"].as_u64() != Some(*course_id)
                || target["forum_id"].as_u64() != activity.and_then(|a| a.instance_id)
            {
                return Err(Error::invalid(
                    "That parent post belongs to a different course or forum.",
                ));
            }
            Ok(target)
        }
        Action::SaveAssignment { .. } | Action::SubmitAssignment { .. } => {
            let a = activity.ok_or_else(|| Error::invalid("Missing assignment."))?;
            if moodle.has_token() {
                let id = instance(a)?;
                let raw = moodle
                    .rest("mod_assign_get_submission_status", json!({"assignid":id}))
                    .await?;
                let submission = &raw["lastattempt"]["submission"];
                let status = submission["status"].as_str().unwrap_or("new");
                Ok(
                    json!({"status":status,"attempt_number":submission["attemptnumber"].as_u64().unwrap_or(0),"modified":if status=="new"{Value::Null}else{submission["timemodified"].clone()},"submissions_enabled":raw["lastattempt"]["submissionsenabled"],"locked":raw["lastattempt"]["locked"],"statement":raw["submissionstatement"],"grading_status":raw["lastattempt"]["gradingstatus"].as_str().unwrap_or("notgraded")}),
                )
            } else {
                let response = moodle.activity_page(a).await?;
                let source = response.text()?;
                let html = scraper::Html::parse_document(&source);
                let status = html
                    .select(&content::selector(".submissionstatustable tr"))
                    .filter(|row| {
                        row.select(&content::selector(
                            ".timeremaining,.submissionoverdue,.earlysubmission,.overdue",
                        ))
                        .next()
                        .is_none()
                    })
                    .map(|e| content::html_text(&e.html(), None))
                    .collect::<Vec<_>>()
                    .join("\n");
                let status_code=html.select(&content::selector(".submissionstatussubmitted,.submissionstatusdraft,.submissionstatusreopened")).next()
                    .and_then(|e|e.value().attr("class")).and_then(|s|s.split_whitespace().find(|s|s.starts_with("submissionstatus"))).unwrap_or("new").to_string();
                if status.is_empty() {
                    return Err(Error::unsupported(
                        "Could not identify the assignment submission state from this theme.",
                    ));
                }
                Ok(
                    json!({"submission_status":status,"status":status_code.trim_start_matches("submissionstatus")}),
                )
            }
        }
        Action::StartQuiz { .. } => {
            let a = activity.ok_or_else(|| Error::invalid("Missing quiz."))?;
            let id = instance(a)?;
            let raw = moodle
                .rest(
                    "mod_quiz_get_user_attempts",
                    json!({"quizid":id,"status":"all"}),
                )
                .await?;
            if let Some(attempt) = raw["attempts"].as_array().and_then(|list| {
                list.iter()
                    .find(|a| a["state"].as_str() == Some("inprogress"))
            }) {
                return Err(Error::new(
                    "attempt_in_progress",
                    format!(
                        "Quiz already has active attempt {}. Read or resume it with moodle_read_quiz_attempt; do not start another attempt.",
                        attempt["id"]
                    ),
                ));
            }
            Ok(json!({"attempts":raw["attempts"]}))
        }
        Action::SaveQuizAnswers {
            attempt_id,
            page,
            fields,
            ..
        } => {
            let raw = moodle
                .rest(
                    "mod_quiz_get_attempt_data",
                    json!({"attemptid":attempt_id,"page":page}),
                )
                .await?;
            if raw["attempt"]["quiz"].as_u64() != activity.and_then(|a| a.instance_id) {
                return Err(Error::new(
                    "attempt_not_found",
                    "The selected attempt belongs to a different quiz.",
                ));
            }
            if raw["attempt"]["state"].as_str() != Some("inprogress") {
                return Err(Error::new(
                    "attempt_not_in_progress",
                    "Quiz attempt is no longer in progress.",
                ));
            }
            let html = raw["questions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|q| q["html"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let html = scraper::Html::parse_document(&html);
            let names: Vec<_> = html
                .select(&content::selector(
                    "input[name],textarea[name],select[name]",
                ))
                .filter_map(|e| e.value().attr("name").map(str::to_string))
                .collect();
            for field in fields {
                if !names.contains(&field.name) {
                    return Err(Error::invalid(format!(
                        "Answer field {} is not on this attempt page.",
                        field.name
                    )));
                }
            }
            let sequence: Vec<_> = html
                .select(&content::selector("input[name]"))
                .filter(|e| {
                    e.value()
                        .attr("name")
                        .is_some_and(|n| n.ends_with("_:sequencecheck"))
                })
                .map(|e| json!({"name":e.value().attr("name"),"value":e.value().attr("value")}))
                .collect();
            Ok(
                json!({"attempt_id":attempt_id,"state":raw["attempt"]["state"],"page":page,"sequence":sequence}),
            )
        }
        Action::FinishQuiz { attempt_id, .. } => {
            let raw = moodle
                .rest(
                    "mod_quiz_get_attempt_summary",
                    json!({"attemptid":attempt_id}),
                )
                .await?;
            let attempt = attempt_record(
                moodle,
                activity.ok_or_else(|| Error::invalid("Missing quiz."))?,
                *attempt_id,
            )
            .await?;
            if attempt["state"].as_str() != Some("inprogress") {
                return Err(Error::new(
                    "attempt_not_in_progress",
                    "Quiz attempt is no longer in progress.",
                ));
            }
            let questions:Vec<_>=raw["questions"].as_array().into_iter().flatten().map(|q|json!({"slot":q["slot"],"status":q["status"],"sequencecheck":q["sequencecheck"],"lastactiontime":q["lastactiontime"],"flagged":q["flagged"]})).collect();
            Ok(
                json!({"attempt":attempt,"questions":questions,"total_unanswered":raw["totalunanswered"]}),
            )
        }
        Action::SetCompletion { .. } => {
            Ok(json!({"completion":activity.and_then(|a|a.completion.as_ref())}))
        }
        _ => Ok(json!({})),
    }
}

fn instance(activity: &Activity) -> Result<u64> {
    activity.instance_id.ok_or_else(|| {
        Error::unsupported("The activity-instance ID is unavailable through this access route.")
    })
}

async fn attempt_record(moodle: &Moodle, activity: &Activity, id: u64) -> Result<Value> {
    let raw = moodle
        .rest(
            "mod_quiz_get_user_attempts",
            json!({"quizid":instance(activity)?,"status":"all"}),
        )
        .await?;
    raw["attempts"]
        .as_array()
        .and_then(|list| list.iter().find(|a| a["id"].as_u64() == Some(id)))
        .cloned()
        .ok_or_else(|| {
            Error::new(
                "attempt_not_found",
                "This attempt does not belong to your account and the selected quiz.",
            )
        })
}

pub async fn execute(moodle: &Moodle, home: &crate::config::Home, id: &str) -> Result<Value> {
    let proposal = Store::open(home)?.proposal(id)?;
    if proposal["profile"] != moodle.profile.id {
        return Err(Error::new(
            "account_mismatch",
            "This proposal belongs to another Moodle account.",
        ));
    }
    if proposal["status"] == "succeeded" {
        return Ok(proposal["receipt"].clone());
    }
    let proposal = Store::open(home)?.claim(&moodle.profile.id, id)?;
    let result = execute_claimed(moodle, &proposal).await;
    match result {
        Ok(mut receipt) => {
            receipt["proposal_id"] = json!(id);
            receipt["completed_at"] = json!(chrono::Utc::now().to_rfc3339());
            Store::open(home)?.finish(id, "succeeded", &receipt)?;
            Ok(receipt)
        }
        Err(error) => {
            // A request may have committed before its connection failed. Do not
            // make the proposal executable again even if the error is retryable.
            let status = if matches!(
                error.code.as_str(),
                "state_conflict" | "invalid_input" | "unsupported"
            ) {
                "failed"
            } else {
                "uncertain"
            };
            Store::open(home)?.finish(id,status,&json!({"error":error,"instruction":"Inspect Moodle's current state before preparing another action. This proposal will not execute again."}))?;
            Err(error)
        }
    }
}

async fn execute_claimed(moodle: &Moodle, proposal: &Value) -> Result<Value> {
    let action: Action = serde_json::from_value(proposal["payload"]["action"].clone())?;
    let activity = if let Some((course, module, _)) = action.target() {
        Some(moodle.activity(course, module).await?)
    } else {
        None
    };
    let state = current_state(moodle, &action, activity.as_ref()).await?;
    if hash(&state)
        != proposal["preconditions"]["state_hash"]
            .as_str()
            .unwrap_or_default()
    {
        return Err(Error::new(
            "state_conflict",
            "Moodle changed after preparation. Review the current state and prepare a new action.",
        ));
    }
    let mut raw=match &action{
        Action::SetCompletion{module_id,completed,..}=>moodle.read_call("core_completion_update_activity_completion_status_manually",json!({"cmid":module_id,"completed":completed})).await?,
        Action::SendMessage{recipient_id,message}=>moodle.read_call("core_message_send_instant_messages",json!({"messages":[{"touserid":recipient_id,"text":message,"textformat":0}]})).await?,
        Action::ReplyForum{post_id,subject,message,..}=>moodle.read_call("mod_forum_add_discussion_post",json!({"postid":post_id,"subject":subject,"message":text_html(message)})).await?,
        Action::CreateDiscussion{subject,message,..}=>moodle.rest("mod_forum_add_discussion",json!({"forumid":instance(activity.as_ref().unwrap())?,"subject":subject,"message":text_html(message)})).await?,
        Action::SaveAssignment{text,..}=>{
            let a=activity.as_ref().unwrap();
            if moodle.has_token(){
                let files:Vec<FileSnapshot>=serde_json::from_value(proposal["payload"]["files"].clone())?;
                let mut item_id=None;
                for file in files{let bytes=base64::engine::general_purpose::STANDARD.decode(&file.bytes).map_err(|_|Error::local("Corrupted upload snapshot."))?;if format!("{:x}",Sha256::digest(&bytes))!=file.sha256{return Err(Error::local("Upload snapshot hash did not match."));}item_id=Some(moodle.upload_draft(file.name,bytes,item_id).await?);}
                let mut plugin=json!({});
                if let Some(text)=text{plugin["onlinetext_editor"]=json!({"text":text_html(text),"format":1,"itemid":0});}
                if let Some(id)=item_id{plugin["files_filemanager"]=json!(id);}
                moodle.rest("mod_assign_save_submission",json!({"assignmentid":instance(a)?,"plugindata":plugin})).await?
            }else{save_assignment_form(moodle,a,text.as_deref().unwrap_or_default()).await?}
        }
        Action::SubmitAssignment{accept_submission_statement,..}=>{
            let a=activity.as_ref().unwrap();
            if moodle.has_token(){moodle.rest("mod_assign_submit_for_grading",json!({"assignmentid":instance(a)?,"acceptsubmissionstatement":accept_submission_statement})).await?}else{submit_assignment_form(moodle,a,*accept_submission_statement).await?}
        }
        Action::StartQuiz{..}=>moodle.rest("mod_quiz_start_attempt",json!({"quizid":instance(activity.as_ref().unwrap())?,"forcenew":false})).await?,
        Action::SaveQuizAnswers{attempt_id,fields,..}=>{
            let mut data=serde_json::to_value(fields)?.as_array().cloned().unwrap_or_default();
            data.extend(state["sequence"].as_array().cloned().unwrap_or_default());
            moodle.rest("mod_quiz_save_attempt",json!({"attemptid":attempt_id,"data":data})).await?
        }
        Action::FinishQuiz{attempt_id,..}=>moodle.rest("mod_quiz_process_attempt",json!({"attemptid":attempt_id,"finishattempt":true,"timeup":false,"data":[]})).await?,
        Action::CreateCalendarEvent{title,description,starts_at,duration_seconds}=>moodle.read_call("core_calendar_create_calendar_events",json!({"events":[{"name":title,"description":text_html(description),"format":1,"eventtype":"user","timestart":starts_at,"timeduration":duration_seconds}]})).await?,
        Action::RespondChoice{option_ids,..}=>moodle.rest("mod_choice_submit_choice_response",json!({"choiceid":instance(activity.as_ref().unwrap())?,"responses":option_ids})).await?,
    };
    // Moodle may return warnings or per-item failures with HTTP 200.
    ensure_success(&raw)?;
    crate::moodle::sanitise(&mut raw);
    let mut verification = verify(moodle, &action, activity.as_ref(), &raw).await?;
    crate::moodle::sanitise(&mut verification);
    Ok(
        json!({"status":"succeeded","action":action.effect(),"result":raw,"verification":verification,"source_url":activity.as_ref().map(|a|&a.source_url)}),
    )
}

fn ensure_success(value: &Value) -> Result<()> {
    if value["status"].as_bool() == Some(false)
        || value["errorcode"].as_str().is_some_and(|s| !s.is_empty())
        || value["warningcode"].as_str().is_some_and(|s| !s.is_empty())
        || value["exception"].is_string()
    {
        return Err(Error::new(
            "action_rejected",
            "Moodle returned an unsuccessful action result.",
        ));
    }
    if let Some(warnings) = value.get("warnings").and_then(Value::as_array)
        && !warnings.is_empty()
    {
        return Err(Error::new(
            "action_warning",
            "Moodle returned warnings; inspect the action in Moodle before continuing.",
        ));
    }
    if let Some(list) = value.as_array() {
        for v in list {
            ensure_success(v)?;
            if v["msgid"].as_i64() == Some(-1) {
                return Err(Error::new(
                    "action_rejected",
                    "Moodle did not send this message.",
                ));
            }
        }
    }
    Ok(())
}

async fn verify(
    moodle: &Moodle,
    action: &Action,
    activity: Option<&Activity>,
    result: &Value,
) -> Result<Value> {
    match action {
        Action::SaveAssignment { .. } | Action::SubmitAssignment { .. } => {
            let state = current_state(moodle, action, activity).await?;
            if matches!(action, Action::SubmitAssignment { .. })
                && state["status"].as_str() != Some("submitted")
            {
                return Err(Error::new(
                    "verification_failed",
                    "Moodle did not report a submitted assignment after the action.",
                ));
            }
            Ok(json!({"method":"re-read submission state","state":state}))
        }
        Action::StartQuiz { .. } => {
            let id = result["attempt"]["id"].as_u64().ok_or_else(|| {
                Error::new("verification_failed", "No attempt identifier was returned.")
            })?;
            let attempt = attempt_record(moodle, activity.unwrap(), id).await?;
            Ok(json!({"method":"re-read attempt","attempt":attempt}))
        }
        Action::SaveQuizAnswers {
            attempt_id, page, ..
        } => {
            let raw = moodle
                .rest(
                    "mod_quiz_get_attempt_data",
                    json!({"attemptid":attempt_id,"page":page}),
                )
                .await?;
            Ok(
                json!({"method":"re-read attempt page","attempt":raw["attempt"],"questions":raw["questions"]}),
            )
        }
        Action::FinishQuiz { attempt_id, .. } => {
            let attempt = attempt_record(moodle, activity.unwrap(), *attempt_id).await?;
            if attempt["state"].as_str() != Some("finished") {
                return Err(Error::new(
                    "verification_failed",
                    "Quiz attempt did not report a finished state.",
                ));
            }
            Ok(json!({"method":"re-read finished attempt","attempt":attempt}))
        }
        Action::SetCompletion {
            course_id,
            module_id,
            completed,
        } => {
            let a = moodle.activity(*course_id, *module_id).await?;
            let state = a.completion.as_ref().and_then(|c| c["state"].as_u64());
            if state.is_none_or(|s| (*completed && s == 0) || (!*completed && s != 0)) {
                return Err(Error::new(
                    "verification_failed",
                    "Moodle completion state could not be confirmed after this action.",
                ));
            }
            Ok(json!({"method":"re-read course completion","completion":a.completion}))
        }
        Action::ReplyForum { post_id, .. } => {
            let id = result["postid"].as_u64().ok_or_else(|| {
                Error::new("verification_failed", "Moodle returned no post identifier.")
            })?;
            let raw = moodle
                .read_call("mod_forum_get_discussion_post", json!({"postid":id}))
                .await?;
            Ok(json!({"method":"re-read created post","parent_post_id":post_id,"post":raw}))
        }
        Action::CreateDiscussion { .. } => {
            let id = result["discussionid"].as_u64().ok_or_else(|| {
                Error::new(
                    "verification_failed",
                    "Moodle returned no discussion identifier.",
                )
            })?;
            let raw = moodle.discussion(id).await?;
            Ok(json!({"method":"re-read created discussion","discussion":raw}))
        }
        Action::SendMessage { .. } => Ok(
            json!({"method":"Moodle returned per-message delivery identifiers","messages":result}),
        ),
        Action::CreateCalendarEvent { .. } => {
            let ids: Vec<_> = result["events"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| e["id"].as_u64())
                .collect();
            if ids.is_empty() {
                return Err(Error::new(
                    "verification_failed",
                    "Moodle returned no calendar event identifier.",
                ));
            }
            let raw = moodle
                .read_call(
                    "core_calendar_get_calendar_events",
                    json!({"events":{"eventids":ids}}),
                )
                .await?;
            Ok(json!({"method":"re-read created calendar events","events":raw}))
        }
        Action::RespondChoice { .. } => {
            let a = activity.unwrap();
            let raw = moodle
                .rest(
                    "mod_choice_get_choice_results",
                    json!({"choiceid":instance(a)?}),
                )
                .await?;
            Ok(json!({"method":"re-read choice results","results":raw}))
        }
    }
}

async fn save_assignment_form(moodle: &Moodle, activity: &Activity, text: &str) -> Result<Value> {
    let url = moodle.http.url(&format!(
        "mod/assign/view.php?id={}&action=editsubmission",
        activity.module_id
    ))?;
    let response = moodle.http.get(url).await?;
    let (target, mut fields) = {
        let source = response.text()?;
        let html = scraper::Html::parse_document(&source);
        let form = html
            .select(&content::selector("form.mform"))
            .find(|f| {
                f.select(&content::selector(
                    "textarea[name='onlinetext_editor[text]']",
                ))
                .next()
                .is_some()
            })
            .ok_or_else(|| {
                Error::unsupported("No supported online-text submission form is available.")
            })?;
        let fields = form_fields(form);
        let target = form
            .value()
            .attr("action")
            .and_then(|a| response.url.join(a).ok())
            .unwrap_or(response.url);
        (target, fields)
    };
    fields.retain(|(name, _)| name != "cancel" && name != "onlinetext_editor[text]");
    fields.push(("onlinetext_editor[text]".into(), text_html(text)));
    fields.push(("onlinetext_editor[format]".into(), "1".into()));
    fields.push(("submitbutton".into(), "Save changes".into()));
    let response = moodle.http.post_form(target, &fields).await?;
    if response.text()?.contains("class=\"error\"") {
        return Err(Error::new(
            "action_rejected",
            "Moodle reported a submission-form error.",
        ));
    }
    Ok(json!({"saved":true,"transport":"session_form"}))
}
async fn submit_assignment_form(
    moodle: &Moodle,
    activity: &Activity,
    accept: bool,
) -> Result<Value> {
    let url = moodle.http.url(&format!(
        "mod/assign/view.php?id={}&action=submit",
        activity.module_id
    ))?;
    let response = moodle.http.get(url).await?;
    let (target, mut fields) = {
        let html = scraper::Html::parse_document(&response.text()?);
        let form=html.select(&content::selector("form")).find(|f|f.select(&content::selector("input[name=action][value=confirmsubmit],input[name='_qf__mod_assign_confirm_submission_form']")).next().is_some()).ok_or_else(||Error::unsupported("No supported final-submission confirmation form is available."))?;
        if form
            .select(&content::selector("input[name=submissionstatement]"))
            .next()
            .is_some()
            && !accept
        {
            return Err(Error::invalid(
                "The assignment requires acceptance of its submission statement.",
            ));
        }
        let fields = form_fields(form);
        let target = form
            .value()
            .attr("action")
            .and_then(|a| response.url.join(a).ok())
            .unwrap_or(response.url);
        (target, fields)
    };
    fields.push((
        "submissionstatement".into(),
        if accept { "1" } else { "0" }.into(),
    ));
    fields.push(("submitbutton".into(), "Continue".into()));
    moodle.http.post_form(target, &fields).await?;
    Ok(json!({"submitted":true,"transport":"session_form"}))
}
fn form_fields(form: scraper::ElementRef<'_>) -> Vec<(String, String)> {
    form.select(&content::selector("input[type=hidden][name]"))
        .filter_map(|e| {
            Some((
                e.value().attr("name")?.to_string(),
                e.value().attr("value").unwrap_or_default().to_string(),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn approval_input_cannot_hide_extra_fields() {
        assert!(
            serde_json::from_value::<Action>(
                json!({"kind":"send_message","recipient_id":4,"message":"hello","confirmed":true})
            )
            .is_err()
        );
    }
    #[test]
    fn quiz_control_fields_are_not_answers() {
        let a = Action::SaveQuizAnswers {
            course_id: 1,
            module_id: 2,
            attempt_id: 3,
            page: 0,
            fields: vec![AnswerField {
                name: "q1:2_-submit".into(),
                value: "1".into(),
            }],
        };
        assert!(a.validate().is_err());
    }
    #[test]
    fn detects_http_200_per_item_failures() {
        assert!(ensure_success(&json!([{"msgid":-1,"errormessage":"no"}])).is_err());
        assert!(ensure_success(&json!({"warnings":[{"warningcode":"no"}]})).is_err());
        assert!(ensure_success(&json!([{"warningcode":"submissionnotready"}])).is_err());
    }
}
