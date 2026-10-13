use axum::{Router, routing::post};
use moocp::{
    actions::Action,
    config::{CredentialStorage, Credentials, Home, Profile, site_url},
    moodle::Moodle,
    store::Store,
};
use rmcp::{ClientHandler, ErrorData, RoleClient, ServiceExt, model::*, service::RequestContext};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct ReviewClient {
    approve: bool,
}
impl ClientHandler for ReviewClient {
    fn get_info(&self) -> ClientConfig {
        let mut info = ClientConfig::default();
        info.capabilities = serde_json::from_value(json!({"elicitation":{"form":{}}})).unwrap();
        info
    }
    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        let ElicitRequestParams::FormElicitationParams { message, .. } = request else {
            panic!("Expected native form review")
        };
        assert!(message.contains("exact Moodle action"));
        assert!(message.contains("Test message"));
        Ok(ElicitResult::new(ElicitationAction::Accept)
            .with_content(json!({"approve":self.approve})))
    }
}

async fn scenario(approve: bool) -> anyhow::Result<()> {
    let posts = Arc::new(AtomicUsize::new(0));
    let counter = posts.clone();
    let app = Router::new().route(
        "/webservice/rest/server.php",
        post(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!([{"msgid":7,"clientmsgid":""}]))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let site = site_url(&format!("http://{}/", listener.local_addr()?), true)?;
    let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir()?;
    let home = Home {
        path: directory.path().to_owned(),
    };
    let profile = Profile {
        id: uuid::Uuid::new_v4().to_string(),
        name: "test".into(),
        url: site,
        user_id: Some(3),
        display_name: Some("Test student".into()),
        storage: CredentialStorage::File,
        allow_http: true,
        authenticated_at: chrono::Utc::now().to_rfc3339(),
    };
    let credentials = Credentials {
        token: Some("fixture-token".into()),
        ..Default::default()
    };
    home.save_profile(&profile)?;
    home.save_credentials(&profile, &credentials)?;
    let moodle = Moodle::new(profile.clone(), credentials)?;
    let proposal = moocp::actions::prepare(
        &moodle,
        &home,
        Action::SendMessage {
            recipient_id: 4,
            message: "Test message".into(),
        },
    )
    .await?;
    let id = proposal["proposal_id"].as_str().unwrap().to_string();
    let (server_transport, client_transport) = tokio::io::duplex(64_000);
    let server = moocp::server::Server::new(home.clone(), None);
    let task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap()
    });
    let client = ReviewClient { approve }.serve(client_transport).await?;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.call_tool(
            CallToolRequestParams::new("moodle_execute_action")
                .with_arguments(json!({"proposal_id":id}).as_object().unwrap().clone()),
        ),
    )
    .await??;
    let state = Store::open(&home)?.proposal(&id)?;
    if approve {
        assert_eq!(state["status"], "succeeded");
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        assert_ne!(result.is_error, Some(true));
        let repeated = client
            .call_tool(
                CallToolRequestParams::new("moodle_execute_action")
                    .with_arguments(json!({"proposal_id":id}).as_object().unwrap().clone()),
            )
            .await?;
        assert_eq!(repeated.structured_content, result.structured_content);
        assert_eq!(posts.load(Ordering::SeqCst), 1);
    } else {
        assert_eq!(state["status"], "rejected");
        assert_eq!(posts.load(Ordering::SeqCst), 0);
        assert_eq!(result.is_error, Some(true));
    }
    let output: Value = result.structured_content.unwrap();
    assert!(!output.to_string().contains("fixture-token"));
    client.cancel().await?;
    task.await?;
    http.abort();
    Ok(())
}

#[tokio::test]
async fn native_review_approval_executes_exactly_once() -> anyhow::Result<()> {
    scenario(true).await
}
#[tokio::test]
async fn native_review_decline_sends_no_moodle_request() -> anyhow::Result<()> {
    scenario(false).await
}
