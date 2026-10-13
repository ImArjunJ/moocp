//! Real-Moodle tests, opt-in because they change a disposable student account.
//! MOOCP_LAB_FIXTURE describes a disposable local instance; never use a real site.
use anyhow::Context;
use moocp::{
    actions::{self, Action},
    config::{Cookie, CredentialStorage, Credentials, Home, Profile, site_url},
    moodle::Moodle,
    store::Store,
};
use serde_json::{Value, json};

async fn lab() -> anyhow::Result<(tempfile::TempDir, Home, Moodle, Value)> {
    let fixture = std::env::var("MOOCP_LAB_FIXTURE")?;
    let fixture: Value = serde_json::from_slice(&std::fs::read(fixture)?)?;
    let directory = tempfile::tempdir()?;
    let home = Home {
        path: directory.path().to_owned(),
    };
    let url = site_url(fixture["site"].as_str().unwrap(), true)?;
    let profile = Profile {
        id: uuid::Uuid::new_v4().to_string(),
        name: "lab".into(),
        url: url.clone(),
        user_id: fixture["user_id"].as_u64(),
        display_name: Some("Moocp Student".into()),
        storage: CredentialStorage::File,
        allow_http: true,
        authenticated_at: chrono::Utc::now().to_rfc3339(),
    };
    // Login to the disposable site using the same cookie/session semantics as a browser.
    let jar = std::sync::Arc::new(reqwest::cookie::Jar::default());
    let client = reqwest::Client::builder()
        .cookie_provider(jar.clone())
        .build()?;
    let login = url.join("login/index.php")?;
    let page = client.get(login.clone()).send().await?.text().await?;
    let html = scraper::Html::parse_document(&page);
    let logintoken = html
        .select(&moocp::content::selector("input[name=logintoken]"))
        .next()
        .and_then(|e| e.value().attr("value"))
        .unwrap_or_default()
        .to_string();
    drop(html);
    let response = client
        .post(login)
        .form(&[
            ("username", fixture["username"].as_str().unwrap()),
            ("password", fixture["password"].as_str().unwrap()),
            ("logintoken", logintoken.as_str()),
        ])
        .send()
        .await?;
    let page = response.text().await?;
    use reqwest::cookie::CookieStore;
    let header = jar
        .cookies(&url)
        .ok_or_else(|| anyhow::anyhow!("No lab login cookies"))?;
    let cookies = header
        .to_str()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .map(|(name, value)| Cookie {
            name: name.into(),
            value: value.into(),
            domain: url.host_str().unwrap().into(),
            path: "/".into(),
            secure: false,
            expires: None,
        })
        .collect();
    let credentials = Credentials {
        cookies,
        token: fixture["token"].as_str().map(str::to_string),
        sesskey: moocp::moodle::extract_sesskey(&page),
    };
    home.save_profile(&profile)?;
    home.save_credentials(&profile, &credentials)?;
    Ok((directory, home, Moodle::new(profile, credentials)?, fixture))
}

#[tokio::test]
#[ignore = "requires disposable Moodle lab and MOOCP_LAB_FIXTURE"]
async fn student_reads_rest_session_and_personal_submission_details() -> anyhow::Result<()> {
    let (_directory, home, moodle, fixture) = lab().await?;
    let course = fixture["course_id"].as_u64().unwrap();
    assert!(moodle.validate().await?["connected"].as_bool().unwrap());
    assert!(moodle.courses().await?.iter().any(|c| c.id == course));
    let outline = moodle.outline(course).await?;
    assert!(outline.activities.iter().any(|a| a.kind == "page"));
    let page = moodle
        .activity(course, fixture["page_id"].as_u64().unwrap())
        .await?;
    let response = moodle.activity_page(&page).await?;
    assert!(
        moocp::content::html_text(&response.text()?, Some(&response.url))
            .contains("Neural networks")
    );
    let assignment = moodle
        .activity(course, fixture["assignment_id"].as_u64().unwrap())
        .await?;
    let details = moodle.activity_details(assignment).await?;
    assert!(
        details["details"]["lastattempt"]["extensionduedate"]
            .as_i64()
            .is_some_and(|n| n > chrono::Utc::now().timestamp())
    );
    moodle
        .deadlines(
            chrono::Utc::now().timestamp(),
            chrono::Utc::now().timestamp() + 30 * 86400,
            None,
            20,
            None,
        )
        .await?;
    moodle.grades(course).await?;
    moodle
        .forum(course, fixture["forum_id"].as_u64().unwrap(), 0)
        .await?;
    moodle.messages(None, 0, 20).await?;
    let mut credentials = home.credentials(&moodle.profile)?;
    credentials.token = None;
    let session = Moodle::new(moodle.profile.clone(), credentials)?;
    assert!(
        session.http.mobile_token().await?.is_some(),
        "Browser session should acquire a mobile service token through the launch flow"
    );
    let outline = session.outline(course).await?;
    assert_eq!(outline.transport, "session_ajax");
    assert!(outline.activities.iter().any(|a| a.kind == "assign"));
    assert!(session.courses().await?.iter().any(|c| c.id == course));
    session.grades(course).await?;
    session
        .forum(course, fixture["forum_id"].as_u64().unwrap(), 0)
        .await?;
    Ok(())
}

async fn execute_approved(moodle: &Moodle, home: &Home, action: Action) -> anyhow::Result<Value> {
    let effect = action.effect();
    let proposal = actions::prepare(moodle, home, action)
        .await
        .with_context(|| format!("Prepare: {effect}"))?;
    let id = proposal["proposal_id"].as_str().unwrap();
    assert!(actions::execute(moodle, home, id).await.is_err());
    Store::open(home)?.approve(id)?;
    let result = actions::execute(moodle, home, id).await.with_context(|| {
        format!(
            "Execute: {effect}; prepared state: {}",
            proposal["preview"]["current_state"]
        )
    })?;
    assert_eq!(result["status"], "succeeded");
    assert_eq!(actions::execute(moodle, home, id).await?, result);
    Ok(result)
}

#[tokio::test]
#[ignore = "writes to disposable Moodle lab; requires MOOCP_LAB_FIXTURE"]
async fn prepares_reviews_and_verifies_student_actions() -> anyhow::Result<()> {
    let (_directory, home, moodle, fixture) = lab().await?;
    let course = fixture["course_id"].as_u64().unwrap();
    execute_approved(
        &moodle,
        &home,
        Action::SetCompletion {
            course_id: course,
            module_id: fixture["page_id"].as_u64().unwrap(),
            completed: true,
        },
    )
    .await?;
    execute_approved(
        &moodle,
        &home,
        Action::SendMessage {
            recipient_id: fixture["peer_id"].as_u64().unwrap(),
            message: format!("Moocp integration check {}", uuid::Uuid::new_v4()),
        },
    )
    .await?;
    let discussion = execute_approved(
        &moodle,
        &home,
        Action::CreateDiscussion {
            course_id: course,
            module_id: fixture["forum_id"].as_u64().unwrap(),
            subject: format!("Moocp lab {}", uuid::Uuid::new_v4()),
            message: "A disposable student discussion.".into(),
        },
    )
    .await?;
    let discussion_id = discussion["result"]["discussionid"].as_u64().unwrap();
    let posts = moodle
        .rest(
            "mod_forum_get_discussion_posts",
            json!({"discussionid":discussion_id}),
        )
        .await?;
    let parent = posts["posts"][0]["id"].as_u64().unwrap();
    execute_approved(
        &moodle,
        &home,
        Action::ReplyForum {
            course_id: course,
            module_id: fixture["forum_id"].as_u64().unwrap(),
            post_id: parent,
            subject: "Re: lab".into(),
            message: "A disposable reply.".into(),
        },
    )
    .await?;
    execute_approved(
        &moodle,
        &home,
        Action::CreateCalendarEvent {
            title: format!("Moocp lab {}", uuid::Uuid::new_v4()),
            description: "Personal integration-test event".into(),
            starts_at: chrono::Utc::now().timestamp() + 86400,
            duration_seconds: 3600,
        },
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "saves coursework and starts/submits a disposable quiz; requires MOOCP_LAB_FIXTURE"]
async fn coursework_upload_and_quiz_lifecycle() -> anyhow::Result<()> {
    let (_directory, home, moodle, fixture) = lab().await?;
    let course = fixture["course_id"].as_u64().unwrap();
    let file = home.path.join("coursework.txt");
    std::fs::write(&file, "Moocp disposable coursework\n")?;
    execute_approved(
        &moodle,
        &home,
        Action::SaveAssignment {
            course_id: course,
            module_id: fixture["assignment_id"].as_u64().unwrap(),
            text: Some("Moocp disposable online text.".into()),
            files: vec![file.to_string_lossy().into_owned()],
        },
    )
    .await?;
    let active = moodle
        .rest(
            "mod_quiz_get_user_attempts",
            json!({"quizid":fixture["quiz_instance"],"status":"unfinished"}),
        )
        .await?;
    let attempt = if let Some(attempt) = active["attempts"]
        .as_array()
        .and_then(|a| a.iter().find(|a| a["state"] == "inprogress"))
    {
        attempt["id"].as_u64().unwrap()
    } else {
        let start = execute_approved(
            &moodle,
            &home,
            Action::StartQuiz {
                course_id: course,
                module_id: fixture["quiz_id"].as_u64().unwrap(),
            },
        )
        .await?;
        start["result"]["attempt"]["id"].as_u64().unwrap()
    };
    let raw = moodle
        .rest(
            "mod_quiz_get_attempt_data",
            json!({"attemptid":attempt,"page":0}),
        )
        .await?;
    let source = raw["questions"][0]["html"].as_str().unwrap();
    let html = scraper::Html::parse_document(source);
    let name = html
        .select(&moocp::content::selector("input[type=text][name]"))
        .next()
        .and_then(|e| e.value().attr("name"))
        .unwrap()
        .to_string();
    drop(html);
    execute_approved(
        &moodle,
        &home,
        Action::SaveQuizAnswers {
            course_id: course,
            module_id: fixture["quiz_id"].as_u64().unwrap(),
            attempt_id: attempt,
            page: 0,
            fields: vec![moocp::actions::AnswerField {
                name,
                value: "gradient".into(),
            }],
        },
    )
    .await?;
    execute_approved(
        &moodle,
        &home,
        Action::FinishQuiz {
            course_id: course,
            module_id: fixture["quiz_id"].as_u64().unwrap(),
            attempt_id: attempt,
        },
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "submits disposable coursework; requires a freshly seeded MOOCP_LAB_FIXTURE"]
async fn final_submission_choice_and_session_form() -> anyhow::Result<()> {
    let (_directory, home, moodle, fixture) = lab().await?;
    let course = fixture["course_id"].as_u64().unwrap();
    let module = fixture["final_assignment_id"].as_u64().unwrap();
    execute_approved(
        &moodle,
        &home,
        Action::SaveAssignment {
            course_id: course,
            module_id: module,
            text: Some("Final submission test.".into()),
            files: vec![],
        },
    )
    .await?;
    let receipt = execute_approved(
        &moodle,
        &home,
        Action::SubmitAssignment {
            course_id: course,
            module_id: module,
            accept_submission_statement: true,
        },
    )
    .await?;
    assert_eq!(receipt["verification"]["state"]["status"], "submitted");
    let options = moodle
        .rest(
            "mod_choice_get_choice_options",
            json!({"choiceid":fixture["choice_instance"]}),
        )
        .await?;
    let option = options["options"][0]["id"].as_u64().unwrap();
    execute_approved(
        &moodle,
        &home,
        Action::RespondChoice {
            course_id: course,
            module_id: fixture["choice_id"].as_u64().unwrap(),
            option_ids: vec![option],
        },
    )
    .await?;
    let mut credentials = home.credentials(&moodle.profile)?;
    credentials.token = None;
    let session = Moodle::new(moodle.profile.clone(), credentials)?;
    let module = fixture["session_assignment_id"].as_u64().unwrap();
    execute_approved(
        &session,
        &home,
        Action::SaveAssignment {
            course_id: course,
            module_id: module,
            text: Some("Session-form draft test.".into()),
            files: vec![],
        },
    )
    .await?;
    execute_approved(
        &session,
        &home,
        Action::SubmitAssignment {
            course_id: course,
            module_id: module,
            accept_submission_statement: true,
        },
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable Moodle lab and installed Chromium; automates only lab credentials"]
async fn native_browser_login_captures_session_and_upgrades_mobile_access() -> anyhow::Result<()> {
    use chromiumoxide::Browser;
    use futures::StreamExt;
    let (_directory, home, moodle, fixture) = lab().await?;
    let browser_home = home.path.clone();
    let username = fixture["username"].as_str().unwrap().to_string();
    let password = fixture["password"].as_str().unwrap().to_string();
    let helper = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if tokio::time::Instant::now() > deadline {
                return;
            }
            let entries = std::fs::read_dir(&browser_home).unwrap();
            for entry in entries.flatten() {
                if !entry.file_name().to_string_lossy().starts_with("login-") {
                    continue;
                }
                let Ok(port) = std::fs::read_to_string(entry.path().join("DevToolsActivePort"))
                else {
                    continue;
                };
                let mut lines = port.lines();
                let Some(number) = lines.next() else {
                    continue;
                };
                let Some(path) = lines.next() else {
                    continue;
                };
                let Ok((browser, mut handler)) =
                    Browser::connect(format!("ws://127.0.0.1:{number}{path}")).await
                else {
                    continue;
                };
                let driver = tokio::spawn(async move {
                    while let Some(event) = handler.next().await {
                        if event.is_err() {
                            break;
                        }
                    }
                });
                loop {
                    if tokio::time::Instant::now() > deadline {
                        driver.abort();
                        return;
                    }
                    for page in browser.pages().await.unwrap_or_default() {
                        let expression = format!(
                            "(() => {{ const u=document.querySelector('#username'),p=document.querySelector('#password'); if(!u||!p)return false;u.value={};p.value={};u.form.submit();return true;}})()",
                            serde_json::to_string(&username).unwrap(),
                            serde_json::to_string(&password).unwrap()
                        );
                        if page
                            .evaluate(expression)
                            .await
                            .ok()
                            .and_then(|r| r.into_value::<bool>().ok())
                            == Some(true)
                        {
                            driver.abort();
                            return;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });
    let mut profile = moodle.profile.clone();
    let outcome = moocp::auth::login(
        &home,
        &mut profile,
        moocp::auth::LoginOptions {
            headless: true,
            timeout_seconds: 90,
            ..Default::default()
        },
    )
    .await;
    helper.abort();
    let mut credentials = outcome?;
    assert!(
        credentials
            .cookies
            .iter()
            .any(|c| c.name.starts_with("MoodleSession"))
    );
    assert!(credentials.sesskey.is_some());
    assert!(moocp::auth::enrich_mobile_token(&mut profile, &mut credentials).await?);
    assert_eq!(profile.user_id, fixture["user_id"].as_u64());
    assert_eq!(
        Moodle::new(profile, credentials)?.validate().await?["connected"],
        true
    );
    Ok(())
}

#[tokio::test]
#[ignore = "uses Chromium to change a disposable Moodle choice; requires MOOCP_LAB_FIXTURE"]
async fn compatibility_browser_interacts_with_real_student_activity() -> anyhow::Result<()> {
    let (_directory, home, moodle, fixture) = lab().await?;
    let activity = moodle
        .activity(
            fixture["course_id"].as_u64().unwrap(),
            fixture["choice_id"].as_u64().unwrap(),
        )
        .await?;
    let mut browser = moocp::browser::Controller::launch(
        &home,
        moodle.profile.clone(),
        home.credentials(&moodle.profile)?,
        url::Url::parse(&activity.source_url)?,
        true,
    )
    .await?;
    let outcome = async {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let snapshot = browser.snapshot().await?;
        let nodes = snapshot["nodes"].as_array().unwrap();
        let radio = nodes
            .iter()
            .rfind(|n| n["type"] == "radio")
            .context("No tutorial choice radio input")?;
        let option = radio["value"]
            .as_str()
            .context("No option ID")?
            .parse::<u64>()?;
        let save = nodes
            .iter()
            .find(|n| n["type"] == "submit" && n["label"].as_str().unwrap_or("").contains("Save"))
            .context("No choice save button")?;
        let proposal = browser.prepare(
            &home,
            moocp::browser::Prepare {
                session_id: browser.session_id.clone(),
                snapshot_id: snapshot["snapshot_id"].as_str().unwrap().into(),
                steps: vec![
                    moocp::browser::Step::Click {
                        reference: radio["ref"].as_str().unwrap().into(),
                    },
                    moocp::browser::Step::Click {
                        reference: save["ref"].as_str().unwrap().into(),
                    },
                ],
                purpose: "Save the student's tutorial preference in the native Moodle UI".into(),
            },
        )?;
        let id = proposal["proposal_id"].as_str().unwrap();
        Store::open(&home)?.approve(id)?;
        browser.execute(&home, id).await?;
        let options = moodle
            .rest(
                "mod_choice_get_choice_options",
                json!({"choiceid":fixture["choice_instance"]}),
            )
            .await?;
        assert!(
            options["options"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o["id"] == option && o["checked"] == true)
        );
        anyhow::Ok(())
    }
    .await;
    browser.close().await;
    outcome
}
