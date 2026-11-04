//! Section summaries are course material, even when the course has no activities.

use scraper::{ElementRef, Html};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    content::{self, Link},
    error::{Error, Result},
    moodle::{Moodle, Outline},
    network::Response,
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Section {
    pub id: u64,
    pub number: Option<u64>,
    pub title: String,
    pub summary: String,
    pub source_url: String,
    pub links: Vec<Link>,
    pub summary_available: bool,
    #[serde(skip)]
    html: Option<String>,
}

impl Section {
    fn set_summary(&mut self, html: &str, base: &Url) {
        self.summary = content::html_text(html, Some(base));
        self.links = content::links(html, base);
        self.html = Some(html.to_owned());
        self.summary_available = true;
    }

    pub(crate) fn response(&self) -> Result<Response> {
        let html = self.html.as_ref().ok_or_else(|| Error::new(
            "section_summary_unavailable",
            "Moodle did not expose this section's summary. Inspect its source URL in the browser.",
        ))?;
        Ok(Response {
            url: Url::parse(&self.source_url)
                .map_err(|_| Error::invalid("Invalid section URL."))?,
            content_type: "text/html; charset=utf-8".into(),
            filename: format!("section-{}.html", self.id),
            body: format!(
                "<main><h1>{}</h1>{html}</main>",
                html_escape::encode_text(&self.title)
            )
            .into_bytes(),
        })
    }
}

impl Moodle {
    pub(crate) fn section_metadata(
        &self,
        id: u64,
        number: Option<u64>,
        title: String,
        html: Option<&str>,
    ) -> Section {
        let mut section = Section {
            id,
            number,
            title,
            summary: String::new(),
            source_url: self.source(&format!("course/section.php?id={id}")),
            links: vec![],
            summary_available: false,
            html: None,
        };
        if let Some(html) = html {
            section.set_summary(
                html,
                &self
                    .profile
                    .url
                    .join(&format!("course/section.php?id={id}"))
                    .expect("static section path"),
            );
        }
        section
    }

    pub(crate) fn sections_from_html(&self, text: &str) -> Vec<Section> {
        parse_sections(text)
            .into_iter()
            .filter_map(|parsed| {
                Some(self.section_metadata(
                    parsed.id?,
                    parsed.number,
                    parsed.title,
                    parsed.summary.as_deref(),
                ))
            })
            .collect()
    }

    pub(crate) async fn hydrate_sections(&self, outline: &mut Outline) {
        if outline.sections.iter().all(|s| s.summary_available) {
            return;
        }
        let page = async {
            self.http
                .get(
                    self.http
                        .url(&format!("course/view.php?id={}", outline.course_id))?,
                )
                .await
        }
        .await;
        let allow_fallbacks = match page {
            Ok(response) => match response.text() {
                Ok(text) => {
                    let parsed = parse_sections(&text);
                    for section in &mut outline.sections {
                        if section.summary_available {
                            continue;
                        }
                        if let Some(html) = parsed
                            .iter()
                            .find(|p| {
                                p.id == Some(section.id)
                                    || (p.id.is_none()
                                        && p.number.is_some()
                                        && p.number == section.number)
                            })
                            .and_then(|p| p.summary.as_deref())
                        {
                            section.set_summary(html, &response.url);
                        }
                    }
                    true
                }
                Err(error) => {
                    outline
                        .warnings
                        .push(format!("Course summaries: {}", error.code));
                    false
                }
            },
            Err(error) => {
                let retry_sections = error.code == "moodle_http_error";
                outline
                    .warnings
                    .push(format!("Course summaries: {}", error.code));
                retry_sections
            }
        };
        // Most themes include all summaries in one page. Bound additional requests for lazy course formats.
        for section in outline
            .sections
            .iter_mut()
            .filter(|s| !s.summary_available)
            .take(if allow_fallbacks { 20 } else { 0 })
        {
            if let Err(error) = self.fetch_section_summary(outline.course_id, section).await {
                outline.warnings.push(format!(
                    "Section {} summary unavailable: {}",
                    section.id, error.code
                ));
                if matches!(
                    error.code.as_str(),
                    "network_error" | "network_timeout" | "reauth_required" | "rate_limited"
                ) {
                    break;
                }
            }
        }
        if outline.sections.iter().any(|s| !s.summary_available) {
            outline.warnings.push("Some section summaries could not be fetched; empty text is not proof that material is absent. Read the section directly or inspect its source URL.".into());
        }
    }

    async fn fetch_section_summary(&self, course_id: u64, section: &mut Section) -> Result<()> {
        let url = self
            .http
            .url(&format!("course/section.php?id={}", section.id))?;
        let response = match self.http.get(url).await {
            Err(error)
                if error.code == "moodle_http_error"
                    && error.message.contains("HTTP 404")
                    && section.number.is_some() =>
            {
                self.http
                    .get(self.http.url(&format!(
                        "course/view.php?id={course_id}&section={}",
                        section.number.unwrap()
                    ))?)
                    .await?
            }
            other => other?,
        };
        let text = response.text()?;
        let parsed = parse_sections(&text);
        let summary = parsed
            .iter()
            .find(|p| {
                p.id == Some(section.id)
                    || (p.id.is_none() && p.number.is_some() && p.number == section.number)
            })
            .and_then(|p| p.summary.clone())
            .or_else(|| {
                // Some themes omit the section wrapper on the dedicated section page.
                if !parsed.is_empty()
                    || !response.url.path().ends_with("/course/section.php")
                    || !response.url.query_pairs().any(|(key, value)| {
                        key == "id" && value.parse::<u64>().ok() == Some(section.id)
                    })
                {
                    return None;
                }
                let html = Html::parse_document(&text);
                let selector =
                    content::selector("#region-main .summarytext, [role=main] .summarytext");
                let mut summaries = html.select(&selector);
                let first = summaries.next()?;
                summaries.next().is_none().then(|| first.inner_html())
            })
            .ok_or_else(|| {
                Error::new(
                    "section_summary_unavailable",
                    "The section page exposed no identifiable summary.",
                )
            })?;
        section.source_url = content::safe_url(&response.url);
        section.set_summary(&summary, &response.url);
        Ok(())
    }

    pub async fn section(&self, course_id: u64, section_id: u64) -> Result<Section> {
        let mut section = self
            .outline_metadata(course_id)
            .await?
            .sections
            .into_iter()
            .find(|s| s.id == section_id)
            .ok_or_else(|| {
                Error::new(
                    "section_not_found",
                    "That section is not visible in this course.",
                )
            })?;
        if !section.summary_available {
            self.fetch_section_summary(course_id, &mut section).await?;
        }
        Ok(section)
    }
}

struct ParsedSection {
    id: Option<u64>,
    number: Option<u64>,
    title: String,
    summary: Option<String>,
}

fn parse_sections(text: &str) -> Vec<ParsedSection> {
    let html = Html::parse_document(text);
    let scope = html
        .select(&content::selector("#region-main, [role=main]"))
        .next()
        .unwrap_or_else(|| html.root_element());
    let wrappers =
        content::selector("[data-for=section][data-id], li.section[id], section.section[id]");
    scope
        .select(&wrappers)
        .map(|wrapper| {
            let id = wrapper.value().attr("data-id").and_then(|s| s.parse().ok());
            let number = wrapper
                .value()
                .attr("id")
                .and_then(|s| s.strip_prefix("section-"))
                .and_then(|s| s.parse().ok())
                .or_else(|| {
                    wrapper
                        .value()
                        .attr("data-sectionid")
                        .and_then(|s| s.parse().ok())
                });
            let title = wrapper
                .select(&content::selector(".sectionname, [data-for=section_title]"))
                .next()
                .map(|e| content::html_text(&e.inner_html(), None))
                .unwrap_or_default();
            let summary = [".summarytext", ".summary"]
                .into_iter()
                .find_map(|selector| {
                    wrapper
                        .select(&content::selector(selector))
                        .find(|element| {
                            element
                                .ancestors()
                                .filter_map(ElementRef::wrap)
                                .find(|parent| wrappers.matches(parent))
                                .is_some_and(|parent| parent.id() == wrapper.id())
                        })
                        .map(|element| element.inner_html())
                });
            ParsedSection {
                id,
                number,
                title,
                summary,
            }
        })
        .collect()
}
