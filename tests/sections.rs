use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Json, Router,
    extract::Query,
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
};
use moocp::{
    config::{Cookie, CredentialStorage, Credentials, Home, Profile},
    moodle::Moodle,
    server::Server,
};
use rmcp::{ServiceExt, model::CallToolRequestParams};
use serde_json::{Value, json};
use url::Url;

fn timetable() -> String {
    "<table><tr><th>Week</th><th>Date</th><th>Topic</th><th>Slides</th></tr><tr><td>4</td><td>23 October 2026</td><td>Lecture 10 Visual recognition</td><td><a href='../pluginfile.php/12/course/section/13925/lecture.pdf'>Slides</a></td></tr><tr><td>5</td><td>30 October 2026</td><td>Lecture 11 Machine learning for Pattern Recognition</td><td><a href='/materials/lecture.html'>Slides and handouts</a></td></tr></table><p>Bring the handout.</p><p>No lecture next Friday.</p>".into()
}

fn course_html(lazy: bool) -> String {
    format!(
        "<aside><div data-for='section' data-id='13925'><div class='summarytext'>Sidebar is not course material</div></div></aside><main id='region-main'><ul><li id='section-0' class='section' data-for='section' data-id='13923'><div class='summary'><div class='summarytext'><ul><li>Course announcements</li></ul></div></div></li><li id='section-2' class='section' data-for='section' data-id='13925'><h3 class='sectionname'>Schedule and Lecture Materials</h3>{}</li></ul></main>",
        if lazy {
            String::new()
        } else {
            format!(
                "<div class='summary'><div class='summarytext'>{}</div></div>",
                timetable()
            )
        }
    )
}

struct Fixture {
    _directory: tempfile::TempDir,
    task: tokio::task::JoinHandle<()>,
    home: Home,
    moodle: Moodle,
    public_requests: Arc<AtomicUsize>,
    private_requests: Arc<AtomicUsize>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new(transport: &str) -> anyhow::Result<Self> {
        let lazy = transport == "lazy";
        let html_only = transport == "html";
        let rest = transport == "rest";
        let failed = transport == "failed";
        let public_requests = Arc::new(AtomicUsize::new(0));
        let private_requests = Arc::new(AtomicUsize::new(0));
        let public_count = public_requests.clone();
        let private_count = private_requests.clone();
        let app = Router::new()
            .route("/moodle/lib/ajax/service.php", post(move || async move {
                if html_only { return Json(json!([{"error":true,"exception":{"errorcode":"invalidfunction","message":"unused"}}])); }
                Json(json!([{"error":false,"data":json!({"section":[{"id":"13923","number":0,"title":"General","hassummary":true},{"id":"13925","number":2,"title":"Schedule and Lecture Materials","hassummary":true}],"cm":[]}).to_string()}]))
            }))
            .route("/moodle/webservice/rest/server.php", post(|| async {
                Json(json!([{"id":13925,"section":2,"name":"Schedule and Lecture Materials","summary":timetable(),"modules":[]}]))
            }))
            .route("/moodle/course/view.php", get(move || async move {
                if failed { return StatusCode::SERVICE_UNAVAILABLE.into_response(); }
                Html(course_html(lazy)).into_response()
            }))
            .route("/moodle/course/section.php", get(move |Query(query): Query<std::collections::HashMap<String,String>>| async move {
                if failed { return StatusCode::SERVICE_UNAVAILABLE.into_response(); }
                assert_eq!(query.get("id").unwrap(), "13925");
                Html(format!("<aside><div data-for='section' data-id='13925'><div class='summarytext'>Sidebar is not course material</div></div></aside><main id='region-main'><div class='summarytext'>{}</div></main>", timetable())).into_response()
            }))
            .route("/moodle/pluginfile.php/12/course/section/13925/lecture.pdf", get(move |headers: HeaderMap| {
                let count = private_count.clone();
                async move {
                    assert_eq!(headers.get("cookie").unwrap(), "MoodleSession=fixture-session");
                    count.fetch_add(1, Ordering::Relaxed);
                    ([("content-type", "application/pdf")], include_bytes!("fixtures/lecture.pdf").as_slice())
                }
            }))
            .route("/materials/lecture.html", get(move |headers: HeaderMap| {
                let count = public_count.clone();
                async move {
                    assert!(!headers.contains_key("cookie"));
                    assert!(!headers.contains_key("authorization"));
                    assert!(!headers.contains_key("referer"));
                    count.fetch_add(1, Ordering::Relaxed);
                    Html("<main><h1>Pattern recognition handout</h1><p>Learning discriminants and classifiers.</p><a href='extra.pdf'>Extra slides</a></main>")
                }
            }))
            .route("/materials/extra.pdf", get(|headers: HeaderMap| async move {
                assert!(!headers.contains_key("cookie"));
                ([("content-type", "application/pdf")], include_bytes!("fixtures/lecture.pdf").as_slice())
            }))
            .route("/moodle/go", get(|| async { Redirect::temporary("/materials/lecture.html?token=do-not-forward") }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let site = Url::parse(&format!("http://{}/moodle/", listener.local_addr()?))?;
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory = tempfile::tempdir()?;
        let home = Home {
            path: directory.path().to_owned(),
        };
        let profile = Profile {
            id: uuid::Uuid::new_v4().to_string(),
            name: "test".into(),
            url: site,
            user_id: Some(3),
            display_name: None,
            storage: CredentialStorage::File,
            allow_http: true,
            authenticated_at: chrono::Utc::now().to_rfc3339(),
        };
        let credentials = Credentials {
            sesskey: Some("fixture-key".into()),
            token: rest.then(|| "fixture-token".into()),
            cookies: vec![Cookie {
                name: "MoodleSession".into(),
                value: "fixture-session".into(),
                domain: "127.0.0.1".into(),
                path: "/".into(),
                secure: false,
                expires: None,
            }],
        };
        home.save_profile(&profile)?;
        home.save_credentials(&profile, &credentials)?;
        let moodle = Moodle::new(profile, credentials)?;
        Ok(Self {
            _directory: directory,
            task,
            home,
            moodle,
            public_requests,
            private_requests,
        })
    }
}

#[tokio::test]
async fn course_summaries_preserve_timetable_links_across_transports() -> anyhow::Result<()> {
    for transport in ["ajax", "lazy", "rest", "html"] {
        let fixture = Fixture::new(transport).await?;
        let outline = fixture.moodle.outline(831).await?;
        assert!(outline.activities.is_empty());
        let section = outline.sections.iter().find(|s| s.id == 13925).unwrap();
        assert!(
            section.summary_available,
            "{transport}: {:?}",
            outline.warnings
        );
        assert!(section.summary.contains("Week | Date | Topic | Slides"));
        assert!(
            section
                .summary
                .contains("30 October 2026 | Lecture 11 Machine learning")
        );
        assert!(section.summary.contains("Bring the handout."));
        assert!(section.summary.contains("No lecture next Friday."));
        assert_eq!(section.links.len(), 2);
        assert_eq!(section.links[0].index, 0);
        assert!(
            section.links[0]
                .url
                .ends_with("/moodle/pluginfile.php/12/course/section/13925/lecture.pdf")
        );
        assert!(section.links[1].url.ends_with("/materials/lecture.html"));
        assert_eq!(fixture.private_requests.load(Ordering::Relaxed), 0);
        assert_eq!(fixture.public_requests.load(Ordering::Relaxed), 0);
    }
    Ok(())
}

#[tokio::test]
async fn missing_summaries_are_explicit_and_do_not_hide_course_metadata() -> anyhow::Result<()> {
    let fixture = Fixture::new("failed").await?;
    let outline = fixture.moodle.outline(831).await?;
    assert_eq!(outline.sections.len(), 2);
    assert!(outline.sections.iter().all(|s| !s.summary_available));
    assert!(!outline.warnings.is_empty());
    assert_eq!(
        fixture.moodle.section(831, 999).await.err().unwrap().code,
        "section_not_found"
    );
    Ok(())
}

#[tokio::test]
async fn section_reads_link_following_indexing_and_search_work_without_modules()
-> anyhow::Result<()> {
    let fixture = Fixture::new("ajax").await?;
    let (server_transport, client_transport) = tokio::io::duplex(64_000);
    let server = Server::new(fixture.home.clone(), None);
    let task = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap()
    });
    let client = ().serve(client_transport).await?;
    async fn call(
        client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
        tool: &'static str,
        args: Value,
    ) -> anyhow::Result<Value> {
        let result = client
            .call_tool(
                CallToolRequestParams::new(tool).with_arguments(args.as_object().unwrap().clone()),
            )
            .await?;
        let value = result.structured_content.unwrap();
        assert_ne!(result.is_error, Some(true), "{tool}: {value}");
        Ok(value)
    }
    let section = call(
        &client,
        "moodle_read",
        json!({"course_id":831,"section_id":13925}),
    )
    .await?;
    assert_eq!(section["reference"], "section:13925");
    assert!(
        section["source_url"]
            .as_str()
            .unwrap()
            .contains("course/section.php?id=13925")
    );
    assert_eq!(section["links"].as_array().unwrap().len(), 2);
    let repeated = call(
        &client,
        "moodle_read",
        json!({"course_id":831,"reference":"section:13925"}),
    )
    .await?;
    assert_eq!(section["links"], repeated["links"]);
    let pdf = call(
        &client,
        "moodle_read",
        json!({"course_id":831,"section_id":13925,"link_index":0}),
    )
    .await?;
    assert!(
        pdf["content"]["text"]
            .as_str()
            .unwrap()
            .contains("negative gradient")
    );
    let page = call(
        &client,
        "moodle_read",
        json!({"course_id":831,"section_id":13925,"link_index":1}),
    )
    .await?;
    assert!(
        page["content"]["text"]
            .as_str()
            .unwrap()
            .contains("Learning discriminants")
    );
    assert_eq!(page["links"].as_array().unwrap().len(), 1);
    let extra = call(
        &client,
        "moodle_read",
        json!({"course_id":831,"reference":page["reference"],"link_index":0}),
    )
    .await?;
    assert!(
        extra["content"]["text"]
            .as_str()
            .unwrap()
            .contains("negative gradient")
    );
    let invalid = client
        .call_tool(
            CallToolRequestParams::new("moodle_read").with_arguments(
                json!({"course_id":831,"module_id":4,"section_id":13925})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    assert_eq!(
        invalid.structured_content.unwrap()["error"]["code"],
        "invalid_input"
    );
    let cross_course = client
        .call_tool(
            CallToolRequestParams::new("moodle_read").with_arguments(
                json!({"course_id":999,"reference":page["reference"]})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await?;
    assert_eq!(
        cross_course.structured_content.unwrap()["error"]["code"],
        "document_not_found"
    );
    moocp::store::Store::open(&fixture.home)?.clear_profile(&fixture.moodle.profile.id)?;
    let mut offset = 0;
    let mut count = 0;
    loop {
        let index = call(
            &client,
            "moodle_index_course",
            json!({"course_id":831,"offset":offset,"limit":1}),
        )
        .await?;
        assert_eq!(index["failures"], json!([]));
        count += index["indexed"].as_array().unwrap().len();
        if let Some(next) = index["next_offset"].as_u64() {
            offset = next;
        } else {
            break;
        }
    }
    assert_eq!(count, 4);
    for query in ["October", "classifiers", "gradient"] {
        let search = call(
            &client,
            "moodle_search",
            json!({"course_id":831,"query":query}),
        )
        .await?;
        let results = search["results"].as_array().unwrap();
        assert!(!results.is_empty(), "{query}");
        for result in results {
            assert!(result["module_id"].is_null());
        }
    }
    assert!(fixture.private_requests.load(Ordering::Relaxed) > 0);
    assert!(fixture.public_requests.load(Ordering::Relaxed) > 0);
    let redirected = fixture
        .moodle
        .material_response(&fixture.moodle.source("go"))
        .await?;
    assert!(!redirected.url.as_str().contains("token="));
    client.cancel().await?;
    task.await?;
    Ok(())
}
