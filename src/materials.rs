//! A bounded catalogue of Moodle material with stable references and explicit discovery coverage.
use crate::{
    content,
    error::{Error, Result},
    moodle::{Activity, Moodle, Section},
    store::Store,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Section,
    Activity,
    File,
    Link,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Args {
    pub course_id: u64,
    pub section_id: Option<u64>,
    pub kind: Option<Kind>,
    /// Match words in titles and section names. Omit to discover every item.
    pub query: Option<String>,
    /// Pass next_cursor unchanged. A changed course/filter requires starting a new catalogue.
    pub cursor: Option<String>,
    #[serde(default = "limit")]
    pub limit: usize,
    /// Discover attachments on supported activity pages. Restricted activities are never fetched.
    #[serde(default = "yes")]
    pub include_files: bool,
}
fn limit() -> usize {
    20
}
fn yes() -> bool {
    true
}
#[derive(Clone, Serialize)]
pub struct Material {
    pub reference: String,
    pub kind: Kind,
    pub title: String,
    pub course_id: u64,
    pub section_id: Option<u64>,
    pub section_title: Option<String>,
    pub module_id: Option<u64>,
    pub parent_reference: Option<String>,
    pub source_url: String,
    pub availability: String,
    pub reader: String,
    pub mime: Option<String>,
    pub size: Option<u64>,
    pub indexed_at: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Link {
    pub reference: String,
    pub course_id: u64,
    #[serde(default)]
    pub module_id: Option<u64>,
    pub source_url: String,
    pub title: String,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    source: usize,
    item: usize,
    revision: String,
    group: Option<String>,
}
enum Group<'a> {
    Section(&'a Section),
    Activity(&'a Activity),
}

pub fn link_reference(course: u64, source: &str) -> Result<String> {
    let mut url = url::Url::parse(source).map_err(|_| Error::invalid("Invalid material URL."))?;
    url.set_fragment(None);
    let clean = content::safe_url(&url);
    Ok(format!(
        "course:{course}:url:{:x}",
        Sha256::digest(clean.as_bytes())
    ))
}
pub fn activity_reference(source: &str) -> Option<(u64, Option<usize>)> {
    let parts: Vec<_> = source.split(':').collect();
    match parts.as_slice() {
        ["cm", id] => Some((id.parse().ok()?, None)),
        ["cm", id, "file", index] => Some((id.parse().ok()?, Some(index.parse().ok()?))),
        _ => None,
    }
}
fn digest(value: &impl Serialize) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

impl Moodle {
    pub async fn materials(&self, store: Store, args: &Args) -> Result<Value> {
        if !(1..=50).contains(&args.limit)
            || args.query.as_ref().is_some_and(|q| q.chars().count() > 200)
        {
            return Err(Error::invalid(
                "Use limit 1–50 and a query up to 200 characters.",
            ));
        }
        let mut cursor = match &args.cursor {
            Some(value) if value.len() <= 1024 => serde_json::from_slice::<Cursor>(
                &URL_SAFE_NO_PAD
                    .decode(value)
                    .map_err(|_| Error::invalid("Invalid catalogue cursor."))?,
            )
            .map_err(|_| Error::invalid("Invalid catalogue cursor."))?,
            Some(_) => return Err(Error::invalid("Invalid catalogue cursor.")),
            None => Cursor::default(),
        };
        let outline = self.outline(args.course_id).await?;
        if args
            .section_id
            .is_some_and(|id| !outline.sections.iter().any(|s| s.id == id))
        {
            return Err(Error::new(
                "section_not_found",
                "That section is not visible in this course.",
            ));
        }
        // Viewing an activity can update its completion state without changing catalogue structure.
        let mut structural_activities = outline.activities.clone();
        for activity in &mut structural_activities {
            activity.completion = None;
        }
        let revision = digest(&(
            &self.profile.id,
            args.course_id,
            &outline.sections,
            &structural_activities,
            args.section_id,
            args.kind,
            &args.query,
            args.include_files,
        ))?;
        if args.cursor.is_some() && cursor.revision != revision {
            return Err(Error::new(
                "catalogue_changed",
                "The course or filters changed. Start a new catalogue without a cursor.",
            ));
        }
        cursor.revision = revision.clone();
        let groups: Vec<_> = outline
            .sections
            .iter()
            .filter(|s| args.section_id.is_none_or(|id| id == s.id))
            .map(Group::Section)
            .chain(
                outline
                    .activities
                    .iter()
                    .filter(|a| args.section_id.is_none_or(|id| a.section_id == Some(id)))
                    .map(Group::Activity),
            )
            .collect();
        if cursor.source > groups.len() {
            return Err(Error::invalid("Catalogue cursor is out of range."));
        }
        let mut items = Vec::new();
        let mut failures = Vec::new();
        let mut checked = 0;
        while cursor.source < groups.len() && items.len() < args.limit && checked < 25 {
            let mut group = Vec::new();
            match groups[cursor.source] {
                Group::Section(section) => {
                    let mut material = self.material(
                        args.course_id,
                        Kind::Section,
                        format!("section:{}", section.id),
                        section.title.clone(),
                        section.source_url.clone(),
                        Some(section.id),
                        None,
                        None,
                    );
                    if !section.summary_available {
                        material.availability = "summary_unavailable".into();
                    }
                    group.push(material);
                    for link in &section.links {
                        let reference = link_reference(args.course_id, &link.url)?;
                        group.push(self.material(
                            args.course_id,
                            Kind::Link,
                            reference,
                            if link.title.is_empty() {
                                link.url.clone()
                            } else {
                                link.title.clone()
                            },
                            link.url.clone(),
                            Some(section.id),
                            None,
                            Some(format!("section:{}", section.id)),
                        ));
                    }
                }
                Group::Activity(activity) => {
                    let parent = format!("cm:{}", activity.module_id);
                    let mut material = self.material(
                        args.course_id,
                        Kind::Activity,
                        parent.clone(),
                        activity.title.clone(),
                        activity.source_url.clone(),
                        activity.section_id,
                        Some(activity.module_id),
                        None,
                    );
                    if activity.kind == "label" {
                        material.source_url = self.source(&format!(
                            "course/view.php?id={}#module-{}",
                            activity.course_id, activity.module_id
                        ));
                    }
                    if !activity.accessible {
                        material.availability = "restricted".into();
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
                        material.reader = "browser".into();
                    }
                    group.push(material);
                    if args.include_files
                        && activity.accessible
                        && readable(&activity.kind)
                        && args.kind.is_none_or(|k| k == Kind::File)
                    {
                        match self.activity_files(activity).await {
                            Ok(files)=>for file in files {
                                if file.source_url.is_empty() {failures.push(json!({"module_id":activity.module_id,"stage":"attachment_discovery","error":{"code":"file_url_missing","message":"Moodle supplied an attachment without a readable URL."}}));continue;}
                                let mut material=self.material(args.course_id,Kind::File,link_reference(args.course_id,&file.source_url)?,file.name,file.source_url,activity.section_id,Some(activity.module_id),Some(parent.clone()));
                                material.mime=(!file.mime.is_empty()).then_some(file.mime);material.size=file.size;group.push(material);
                            },
                            Err(error)=>failures.push(json!({"module_id":activity.module_id,"stage":"attachment_discovery","error":error})),
                        }
                    }
                }
            }
            for material in &mut group {
                material.section_title = outline
                    .sections
                    .iter()
                    .find(|s| Some(s.id) == material.section_id)
                    .map(|s| s.title.clone());
            }
            let group_hash = digest(
                &group
                    .iter()
                    .map(|m| (&m.reference, &m.title, &m.source_url, &m.availability))
                    .collect::<Vec<_>>(),
            )?;
            if cursor.item > 0 && cursor.group.as_deref() != Some(&group_hash) {
                return Err(Error::new(
                    "catalogue_changed",
                    "Attachments changed while paging. Start a new catalogue.",
                ));
            }
            if cursor.item > group.len() {
                return Err(Error::invalid("Catalogue cursor is out of range."));
            }
            while cursor.item < group.len() && items.len() < args.limit {
                let mut material = group[cursor.item].clone();
                cursor.item += 1;
                if args.kind.is_some_and(|k| material.kind != k) {
                    continue;
                }
                if let Some(query) = &args.query {
                    let haystack = format!(
                        "{} {}",
                        material.title,
                        material.section_title.as_deref().unwrap_or_default()
                    )
                    .to_lowercase();
                    if !query
                        .split_whitespace()
                        .all(|word| haystack.contains(&word.to_lowercase()))
                    {
                        continue;
                    }
                }
                if matches!(material.kind, Kind::File | Kind::Link) {
                    store.remember_material(
                        &self.profile.id,
                        &Link {
                            reference: material.reference.clone(),
                            course_id: args.course_id,
                            module_id: material.module_id,
                            source_url: material.source_url.clone(),
                            title: material.title.clone(),
                        },
                    )?;
                }
                material.indexed_at = store.indexed_at(&self.profile.id, &material.reference)?;
                items.push(material);
            }
            if cursor.item == group.len() {
                cursor.source += 1;
                cursor.item = 0;
                cursor.group = None;
            } else {
                cursor.group = Some(group_hash);
            }
            checked += 1;
        }
        let next = if cursor.source < groups.len() {
            Some(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor)?))
        } else {
            None
        };
        Ok(
            json!({"course_id":args.course_id,"items":items,"next_cursor":next,"revision":revision,"observed_at":outline.observed_at,"warnings":outline.warnings,"failures":failures,"coverage":{"sections":outline.sections.len(),"activities":outline.activities.len(),"missing_summaries":outline.sections.iter().filter(|s|!s.summary_available).count(),"source_groups_checked_this_page":checked,"source_groups_remaining":groups.len().saturating_sub(cursor.source),"attachment_discovery":if args.include_files{"Supported, accessible activity groups visited on this page."}else{"Disabled; this catalogue omits attachments."},"external_links":"Discoverable metadata only; no provider sign-in or external fetch is performed by this catalogue."}}),
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn material(
        &self,
        course_id: u64,
        kind: Kind,
        reference: String,
        title: String,
        source_url: String,
        section_id: Option<u64>,
        module_id: Option<u64>,
        parent_reference: Option<String>,
    ) -> Material {
        let first_party = url::Url::parse(&source_url)
            .ok()
            .is_some_and(|u| self.http.check_url(&u).is_ok());
        Material {
            reference,
            kind,
            title,
            course_id,
            section_id,
            section_title: None,
            module_id,
            parent_reference,
            source_url,
            availability: if first_party {
                "discoverable"
            } else {
                "external"
            }
            .into(),
            reader: if first_party {
                "moodle"
            } else {
                "public_http_only"
            }
            .into(),
            mime: None,
            size: None,
            indexed_at: None,
        }
    }
}
fn readable(kind: &str) -> bool {
    matches!(
        kind,
        "page" | "resource" | "folder" | "book" | "label" | "assign"
    )
}
