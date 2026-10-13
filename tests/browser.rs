use axum::{Router, routing::get};
use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;
use serde_json::Value;

#[tokio::test]
#[ignore = "requires an installed Chromium-family browser"]
async fn requires_positive_same_site_login_and_captures_scoped_cookies() -> anyhow::Result<()> {
    let app=Router::new().route("/",get(||async{axum::response::Html("<html><body><a href='/login/logout.php?sesskey=abc123'>Log out</a><script>window.M={cfg:{sesskey:'abc123',userId:42,wwwroot:location.origin}};document.cookie='MoodleSession=fixture;path=/';</script></body></html>")}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let site = format!("http://{}/", listener.local_addr()?);
    let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir()?;
    let config = BrowserConfig::builder()
        .user_data_dir(directory.path())
        .respect_https_errors()
        .arg("remote-debugging-address=127.0.0.1")
        .build()
        .map_err(anyhow::Error::msg)?;
    let (mut browser, mut handler) = Browser::launch(config).await?;
    let driver = tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if event.is_err() {
                break;
            }
        }
    });
    let outcome = async {
        let page = browser.new_page(&site).await?;
        let expression = format!(
            "({})({})",
            moocp::auth::LOGIN_SNAPSHOT,
            serde_json::to_string(&site)?
        );
        let snapshot = page
            .evaluate(expression.clone())
            .await?
            .into_value::<Value>()?;
        assert_eq!(snapshot["user_id"], 42);
        assert_eq!(snapshot["sesskey"], "abc123");
        assert!(
            page.get_cookies()
                .await?
                .iter()
                .any(|c| c.name == "MoodleSession" && c.path == "/")
        );
        page.evaluate("document.body.classList.add('notloggedin')")
            .await?;
        assert!(
            page.evaluate(expression.clone())
                .await?
                .into_value::<Value>()
                .unwrap_or(Value::Null)
                .is_null()
        );
        page.evaluate("document.body.classList.remove('notloggedin');M.cfg.userId=0")
            .await?;
        assert!(
            page.evaluate(expression.clone())
                .await?
                .into_value::<Value>()
                .unwrap_or(Value::Null)
                .is_null()
        );
        page.evaluate("M.cfg.userId=42;document.querySelector('a').remove()")
            .await?;
        assert!(
            page.evaluate(expression.clone())
                .await?
                .into_value::<Value>()
                .unwrap_or(Value::Null)
                .is_null()
        );
        let other = format!(
            "({})(\"https://other.example/\")",
            moocp::auth::LOGIN_SNAPSHOT
        );
        assert!(
            page.evaluate(other)
                .await?
                .into_value::<Value>()
                .unwrap_or(Value::Null)
                .is_null()
        );
        anyhow::Ok(())
    }
    .await;
    let _ = browser.close().await;
    let _ = browser.wait().await;
    driver.abort();
    http.abort();
    outcome
}
