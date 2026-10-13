//! First-party Moodle UI compatibility path. Only fixed DOM operations are exposed.
use chromiumoxide::{
    Browser, BrowserConfig, Page,
    cdp::browser_protocol::{network::CookieParam, page::CaptureScreenshotFormat},
};
use futures::StreamExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Duration};

use crate::{
    config::{Credentials, Home, Profile, private_dir},
    content,
    error::{Error, Result},
    store::Store,
};

const SNAPSHOT_JS: &str = r#"() => {
    if (document.body?.classList.contains('notloggedin') || document.querySelector('input[type=password][name=password]')) return {login_required:true};
    window.__moocpNodes ||= new Map(); window.__moocpNext ||= 1;
    const nodes=[]; const frames=[];
    const scan=(doc, ox=0, oy=0, depth=0) => {
        if(depth>4||nodes.length>=400)return;
        for(const e of doc.querySelectorAll('a[href],button,input:not([type=hidden]),textarea,select,[role=button],[contenteditable=true],canvas')) {
            const r=e.getBoundingClientRect();if(e.type!=='file'&&(!r.width||!r.height||getComputedStyle(e).visibility==='hidden'))continue;
            const known=[...window.__moocpNodes].find(([k,v])=>v.e===e);
            const ref=known?known[0]:'e'+window.__moocpNext++;
            window.__moocpNodes.set(ref,{e,ox,oy});
            const type=e.type||e.getAttribute('role')||e.tagName.toLowerCase();
            const label=e.labels?.[0]?.innerText||e.getAttribute('aria-label')||e.getAttribute('placeholder')||e.innerText||(type==='password'?'':e.value)||e.title||e.name||type;
            nodes.push({ref,type,label:String(label).slice(0,400),name:e.name||null,
                value:type==='password'?null:(e.value??null),checked:e.checked??null,disabled:!!e.disabled,
                href:e.tagName==='A'?e.href:null,x:Math.round(r.x+ox+r.width/2),y:Math.round(r.y+oy+r.height/2),
                options:e.tagName==='SELECT'?[...e.options].map(o=>({value:o.value,label:o.text,selected:o.selected})):null});
            if(nodes.length>=400)break;
        }
        for(const frame of doc.querySelectorAll('iframe,frame')) {
            const r=frame.getBoundingClientRect();
            try {if(frame.contentDocument){scan(frame.contentDocument,ox+r.x,oy+r.y,depth+1);}else{frames.push({src:frame.src,accessible:false});}}
            catch {frames.push({src:frame.src,accessible:false});}
        }
    };
    scan(document);
    for(const [ref,node] of window.__moocpNodes){if(!node.e.isConnected)window.__moocpNodes.delete(ref);}
    const scope=document.querySelector('#region-main,[role=main]')||document.body;
    const stable=scope?.cloneNode(true);
    stable?.querySelectorAll('script,style,nav,header,footer,.timer,.timeremaining,[role=timer],#quiz-time-left,#quiz-timer-wrapper,#lesson-timer').forEach(e=>e.remove());
    return {title:document.title,url:location.href,user_id:Number(window.M?.cfg?.userId||document.body?.dataset.userid||0),text:(scope?.innerText||'').slice(0,20000),nodes,frames,
        stable_text:(stable?.textContent||'').replace(/\s+/g,' ').slice(0,20000),
        viewport:{width:innerWidth,height:innerHeight},limits:{max_nodes:400,max_text_chars:20000}};
}"#;

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    /// Click an element reference from the most recent browser snapshot.
    Click { reference: String },
    /// Set an ordinary input, textarea, or rich text field. Password fields are excluded.
    Fill { reference: String, text: String },
    Select {
        reference: String,
        values: Vec<String>,
    },
    /// Attach explicitly chosen local files to a Moodle file input after review.
    AttachFiles {
        reference: String,
        files: Vec<String>,
    },
    /// Keyboard navigation or activation after focusing the referenced element.
    PressKey { reference: String, key: String },
    /// For canvas/visual interfaces: coordinates must be inside the captured viewport.
    ClickAt { x: u32, y: u32 },
    /// Drag within a canvas or other visual interface, using screenshot coordinates.
    Drag {
        from_x: u32,
        from_y: u32,
        to_x: u32,
        to_y: u32,
    },
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Prepare {
    pub session_id: String,
    pub snapshot_id: String,
    /// Up to twelve interactions with elements already visible in this snapshot.
    pub steps: Vec<Step>,
    /// Explain the intended student outcome, e.g. "save my lesson response".
    pub purpose: String,
}

#[derive(Serialize, Deserialize)]
struct Payload {
    browser: bool,
    session_id: String,
    snapshot_id: String,
    steps: Vec<Step>,
    #[serde(default)]
    staged_files: Vec<StagedFile>,
}

#[derive(Serialize, Deserialize)]
struct StagedFile {
    path: String,
    name: String,
    sha256: String,
    size: usize,
}

pub struct Controller {
    browser: Browser,
    page: Page,
    driver: tokio::task::JoinHandle<()>,
    directory: PathBuf,
    profile: Profile,
    credential_tag: String,
    pub session_id: String,
    pub last_snapshot: Option<Value>,
    known_tabs: std::collections::HashSet<String>,
}

fn cdp_error(_: impl std::fmt::Display) -> Error {
    Error::new(
        "browser_interaction",
        "Browser interaction could not be completed. Take a new snapshot and inspect the visible page before continuing.",
    )
}
fn digest(value: &Value) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).unwrap_or_default())
    )
}
fn credential_tag(credentials: &Credentials) -> Result<String> {
    Ok(digest(&serde_json::to_value(credentials)?))
}

impl Controller {
    pub async fn launch(
        home: &Home,
        profile: Profile,
        credentials: Credentials,
        url: url::Url,
        headless: bool,
    ) -> Result<Self> {
        if credentials.cookies.is_empty() {
            return Err(Error::new(
                "browser_login_required",
                "The compatibility browser needs a Moodle browser session. Run moocp login.",
            ));
        }
        let directory = home.path.join(format!("browser-{}", uuid::Uuid::new_v4()));
        private_dir(&directory)?;
        let mut builder = BrowserConfig::builder()
            .user_data_dir(&directory)
            .respect_https_errors()
            .arg("remote-debugging-address=127.0.0.1")
            .request_timeout(Duration::from_secs(15));
        if !headless {
            builder = builder.with_head();
        }
        let config = builder.build().map_err(|_| {
            Error::new(
                "browser_missing",
                "Install a Chromium-family browser for UI compatibility tooling.",
            )
        })?;
        let (browser, mut handler) = Browser::launch(config).await.map_err(cdp_error)?;
        let driver = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if event.is_err() {
                    break;
                }
            }
        });
        let params:Vec<CookieParam>=credentials.cookies.iter().filter(|c|c.expires.is_none_or(|t|t<0.0||t>chrono::Utc::now().timestamp() as f64)).map(|c|{
            let mut value=json!({"name":c.name,"value":c.value,"domain":c.domain,"path":c.path,"secure":c.secure});
            if let Some(expires)=c.expires.filter(|e|*e>0.0){value["expires"]=json!(expires);}
            serde_json::from_value(value).map_err(Error::from)
        }).collect::<Result<_>>()?;
        browser.set_cookies(params).await.map_err(cdp_error)?;
        let page = browser.new_page("about:blank").await.map_err(cdp_error)?;
        let known_tabs = browser
            .pages()
            .await
            .map_err(cdp_error)?
            .iter()
            .map(|p| p.target_id().as_ref().to_owned())
            .collect();
        let mut controller = Self {
            browser,
            page,
            driver,
            directory,
            profile,
            credential_tag: credential_tag(&credentials)?,
            session_id: uuid::Uuid::new_v4().to_string(),
            last_snapshot: None,
            known_tabs,
        };
        controller.check_url(&url)?;
        let outcome = async {
            controller
                .page
                .goto(url.as_str())
                .await
                .map_err(cdp_error)?;
            controller.snapshot().await
        }
        .await;
        if let Err(error) = outcome {
            controller.close().await;
            return Err(error);
        }
        Ok(controller)
    }
    pub fn check_account(&self, profile: &Profile, credentials: &Credentials) -> Result<()> {
        if self.profile.id != profile.id || self.credential_tag != credential_tag(credentials)? {
            return Err(Error::new(
                "browser_account_changed",
                "Moodle login changed. Close and reopen the compatibility browser before interacting.",
            ));
        }
        Ok(())
    }
    pub async fn tabs(&self) -> Result<Value> {
        let mut tabs = Vec::new();
        let mut external_count = 0;
        for page in self.browser.pages().await.map_err(cdp_error)? {
            if let Some(url) = page
                .url()
                .await
                .map_err(cdp_error)?
                .and_then(|u| url::Url::parse(&u).ok())
            {
                if self.check_url(&url).is_ok() {
                    tabs.push(json!({"tab_id":page.target_id().as_ref().to_owned(),"source_url":content::safe_url(&url),"active":page.target_id()==self.page.target_id()}));
                } else if matches!(url.scheme(), "https" | "http") {
                    external_count += 1;
                }
            }
        }
        Ok(json!({"tabs":tabs,"external_tabs_not_inspected":external_count}))
    }
    pub async fn switch_tab(&mut self, id: &str) -> Result<Value> {
        for page in self.browser.pages().await.map_err(cdp_error)? {
            if page.target_id().as_ref() == id {
                let url = page
                    .url()
                    .await
                    .map_err(cdp_error)?
                    .and_then(|s| url::Url::parse(&s).ok())
                    .ok_or_else(|| Error::invalid("This tab has no readable page."))?;
                self.check_url(&url)?;
                self.page = page;
                self.last_snapshot = None;
                return self.snapshot().await;
            }
        }
        Err(Error::new(
            "browser_tab_missing",
            "No Moodle tab with that identifier.",
        ))
    }
    async fn follow_popup(&mut self) -> Result<()> {
        for page in self.browser.pages().await.map_err(cdp_error)? {
            let id = page.target_id().as_ref().to_owned();
            if !self.known_tabs.contains(&id)
                && let Some(url) = page
                    .url()
                    .await
                    .map_err(cdp_error)?
                    .and_then(|s| url::Url::parse(&s).ok())
                && self.check_url(&url).is_ok()
            {
                self.known_tabs.insert(id);
                self.page = page;
                self.last_snapshot = None;
            }
        }
        Ok(())
    }
    fn check_url(&self, url: &url::Url) -> Result<()> {
        if url.origin() != self.profile.url.origin()
            || !url.path().starts_with(self.profile.url.path())
        {
            return Err(Error::new(
                "browser_external_origin",
                "This browser page is outside the connected Moodle site. Continue it yourself in the visible browser; Moocp will not inspect external login/providers.",
            ));
        }
        let relative = url
            .path()
            .strip_prefix(self.profile.url.path())
            .unwrap_or_default();
        if relative.starts_with("login/") || relative.starts_with("auth/") {
            return Err(Error::auth());
        }
        Ok(())
    }
    pub async fn snapshot(&mut self) -> Result<Value> {
        let url = self
            .page
            .url()
            .await
            .map_err(cdp_error)?
            .and_then(|s| url::Url::parse(&s).ok())
            .ok_or_else(|| Error::new("browser_page_missing", "No active Moodle browser page."))?;
        self.check_url(&url)?;
        let mut data = None;
        for _ in 0..30 {
            if let Ok(result) = self
                .page
                .evaluate(format!("JSON.stringify(({SNAPSHOT_JS})())"))
                .await
                && let Ok(text) = result.into_value::<String>()
            {
                data = serde_json::from_str::<Value>(&text).ok();
                if data.is_some() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let mut data = data.ok_or_else(|| {
            Error::new(
                "browser_page_loading",
                "The page is still changing. Take another snapshot shortly.",
            )
        })?;
        let observed_url = data["url"]
            .as_str()
            .and_then(|s| url::Url::parse(s).ok())
            .ok_or_else(Error::auth)?;
        self.check_url(&observed_url)?;
        if observed_url != url {
            return Err(Error::new(
                "browser_page_changed",
                "The browser navigated during observation. Take a fresh snapshot.",
            ));
        }
        if data["login_required"] == true {
            return Err(Error::auth());
        }
        if data["user_id"]
            .as_u64()
            .is_some_and(|id| id > 0 && self.profile.user_id.is_some_and(|expected| id != expected))
        {
            return Err(Error::new(
                "browser_account_changed",
                "The visible browser is signed in as another Moodle user. Reconnect the intended account.",
            ));
        }
        // Hash semantic controls, excluding countdowns, scripts, and credentials.
        let state = json!({"url":content::safe_url(&url),"nodes":data["nodes"],"frames":data["frames"],"stable_text":data["stable_text"]});
        data["snapshot_id"] = json!(digest(&state));
        data["session_id"] = json!(self.session_id);
        data["content_is_untrusted"] = json!(true);
        data["source_url"] = json!(content::safe_url(&url));
        data["observed_at"] = json!(chrono::Utc::now().to_rfc3339());
        if let Some(object) = data.as_object_mut() {
            object.remove("url");
            object.remove("stable_text");
        }
        for node in data["nodes"].as_array_mut().into_iter().flatten() {
            if let Some(href) = node["href"].as_str().and_then(|s| url::Url::parse(s).ok()) {
                node["href"] = json!(content::safe_url(&href));
            }
        }
        for frame in data["frames"].as_array_mut().into_iter().flatten() {
            if let Some(src) = frame["src"].as_str().and_then(|s| url::Url::parse(s).ok()) {
                frame["src"] = json!(content::safe_url(&src));
            }
        }
        self.last_snapshot = Some(data.clone());
        Ok(data)
    }
    pub async fn screenshot(&mut self) -> Result<Vec<u8>> {
        self.snapshot().await?;
        let url = self
            .page
            .url()
            .await
            .map_err(cdp_error)?
            .and_then(|s| url::Url::parse(&s).ok())
            .ok_or_else(Error::auth)?;
        self.check_url(&url)?;
        let png = self
            .page
            .screenshot(
                chromiumoxide::page::ScreenshotParams::builder()
                    .format(CaptureScreenshotFormat::Png)
                    .full_page(false)
                    .build(),
            )
            .await
            .map_err(cdp_error)?;
        let captured_url = self
            .page
            .url()
            .await
            .map_err(cdp_error)?
            .and_then(|s| url::Url::parse(&s).ok())
            .ok_or_else(Error::auth)?;
        self.check_url(&captured_url)?;
        if captured_url != url {
            return Err(Error::new(
                "browser_page_changed",
                "The browser navigated during capture. Take a fresh screenshot.",
            ));
        }
        if png.len() > 5_000_000 {
            return Err(Error::new(
                "size_limit",
                "Browser screenshot exceeds the 5 MB limit.",
            ));
        }
        Ok(png)
    }
    pub fn prepare(&self, home: &Home, mut args: Prepare) -> Result<Value> {
        if args.session_id != self.session_id {
            return Err(Error::new(
                "browser_session_changed",
                "This snapshot belongs to a different browser session.",
            ));
        }
        let snapshot = self.last_snapshot.as_ref().ok_or_else(|| {
            Error::new(
                "browser_snapshot_required",
                "Take a snapshot before preparing interactions.",
            )
        })?;
        if snapshot["snapshot_id"] != args.snapshot_id {
            return Err(Error::new(
                "browser_snapshot_changed",
                "Prepare from the most recent browser snapshot.",
            ));
        }
        if args.steps.is_empty()
            || args.steps.len() > 12
            || args.purpose.trim().is_empty()
            || args.purpose.len() > 2000
        {
            return Err(Error::invalid(
                "Provide a purpose and 1–12 browser interactions.",
            ));
        }
        let mut targets = Vec::new();
        for step in &args.steps {
            let reference = match step {
                Step::Click { reference }
                | Step::Fill { reference, .. }
                | Step::Select { reference, .. }
                | Step::PressKey { reference, .. }
                | Step::AttachFiles { reference, .. } => Some(reference),
                Step::ClickAt { x, y } => {
                    if *x as u64 >= snapshot["viewport"]["width"].as_u64().unwrap_or(0)
                        || *y as u64 >= snapshot["viewport"]["height"].as_u64().unwrap_or(0)
                    {
                        return Err(Error::invalid(
                            "Coordinates must be inside the captured viewport.",
                        ));
                    }
                    None
                }
                Step::Drag {
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                } => {
                    let width = snapshot["viewport"]["width"].as_u64().unwrap_or(0);
                    let height = snapshot["viewport"]["height"].as_u64().unwrap_or(0);
                    if *from_x as u64 >= width
                        || *to_x as u64 >= width
                        || *from_y as u64 >= height
                        || *to_y as u64 >= height
                    {
                        return Err(Error::invalid(
                            "Drag coordinates must be inside the viewport.",
                        ));
                    }
                    None
                }
            };
            if let Some(reference) = reference {
                if !valid_ref(reference) {
                    return Err(Error::invalid(
                        "Use an element reference returned by the browser snapshot.",
                    ));
                }
                let node = snapshot["nodes"]
                    .as_array()
                    .and_then(|nodes| nodes.iter().find(|n| n["ref"] == *reference))
                    .ok_or_else(|| {
                        Error::new(
                            "browser_reference_missing",
                            "That element is absent from the snapshot.",
                        )
                    })?;
                if node["type"] == "password"
                    || (node["type"] == "file" && !matches!(step, Step::AttachFiles { .. }))
                    || node["disabled"] == true
                {
                    return Err(Error::unsupported(
                        "Sensitive credentials and disabled controls require direct student interaction; file inputs require AttachFiles.",
                    ));
                }
                if matches!(step, Step::AttachFiles { .. }) && node["type"] != "file" {
                    return Err(Error::invalid(
                        "AttachFiles requires a file input reference.",
                    ));
                }
                if let Some(href) = node["href"].as_str()
                    && let Ok(url) = url::Url::parse(href)
                    && matches!(url.scheme(), "http" | "https")
                {
                    self.check_url(&url)?;
                }
                if let Step::Fill { text, .. } = step
                    && text.len() > 100_000
                {
                    return Err(Error::invalid("Browser input exceeds the 100 KB limit."));
                }
                if let Step::PressKey { key, .. } = step
                    && !matches!(
                        key.as_str(),
                        "Enter"
                            | "Tab"
                            | "Escape"
                            | "ArrowUp"
                            | "ArrowDown"
                            | "ArrowLeft"
                            | "ArrowRight"
                            | "Space"
                            | "Backspace"
                            | "Delete"
                            | "Home"
                            | "End"
                            | "PageUp"
                            | "PageDown"
                    )
                {
                    return Err(Error::invalid("Unsupported navigation key."));
                }
                targets.push(node.clone());
            }
        }
        let (staged_files, displayed_steps) =
            stage_uploads(home, &self.profile.id, &mut args.steps)?;
        let payload = Payload {
            browser: true,
            session_id: args.session_id,
            snapshot_id: args.snapshot_id,
            steps: args.steps.clone(),
            staged_files,
        };
        let preview = json!({"account":self.profile.display_name,"site":self.profile.url,"purpose":args.purpose,"source_url":snapshot["source_url"],"steps":displayed_steps,"targets":targets,"files":payload.staged_files.iter().map(|f|json!({"name":f.name,"sha256":f.sha256,"bytes":f.size})).collect::<Vec<_>>(),"effect":"Interact with Moodle's actual browser UI. Page scripts may save responses, start timers, upload files, publish, or submit. The exact interactions are reviewed; plugin-specific business outcomes must be checked on the resulting page."});
        Store::open(home)?.prepare(
            &self.profile.id,
            &serde_json::to_value(payload)?,
            &preview,
            &json!({"snapshot_id":snapshot["snapshot_id"]}),
        )
    }
    pub async fn execute(&mut self, home: &Home, id: &str) -> Result<Value> {
        let proposal = Store::open(home)?.proposal(id)?;
        if proposal["profile"] != self.profile.id {
            return Err(Error::new(
                "account_mismatch",
                "Browser proposal belongs to another account.",
            ));
        }
        if proposal["status"] == "succeeded" {
            return Ok(proposal["receipt"].clone());
        }
        let payload: Payload = serde_json::from_value(proposal["payload"].clone())?;
        if payload.session_id != self.session_id {
            return Err(Error::new(
                "browser_session_changed",
                "Reopened browser sessions cannot replay old interactions. Prepare a new action.",
            ));
        }
        let snapshot = self.snapshot().await?;
        if snapshot["snapshot_id"] != payload.snapshot_id {
            return Err(Error::new(
                "browser_snapshot_changed",
                "The page controls changed after preparation. Take a fresh snapshot and prepare a new action.",
            ));
        }
        Store::open(home)?.claim(&self.profile.id, id)?;
        let mut completed = 0;
        let outcome = async {
            for file in &payload.staged_files {
                use std::io::Read;
                let mut bytes = Vec::new();
                std::fs::File::open(&file.path)?
                    .take(crate::network::MAX_DOWNLOAD_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() != file.size
                    || format!("{:x}", Sha256::digest(&bytes)) != file.sha256
                {
                    return Err(Error::new(
                        "upload_snapshot_changed",
                        "A staged upload changed after review. Nothing further will be executed.",
                    ));
                }
            }
            for step in payload.steps {
                self.step(step).await?;
                completed += 1;
                tokio::time::sleep(Duration::from_millis(250)).await;
                self.follow_popup().await?;
            }
            self.snapshot().await
        }
        .await;
        match outcome {
            Ok(snapshot) => {
                let receipt = json!({"status":"succeeded","proposal_id":id,"action":"Reviewed browser interactions completed","completed_steps":completed,"completed_at":chrono::Utc::now().to_rfc3339(),"verification":{"method":"Observed browser state after interaction; not an application-specific Moodle API receipt","snapshot":snapshot}});
                Store::open(home)?.finish(id, "succeeded", &receipt)?;
                Ok(receipt)
            }
            Err(error) => {
                Store::open(home)?.finish(id,"uncertain",&json!({"error":error,"completed_steps":completed,"instruction":"Inspect the visible browser and Moodle state. These interactions cannot be blindly repeated."}))?;
                Err(error)
            }
        }
    }
    async fn step(&self, step: Step) -> Result<()> {
        match step {
            Step::AttachFiles { reference, files } => {
                use chromiumoxide::cdp::{
                    browser_protocol::dom::SetFileInputFilesParams,
                    js_protocol::runtime::EvaluateParams,
                };
                let expression = format!(
                    "(() => {{const e=window.__moocpNodes?.get({})?.e;if(!e?.isConnected||e.type!=='file')throw new Error('stale');return e;}})()",
                    serde_json::to_string(&reference)?
                );
                let evaluation = EvaluateParams::builder()
                    .expression(expression)
                    .return_by_value(false)
                    .build()
                    .map_err(cdp_error)?;
                let object = self
                    .page
                    .evaluate_expression(evaluation)
                    .await
                    .map_err(cdp_error)?;
                let id = object.object().object_id.clone().ok_or_else(|| {
                    Error::new(
                        "browser_reference_missing",
                        "File input is no longer present.",
                    )
                })?;
                let params = SetFileInputFilesParams::builder()
                    .files(files)
                    .object_id(id)
                    .build()
                    .map_err(cdp_error)?;
                self.page.execute(params).await.map_err(cdp_error)?;
            }
            Step::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
            } => {
                use chromiumoxide::cdp::browser_protocol::input::{
                    DispatchMouseEventParams, DispatchMouseEventType, MouseButton,
                };
                for i in 0..=12 {
                    let fraction = i as f64 / 12.0;
                    let x = from_x as f64 + (to_x as f64 - from_x as f64) * fraction;
                    let y = from_y as f64 + (to_y as f64 - from_y as f64) * fraction;
                    let kind = if i == 0 {
                        DispatchMouseEventType::MousePressed
                    } else if i == 12 {
                        DispatchMouseEventType::MouseReleased
                    } else {
                        DispatchMouseEventType::MouseMoved
                    };
                    let params = DispatchMouseEventParams::builder()
                        .r#type(kind)
                        .x(x)
                        .y(y)
                        .button(MouseButton::Left)
                        .buttons(if i == 12 { 0 } else { 1 })
                        .click_count(1)
                        .build()
                        .map_err(cdp_error)?;
                    self.page.execute(params).await.map_err(cdp_error)?;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            Step::ClickAt { x, y } => {
                self.page
                    .click(chromiumoxide::layout::Point::new(x as f64, y as f64))
                    .await
                    .map_err(cdp_error)?;
            }
            Step::Click { reference } => {
                let ref_json = serde_json::to_string(&reference)?;
                let point=self.page.evaluate(format!("(() => {{ const n=window.__moocpNodes?.get({ref_json}); if(!n?.e.isConnected)throw new Error('stale'); n.e.scrollIntoView({{block:'center'}}); const r=n.e.getBoundingClientRect();let ox=0,oy=0,w=n.e.ownerDocument.defaultView;while(w!==window){{const f=w.frameElement,fr=f.getBoundingClientRect();ox+=fr.x;oy+=fr.y;w=f.ownerDocument.defaultView;}}return {{x:r.x+ox+r.width/2,y:r.y+oy+r.height/2}};}})()")).await.map_err(cdp_error)?.into_value::<Value>()?;
                self.page
                    .click(chromiumoxide::layout::Point::new(
                        point["x"].as_f64().unwrap_or(0.0),
                        point["y"].as_f64().unwrap_or(0.0),
                    ))
                    .await
                    .map_err(cdp_error)?;
            }
            Step::Fill { reference, text } => {
                let values = serde_json::to_string(&json!({"reference":reference,"text":text}))?;
                self.page.evaluate(format!("((a) => {{ const e=window.__moocpNodes?.get(a.reference)?.e;if(!e?.isConnected||e.type==='password'||e.type==='file')throw new Error('unavailable');e.focus();if(e.isContentEditable)e.textContent=a.text;else{{const proto=e.tagName==='TEXTAREA'?HTMLTextAreaElement.prototype:HTMLInputElement.prototype;Object.getOwnPropertyDescriptor(proto,'value').set.call(e,a.text);}}e.dispatchEvent(new Event('input',{{bubbles:true}}));e.dispatchEvent(new Event('change',{{bubbles:true}}));return true;}})({values})")).await.map_err(cdp_error)?;
            }
            Step::Select { reference, values } => {
                let values =
                    serde_json::to_string(&json!({"reference":reference,"values":values}))?;
                self.page.evaluate(format!("((a) => {{const e=window.__moocpNodes?.get(a.reference)?.e;if(!e?.isConnected||e.tagName!=='SELECT')throw new Error('unavailable');if(a.values.some(v=>![...e.options].some(o=>o.value===v)))throw new Error('option');for(const o of e.options)o.selected=a.values.includes(o.value);e.dispatchEvent(new Event('change',{{bubbles:true}}));return true;}})({values})")).await.map_err(cdp_error)?;
            }
            Step::PressKey { reference, key } => {
                self.page.evaluate(format!("(() => {{const e=window.__moocpNodes?.get({})?.e;if(!e?.isConnected)throw new Error('stale');e.focus();return true;}})()",serde_json::to_string(&reference)?)).await.map_err(cdp_error)?;
                self.page
                    .find_element(":focus")
                    .await
                    .map_err(cdp_error)?
                    .press_key(&key)
                    .await
                    .map_err(cdp_error)?;
            }
        }
        Ok(())
    }
    pub async fn close(mut self) {
        let _ = self.browser.close().await;
        let _ = self.browser.wait().await;
        self.driver.abort();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn valid_ref(reference: &str) -> bool {
    reference
        .strip_prefix('e')
        .is_some_and(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
}

fn stage_uploads(
    home: &Home,
    profile: &str,
    steps: &mut [Step],
) -> Result<(Vec<StagedFile>, Vec<Step>)> {
    crate::config::validate_profile_id(profile)?;
    let mut staged = Vec::new();
    let mut displayed = steps.to_vec();
    let mut total = 0usize;
    for (index, step) in steps.iter_mut().enumerate() {
        if let Step::AttachFiles { files, .. } = step {
            if files.is_empty() || files.len() > 20 {
                return Err(Error::invalid("Choose 1–20 local files."));
            }
            let mut copies = Vec::new();
            let mut names = Vec::new();
            for source in files.iter() {
                use std::io::Read;
                let source = std::path::Path::new(source);
                if !source.is_absolute() {
                    return Err(Error::invalid("Upload paths must be absolute."));
                }
                let mut input = std::fs::File::open(source).map_err(|_| {
                    Error::invalid("Could not open an explicitly chosen upload file.")
                })?;
                if !input.metadata()?.is_file() {
                    return Err(Error::invalid("Upload paths must be ordinary files."));
                }
                let mut bytes = Vec::new();
                input
                    .by_ref()
                    .take(crate::network::MAX_DOWNLOAD_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)?;
                total += bytes.len();
                if bytes.len() > crate::network::MAX_DOWNLOAD_BYTES || total > 50_000_000 {
                    return Err(Error::new(
                        "size_limit",
                        "Browser upload exceeds the 25 MB per-file / 50 MB total limit.",
                    ));
                }
                let name = source
                    .file_name()
                    .and_then(|s| s.to_str())
                    .ok_or_else(|| Error::invalid("Filename must be valid Unicode."))?
                    .to_string();
                let folder = home
                    .path
                    .join("proposal-files")
                    .join(profile)
                    .join(uuid::Uuid::new_v4().to_string());
                private_dir(&folder)?;
                let target = folder.join(&name);
                crate::config::private_write(&target, &bytes)?;
                let path = target.canonicalize()?.to_string_lossy().into_owned();
                staged.push(StagedFile {
                    path: path.clone(),
                    name: name.clone(),
                    sha256: format!("{:x}", Sha256::digest(&bytes)),
                    size: bytes.len(),
                });
                if staged.len() > 20 {
                    return Err(Error::new(
                        "size_limit",
                        "A browser proposal can attach at most 20 files.",
                    ));
                }
                copies.push(path);
                names.push(name);
            }
            *files = copies;
            if let Step::AttachFiles { files, .. } = &mut displayed[index] {
                *files = names;
            }
        }
    }
    Ok((staged, displayed))
}
