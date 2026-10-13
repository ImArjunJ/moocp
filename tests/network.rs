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
