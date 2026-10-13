use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn legacy_stdio_clients_negotiate_and_discover_tools() -> anyhow::Result<()> {
    for version in ["2024-11-05", "2025-11-25"] {
        let directory = tempfile::tempdir()?;
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_moocp"))
            .arg("--data-dir")
            .arg(directory.path())
            .arg("serve")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let request = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":version,"capabilities":{},"clientInfo":{"name":"legacy-test","version":"1"}}});
        input.write_all(format!("{request}\n").as_bytes()).await?;
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            output.read_line(&mut line),
        )
        .await??;
        let response: Value = serde_json::from_str(&line)?;
        assert_eq!(response["result"]["protocolVersion"], version);
        input.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n").await?;
        line.clear();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            output.read_line(&mut line),
        )
        .await??;
        let response: Value = serde_json::from_str(&line)?;
        assert!(
            response["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "moodle_read")
        );
        child.kill().await?;
        child.wait().await?;
    }
    Ok(())
}
