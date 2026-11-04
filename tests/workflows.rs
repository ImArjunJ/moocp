#[path = "support/campus.rs"]
mod campus;
use rmcp::{ServiceExt, model::CallToolRequestParams};
use serde_json::{Value, json};

#[tokio::test]
async fn student_discovers_and_reads_materials_without_guessing_activity_ids() -> anyhow::Result<()>
{
    for session in [false, true] {
        let fixture = campus::Campus::new(session).await?;
        let (a, b) = tokio::io::duplex(64_000);
        let server = moocp::server::Server::new(fixture.home.clone(), None);
        let task =
            tokio::spawn(async move { server.serve(a).await.unwrap().waiting().await.unwrap() });
        let client = ().serve(b).await?;
        let mut cursor = Value::Null;
        let mut materials = Vec::new();
        for _ in 0..30 {
            let response = client
                .call_tool(
                    CallToolRequestParams::new("moodle_list_materials").with_arguments(
                        json!({"course_id":42,"limit":2,"cursor":cursor})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await?;
            assert_ne!(
                response.is_error,
                Some(true),
                "{:?}",
                response.structured_content
            );
            let page = response.structured_content.unwrap();
            materials.extend(page["items"].as_array().unwrap().clone());
            cursor = page["next_cursor"].clone();
            if cursor.is_null() {
                break;
            }
        }
        assert!(cursor.is_null());
        for kind in ["section", "activity", "file", "link"] {
            assert!(
                materials.iter().any(|m| m["kind"] == kind),
                "Missing {kind}"
            );
        }
        let slides = materials
            .iter()
            .find(|m| m["kind"] == "link" && m["title"] == "Neural network training")
            .unwrap();
        let read = client
            .call_tool(
                CallToolRequestParams::new("moodle_read").with_arguments(
                    json!({"course_id":42,"reference":slides["reference"]})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await?;
        assert_ne!(read.is_error, Some(true), "{:?}", read.structured_content);
        assert!(
            read.structured_content.unwrap()["content"]["text"]
                .as_str()
                .unwrap()
                .contains("negative gradient")
        );
        assert!(
            materials
                .iter()
                .any(|m| m["title"] == "Restricted Week 6 slides"
                    && m["availability"] == "restricted")
        );
        assert_eq!(fixture.traffic.count("activity:12"), 0);
        client.cancel().await?;
        task.await?;
    }
    Ok(())
}

#[tokio::test]
async fn personal_coursework_preserves_extensions_drafts_groups_and_unknowns() -> anyhow::Result<()>
{
    let fixture = campus::Campus::new(false).await?;
    let (a, b) = tokio::io::duplex(64_000);
    let server = moocp::server::Server::new(fixture.home.clone(), None);
    let task = tokio::spawn(async move { server.serve(a).await.unwrap().waiting().await.unwrap() });
    let client = ().serve(b).await?;
    let response = client
        .call_tool(
            CallToolRequestParams::new("moodle_get_coursework")
                .with_arguments(json!({"course_id":42}).as_object().unwrap().clone()),
        )
        .await?;
    assert_ne!(
        response.is_error,
        Some(true),
        "{:?}",
        response.structured_content
    );
    let data = response.structured_content.unwrap();
    let items = data["items"].as_array().unwrap();
    let draft = items.iter().find(|i| i["module_id"] == 20).unwrap();
    assert_eq!(draft["submission_status"], "draft");
    assert_eq!(draft["requires_final_submission"], true);
    assert_eq!(draft["due"]["timestamp"], campus::EXTENSION);
    assert_eq!(draft["cutoff"]["timestamp"], campus::CUTOFF);
    assert_eq!(draft["due"]["basis"], "personal_extension");
    let unknown = items.iter().find(|i| i["module_id"] == 21).unwrap();
    assert_eq!(unknown["submission_status"], "unknown");
    assert!(unknown["requires_final_submission"].is_null());
    let group = items.iter().find(|i| i["module_id"] == 22).unwrap();
    assert_eq!(group["submission_status"], "submitted");
    assert_eq!(group["group_submission"], true);
    assert_eq!(group["feedback_available"], true);
    let quiz = items.iter().find(|i| i["module_id"] == 30).unwrap();
    assert_eq!(quiz["attempts_remaining"], 1);
    assert_eq!(quiz["time_limit_seconds"], 3600);
    assert_eq!(fixture.traffic.count("mod_quiz_start_attempt"), 0);
    assert_eq!(fixture.traffic.count("mod_assign_submit_for_grading"), 0);
    client.cancel().await?;
    task.await?;
    Ok(())
}

#[tokio::test]
async fn coursework_session_dates_and_catalogue_cursor_boundaries_are_explicit()
-> anyhow::Result<()> {
    use moocp::{coursework, materials, moodle::Moodle, store::Store};
    let fixture = campus::Campus::new(true).await?;
    let moodle = Moodle::new(
        fixture.profile.clone(),
        fixture.home.credentials(&fixture.profile)?,
    )?;
    let args: coursework::Args =
        serde_json::from_value(json!({"course_id":42,"kind":"assignment","limit":1}))?;
    let result = moodle.coursework(&args).await?;
    assert_eq!(result["items"][0]["due"]["timestamp"], campus::EXTENSION);
    assert_eq!(result["items"][0]["submission_status"], "draft");
    assert_eq!(result["next_offset"], 1);
    assert_eq!(fixture.traffic.count("mod_assign_get_submission_status"), 0);
    let args: materials::Args =
        serde_json::from_value(json!({"course_id":42,"limit":1,"include_files":false}))?;
    let first = moodle.materials(Store::open(&fixture.home)?, &args).await?;
    let changed: materials::Args = serde_json::from_value(
        json!({"course_id":42,"limit":1,"include_files":false,"query":"slides","cursor":first["next_cursor"]}),
    )?;
    assert_eq!(
        moodle
            .materials(Store::open(&fixture.home)?, &changed)
            .await
            .err()
            .unwrap()
            .code,
        "catalogue_changed"
    );
    let invalid: materials::Args =
        serde_json::from_value(json!({"course_id":42,"cursor":"invalid"}))?;
    assert_eq!(
        moodle
            .materials(Store::open(&fixture.home)?, &invalid)
            .await
            .err()
            .unwrap()
            .code,
        "invalid_input"
    );
    Ok(())
}

#[tokio::test]
async fn discovered_file_rechecks_restrictions_before_fetching() -> anyhow::Result<()> {
    use std::sync::atomic::Ordering;
    let fixture = campus::Campus::new(false).await?;
    let (a, b) = tokio::io::duplex(64_000);
    let server = moocp::server::Server::new(fixture.home.clone(), None);
    let task = tokio::spawn(async move { server.serve(a).await.unwrap().waiting().await.unwrap() });
    let client = ().serve(b).await?;
    let result = client
        .call_tool(
            CallToolRequestParams::new("moodle_list_materials").with_arguments(
                json!({"course_id":42,"limit":50})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    assert_ne!(result.is_error, Some(true));
    let page = result.structured_content.unwrap();
    let file = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["kind"] == "file" && m["module_id"] == 8)
        .unwrap();
    fixture
        .traffic
        .resource_locked
        .store(true, Ordering::Relaxed);
    let read = client
        .call_tool(
            CallToolRequestParams::new("moodle_read").with_arguments(
                json!({"course_id":42,"reference":file["reference"]})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    assert_eq!(
        read.structured_content.unwrap()["error"]["code"],
        "activity_locked"
    );
    assert_eq!(fixture.traffic.count("file:lecture.pdf"), 0);
    client.cancel().await?;
    task.await?;
    Ok(())
}
