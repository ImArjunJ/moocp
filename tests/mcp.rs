use rmcp::{ServiceExt, model::CallToolRequestParams, transport::TokioChildProcess};
use serde_json::json;

#[tokio::test]
async fn subprocess_handshake_discovery_and_unconfigured_errors() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_moocp"));
    command.arg("--data-dir").arg(directory.path()).arg("serve");
    let client = ().serve(TokioChildProcess::new(command)?).await?;
    let tools = client.list_all_tools().await?;
    assert_eq!(tools.len(), 26);
    for name in [
        "moodle_browser_open",
        "moodle_browser_tabs",
        "moodle_browser_switch_tab",
        "moodle_prepare_browser_action",
    ] {
        assert!(tools.iter().any(|tool| tool.name == name));
    }
    let read = tools.iter().find(|t| t.name == "moodle_read").unwrap();
    assert_eq!(
        read.annotations.as_ref().and_then(|a| a.read_only_hint),
        Some(true)
    );
    let write = tools
        .iter()
        .find(|t| t.name == "moodle_execute_action")
        .unwrap();
    assert_eq!(
        write.annotations.as_ref().and_then(|a| a.destructive_hint),
        Some(true)
    );
    let status = client
        .call_tool(CallToolRequestParams::new("moodle_status"))
        .await?;
    assert_eq!(
        status.structured_content.as_ref().unwrap()["configured"],
        false
    );
    let courses = client
        .call_tool(
            CallToolRequestParams::new("moodle_list_courses")
                .with_arguments(json!({}).as_object().unwrap().clone()),
        )
        .await?;
    assert_eq!(courses.is_error, Some(true));
    assert_eq!(
        courses.structured_content.as_ref().unwrap()["error"]["code"],
        "not_configured"
    );
    let resources = client.list_all_resources().await?;
    assert!(resources.iter().any(|r| r.uri == "moocp://help"));
    client.cancel().await?;
    Ok(())
}

#[test]
fn cli_review_refuses_piped_approval() {
    let directory = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_moocp"))
        .arg("--data-dir")
        .arg(directory.path())
        .args(["review", "anything"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Piped or unattended approval"));
}
