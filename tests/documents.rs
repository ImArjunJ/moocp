use moocp::content;
use std::io::{Cursor, Write};

fn archive(name: &str, body: &str) -> Vec<u8> {
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    archive
        .start_file(name, zip::write::SimpleFileOptions::default())
        .unwrap();
    archive.write_all(body.as_bytes()).unwrap();
    archive.finish().unwrap().into_inner()
}

#[test]
fn reads_office_xml_and_preserves_text_entities() {
    let bytes = archive(
        "word/document.xml",
        r#"<w:document xmlns:w="urn:word"><w:body><w:p><w:r><w:t>Gradients &amp; weights</w:t></w:r></w:p></w:body></w:document>"#,
    );
    let document = content::extract(
        &bytes,
        "lecture.docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        None,
    )
    .unwrap();
    assert!(document.text.contains("Gradients & weights"));
    assert_eq!(document.format, "docx");
}

#[test]
fn rejects_traversal_and_lists_nested_binaries_without_executing() {
    let bytes = archive("../outside.py", "print('never executed')");
    assert_eq!(
        content::extract(&bytes, "files.zip", "application/zip", None)
            .err()
            .unwrap()
            .code,
        "unsafe_archive"
    );
    let bytes = archive("binary.exe", "MZ");
    let document = content::extract(&bytes, "files.zip", "application/zip", None).unwrap();
    assert!(document.text.contains("Listed only"));
}

#[test]
fn pdf_passages_have_page_locations() {
    let document = content::extract(
        include_bytes!("fixtures/lecture.pdf"),
        "lecture.pdf",
        "application/pdf",
        None,
    )
    .unwrap();
    assert!(document.text.contains("--- Page 1 ---"));
    assert!(document.text.contains("negative gradient"));
    assert!(document.warnings.iter().any(|w| w.contains("diagrams")));
}
