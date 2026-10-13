use std::{path::PathBuf, time::Duration};

use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;
use serde_json::Value;

use crate::{
    config::{Cookie, Credentials, Home, Profile},
    error::{Error, Result},
};

// A session cookie and sesskey also exist before login. Require a same-site
// logout link and a signed-in body; never mistake the SSO provider for Moodle.
pub const LOGIN_SNAPSHOT: &str = r#"(siteUrl) => {
    const site = new URL(siteUrl), current = new URL(location.href);
    const root = site.pathname.replace(/\/$/, '');
    if (current.origin !== site.origin || !(current.pathname === root || current.pathname.startsWith(root + '/'))) return null;
    const relative = current.pathname.slice(root.length);
    if (/^\/(login|auth)(\/|$)/.test(relative)) return null;
    const cfg = window.M?.cfg;
    if (!cfg?.sesskey || !document.body || document.body.classList.contains('notloggedin')) return null;
    if (cfg.wwwroot && new URL(cfg.wwwroot).href.replace(/\/$/, '') !== site.href.replace(/\/$/, '')) return null;
    if (cfg.userId !== undefined && !(Number(cfg.userId) > 0)) return null;
    const logout = [...document.querySelectorAll('a[href]')].some(a => {
        const u = new URL(a.href, current);
        return u.origin === site.origin && u.pathname === root + '/login/logout.php' && u.searchParams.get('sesskey') === cfg.sesskey;
    });
    if (!logout) return null;
    const menu = document.querySelector('.usermenu, [data-region=user-menu]');
    const profile = menu && [...menu.querySelectorAll('a[href]')].find(a => /\/user\/(profile|view)\.php/.test(a.href));
    const uid = Number(cfg.userId || document.body.dataset.userid || (profile && new URL(profile.href).searchParams.get('id')) || 0);
    return {sesskey: cfg.sesskey, user_id: uid > 0 ? uid : null,
        display_name: menu?.querySelector('.usertext, .userbutton .usertext')?.textContent?.trim() || null};
}"#;

#[derive(Default)]
pub struct LoginOptions {
    pub browser: Option<PathBuf>,
    pub headless: bool,
    pub timeout_seconds: u64,
}

struct BrowserDirectory(PathBuf);
impl Drop for BrowserDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub async fn login(
    home: &Home,
    profile: &mut Profile,
    options: LoginOptions,
) -> Result<Credentials> {
    home.ensure()?;
    let directory = BrowserDirectory(home.path.join(format!("login-{}", uuid::Uuid::new_v4())));
    crate::config::private_dir(&directory.0)?;
    let mut builder = BrowserConfig::builder()
        .user_data_dir(&directory.0)
        .respect_https_errors()
        .arg("remote-debugging-address=127.0.0.1")
        .request_timeout(Duration::from_secs(15));
    if !options.headless {
        builder = builder.with_head();
    }
    if let Some(path) = options.browser {
        builder = builder.chrome_executable(path);
    }
    let config = builder.build().map_err(|_| Error::new("browser_missing", "Could not find Chrome, Chromium, or Edge. Install one or pass --browser /path/to/browser."))?;
    let (mut browser, mut handler) = Browser::launch(config).await
        .map_err(|_| Error::new("browser_launch_failed", "Could not open the sign-in browser. Check your desktop session or choose a browser with --browser."))?;
    let driver = tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if event.is_err() {
                break;
            }
        }
    });
    let login_url = profile
        .url
        .join("login/index.php")
        .map_err(|_| Error::invalid("Invalid login URL."))?;
    let start = browser.new_page(login_url.as_str()).await;
    let outcome = async {
        start.map_err(|_| {
            Error::new(
                "browser_navigation",
                "The browser could not load the Moodle sign-in page.",
            )
        })?;
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(options.timeout_seconds.max(30));
        loop {
            if tokio::time::Instant::now() > deadline {
                return Err(Error::new(
                    "login_timeout",
                    "Sign-in timed out. Run moocp login and complete SSO/MFA in the browser.",
                ));
            }
            // SSO may open a popup or complete in a different tab.
            for page in browser.pages().await.unwrap_or_default() {
                let expression = format!(
                    "({LOGIN_SNAPSHOT})({})",
                    serde_json::to_string(profile.url.as_str())?
                );
                let snapshot = match page.evaluate(expression).await {
                    Ok(value) => value.into_value::<Value>().unwrap_or(Value::Null),
                    Err(_) => continue,
                };
                let Some(sesskey) = snapshot["sesskey"].as_str() else {
                    continue;
                };
                let cookies = page.get_cookies().await.map_err(|_| {
                    Error::new("browser_session", "Could not capture the Moodle session.")
                })?;
                let host = profile.url.host_str().unwrap_or_default();
                let cookies: Vec<Cookie> = cookies
                    .into_iter()
                    .filter(|c| {
                        let domain = c.domain.trim_start_matches('.');
                        host == domain
                            || (c.domain.starts_with('.') && host.ends_with(&format!(".{domain}")))
                    })
                    .map(|c| Cookie {
                        name: c.name,
                        value: c.value,
                        domain: c.domain,
                        path: c.path,
                        secure: c.secure,
                        expires: if c.session { None } else { Some(c.expires) },
                    })
                    .collect();
                if cookies.is_empty() {
                    return Err(Error::new(
                        "browser_session",
                        "Signed-in Moodle page had no session cookies.",
                    ));
                }
                profile.user_id = snapshot["user_id"].as_u64();
                profile.display_name = snapshot["display_name"].as_str().map(str::to_string);
                profile.authenticated_at = chrono::Utc::now().to_rfc3339();
                return Ok(Credentials {
                    cookies,
                    sesskey: Some(sesskey.to_string()),
                    token: None,
                });
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    .await;
    let _ = browser.close().await;
    let _ = browser.wait().await;
    driver.abort();
    outcome
}

pub async fn enrich_mobile_token(
    profile: &mut Profile,
    credentials: &mut Credentials,
) -> Result<bool> {
    let http = crate::network::Http::new(profile.url.clone(), credentials)?;
    if let Some(token) = http.mobile_token().await? {
        credentials.token = Some(token);
        let api = crate::moodle::Moodle::new(profile.clone(), credentials.clone())?;
        let info = api
            .rest("core_webservice_get_site_info", serde_json::json!({}))
            .await?;
        profile.user_id = info["userid"].as_u64().or(profile.user_id);
        profile.display_name = info["fullname"]
            .as_str()
            .map(str::to_string)
            .or(profile.display_name.take());
        Ok(true)
    } else {
        Ok(false)
    }
}
