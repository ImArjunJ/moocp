use axum::{Router, routing::get};
use moocp::{
    browser::{Controller, Prepare, Step},
    config::{Cookie, CredentialStorage, Credentials, Home, Profile, site_url},
    store::Store,
};

#[tokio::test]
#[ignore = "requires installed Chromium"]
async fn browser_snapshots_reviewed_form_interactions_and_stale_state() -> anyhow::Result<()> {
    let app=Router::new().route("/",get(||async{axum::response::Html(r#"<html><body><main role=main><h1>Plugin activity</h1><label>Response<input id=answer></label><label>Password<input type=password value='private-password'></label><select id=choice><option value=a>A</option><option value=b>B</option></select><button id=save onclick="document.querySelector('#result').textContent='Saved: '+document.querySelector('#answer').value+' / '+document.querySelector('#choice').value">Save response</button><p id=result></p><input type=file onchange="const f=this.files[0];f.text().then(t=>document.querySelector('#upload-result').textContent=f.name+':'+t)"><p id=upload-result></p><canvas width=120 height=60 onmouseup="document.querySelector('#drag-result').textContent='Dragged'"></canvas><p id=drag-result></p><button type=button onclick="window.open('/popup')">Open activity popup</button></main></body></html>"#)})).route("/popup",get(||async{axum::response::Html("<html><body><main><h1>Activity popup</h1></main></body></html>")}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = site_url(&format!("http://{}/", listener.local_addr()?), true)?;
    let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir()?;
    let home = Home {
        path: directory.path().to_owned(),
    };
    let profile = Profile {
        id: uuid::Uuid::new_v4().to_string(),
        name: "test".into(),
        url: url.clone(),
        user_id: None,
        display_name: Some("Test student".into()),
        storage: CredentialStorage::File,
        allow_http: true,
        authenticated_at: chrono::Utc::now().to_rfc3339(),
    };
    let credentials = Credentials {
        cookies: vec![Cookie {
            name: "MoodleSession".into(),
            value: "fixture".into(),
            domain: "127.0.0.1".into(),
            path: "/".into(),
            secure: false,
            expires: None,
        }],
        ..Default::default()
    };
    let mut browser =
        Controller::launch(&home, profile.clone(), credentials.clone(), url, true).await?;
    let outcome = async {
        let snapshot = browser.snapshot().await?;
        assert!(!snapshot.to_string().contains("private-password"));
        let nodes = snapshot["nodes"].as_array().unwrap();
        let input = nodes.iter().find(|n| n["type"] == "text").unwrap()["ref"]
            .as_str()
            .unwrap()
            .to_string();
        let select = nodes.iter().find(|n| n["type"] == "select-one").unwrap()["ref"]
            .as_str()
            .unwrap()
            .to_string();
        let button = nodes.iter().find(|n| n["type"] == "submit").unwrap()["ref"]
            .as_str()
            .unwrap()
            .to_string();
        let password = nodes.iter().find(|n| n["type"] == "password").unwrap()["ref"]
            .as_str()
            .unwrap()
            .to_string();
        let sid = snapshot["snapshot_id"].as_str().unwrap().to_string();
        assert!(
            browser
                .prepare(
                    &home,
                    Prepare {
                        session_id: browser.session_id.clone(),
                        snapshot_id: sid.clone(),
                        steps: vec![Step::Fill {
                            reference: password,
                            text: "no".into()
                        }],
                        purpose: "Do not expose secrets".into()
                    }
                )
                .is_err()
        );
        let proposal = browser.prepare(
            &home,
            Prepare {
                session_id: browser.session_id.clone(),
                snapshot_id: sid.clone(),
                steps: vec![
                    Step::Fill {
                        reference: input,
                        text: "Student response".into(),
                    },
                    Step::Select {
                        reference: select,
                        values: vec!["b".into()],
                    },
                    Step::Click { reference: button },
                ],
                purpose: "Save my plugin response".into(),
            },
        )?;
        let id = proposal["proposal_id"].as_str().unwrap();
        assert!(browser.execute(&home, id).await.is_err());
        Store::open(&home)?.approve(id)?;
        let receipt = browser.execute(&home, id).await?;
        assert_eq!(receipt["completed_steps"], 3);
        assert!(
            receipt["verification"]["snapshot"]["text"]
                .as_str()
                .unwrap()
                .contains("Saved: Student response / B")
                || receipt["verification"]["snapshot"]["text"]
                    .as_str()
                    .unwrap()
                    .contains("Saved: Student response / b")
        );
        assert_eq!(browser.execute(&home, id).await?, receipt);
        assert!(
            browser
                .prepare(
                    &home,
                    Prepare {
                        session_id: browser.session_id.clone(),
                        snapshot_id: sid,
                        steps: vec![Step::ClickAt { x: 1, y: 1 }],
                        purpose: "Stale snapshot".into()
                    }
                )
                .is_err()
        );
        assert!(browser.screenshot().await?.starts_with(b"\x89PNG"));
        let source = home.path.join("coursework.txt");
        std::fs::write(&source, "immutable draft")?;
        let snapshot = browser.snapshot().await?;
        let file_ref = snapshot["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["type"] == "file")
            .unwrap()["ref"]
            .as_str()
            .unwrap()
            .to_string();
        let upload = browser.prepare(
            &home,
            Prepare {
                session_id: browser.session_id.clone(),
                snapshot_id: snapshot["snapshot_id"].as_str().unwrap().into(),
                steps: vec![Step::AttachFiles {
                    reference: file_ref,
                    files: vec![source.to_string_lossy().into_owned()],
                }],
                purpose: "Attach the reviewed coursework draft".into(),
            },
        )?;
        std::fs::write(&source, "changed after preparation")?;
        let id = upload["proposal_id"].as_str().unwrap();
        Store::open(&home)?.approve(id)?;
        let receipt = browser.execute(&home, id).await?;
        assert!(
            receipt["verification"]["snapshot"]["text"]
                .as_str()
                .unwrap()
                .contains("coursework.txt:immutable draft")
        );
        let snapshot = browser.snapshot().await?;
        let canvas = snapshot["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["type"] == "canvas")
            .unwrap();
        let x = canvas["x"].as_u64().unwrap() as u32;
        let y = canvas["y"].as_u64().unwrap() as u32;
        let drag = browser.prepare(
            &home,
            Prepare {
                session_id: browser.session_id.clone(),
                snapshot_id: snapshot["snapshot_id"].as_str().unwrap().into(),
                steps: vec![Step::Drag {
                    from_x: x - 10,
                    from_y: y,
                    to_x: x + 10,
                    to_y: y,
                }],
                purpose: "Drag within the canvas".into(),
            },
        )?;
        let id = drag["proposal_id"].as_str().unwrap();
        Store::open(&home)?.approve(id)?;
        let receipt = browser.execute(&home, id).await?;
        assert!(
            receipt["verification"]["snapshot"]["text"]
                .as_str()
                .unwrap()
                .contains("Dragged")
        );
        let tabs = browser.tabs().await?;
        let original = tabs["tabs"][0]["tab_id"].as_str().unwrap().to_owned();
        let snapshot = browser.snapshot().await?;
        let popup_ref = snapshot["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["label"] == "Open activity popup")
            .unwrap()["ref"]
            .as_str()
            .unwrap()
            .into();
        let popup = browser.prepare(
            &home,
            Prepare {
                session_id: browser.session_id.clone(),
                snapshot_id: snapshot["snapshot_id"].as_str().unwrap().into(),
                steps: vec![Step::Click {
                    reference: popup_ref,
                }],
                purpose: "Open the activity popup".into(),
            },
        )?;
        let id = popup["proposal_id"].as_str().unwrap();
        Store::open(&home)?.approve(id)?;
        let receipt = browser.execute(&home, id).await?;
        assert!(
            receipt["verification"]["snapshot"]["text"]
                .as_str()
                .unwrap()
                .contains("Activity popup")
        );
        assert_eq!(browser.tabs().await?["tabs"].as_array().unwrap().len(), 2);
        let restored = browser.switch_tab(&original).await?;
        assert!(
            restored["text"]
                .as_str()
                .unwrap()
                .contains("Plugin activity")
        );
        let changed = Credentials {
            cookies: vec![],
            ..Default::default()
        };
        assert!(browser.check_account(&profile, &changed).is_err());
        anyhow::Ok(())
    }
    .await;
    browser.close().await;
    http.abort();
    outcome
}

#[tokio::test]
#[ignore = "requires installed Chromium"]
async fn uninstall_closes_only_its_isolated_browser() -> anyhow::Result<()> {
    let app = Router::new().route(
        "/",
        get(|| async {
            axum::response::Html("<html><body><main>Student activity</main></body></html>")
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = site_url(&format!("http://{}/", listener.local_addr()?), true)?;
    let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir()?;
    #[cfg(unix)]
    let browser_root = {
        let alias = directory.path().join("browser root with spaces");
        std::os::unix::fs::symlink(directory.path(), &alias)?;
        alias
    };
    #[cfg(windows)]
    let browser_root = directory.path().to_owned();
    let home = Home {
        path: browser_root.join("removed"),
    };
    let other = Home {
        path: directory.path().join("preserved"),
    };
    let profile = Profile {
        id: uuid::Uuid::new_v4().to_string(),
        name: "test".into(),
        url: url.clone(),
        user_id: None,
        display_name: None,
        storage: CredentialStorage::File,
        allow_http: true,
        authenticated_at: chrono::Utc::now().to_rfc3339(),
    };
    let credentials = Credentials {
        cookies: vec![Cookie {
            name: "MoodleSession".into(),
            value: "fixture".into(),
            domain: "127.0.0.1".into(),
            path: "/".into(),
            secure: false,
            expires: None,
        }],
        ..Default::default()
    };
    let mut shared_profile = profile.clone();
    shared_profile.id = uuid::Uuid::new_v4().to_string();
    shared_profile.name = "other".into();
    home.save_config(&moocp::config::Config {
        active_profile: Some(profile.id.clone()),
        profiles: vec![profile.clone(), shared_profile.clone()],
    })?;
    let mut removed = Controller::launch(
        &home,
        profile.clone(),
        credentials.clone(),
        url.clone(),
        true,
    )
    .await?;
    let mut shared = Controller::launch(
        &home,
        shared_profile,
        credentials.clone(),
        url.clone(),
        true,
    )
    .await?;
    let mut preserved = Controller::launch(&other, profile.clone(), credentials, url, true).await?;
    let outcome = async {
        let executable = directory.path().join("moocp");
        std::fs::write(&executable, b"fixture")?;
        moocp::uninstall::run(&home, Some(&profile.id), true, &executable, &[]).await?;
        assert!(removed.snapshot().await.is_err());
        assert!(
            shared.snapshot().await?["text"]
                .as_str()
                .unwrap()
                .contains("Student activity")
        );
        assert_eq!(
            std::fs::read_dir(&home.path)?
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("browser-"))
                .count(),
            1
        );
        moocp::uninstall::run(&home, None, true, &executable, &[]).await?;
        assert!(!home.path.exists());
        assert!(
            preserved.snapshot().await?["text"]
                .as_str()
                .unwrap()
                .contains("Student activity")
        );
        assert!(removed.snapshot().await.is_err());
        assert!(shared.snapshot().await.is_err());
        anyhow::Ok(())
    }
    .await;
    removed.close().await;
    shared.close().await;
    preserved.close().await;
    http.abort();
    outcome
}
