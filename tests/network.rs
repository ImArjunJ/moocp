use axum::{Router, response::Redirect, routing::get};
use moocp::{config::Credentials, network::Http};
use url::Url;

#[tokio::test]
async fn refuses_external_redirects_and_detects_login_pages() {
    let app = Router::new()
        .route(
            "/external",
            get(|| async { Redirect::temporary("https://other.example/private") }),
        )
        .route(
            "/expired",
            get(|| async {
                axum::response::Html("<form><input name=username><input type=password></form>")
            }),
        )
        .route(
            "/login-redirect",
            get(|| async { Redirect::temporary("/login/index.php") }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = Http::new(url.clone(), &Credentials::default()).unwrap();
    assert_eq!(
        http.get(url.join("external").unwrap())
            .await
            .err()
            .unwrap()
            .code,
        "external_redirect"
    );
    assert_eq!(
        http.get(url.join("expired").unwrap())
            .await
            .err()
            .unwrap()
            .code,
        "reauth_required"
    );
    assert_eq!(
        http.get(url.join("login-redirect").unwrap())
            .await
            .err()
            .unwrap()
            .code,
        "reauth_required"
    );
    task.abort();
}

#[tokio::test]
async fn material_links_cannot_reach_new_private_origins_or_reattach_cookies() -> anyhow::Result<()>
{
    use axum::http::HeaderMap;
    use moocp::config::Cookie;
    let app = Router::new()
        .route(
            "/public/start",
            get(|| async { Redirect::temporary("/moodle/final?token=discard-me") }),
        )
        .route(
            "/moodle/final",
            get(|headers: HeaderMap| async move {
                assert!(!headers.contains_key("cookie"));
                assert!(!headers.contains_key("authorization"));
                assert!(!headers.contains_key("referer"));
                axum::response::Html("<main>Public material</main>")
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let site = Url::parse(&format!("http://{address}/moodle/"))?;
    let credentials = Credentials {
        cookies: vec![Cookie {
            name: "MoodleSession".into(),
            value: "fixture-session".into(),
            domain: "127.0.0.1".into(),
            path: "/".into(),
            secure: false,
            expires: None,
        }],
        ..Default::default()
    };
    let http = Http::new(site.clone(), &credentials)?;
    let response = http.get_material(site.join("/public/start")?).await?;
    assert!(response.text()?.contains("Public material"));
    assert!(!response.url.as_str().contains("token="));
    let error = http
        .get_material(Url::parse("http://127.0.0.1:1/private")?)
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "external_address");
    let error = http
        .get_material(Url::parse("http://[::1]:1/private")?)
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "external_address");
    task.abort();
    Ok(())
}

#[tokio::test]
#[ignore = "downloads a public W3C PDF to verify anonymous external material retrieval"]
async fn public_external_pdf_can_be_read() -> anyhow::Result<()> {
    let http = Http::new(
        Url::parse("https://moodle.example.invalid/")?,
        &Credentials::default(),
    )?;
    let response = http
        .get_material(Url::parse(
            "https://www.w3.org/WAI/ER/tests/xhtml/testfiles/resources/pdf/dummy.pdf",
        )?)
        .await?;
    assert!(response.body.starts_with(b"%PDF-"));
    let document = moocp::content::extract(
        &response.body,
        &response.filename,
        &response.content_type,
        Some(&response.url),
    )?;
    assert!(document.text.contains("Dummy PDF file"));
    Ok(())
}
