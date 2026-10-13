//! Moodle contracts over REST, session AJAX, and verified forms; module and instance IDs stay distinct.

use std::{collections::BTreeMap, sync::Arc};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use url::Url;

use crate::{
    config::{Credentials, Profile},
    content,
    error::{Error, Result},
    network::{Http, Response},
};

#[derive(Clone)]
pub struct Moodle {
    pub profile: Profile,
    pub http: Http,
    credentials: Credentials,
    sesskey: Arc<Mutex<Option<String>>>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Course {
    pub id: u64,
    pub name: String,
    pub shortname: String,
    pub summary: String,
    pub source_url: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Section {
    pub id: u64,
    pub number: Option<u64>,
    pub title: String,
    pub summary: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct File {
    pub index: usize,
    pub name: String,
    pub mime: String,
    pub size: Option<u64>,
    pub source_url: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Activity {
    pub module_id: u64,
    pub course_id: u64,
    pub instance_id: Option<u64>,
    pub kind: String,
    pub accessible: bool,
    pub title: String,
    pub section_id: Option<u64>,
    pub source_url: String,
    pub description: String,
    pub availability: Option<String>,
    pub completion: Option<Value>,
    pub files: Vec<File>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Outline {
    pub course_id: u64,
    pub sections: Vec<Section>,
    pub activities: Vec<Activity>,
    pub transport: String,
    pub observed_at: String,
    pub warnings: Vec<String>,
}

impl Moodle {
    pub fn new(profile: Profile, credentials: Credentials) -> Result<Self> {
        let http = Http::new(profile.url.clone(), &credentials)?;
        Ok(Self {
            profile,
            http,
            sesskey: Arc::new(Mutex::new(credentials.sesskey.clone())),
            credentials,
        })
    }
    pub fn has_token(&self) -> bool {
        self.credentials.token.is_some()
    }
    pub fn source(&self, path: &str) -> String {
        self.profile
            .url
            .join(path)
            .map(|u| content::safe_url(&u))
            .unwrap_or_default()
    }
    pub async fn rest(&self, function: &str, args: Value) -> Result<Value> {
        let token = self.credentials.token.as_deref().ok_or_else(|| Error::unsupported("This operation requires a Moodle mobile service token. Browser login may enable one automatically; otherwise this site needs a session/page adapter."))?;
        let mut form = vec![
            ("wstoken".into(), token.into()),
            ("wsfunction".into(), function.into()),
            ("moodlewsrestformat".into(), "json".into()),
        ];
        flatten_form("", &args, &mut form);
        let response = self
            .http
            .post_form(self.http.url("webservice/rest/server.php")?, &form)
            .await?;
        let value = response.json()?;
        if value.get("exception").is_some() {
            return Err(api_error(&value));
        }
        Ok(value)
    }
    pub async fn session_key(&self) -> Result<String> {
        let mut cache = self.sesskey.lock().await;
        if let Some(key) = cache.as_ref() {
            return Ok(key.clone());
        }
        let response = self.http.get(self.http.url("my/")?).await?;
        let key = extract_sesskey(&response.text()?).ok_or_else(Error::auth)?;
        *cache = Some(key.clone());
        Ok(key)
    }
    pub async fn ajax(&self, function: &str, args: Value) -> Result<Value> {
        let key = self.session_key().await?;
        let mut url = self.http.url("lib/ajax/service.php")?;
        url.query_pairs_mut().append_pair("sesskey", &key);
        let response = self
            .http
            .post_json(url, &json!([{"index":0,"methodname":function,"args":args}]))
            .await?;
        let values = response.json()?;
        let entry = values
            .as_array()
            .and_then(|v| v.first())
            .ok_or_else(|| Error::new("invalid_response", "Moodle AJAX response had no result."))?;
        if entry["error"].as_bool() == Some(true) {
            return Err(api_error(&entry["exception"]));
        }
        let data = entry
            .get("data")
            .cloned()
            .ok_or_else(|| Error::new("invalid_response", "Moodle AJAX response had no data."))?;
        // core_courseformat_get_state returns JSON inside a string.
        Ok(if let Some(text) = data.as_str() {
            serde_json::from_str(text).unwrap_or(data)
        } else {
            data
        })
    }
    pub async fn read_call(&self, function: &str, args: Value) -> Result<Value> {
        if self.has_token() {
            match self.rest(function, args.clone()).await {
                Ok(result) => return Ok(result),
                Err(error) if error.code == "function_unavailable" => (),
                Err(error) => return Err(error),
            }
        }
        self.ajax(function, args).await
    }
    pub async fn validate(&self) -> Result<Value> {
        if self.has_token() {
            let info = self
                .rest("core_webservice_get_site_info", json!({}))
                .await?;
            return Ok(
                json!({"connected":true,"site":info["sitename"],"user_id":info["userid"],"display_name":info["fullname"],"access":"session_and_mobile_token","functions":info["functions"].as_array().map(|f|f.iter().filter_map(|v|v["name"].as_str()).collect::<Vec<_>>()).unwrap_or_default()}),
            );
        }
        let response = self.http.get(self.http.url("my/")?).await?;
        let text = response.text()?;
        let key = extract_sesskey(&text).ok_or_else(Error::auth)?;
        *self.sesskey.lock().await = Some(key);
        Ok(
            json!({"connected":true,"access":"browser_session","user_id":self.profile.user_id,"display_name":self.profile.display_name,"mobile_token":false}),
        )
    }
    pub async fn courses(&self) -> Result<Vec<Course>> {
        let raw = if self.has_token() {
            let user = if let Some(id) = self.profile.user_id {
                id
            } else {
                self.rest("core_webservice_get_site_info", json!({}))
                    .await?["userid"]
                    .as_u64()
                    .ok_or_else(|| {
                        Error::new("invalid_response", "Moodle did not return your user ID.")
                    })?
            };
            self.rest("core_enrol_get_users_courses", json!({"userid":user}))
                .await?
        } else {
            let mut courses = Vec::new();
            let mut offset = 0;
            loop {
                let raw=self.ajax("core_course_get_enrolled_courses_by_timeline_classification",json!({"classification":"all","limit":100,"offset":offset,"sort":"fullname"})).await?;
                let page = raw["courses"].as_array().ok_or_else(|| {
                    Error::new("invalid_response", "Moodle returned no course list.")
                })?;
                courses.extend(page.clone());
                if page.len() < 100 {
                    break;
                }
                offset += 100;
                if offset >= 2000 {
                    return Err(Error::new(
                        "size_limit",
                        "Account has more than 2000 courses; course discovery needs a narrower adapter.",
                    ));
                }
            }
            json!(courses)
        };
        let list = raw
            .as_array()
            .ok_or_else(|| Error::new("invalid_response", "Invalid Moodle course list."))?;
        list.iter()
            .map(|v| {
                let id = v["id"].as_u64().ok_or_else(|| {
                    Error::new("invalid_response", "Course has no numeric identifier.")
                })?;
                Ok(Course {
                    id,
                    name: plain(&v["fullname"]),
                    shortname: plain(&v["shortname"]),
                    summary: plain(&v["summary"]),
                    source_url: self.source(&format!("course/view.php?id={id}")),
                })
            })
            .collect()
    }
    pub async fn outline(&self, course_id: u64) -> Result<Outline> {
        if self.has_token() {
            match self
                .rest("core_course_get_contents", json!({"courseid":course_id}))
                .await
            {
                Ok(raw) => return self.normalise_contents(course_id, &raw),
                Err(e) if e.code == "function_unavailable" => (),
                Err(e) => return Err(e),
            }
        }
        match self
            .ajax("core_courseformat_get_state", json!({"courseid":course_id}))
            .await
        {
            Ok(raw) => self.normalise_state(course_id, &raw),
            Err(e) if matches!(e.code.as_str(), "function_unavailable" | "unsupported") => {
                self.outline_html(course_id).await
            }
            Err(e) => Err(e),
        }
    }
    fn normalise_contents(&self, course_id: u64, raw: &Value) -> Result<Outline> {
        let list = raw
            .as_array()
            .ok_or_else(|| Error::new("invalid_response", "Invalid course sections."))?;
        let mut sections = Vec::new();
        let mut activities = Vec::new();
        for section in list {
            let sid = section["id"].as_u64().unwrap_or(0);
            sections.push(Section {
                id: sid,
                number: section["section"].as_u64(),
                title: plain(&section["name"]),
                summary: plain(&section["summary"]),
            });
            for module in section["modules"].as_array().into_iter().flatten() {
                let cmid = module["id"]
                    .as_u64()
                    .ok_or_else(|| Error::new("invalid_response", "Activity has no module ID."))?;
                let files = module["contents"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|f| f["type"].as_str() == Some("file"))
                    .enumerate()
                    .map(|(index, f)| File {
                        index,
                        name: plain(&f["filename"]),
                        mime: f["mimetype"].as_str().unwrap_or_default().into(),
                        size: f["filesize"].as_u64(),
                        source_url: f["fileurl"]
                            .as_str()
                            .and_then(|u| Url::parse(u).ok())
                            .map(|u| content::safe_url(&u))
                            .unwrap_or_default(),
                    })
                    .collect();
                activities.push(Activity {
                    module_id: cmid,
                    course_id,
                    instance_id: module["instance"].as_u64(),
                    kind: module["modname"].as_str().unwrap_or("unknown").into(),
                    accessible: module["uservisible"].as_bool().unwrap_or(true),
                    title: plain(&module["name"]),
                    section_id: Some(sid),
                    source_url: module["url"]
                        .as_str()
                        .and_then(|u| self.profile.url.join(u).ok())
                        .map(|u| content::safe_url(&u))
                        .unwrap_or_default(),
                    description: plain(&module["description"]),
                    availability: module["availabilityinfo"]
                        .as_str()
                        .map(|s| content::html_text(s, None)),
                    completion: module.get("completiondata").cloned(),
                    files,
                });
            }
        }
        Ok(Outline {
            course_id,
            sections,
            activities,
            transport: "rest".into(),
            observed_at: chrono::Utc::now().to_rfc3339(),
            warnings: vec![],
        })
    }
    fn normalise_state(&self, course_id: u64, raw: &Value) -> Result<Outline> {
        let modules = raw["cm"]
            .as_array()
            .ok_or_else(|| Error::new("invalid_response", "Course state has no module list."))?;
        let sections = raw["section"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| Section {
                id: number(&s["id"]).unwrap_or(0),
                number: number(&s["number"]),
                title: plain(s.get("title").unwrap_or(&s["name"])),
                summary: plain(&s["summary"]),
            })
            .collect();
        let mut activities = Vec::new();
        for module in modules {
            let cmid = number(&module["id"])
                .ok_or_else(|| Error::new("invalid_response", "Activity has no module ID."))?;
            let kind = module
                .get("module")
                .unwrap_or(&module["modname"])
                .as_str()
                .unwrap_or("unknown")
                .to_ascii_lowercase();
            let kind = match kind.as_str() {
                "file" => "resource",
                "assignment" => "assign",
                "external tool" => "lti",
                "text and media area" => "label",
                _ => kind.as_str(),
            }
            .to_string();
            let url = module["url"]
                .as_str()
                .and_then(|u| self.profile.url.join(u).ok())
                .map(|u| content::safe_url(&u))
                .unwrap_or_else(|| self.source(&format!("mod/{kind}/view.php?id={cmid}")));
            activities.push(Activity {
                module_id: cmid,
                course_id,
                instance_id: number(&module["instance"]),
                kind,
                accessible: module["uservisible"].as_bool().unwrap_or(true),
                title: plain(&module["name"]),
                section_id: number(&module["sectionid"]),
                source_url: url,
                description: plain(&module["content"]),
                availability: module["availability"]
                    .as_str()
                    .map(|s| content::html_text(s, None)),
                completion: module
                    .get("completionstate")
                    .map(|state| json!({"state":state})),
                files: vec![],
            });
        }
        Ok(Outline {
            course_id,
            sections,
            activities,
            transport: "session_ajax".into(),
            observed_at: chrono::Utc::now().to_rfc3339(),
            warnings: vec![
                "File lists and activity-instance IDs may require reading an activity page.".into(),
            ],
        })
    }
    async fn outline_html(&self, course_id: u64) -> Result<Outline> {
        let response = self
            .http
            .get(self.http.url(&format!("course/view.php?id={course_id}"))?)
            .await?;
        let text = response.text()?;
        let html = scraper::Html::parse_document(&text);
        let mut activities = BTreeMap::new();
        for link in html.select(&content::selector(
            "#region-main a[href], [role=main] a[href]",
        )) {
            let Some(url) = link
                .value()
                .attr("href")
                .and_then(|s| response.url.join(s).ok())
            else {
                continue;
            };
            if url.origin() != self.profile.url.origin() {
                continue;
            }
            let relative = url
                .path()
                .strip_prefix(self.profile.url.path())
                .unwrap_or_default();
            let components: Vec<_> = relative.split('/').collect();
            if components.len() != 3 || components[0] != "mod" || components[2] != "view.php" {
                continue;
            }
            let Some(cmid) = query_id(&url, "id") else {
                continue;
            };
            let title = link.text().collect::<Vec<_>>().join(" ").trim().to_string();
            activities.entry(cmid).or_insert(Activity {
                module_id: cmid,
                course_id,
                instance_id: None,
                kind: components[1].into(),
                accessible: true,
                title,
                section_id: None,
                source_url: content::safe_url(&url),
                description: String::new(),
                availability: None,
                completion: None,
                files: vec![],
            });
        }
        if activities.is_empty() {
            return Err(Error::new(
                "unsupported_course_format",
                "This course format exposed no readable activity links. Open the source URL in Moodle.",
            ));
        }
        Ok(Outline{course_id,sections:vec![],activities:activities.into_values().collect(),transport:"html".into(),observed_at:chrono::Utc::now().to_rfc3339(),warnings:vec!["Course-format AJAX unavailable. HTML fallback cannot guarantee complete section/restriction metadata.".into()]})
    }
    pub async fn activity(&self, course_id: u64, module_id: u64) -> Result<Activity> {
        self.outline(course_id)
            .await?
            .activities
            .into_iter()
            .find(|a| a.module_id == module_id)
            .ok_or_else(|| {
                Error::new(
                    "activity_not_found",
                    "That activity is not visible in this course.",
                )
            })
    }
    pub async fn activity_page(&self, activity: &Activity) -> Result<Response> {
        if !activity.accessible {
            return Err(Error::new(
                "activity_locked",
                activity.availability.clone().unwrap_or_else(|| {
                    "This activity is currently unavailable according to Moodle restrictions."
                        .into()
                }),
            ));
        }
        if self.has_token() && matches!(activity.kind.as_str(), "page" | "assign" | "quiz") {
            let function = match activity.kind.as_str() {
                "page" => "mod_page_get_pages_by_courses",
                "assign" => "mod_assign_get_assignments",
                _ => "mod_quiz_get_quizzes_by_courses",
            };
            match self
                .rest(function, json!({"courseids":[activity.course_id]}))
                .await
            {
                Ok(raw) => {
                    let entries = if activity.kind == "assign" {
                        raw["courses"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .flat_map(|c| c["assignments"].as_array().into_iter().flatten())
                            .collect::<Vec<_>>()
                    } else {
                        raw[if activity.kind == "page" {
                            "pages"
                        } else {
                            "quizzes"
                        }]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                    };
                    if let Some(item) = entries
                        .into_iter()
                        .find(|v| v["id"].as_u64() == activity.instance_id)
                    {
                        let body = format!(
                            "<main role=\"main\"><h1>{}</h1>{}{}</main>",
                            html_escape::encode_text(&activity.title),
                            item["intro"].as_str().unwrap_or_default(),
                            item["content"].as_str().unwrap_or_default()
                        );
                        return Ok(Response {
                            url: Url::parse(&activity.source_url)
                                .map_err(|_| Error::invalid("Invalid activity URL."))?,
                            content_type: "text/html; charset=utf-8".into(),
                            filename: format!("{}.html", activity.module_id),
                            body: body.into_bytes(),
                        });
                    }
                }
                Err(e) if e.code == "function_unavailable" => (),
                Err(e) => return Err(e),
            }
        }
        if activity.source_url.is_empty() {
            return Err(Error::unsupported("This activity has no readable page."));
        }
        if !matches!(
            activity.kind.as_str(),
            "resource"
                | "page"
                | "folder"
                | "book"
                | "assign"
                | "quiz"
                | "forum"
                | "url"
                | "label"
                | "choice"
                | "feedback"
                | "lesson"
                | "workshop"
                | "wiki"
                | "data"
                | "h5pactivity"
                | "scorm"
        ) {
            return Err(Error::unsupported(format!(
                "The {} plugin has no verified typed reader. Use moodle_browser_open for its first-party interface.",
                activity.kind
            )));
        }
        self.http
            .get(
                Url::parse(&activity.source_url)
                    .map_err(|_| Error::invalid("Invalid activity URL."))?,
            )
            .await
    }
    pub async fn activity_files(&self, activity: &Activity) -> Result<Vec<File>> {
        if !activity.files.is_empty() {
            return Ok(activity.files.clone());
        }
        let response = self.activity_page(activity).await?;
        if !response.content_type.contains("html") {
            return Ok(vec![File {
                index: 0,
                name: response.filename,
                mime: response.content_type,
                size: Some(response.body.len() as u64),
                source_url: content::safe_url(&response.url),
            }]);
        }
        let mut files = Vec::new();
        for link in content::links(&response.text()?, &response.url) {
            let url = Url::parse(&link.url).map_err(|_| Error::invalid("Invalid file URL."))?;
            if self.http.check_url(&url).is_err() || !url.path().contains("pluginfile.php") {
                continue;
            }
            if url.path().contains("/theme_") || url.path().contains("/user/icon/") {
                continue;
            }
            if files.iter().any(|f: &File| f.source_url == link.url) {
                continue;
            }
            let filename = url
                .path_segments()
                .and_then(|mut s| s.next_back())
                .unwrap_or("file")
                .to_string();
            files.push(File {
                index: files.len(),
                name: if link.title.is_empty() {
                    filename
                } else {
                    link.title
                },
                mime: String::new(),
                size: None,
                source_url: link.url,
            });
        }
        Ok(files)
    }

    pub async fn forum_post_target(&self, post_id: u64) -> Result<Value> {
        if self.has_token() {
            let raw = self
                .rest("mod_forum_get_discussion_post", json!({"postid":post_id}))
                .await?;
            let discussion = raw["post"]["discussionid"]
                .as_u64()
                .or_else(|| raw["post"]["discussion"].as_u64())
                .ok_or_else(|| {
                    Error::new(
                        "invalid_response",
                        "Forum post had no discussion identifier.",
                    )
                })?;
            let data = self
                .rest(
                    "mod_forum_get_discussion_posts",
                    json!({"discussionid":discussion}),
                )
                .await?;
            Ok(
                json!({"discussion_id":discussion,"forum_id":data["forumid"],"course_id":data["courseid"],"post_id":post_id}),
            )
        } else {
            Err(Error::unsupported(
                "Forum reply target verification currently requires mobile service access.",
            ))
        }
    }
    pub async fn file_response(&self, file: &File) -> Result<Response> {
        let mut url =
            Url::parse(&file.source_url).map_err(|_| Error::invalid("Invalid Moodle file URL."))?;
        if url.path().contains("/webservice/pluginfile.php")
            && let Some(token) = self.credentials.token.as_deref()
        {
            url.query_pairs_mut().append_pair("token", token);
        }
        self.http.get(url).await
    }
    pub async fn deadlines(
        &self,
        from: i64,
        to: i64,
        course_id: Option<u64>,
        limit: usize,
        after_event: Option<u64>,
    ) -> Result<Value> {
        if to <= from || to - from > 366 * 86400 || !(1..=100).contains(&limit) {
            return Err(Error::invalid(
                "Use a positive date range up to 366 days and limit between 1 and 100.",
            ));
        }
        let mut args = json!({"timesortfrom":from,"timesortto":to,"limitnum":limit});
        if let Some(id) = after_event {
            args["aftereventid"] = json!(id);
        }
        let raw = if let Some(id) = course_id {
            args["courseid"] = json!(id);
            self.read_call("core_calendar_get_action_events_by_course", args)
                .await?
        } else {
            self.read_call("core_calendar_get_action_events_by_timesort", args)
                .await?
        };
        let events = raw["events"]
            .as_array()
            .ok_or_else(|| Error::new("invalid_response", "Calendar response has no events."))?;
        let data:Vec<_>=events.iter().map(|e|json!({"id":e["id"],"title":plain(&e["name"]),"description":plain(&e["description"]),"course_id":e["course"]["id"],"activity_type":e["modulename"],"event_type":e["eventtype"],"time_start":timestamp(&e["timestart"]),"sort_time":e["timesort"],"source_url":clean_url_value(e.get("url").unwrap_or(&e["action"]["url"])),"action":plain(&e["action"]["name"])})).collect();
        let next = if events.len() == limit {
            events
                .last()
                .map(|e| json!({"from":e["timesort"],"until":to,"after_event_id":e["id"]}))
        } else {
            None
        };
        Ok(
            json!({"events":data,"next_cursor":next,"observed_at":chrono::Utc::now().to_rfc3339(),"source_url":self.source("calendar/view.php"),"scope":"Moodle action calendar; activities without calendar events may have additional restrictions or dates. Read the activity for authoritative personal details."}),
        )
    }
    pub async fn grades(&self, course_id: u64) -> Result<Value> {
        if self.has_token() {
            let user = self.profile.user_id.ok_or_else(Error::auth)?;
            match self
                .rest(
                    "gradereport_user_get_grade_items",
                    json!({"courseid":course_id,"userid":user}),
                )
                .await
            {
                Ok(mut raw) => {
                    sanitise(&mut raw);
                    return Ok(
                        json!({"data":raw,"transport":"rest","observed_at":chrono::Utc::now().to_rfc3339(),"source_url":self.source(&format!("grade/report/user/index.php?id={course_id}"))}),
                    );
                }
                Err(e) if e.code == "function_unavailable" => (),
                Err(e) => return Err(e),
            }
        }
        let response = self
            .http
            .get(
                self.http
                    .url(&format!("grade/report/user/index.php?id={course_id}"))?,
            )
            .await?;
        let text = content::html_text(&response.text()?, Some(&response.url));
        Ok(
            json!({"text":text,"transport":"html","observed_at":chrono::Utc::now().to_rfc3339(),"source_url":content::safe_url(&response.url)}),
        )
    }
    pub async fn activity_details(&self, mut activity: Activity) -> Result<Value> {
        if !activity.accessible {
            return Ok(
                json!({"activity":activity,"text":"","details":null,"supported_operations":[],"warnings":["This activity is visible in the outline but Moodle currently restricts access."],"observed_at":chrono::Utc::now().to_rfc3339()}),
            );
        }
        let mut warnings = Vec::new();
        let response = match self.activity_page(&activity).await {
            Ok(response) => response,
            Err(error) if error.code == "unsupported" => {
                return Ok(
                    json!({"activity":activity,"text":"","details":null,"reader_supported":false,"next_step":"Use moodle_browser_open with this course_id and module_id to inspect and interact with the plugin UI.","warnings":[error.message],"observed_at":chrono::Utc::now().to_rfc3339()}),
                );
            }
            Err(error) => return Err(error),
        };
        let text = if response.content_type.contains("html") {
            content::html_text(&response.text()?, Some(&response.url))
        } else {
            String::new()
        };
        if activity.instance_id.is_none() && response.content_type.contains("html") {
            let source = response.text()?;
            let html = scraper::Html::parse_document(&source);
            for el in html.select(&content::selector(
                "input[name=assign], input[name=quiz], input[name=forum], input[name=instance]",
            )) {
                if let Some(id) = el.value().attr("value").and_then(|s| s.parse().ok()) {
                    activity.instance_id = Some(id);
                    break;
                }
            }
            // Assignment links expose instance IDs through ?a=; quiz links through ?q=.
            if activity.instance_id.is_none() {
                let key = if activity.kind == "assign" {
                    "a"
                } else if activity.kind == "quiz" {
                    "q"
                } else {
                    "f"
                };
                activity.instance_id = content::links(&source, &response.url)
                    .iter()
                    .filter_map(|l| Url::parse(&l.url).ok())
                    .filter(|u| u.path() == response.url.path())
                    .find_map(|u| query_id(&u, key));
            }
        }
        let files = self.activity_files(&activity).await.unwrap_or_else(|e| {
            warnings.push(e.message);
            Vec::new()
        });
        activity.files = files;
        let mut details = Value::Null;
        if self.has_token()
            && activity.kind == "assign"
            && let Some(id) = activity.instance_id
        {
            match self
                .rest("mod_assign_get_submission_status", json!({"assignid":id}))
                .await
            {
                Ok(mut v) => {
                    sanitise(&mut v);
                    details = v;
                }
                Err(e) => warnings.push(e.message),
            }
        }
        if self.has_token()
            && activity.kind == "quiz"
            && let Some(id) = activity.instance_id
        {
            match self
                .rest(
                    "mod_quiz_get_user_attempts",
                    json!({"quizid":id,"status":"all"}),
                )
                .await
            {
                Ok(mut v) => {
                    sanitise(&mut v);
                    details = v;
                }
                Err(e) => warnings.push(e.message),
            }
        }
        Ok(
            json!({"activity":activity,"text":text.chars().take(25_000).collect::<String>(),"text_truncated":text.chars().count()>25_000,"details":details,"warnings":warnings,"observed_at":chrono::Utc::now().to_rfc3339()}),
        )
    }
    pub async fn forum(&self, course_id: u64, module_id: u64, page: u64) -> Result<Value> {
        let activity = self.activity(course_id, module_id).await?;
        if activity.kind != "forum" {
            return Err(Error::invalid("Select a forum activity."));
        }
        if self.has_token() {
            let id = activity
                .instance_id
                .ok_or_else(|| Error::unsupported("Forum instance ID is unavailable."))?;
            let mut value = self
                .rest(
                    "mod_forum_get_forum_discussions",
                    json!({"forumid":id,"page":page,"perpage":20}),
                )
                .await?;
            sanitise(&mut value);
            return Ok(
                json!({"data":value,"source_url":activity.source_url,"observed_at":chrono::Utc::now().to_rfc3339()}),
            );
        }
        let mut url =
            Url::parse(&activity.source_url).map_err(|_| Error::invalid("Invalid forum URL."))?;
        url.query_pairs_mut().append_pair("page", &page.to_string());
        let response = self.http.get(url).await?;
        Ok(
            json!({"text":content::html_text(&response.text()?,Some(&response.url)),"links":content::links(&response.text()?,&response.url),"source_url":content::safe_url(&response.url),"transport":"html","observed_at":chrono::Utc::now().to_rfc3339()}),
        )
    }
    pub async fn discussion(&self, discussion_id: u64) -> Result<Value> {
        if self.has_token() {
            let mut value = self
                .rest(
                    "mod_forum_get_discussion_posts",
                    json!({"discussionid":discussion_id}),
                )
                .await?;
            sanitise(&mut value);
            return Ok(json!({"data":value,"observed_at":chrono::Utc::now().to_rfc3339()}));
        }
        let response = self
            .http
            .get(
                self.http
                    .url(&format!("mod/forum/discuss.php?d={discussion_id}"))?,
            )
            .await?;
        Ok(
            json!({"text":content::html_text(&response.text()?,Some(&response.url)),"source_url":content::safe_url(&response.url),"observed_at":chrono::Utc::now().to_rfc3339()}),
        )
    }
    pub async fn messages(
        &self,
        conversation: Option<u64>,
        offset: u64,
        limit: u64,
    ) -> Result<Value> {
        if !(1..=50).contains(&limit) {
            return Err(Error::invalid("limit must be between 1 and 50."));
        }
        let user=self.profile.user_id.ok_or_else(||Error::unsupported("This Moodle session does not expose your user ID. Mobile token login may enable messaging."))?;
        let mut raw = if let Some(id) = conversation {
            self.read_call(
                "core_message_get_conversation_messages",
                json!({"currentuserid":user,"convid":id,"limitfrom":offset,"limitnum":limit}),
            )
            .await?
        } else {
            self.read_call(
                "core_message_get_conversations",
                json!({"userid":user,"limitfrom":offset,"limitnum":limit}),
            )
            .await?
        };
        sanitise(&mut raw);
        Ok(json!({"data":raw,"observed_at":chrono::Utc::now().to_rfc3339()}))
    }
    pub async fn notifications(&self, offset: u64, limit: u64) -> Result<Value> {
        if !(1..=50).contains(&limit) {
            return Err(Error::invalid("limit must be between 1 and 50."));
        }
        let user = self
            .profile
            .user_id
            .ok_or_else(|| Error::unsupported("Moodle has not exposed your user ID."))?;
        let mut raw = self
            .read_call(
                "core_message_get_popup_notifications",
                json!({"useridto":user,"limitfrom":offset,"limitnum":limit}),
            )
            .await?;
        sanitise(&mut raw);
        Ok(json!({"data":raw,"observed_at":chrono::Utc::now().to_rfc3339()}))
    }
    pub async fn quiz_attempt(&self, attempt_id: u64, page: u64, review: bool) -> Result<Value> {
        let function = if review {
            "mod_quiz_get_attempt_review"
        } else {
            "mod_quiz_get_attempt_data"
        };
        if self.has_token() {
            let mut raw = self
                .rest(function, json!({"attemptid":attempt_id,"page":page}))
                .await?;
            normalise_questions(&mut raw, &self.profile.url);
            sanitise(&mut raw);
            return Ok(
                json!({"data":raw,"observed_at":chrono::Utc::now().to_rfc3339(),"side_effects":"Reading an existing attempt does not start or submit an attempt."}),
            );
        }
        let path = if review { "review.php" } else { "attempt.php" };
        let response = self
            .http
            .get(
                self.http
                    .url(&format!("mod/quiz/{path}?attempt={attempt_id}&page={page}"))?,
            )
            .await?;
        let source = response.text()?;
        let html = scraper::Html::parse_document(&source);
        let questions:Vec<_>=html.select(&content::selector(".que")).map(|q|json!({"reference":q.value().attr("id"),"text":content::html_text(&q.html(),Some(&response.url)),"type":q.value().attr("class"),"fields":q.select(&content::selector("input:not([type=hidden]),textarea,select")).filter_map(|f|f.value().attr("name").map(|name|json!({"name":name,"type":f.value().attr("type").unwrap_or(f.value().name()),"value":f.value().attr("value")}))).collect::<Vec<_>>()})).collect();
        Ok(
            json!({"questions":questions,"source_url":content::safe_url(&response.url),"page":page,"observed_at":chrono::Utc::now().to_rfc3339(),"transport":"html"}),
        )
    }
    pub async fn upload_draft(
        &self,
        filename: String,
        bytes: Vec<u8>,
        item_id: Option<u64>,
    ) -> Result<u64> {
        if self.has_token() {
            let fields = vec![
                (
                    "token".into(),
                    self.credentials.token.clone().unwrap_or_default(),
                ),
                ("filepath".into(), "/".into()),
                ("itemid".into(), item_id.unwrap_or(0).to_string()),
            ];
            let raw = self
                .http
                .upload(
                    self.http.url("webservice/upload.php")?,
                    fields,
                    filename,
                    bytes,
                )
                .await?;
            if raw.get("exception").is_some() {
                return Err(api_error(&raw));
            }
            raw.as_array()
                .and_then(|a| a.first())
                .and_then(|v| number(&v["itemid"]))
                .ok_or_else(|| {
                    Error::new(
                        "upload_failed",
                        "Moodle did not return a draft-file identifier.",
                    )
                })
        } else {
            Err(Error::unsupported(
                "File upload requires the mobile service on this release; session file-manager repository discovery is not yet verified.",
            ))
        }
    }
}

pub fn flatten_form(prefix: &str, value: &Value, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let name = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}[{key}]")
                };
                flatten_form(&name, value, out);
            }
        }
        Value::Array(list) => {
            for (index, value) in list.iter().enumerate() {
                flatten_form(&format!("{prefix}[{index}]"), value, out);
            }
        }
        Value::Null => (),
        Value::Bool(v) => out.push((prefix.into(), if *v { "1" } else { "0" }.into())),
        Value::String(v) => out.push((prefix.into(), v.clone())),
        value => out.push((prefix.into(), value.to_string())),
    }
}
pub fn api_error(value: &Value) -> Error {
    let code = value["errorcode"].as_str().unwrap_or("moodle_error");
    match code {
        "invalidtoken"
        | "servicerequireslogin"
        | "requireloginerror"
        | "invalidsesskey"
        | "notloggedin" => Error::auth(),
        "accessexception" | "wsfunctionnotavailable" | "invalidfunction" | "missingfunc" => {
            Error::new(
                "function_unavailable",
                "This function is not enabled for your Moodle access route.",
            )
        }
        "nopermissions" | "cannotview" | "requirecapability" => Error::new(
            "permission_denied",
            "Your Moodle account is not allowed to perform this operation.",
        ),
        _ => Error::new(
            "moodle_error",
            format!(
                "Moodle rejected this operation ({code}). Open the activity in Moodle for its full explanation."
            ),
        ),
    }
}
pub fn extract_sesskey(source: &str) -> Option<String> {
    let html = scraper::Html::parse_document(source);
    for a in html.select(&content::selector("a[href]")) {
        let href = a.value().attr("href")?;
        if href.contains("/login/logout.php")
            && let Ok(url) = Url::parse(href)
            && let Some((_, key)) = url.query_pairs().find(|(k, _)| k == "sesskey")
        {
            return Some(key.into_owned());
        }
    }
    None
}
pub fn plain(value: &Value) -> String {
    content::html_text(value.as_str().unwrap_or_default(), None)
}
pub fn number(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}
pub fn query_id(url: &Url, key: &str) -> Option<u64> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.parse().ok())
}
pub fn timestamp(value: &Value) -> Option<String> {
    value
        .as_i64()
        .filter(|n| *n > 0)
        .and_then(|n| chrono::DateTime::from_timestamp(n, 0))
        .map(|t| t.to_rfc3339())
}
pub fn clean_url_value(value: &Value) -> Value {
    value
        .as_str()
        .and_then(|s| Url::parse(s).ok())
        .map(|u| json!(content::safe_url(&u)))
        .unwrap_or(Value::Null)
}
pub fn sanitise(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for key in ["token", "wstoken", "sesskey", "privatetoken", "password"] {
                map.remove(key);
            }
            for v in map.values_mut() {
                sanitise(v);
            }
        }
        Value::Array(list) => {
            for v in list {
                sanitise(v);
            }
        }
        Value::String(s) => {
            if s.contains('<') && s.contains('>') {
                *s = content::html_text(s, None);
            }
            if let Ok(url) = Url::parse(s)
                && matches!(url.scheme(), "http" | "https")
            {
                *s = content::safe_url(&url);
            }
        }
        _ => (),
    }
}

pub fn normalise_questions(value: &mut Value, base: &Url) {
    for q in value["questions"].as_array_mut().into_iter().flatten() {
        if let Some(source) = q["html"].as_str().map(str::to_string) {
            let html = scraper::Html::parse_document(&source);
            let fields:Vec<_>=html.select(&content::selector("input[name],textarea[name],select[name]")).filter_map(|e|{
                let name=e.value().attr("name")?;
                if !name.starts_with('q') || name.contains("_:sequencecheck") || name.contains("_:flagged") || name.ends_with("_-submit") || name.ends_with("_-check"){return None;}
                Some(json!({"name":name,"type":e.value().attr("type").unwrap_or(e.value().name()),"value":e.value().attr("value").map(str::to_string).unwrap_or_else(||e.text().collect::<Vec<_>>().join("")),"checked":e.value().attr("checked").is_some(),"options":e.select(&content::selector("option")).map(|o|json!({"value":o.value().attr("value"),"label":o.text().collect::<Vec<_>>().join(""),"selected":o.value().attr("selected").is_some()})).collect::<Vec<_>>()}))
            }).collect();
            q["text"] = json!(content::html_text(&source, Some(base)));
            q["fields"] = json!(fields);
            if let Some(o) = q.as_object_mut() {
                o.remove("html");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rest_arrays_use_moodle_bracket_encoding() {
        let mut out = vec![];
        flatten_form(
            "",
            &json!({"courseids":[42,7],"options":[{"name":"a","value":true}]}),
            &mut out,
        );
        assert!(out.contains(&("courseids[0]".into(), "42".into())));
        assert!(out.contains(&("options[0][value]".into(), "1".into())));
    }
    #[test]
    fn strips_secrets_from_structured_urls() {
        let mut v = json!({"token":"secret","file":"https://secret:secret@example.edu/file?token=secret&forcedownload=1"});
        sanitise(&mut v);
        assert!(!v.to_string().contains("secret"));
        assert!(v["file"].as_str().unwrap().contains("forcedownload"));
    }
    #[tokio::test]
    async fn locked_activities_keep_metadata_without_fetching_and_custom_plugins_offer_browser() {
        let moodle = Moodle::new(
            Profile {
                id: uuid::Uuid::new_v4().to_string(),
                name: "test".into(),
                url: Url::parse("https://example.invalid/").unwrap(),
                user_id: Some(3),
                display_name: None,
                storage: crate::config::CredentialStorage::File,
                allow_http: false,
                authenticated_at: chrono::Utc::now().to_rfc3339(),
            },
            Credentials::default(),
        )
        .unwrap();
        let outline = moodle.normalise_contents(2, &json!([{"id":1,"name":"Week 1","modules":[{"id":9,"instance":4,"modname":"customplugin","name":"Restricted activity","uservisible":false,"url":"https://example.invalid/mod/customplugin/view.php?id=9"}]}])).unwrap();
        let mut activity = outline.activities[0].clone();
        assert!(!activity.accessible);
        assert_eq!(
            moodle.activity_page(&activity).await.err().unwrap().code,
            "activity_locked"
        );
        let locked = moodle.activity_details(activity.clone()).await.unwrap();
        assert_eq!(locked["activity"]["module_id"], 9);
        assert_eq!(locked["supported_operations"], json!([]));
        activity.accessible = true;
        let plugin = moodle.activity_details(activity).await.unwrap();
        assert_eq!(plugin["reader_supported"], false);
        assert!(
            plugin["next_step"]
                .as_str()
                .unwrap()
                .contains("moodle_browser_open")
        );
    }
    #[test]
    fn quiz_html_is_structured_without_flag_credentials_or_scripts() {
        let mut value = json!({"questions":[{"slot":1,"html":"<div class='que'><input type='hidden' value='sesskey=secret'><script>window.secret='secret';</script><p>What is a gradient?</p><input type='text' name='q1:1_answer' value='direction'></div>"}]});
        normalise_questions(&mut value, &Url::parse("https://example.edu/").unwrap());
        sanitise(&mut value);
        assert!(!value.to_string().contains("secret"));
        assert!(value["questions"][0].get("html").is_none());
        assert_eq!(value["questions"][0]["fields"][0]["name"], "q1:1_answer");
        assert_eq!(value["questions"][0]["fields"][0]["value"], "direction");
    }
}
