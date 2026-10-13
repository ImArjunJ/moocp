use std::{sync::Arc, time::Duration};

use reqwest::{
    Client, Method, StatusCode,
    header::{CONTENT_TYPE, COOKIE, LOCATION},
};
use serde_json::Value;
use tokio::sync::Mutex;
use url::Url;

use crate::{
    config::{Cookie, Credentials},
    error::{Error, Result},
};

pub const MAX_DOWNLOAD_BYTES: usize = 25_000_000;

#[derive(Clone)]
pub struct Http {
    client: Client,
    pub site: Url,
    cookies: Vec<Cookie>,
    gate: Arc<Mutex<tokio::time::Instant>>,
}

pub struct Response {
    pub url: Url,
    pub content_type: String,
    pub filename: String,
    pub body: Vec<u8>,
}

impl Response {
    pub fn text(&self) -> Result<String> {
        String::from_utf8(self.body.clone()).map_err(|_| {
            Error::new(
                "invalid_response",
                "Expected UTF-8 text; received binary content.",
            )
        })
    }
    pub fn json(&self) -> Result<Value> {
        Ok(serde_json::from_slice(&self.body)?)
    }
}

impl Http {
    /// Moodle's mobile launch returns a custom-scheme redirect. Keep its token
    /// entirely outside generic request errors, URLs, and MCP output.
    pub async fn mobile_token(&self) -> Result<Option<String>> {
        use base64::Engine;
        let passport = uuid::Uuid::new_v4().to_string();
        let mut url = self.url("admin/tool/mobile/launch.php")?;
        url.query_pairs_mut().extend_pairs([
            ("service", "moodle_mobile_app"),
            ("passport", passport.as_str()),
            ("urlscheme", "moocp"),
            ("confirmed", "1"),
        ]);
        self.pace().await;
        let response = self
            .client
            .get(url.clone())
            .header(COOKIE, self.cookie_header(&url))
            .send()
            .await
            .map_err(network_error)?;
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let location = if let Some(location) = location {
            location
        } else {
            if !response.status().is_success() {
                return Ok(None);
            }
            let mut response = response;
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(network_error)? {
                if body.len() + chunk.len() > 1_000_000 {
                    return Ok(None);
                }
                body.extend_from_slice(&chunk);
            }
            let html = scraper::Html::parse_document(&String::from_utf8_lossy(&body));
            let Some(location) = html
                .select(&crate::content::selector("a#launchapp[href]"))
                .next()
                .and_then(|a| a.value().attr("href"))
                .map(str::to_string)
            else {
                return Ok(None);
            };
            location
        };
        let Some((scheme, encoded)) = location.split_once("://token=") else {
            return Ok(None);
        };
        if scheme.is_empty() || matches!(scheme, "http" | "https") {
            return Ok(None);
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| {
                Error::new(
                    "invalid_mobile_callback",
                    "Invalid mobile sign-in response.",
                )
            })?;
        let decoded = String::from_utf8(decoded).map_err(|_| {
            Error::new(
                "invalid_mobile_callback",
                "Invalid mobile sign-in response.",
            )
        })?;
        let mut parts = decoded.split(":::");
        let site_id = parts.next().unwrap_or_default();
        let expected = format!(
            "{:x}",
            md5::compute(format!(
                "{}{passport}",
                self.site.as_str().trim_end_matches('/')
            ))
        );
        if site_id != expected {
            return Err(Error::new(
                "invalid_mobile_callback",
                "Mobile sign-in response did not match this site and login request.",
            ));
        }
        let token = parts.next().filter(|s| !s.is_empty()).ok_or_else(|| {
            Error::new(
                "invalid_mobile_callback",
                "Mobile sign-in response contained no token.",
            )
        })?;
        Ok(Some(token.to_string()))
    }
    pub fn new(site: Url, credentials: &Credentials) -> Result<Self> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(45))
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("Moocp/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| {
                Error::new(
                    "network_configuration",
                    "Could not initialise TLS networking.",
                )
            })?;
        Ok(Self {
            client,
            site,
            cookies: credentials.cookies.clone(),
            gate: Arc::new(Mutex::new(tokio::time::Instant::now())),
        })
    }
    pub fn url(&self, path: &str) -> Result<Url> {
        let url = self
            .site
            .join(path)
            .map_err(|_| Error::invalid("Invalid Moodle URL."))?;
        self.check_url(&url)?;
        Ok(url)
    }
    pub fn check_url(&self, url: &Url) -> Result<()> {
        if url.origin() != self.site.origin()
            || !url.path().starts_with(self.site.path())
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(Error::new(
                "external_link",
                "This resource is outside the connected Moodle site. Open its source link separately; Moodle credentials will not be sent.",
            ));
        }
        Ok(())
    }
    pub async fn get(&self, url: Url) -> Result<Response> {
        self.request(Method::GET, url, None, None).await
    }
    pub async fn post_json(&self, url: Url, body: &Value) -> Result<Response> {
        self.request(Method::POST, url, Some(body), None).await
    }
    pub async fn post_form(&self, url: Url, body: &[(String, String)]) -> Result<Response> {
        self.request(Method::POST, url, None, Some(body)).await
    }

    async fn pace(&self) {
        let mut gate = self.gate.lock().await;
        tokio::time::sleep_until(*gate).await;
        *gate = tokio::time::Instant::now() + Duration::from_millis(120);
    }

    fn cookie_header(&self, url: &Url) -> String {
        let host = url.host_str().unwrap_or_default();
        let now = chrono::Utc::now().timestamp() as f64;
        self.cookies
            .iter()
            .filter(|c| {
                let domain = c.domain.trim_start_matches('.');
                let domain_ok = if c.domain.starts_with('.') {
                    host == domain || host.ends_with(&format!(".{domain}"))
                } else {
                    host == domain
                };
                let path_ok = url.path() == c.path
                    || (url.path().starts_with(&c.path)
                        && (c.path.ends_with('/')
                            || url.path().as_bytes().get(c.path.len()) == Some(&b'/')));
                domain_ok
                    && path_ok
                    && (!c.secure || url.scheme() == "https")
                    && c.expires.is_none_or(|expiry| expiry < 0.0 || expiry > now)
                    && !c.name.contains(['\r', '\n', ';', '='])
                    && !c.value.contains(['\r', '\n', ';'])
            })
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }

    async fn request(
        &self,
        mut method: Method,
        mut url: Url,
        json: Option<&Value>,
        form: Option<&[(String, String)]>,
    ) -> Result<Response> {
        for _ in 0..8 {
            self.check_url(&url)?;
            let relative = url
                .path()
                .strip_prefix(self.site.path())
                .unwrap_or_default();
            if relative.starts_with("login/") || relative.starts_with("auth/") {
                return Err(Error::auth());
            }
            self.pace().await;
            let cookies = self.cookie_header(&url);
            let mut request = self.client.request(method.clone(), url.clone());
            if !cookies.is_empty() {
                request = request.header(COOKIE, cookies);
            }
            if method == Method::POST {
                if let Some(json) = json {
                    request = request.json(json);
                }
                if let Some(form) = form {
                    request = request.form(form);
                }
            }
            let mut response = request.send().await.map_err(network_error)?;
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED {
                return Err(Error::auth());
            }
            if status == StatusCode::FORBIDDEN {
                return Err(Error::new(
                    "permission_denied",
                    "Moodle denied access to this resource.",
                ));
            }
            if status == StatusCode::TOO_MANY_REQUESTS {
                return Err(Error {
                    code: "rate_limited".into(),
                    message: "Moodle is rate limiting requests. Retry later.".into(),
                    retryable: true,
                });
            }
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| {
                        Error::new("invalid_response", "Moodle redirect had no location.")
                    })?;
                let next = url
                    .join(location)
                    .map_err(|_| Error::new("invalid_response", "Invalid redirect URL."))?;
                if next.origin() != self.site.origin() {
                    return Err(Error::new(
                        "external_redirect",
                        "Moodle redirected outside its site, possibly for expired SSO. Reconnect with moocp login or open the source in your browser.",
                    ));
                }
                if status == StatusCode::SEE_OTHER
                    || ((status == StatusCode::FOUND || status == StatusCode::MOVED_PERMANENTLY)
                        && method == Method::POST)
                {
                    method = Method::GET;
                }
                url = next;
                continue;
            }
            if !status.is_success() {
                return Err(Error {
                    code: "moodle_http_error".into(),
                    message: format!("Moodle returned HTTP {}.", status.as_u16()),
                    retryable: status.is_server_error(),
                });
            }
            if response
                .content_length()
                .is_some_and(|n| n > MAX_DOWNLOAD_BYTES as u64)
            {
                return Err(Error::new(
                    "size_limit",
                    "Resource exceeds the 25 MB download limit.",
                ));
            }
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let filename = response
                .headers()
                .get("content-disposition")
                .and_then(|v| v.to_str().ok())
                .and_then(disposition_filename)
                .unwrap_or_else(|| {
                    url.path_segments()
                        .and_then(|mut s| s.next_back())
                        .unwrap_or("resource")
                        .to_string()
                });
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(network_error)? {
                if body.len() + chunk.len() > MAX_DOWNLOAD_BYTES {
                    return Err(Error::new(
                        "size_limit",
                        "Resource exceeds the 25 MB download limit.",
                    ));
                }
                body.extend_from_slice(&chunk);
            }
            if content_type.contains("text/html") {
                let text = String::from_utf8_lossy(&body);
                if crate::content::is_login_page(&text) {
                    return Err(Error::auth());
                }
            }
            return Ok(Response {
                url,
                content_type,
                filename,
                body,
            });
        }
        Err(Error::new(
            "redirect_limit",
            "Moodle redirected too many times.",
        ))
    }

    pub async fn upload(
        &self,
        url: Url,
        fields: Vec<(String, String)>,
        name: String,
        data: Vec<u8>,
    ) -> Result<Value> {
        self.check_url(&url)?;
        self.pace().await;
        if data.len() > MAX_DOWNLOAD_BYTES {
            return Err(Error::new("size_limit", "Upload exceeds the 25 MB limit."));
        }
        let mut form = reqwest::multipart::Form::new().part(
            "file_1",
            reqwest::multipart::Part::bytes(data).file_name(name),
        );
        for (key, value) in fields {
            form = form.text(key, value);
        }
        let response = self
            .client
            .post(url.clone())
            .header(COOKIE, self.cookie_header(&url))
            .multipart(form)
            .send()
            .await
            .map_err(network_error)?;
        if response.status().is_redirection() || response.status() == StatusCode::UNAUTHORIZED {
            return Err(Error::auth());
        }
        if !response.status().is_success() {
            return Err(Error::new(
                "upload_failed",
                format!("Upload returned HTTP {}.", response.status().as_u16()),
            ));
        }
        // Upload responses are small; still enforce a response bound before decoding.
        if response.content_length().is_some_and(|n| n > 1_000_000) {
            return Err(Error::new(
                "size_limit",
                "Upload response was unexpectedly large.",
            ));
        }
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(network_error)? {
            if bytes.len() + chunk.len() > 1_000_000 {
                return Err(Error::new(
                    "size_limit",
                    "Upload response was unexpectedly large.",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(serde_json::from_slice(&bytes)?)
    }
}

fn disposition_filename(value: &str) -> Option<String> {
    value.split(';').find_map(|part| {
        part.trim()
            .strip_prefix("filename=")
            .map(|s| s.trim_matches('"').to_string())
    })
}

fn network_error(error: reqwest::Error) -> Error {
    // reqwest Display may contain credential-bearing URLs: never expose it.
    Error {
        code: if error.is_timeout() {
            "network_timeout"
        } else {
            "network_error"
        }
        .into(),
        message: if error.is_timeout() {
            "Moodle did not respond before the timeout."
        } else {
            "Could not reach Moodle. Check your connection, VPN, and TLS certificates."
        }
        .into(),
        retryable: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restricts_origin_and_cookie_path() {
        let credentials = Credentials {
            cookies: vec![Cookie {
                name: "MoodleSession".into(),
                value: "secret".into(),
                domain: "example.edu".into(),
                path: "/moodle".into(),
                secure: true,
                expires: None,
            }],
            ..Default::default()
        };
        let http = Http::new(
            Url::parse("https://example.edu/moodle/").unwrap(),
            &credentials,
        )
        .unwrap();
        assert!(
            http.check_url(&Url::parse("https://evil.example/moodle/").unwrap())
                .is_err()
        );
        assert!(
            http.check_url(&Url::parse("https://example.edu/other/").unwrap())
                .is_err()
        );
        assert_eq!(
            http.cookie_header(&Url::parse("https://example.edu/moodle/my/").unwrap()),
            "MoodleSession=secret"
        );
        assert_eq!(
            http.cookie_header(&Url::parse("https://example.edu/moodle-other/").unwrap()),
            ""
        );
        assert_eq!(
            http.cookie_header(&Url::parse("http://example.edu/moodle/").unwrap()),
            ""
        );
    }
}
