//! Synthetic student workflows shared by CI and the opt-in agent evaluation runner.
#![allow(dead_code)]
use axum::{
    Json, Router,
    extract::{Form, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use moocp::config::{Cookie, CredentialStorage, Credentials, Home, Profile};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use url::Url;

pub const COURSE: u64 = 42;
pub const DUE: i64 = 1_920_283_200;
pub const EXTENSION: i64 = DUE + 86400;
pub const CUTOFF: i64 = DUE + 172800;

#[derive(Default)]
pub struct Traffic {
    pub requests: Mutex<HashMap<String, usize>>,
    pub resource_locked: std::sync::atomic::AtomicBool,
}
impl Traffic {
    pub fn record(&self, name: &str) {
        *self
            .requests
            .lock()
            .unwrap()
            .entry(name.into())
            .or_default() += 1;
    }
    pub fn count(&self, name: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .unwrap_or(0)
    }
}
#[derive(Clone)]
struct Site {
    root: Url,
    session: bool,
    traffic: Arc<Traffic>,
}

pub struct Campus {
    pub directory: tempfile::TempDir,
    pub home: Home,
    pub profile: Profile,
    pub traffic: Arc<Traffic>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Campus {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Campus {
    pub async fn new(session: bool) -> anyhow::Result<Self> {
        let directory = tempfile::tempdir()?;
        let home = Home {
            path: directory.path().join("account"),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let root = Url::parse(&format!("http://{}/moodle/", listener.local_addr()?))?;
        let traffic = Arc::new(Traffic::default());
        let site = Site {
            root: root.clone(),
            session,
            traffic: traffic.clone(),
        };
        let app = Router::new()
            .route("/moodle/webservice/rest/server.php", post(rest))
            .route("/moodle/lib/ajax/service.php", post(ajax))
            .route("/moodle/course/view.php", get(course))
            .route("/moodle/course/section.php", get(course))
            .route("/moodle/mod/{kind}/view.php", get(activity))
            .route("/moodle/pluginfile.php/{name}", get(file))
            .with_state(site);
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let profile = Profile {
            id: uuid::Uuid::new_v4().to_string(),
            name: "evaluation".into(),
            url: root,
            user_id: Some(3),
            display_name: Some("Synthetic student".into()),
            storage: CredentialStorage::File,
            allow_http: true,
            authenticated_at: chrono::Utc::now().to_rfc3339(),
        };
        home.save_profile(&profile)?;
        home.save_credentials(
            &profile,
            &Credentials {
                token: (!session).then(|| "fixture-token".into()),
                sesskey: Some("fixture-key".into()),
                cookies: vec![Cookie {
                    name: "MoodleSession".into(),
                    value: "fixture-session".into(),
                    domain: "127.0.0.1".into(),
                    path: "/moodle/".into(),
                    secure: false,
                    expires: None,
                }],
            },
        )?;
        Ok(Self {
            directory,
            home,
            profile,
            traffic,
            task,
        })
    }
}
fn summary() -> &'static str {
    "<table><tr><th>Week</th><th>Topic</th><th>Slides</th></tr><tr><td>5</td><td>Gradient descent</td><td><a href='../pluginfile.php/lecture.pdf'>Neural network training</a></td></tr></table><a href='https://files.example.invalid/protected.pdf'>External reading</a>"
}
fn module(site: &Site, id: u64, instance: u64, kind: &str, name: &str, visible: bool) -> Value {
    json!({"id":id,"instance":instance,"modname":kind,"module":kind,"name":name,"uservisible":visible,"sectionid":5101,"url":site.root.join(&format!("mod/{kind}/view.php?id={id}")).unwrap().as_str(),"contents":[]})
}
fn modules(site: &Site) -> Vec<Value> {
    let mut resource = module(
        site,
        8,
        80,
        "resource",
        "Original slides",
        !site
            .traffic
            .resource_locked
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    resource["contents"] = json!([{"type":"file","filename":"lecture.pdf","mimetype":"application/pdf","filesize":1024,"fileurl":site.root.join("pluginfile.php/lecture.pdf").unwrap().as_str()}]);
    vec![
        module(site, 7, 70, "page", "Lecture notes", true),
        resource,
        module(site, 9, 90, "folder", "Handouts", true),
        module(site, 10, 100, "label", "Announcements", true),
        module(site, 11, 110, "book", "Study guide", true),
        module(site, 12, 120, "resource", "Restricted Week 6 slides", false),
        module(site, 20, 200, "assign", "Vision report", true),
        module(site, 21, 210, "assign", "Unavailable personal status", true),
        module(site, 22, 220, "assign", "Completed exercise", true),
        module(site, 30, 300, "quiz", "Revision quiz", true),
    ]
}
fn contents(site: &Site) -> Value {
    json!([{"id":5100,"section":0,"name":"General","summary":"","modules":[]},{"id":5101,"section":5,"name":"Week 5","summary":summary(),"modules":modules(site)}])
}
fn assignment(id: u64) -> Value {
    json!({"id":id,"cmid":id/10,"course":42,"name":"Coursework","duedate":DUE,"cutoffdate":CUTOFF,"allowsubmissionsfromdate":DUE-864000,"submissiondrafts":1,"teamsubmission":id==220,"requireallteammemberssubmit":id==220,"maxattempts":3,"intro":"<p>Use the rubric and explain your evidence.</p>"})
}
async fn rest(State(site): State<Site>, Form(args): Form<HashMap<String, String>>) -> Json<Value> {
    let function = args
        .get("wsfunction")
        .map(String::as_str)
        .unwrap_or_default();
    site.traffic.record(function);
    let denied = || json!({"exception":"moodle_exception","errorcode":"nopermissions"});
    let value = match function {
        "core_course_get_contents" => {
            let mut content = contents(&site);
            for activity in content[1]["modules"].as_array_mut().unwrap() {
                activity["completiondata"] =
                    json!({"state":site.traffic.count("core_course_get_contents")%2});
            }
            content
        }
        "core_enrol_get_users_courses" => {
            json!([{"id":42,"fullname":"Computer Vision","shortname":"CV","summary":"Synthetic course"}])
        }
        "mod_assign_get_assignments" => {
            json!({"courses":[{"id":42,"assignments":[assignment(200),assignment(210),assignment(220)]}],"warnings":[]})
        }
        "mod_assign_get_submission_status" => match args.get("assignid").map(String::as_str) {
            Some("200") => {
                json!({"lastattempt":{"submission":{"status":"draft","attemptnumber":0,"timemodified":DUE-100},"extensionduedate":EXTENSION,"cansubmit":true,"canedit":true,"locked":false,"submissionsenabled":true,"gradingstatus":"notgraded"},"warnings":[]})
            }
            Some("220") => {
                json!({"lastattempt":{"submission":{"status":"new"},"teamsubmission":{"status":"submitted","attemptnumber":1},"submissiongroup":9,"submissiongroupmemberswhoneedtosubmit":[],"extensionduedate":0,"canedit":false,"cansubmit":false,"locked":true,"submissionsenabled":true,"gradingstatus":"released"},"feedback":{"gradefordisplay":"72 / 100","gradeddate":DUE-50},"warnings":[]})
            }
            _ => denied(),
        },
        "mod_quiz_get_quizzes_by_courses" => {
            json!({"quizzes":[{"id":300,"coursemodule":30,"course":42,"name":"Revision quiz","timeopen":DUE-864000,"timeclose":CUTOFF,"timelimit":3600,"attempts":2,"intro":"<p>Revision questions</p>"}],"warnings":[]})
        }
        "mod_quiz_get_user_attempts" => {
            json!({"attempts":[{"id":600,"attempt":1,"state":"finished","preview":false,"timefinish":DUE-500}],"warnings":[]})
        }
        "mod_page_get_pages_by_courses" => {
            json!({"pages":[{"id":70,"content":"<p>Neural networks use gradient descent.</p>","intro":""}]})
        }
        _ => json!({"exception":"moodle_exception","errorcode":"invalidfunction"}),
    };
    Json(value)
}
async fn ajax(State(site): State<Site>, Json(args): Json<Value>) -> Json<Value> {
    let function = args[0]["methodname"].as_str().unwrap_or_default();
    site.traffic.record(function);
    let data = match function {
        "core_courseformat_get_state" => {
            let mut list = modules(&site);
            for item in &mut list {
                item.as_object_mut().unwrap().remove("instance");
                item.as_object_mut().unwrap().remove("contents");
            }
            json!({"section":[{"id":5100,"number":0,"title":"General","hassummary":false},{"id":5101,"number":5,"title":"Week 5","hassummary":true}],"cm":list})
        }
        "core_course_get_enrolled_courses_by_timeline_classification" => {
            json!({"courses":[{"id":42,"fullname":"Computer Vision","shortname":"CV"}]})
        }
        "core_enrol_get_users_courses" => {
            json!([{"id":42,"fullname":"Computer Vision","shortname":"CV"}])
        }
        _ => return Json(json!([{"error":true,"exception":{"errorcode":"invalidfunction"}}])),
    };
    Json(json!([{"error":false,"data":data}]))
}
async fn course(State(site): State<Site>) -> Html<String> {
    site.traffic.record("course_page");
    Html(format!(
        "<main id='region-main'><li id='section-5' class='section' data-for='section' data-id='5101'><h3 class='sectionname'>Week 5</h3><div class='summarytext'>{}</div><li id='module-10' class='activity label'><div class='activity-altcontent'><ul><li>Bring the printed rubric.</li></ul></div></li></li></main>",
        summary()
    ))
}
async fn activity(
    State(site): State<Site>,
    axum::extract::Path((kind,)): axum::extract::Path<(String,)>,
    Query(args): Query<HashMap<String, String>>,
) -> Response {
    let id = args
        .get("id")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    site.traffic.record(&format!("activity:{id}"));
    let body=match (kind.as_str(),id) {
        (_,12) => return StatusCode::FORBIDDEN.into_response(),
        ("assign",20..=22) => format!("<main id='region-main'><input name='assign' value='{}' type='hidden'><div class='submissionstatustable'><table><tr><th>Submission status</th><td class='submissionstatus{}'>{}</td></tr><tr><th>Extension due date</th><td><time datetime='{}'>Personal extension</time></td></tr><tr><th>Cut-off date</th><td><time datetime='{}'>Final cutoff</time></td></tr></table></div><a href='?id={id}&amp;action=editsubmission'>Edit submission</a><a href='?id={id}&amp;action=submit'>Submit assignment</a></main>",id*10,if id==22{"submitted"}else{"draft"},if id==22{"Submitted for grading"}else{"Draft (not submitted)"},chrono::DateTime::from_timestamp(EXTENSION,0).unwrap().to_rfc3339(),chrono::DateTime::from_timestamp(CUTOFF,0).unwrap().to_rfc3339()),
        ("folder",_) => "<main><a href='../../pluginfile.php/handout.txt'>Handout</a><a href='../../pluginfile.php/rubric.txt'>Rubric</a></main>".into(),
        ("resource",_) => "<main><a href='../../pluginfile.php/lecture.pdf'>Slides</a></main>".into(),
        ("label",_) => "<main><li id='module-10'><div class='activity-altcontent'>Bring the printed rubric.</div></li></main>".into(),
        ("book",_) => "<main><h1>Study guide</h1><p>Read the rubric carefully.</p></main>".into(),
        ("quiz",_) => "<main><input type='hidden' name='quiz' value='300'><div class='quizinfo'>Attempts allowed: 2</div><table class='quizattemptsummary'><tbody><tr><td>1</td><td class='state'>Finished</td></tr></tbody></table></main>".into(),
        _ => "<main><p>Neural networks use gradient descent.</p></main>".into(),
    };
    Html(body).into_response()
}
async fn file(
    State(site): State<Site>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    site.traffic.record(&format!("file:{name}"));
    match name.as_str() {
        "lecture.pdf" => (
            [("content-type", "application/pdf")],
            include_bytes!("../fixtures/lecture.pdf").as_slice(),
        )
            .into_response(),
        "handout.txt" => (
            [("content-type", "text/plain")],
            "Explain gradient descent.",
        )
            .into_response(),
        "rubric.txt" => (
            [("content-type", "text/plain")],
            "State assumptions and cite sources.",
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}
