use std::{
    io::{Cursor, Read},
    path::Path,
};

use ego_tree::NodeRef;
use scraper::{Html, Node, Selector};
use serde::Serialize;
use url::Url;

use crate::error::{Error, Result};

pub const MAX_TEXT_CHARS: usize = 200_000;

#[derive(Clone, Serialize)]
pub struct Document {
    pub text: String,
    pub format: String,
    pub warnings: Vec<String>,
    pub truncated: bool,
}

#[derive(Clone, Serialize)]
pub struct Link {
    pub title: String,
    pub url: String,
}

pub fn selector(value: &str) -> Selector {
    Selector::parse(value).expect("static CSS selector")
}

pub fn safe_url(url: &Url) -> String {
    let mut clean = url.clone();
    let _ = clean.set_username("");
    let _ = clean.set_password(None);
    let query: Vec<(String, String)> = clean
        .query_pairs()
        .filter(|(key, _)| {
            !matches!(
                key.to_ascii_lowercase().as_str(),
                "token" | "wstoken" | "sesskey" | "privatetoken" | "key" | "password"
            )
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    clean.set_query(None);
    if !query.is_empty() {
        clean.query_pairs_mut().extend_pairs(query);
    }
    clean.to_string()
}

pub fn is_login_page(source: &str) -> bool {
    let html = Html::parse_document(source);
    html.select(&selector("form")).any(|form| {
        form.select(&selector("input[type=password]"))
            .next()
            .is_some()
            && (form
                .select(&selector("input[name=username]"))
                .next()
                .is_some()
                || form
                    .value()
                    .attr("action")
                    .is_some_and(|a| a.contains("/login/")))
    }) || html.select(&selector("body.notloggedin")).next().is_some()
}

pub fn html_text(source: &str, base: Option<&Url>) -> String {
    let html = Html::parse_document(source);
    let scope = html
        .select(&selector("#region-main, [role=main]"))
        .next()
        .or_else(|| html.select(&selector("body")).next())
        .unwrap_or_else(|| html.root_element());
    let mut result = String::new();
    walk(*scope, base, &mut result, false);
    let result = result
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    let collapse = regex::Regex::new(r"\n{3,}").expect("static regex");
    collapse.replace_all(result.trim(), "\n\n").into_owned()
}

fn walk(node: NodeRef<'_, Node>, base: Option<&Url>, out: &mut String, pre: bool) {
    match node.value() {
        Node::Text(text) => {
            if pre {
                out.push_str(text);
            } else {
                let normal = text.split_whitespace().collect::<Vec<_>>().join(" ");
                if !normal.is_empty() {
                    if !out.is_empty() && !out.ends_with(['\n', ' ', '[']) {
                        out.push(' ');
                    }
                    out.push_str(&normal);
                }
            }
        }
        Node::Element(element) => {
            let tag = element.name();
            if matches!(
                tag,
                "script" | "style" | "nav" | "header" | "footer" | "noscript" | "button"
            ) {
                return;
            }
            if element.attr("aria-hidden") == Some("true") || element.attr("hidden").is_some() {
                return;
            }
            let block = matches!(
                tag,
                "p" | "div"
                    | "section"
                    | "article"
                    | "tr"
                    | "li"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "pre"
                    | "blockquote"
            );
            if block && !out.ends_with('\n') {
                out.push('\n');
            }
            if tag == "br" {
                out.push('\n');
            }
            if tag == "li" {
                out.push_str("- ");
            }
            if matches!(tag, "th" | "td") && !out.ends_with('\n') {
                out.push_str(" | ");
            }
            if matches!(tag, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
                out.push_str(&"#".repeat(tag.as_bytes()[1] as usize - b'0' as usize));
                out.push(' ');
            }
            if tag == "pre" {
                out.push_str("```\n");
            }
            if tag == "img" {
                out.push_str("[Image: ");
                out.push_str(element.attr("alt").unwrap_or("no alternative text"));
                out.push(']');
                if let Some(url) = element
                    .attr("src")
                    .and_then(|s| base.and_then(|b| b.join(s).ok()))
                    && matches!(url.scheme(), "https" | "http")
                {
                    out.push_str(&format!(" ({})", safe_url(&url)));
                }
            }
            for child in node.children() {
                walk(child, base, out, pre || tag == "pre");
            }
            if tag == "a"
                && let Some(url) = element
                    .attr("href")
                    .and_then(|s| base.and_then(|b| b.join(s).ok()))
                && matches!(url.scheme(), "https" | "http")
            {
                out.push_str(&format!(" ({})", safe_url(&url)));
            }
            if tag == "pre" {
                out.push_str("\n```\n");
            }
            if block && !out.ends_with('\n') {
                out.push('\n');
            }
        }
        _ => {
            for child in node.children() {
                walk(child, base, out, pre);
            }
        }
    }
}

pub fn links(source: &str, base: &Url) -> Vec<Link> {
    let html = Html::parse_document(source);
    let scope = html
        .select(&selector("#region-main, [role=main]"))
        .next()
        .unwrap_or_else(|| html.root_element());
    scope
        .select(&selector("a[href], iframe[src], object[data], embed[src]"))
        .filter_map(|element| {
            let raw = element
                .value()
                .attr("href")
                .or_else(|| element.value().attr("src"))
                .or_else(|| element.value().attr("data"))?;
            let url = base.join(raw).ok()?;
            if !matches!(url.scheme(), "https" | "http") {
                return None;
            }
            Some(Link {
                title: element
                    .text()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .trim()
                    .to_string(),
                url: safe_url(&url),
            })
        })
        .collect()
}

pub fn extract(data: &[u8], filename: &str, mime: &str, base: Option<&Url>) -> Result<Document> {
    let extension = Path::new(filename)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut warnings = Vec::new();
    let (mut text, format) = if data.starts_with(b"%PDF-") {
        let bytes = data.to_vec();
        let pages = std::panic::catch_unwind(|| {
            pdf_extract::extract_text_from_mem_by_pages(&bytes)
        })
        .map_err(|_| {
            Error::new(
                "extraction_failed",
                "PDF parser could not read this document.",
            )
        })?
        .map_err(|_| {
            Error::new(
                "extraction_failed",
                "PDF text extraction failed. Download the original document for visual reading.",
            )
        })?;
        let has_text = pages.iter().any(|text| !text.trim().is_empty());
        let text = pages
            .into_iter()
            .enumerate()
            .map(|(i, text)| format!("\n--- Page {} ---\n{text}", i + 1))
            .collect::<Vec<_>>()
            .join("\n");
        let text = if has_text { text } else { String::new() };
        warnings.push("PDF text extraction may omit diagrams, layout, equations, or scanned pages. Use the original document when these matter.".into());
        (text, "pdf")
    } else if data.starts_with(b"PK\x03\x04")
        || matches!(extension.as_str(), "docx" | "pptx" | "zip")
    {
        (
            archive_text(data, &extension)?,
            if extension == "docx" {
                "docx"
            } else if extension == "pptx" {
                "pptx"
            } else {
                "zip"
            },
        )
    } else if mime.contains("html") || matches!(extension.as_str(), "html" | "htm") {
        (html_text(&String::from_utf8_lossy(data), base), "html")
    } else if mime.starts_with("text/")
        || matches!(mime, "application/json" | "application/xml")
        || is_text_extension(&extension)
    {
        if data.iter().take(4096).any(|b| *b == 0) {
            return Err(Error::unsupported(
                "Binary content cannot be read as text. Download the original file.",
            ));
        }
        (
            String::from_utf8_lossy(data)
                .trim_start_matches('\u{feff}')
                .to_string(),
            "text",
        )
    } else {
        return Err(Error::unsupported(
            "This format has no text reader. Download the original file for your CLI's native reader.",
        ));
    };
    if text.trim().is_empty() {
        warnings
            .push("No extractable text. This document may require visual reading or OCR.".into());
    }
    let truncated = text.chars().count() > MAX_TEXT_CHARS;
    if truncated {
        text = text.chars().take(MAX_TEXT_CHARS).collect();
        warnings.push("Extraction reached the 200,000-character limit. Download the original for the remainder.".into());
    }
    Ok(Document {
        text,
        format: format.into(),
        warnings,
        truncated,
    })
}

fn is_text_extension(extension: &str) -> bool {
    matches!(
        extension,
        "txt"
            | "md"
            | "csv"
            | "json"
            | "xml"
            | "yaml"
            | "yml"
            | "toml"
            | "py"
            | "js"
            | "ts"
            | "css"
            | "java"
            | "c"
            | "h"
            | "cpp"
            | "rs"
            | "hs"
            | "ml"
            | "v"
            | "sh"
            | "sql"
            | "ipynb"
            | "tex"
    )
}

fn archive_text(data: &[u8], extension: &str) -> Result<String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(data)).map_err(|_| {
        Error::new(
            "extraction_failed",
            "Could not read this ZIP/Office document.",
        )
    })?;
    if archive.len() > 250 {
        return Err(Error::new(
            "size_limit",
            "Archive exceeds the 250-member limit.",
        ));
    }
    let mut expanded: u64 = 0;
    for i in 0..archive.len() {
        let member = archive
            .by_index(i)
            .map_err(|_| Error::new("extraction_failed", "Could not inspect archive."))?;
        expanded = expanded.saturating_add(member.size());
    }
    if expanded > 40_000_000 {
        return Err(Error::new(
            "size_limit",
            "Archive exceeds the 40 MB expanded-size limit.",
        ));
    }
    let mut text = String::new();
    for i in 0..archive.len() {
        let mut member = archive
            .by_index(i)
            .map_err(|_| Error::new("extraction_failed", "Could not read archive member."))?;
        let name = member.name().to_string();
        if member.is_dir() {
            continue;
        }
        if member.enclosed_name().is_none()
            || name.contains('\\')
            || member.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000)
        {
            return Err(Error::new(
                "unsafe_archive",
                "Archive contains an unsafe path or symbolic link.",
            ));
        }
        if member.size() > 8_000_000 || member.size() > member.compressed_size().max(1) * 200 {
            return Err(Error::new(
                "size_limit",
                "Archive member exceeds the expansion limit.",
            ));
        }
        let office = extension == "docx" && name == "word/document.xml"
            || extension == "pptx"
                && name.starts_with("ppt/slides/slide")
                && name.ends_with(".xml");
        if matches!(extension, "docx" | "pptx") && !office {
            continue;
        }
        text.push_str(&format!("\n## {name}\n"));
        let suffix = Path::new(&name)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if !office && !is_text_extension(suffix) {
            text.push_str("[Listed only: unsupported or nested binary file]\n");
            continue;
        }
        let mut bytes = Vec::new();
        member.by_ref().take(8_000_001).read_to_end(&mut bytes)?;
        if bytes.len() > 8_000_000 {
            return Err(Error::new(
                "size_limit",
                "Archive member exceeds its declared expansion limit.",
            ));
        }
        if office {
            let mut reader = quick_xml::Reader::from_reader(bytes.as_slice());
            loop {
                match reader.read_event() {
                    Ok(quick_xml::events::Event::Text(t)) => {
                        text.push_str(&html_escape::decode_html_entities(
                            &String::from_utf8_lossy(t.as_ref()),
                        ));
                    }
                    Ok(quick_xml::events::Event::End(e))
                        if matches!(e.local_name().as_ref(), b"p" | b"tr") =>
                    {
                        text.push('\n')
                    }
                    Ok(quick_xml::events::Event::End(e)) if e.local_name().as_ref() == b"t" => {
                        text.push(' ')
                    }
                    Ok(quick_xml::events::Event::GeneralRef(e)) => {
                        text.push_str(&html_escape::decode_html_entities(&format!(
                            "&{};",
                            String::from_utf8_lossy(e.as_ref())
                        )))
                    }
                    Ok(quick_xml::events::Event::Eof) => break,
                    Err(_) => return Err(Error::new("extraction_failed", "Malformed Office XML.")),
                    _ => (),
                }
            }
        } else {
            text.push_str(&String::from_utf8_lossy(&bytes));
        }
        if text.len() > MAX_TEXT_CHARS * 4 {
            break;
        }
    }
    Ok(text)
}

pub fn slice_text(text: &str, offset: usize, limit: usize) -> Result<serde_json::Value> {
    if !(1..=30_000).contains(&limit) {
        return Err(Error::invalid(
            "limit must be between 1 and 30000 characters.",
        ));
    }
    let total = text.chars().count();
    if offset > total {
        return Err(Error::invalid("offset is beyond the end of the document."));
    }
    let part: String = text.chars().skip(offset).take(limit).collect();
    let end = offset + part.chars().count();
    Ok(
        serde_json::json!({ "text": part, "offset": offset, "total_chars": total, "next_offset": if end < total { Some(end) } else { None } }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retains_structure_code_and_links_without_page_chrome() {
        let html = "<nav>Noise</nav><main role=main><h2>Lecture</h2><p>Read <a href='notes.pdf?wstoken=secret'>notes</a></p><pre>x = 1\n  y = 2</pre><img src='diagram.png' alt='Graph'></main>";
        let text = html_text(
            html,
            Some(&Url::parse("https://example.edu/course/").unwrap()),
        );
        assert!(text.contains("## Lecture"));
        assert!(text.contains("x = 1\n  y = 2"));
        assert!(text.contains("https://example.edu/course/notes.pdf"));
        assert!(!text.contains("secret"));
        assert!(!text.contains("Noise"));
        assert!(text.contains("Graph"));
    }
    #[test]
    fn slices_unicode_by_characters() {
        let result = slice_text("a🙂é終", 1, 2).unwrap();
        assert_eq!(result["text"], "🙂é");
        assert_eq!(result["next_offset"], 3);
    }
    #[test]
    fn detects_login_forms_and_rejects_binary_text() {
        assert!(is_login_page(
            "<form><input name=username><input type=password></form>"
        ));
        assert!(extract(b"abc\0def", "file.txt", "text/plain", None).is_err());
    }
}
