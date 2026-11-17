//! 문서 속성(schema v8) 통합 테스트: Office `docProps/core.xml`과 PDF Info 사전의
//! 작성자·제목·작성일·수정일이 rebuild_index 전체 경로를 거쳐 색인되고, 본문에
//! 없는 작성자 이름으로도 검색되며, 검색 결과에 속성이 실려 나오는지 확인한다.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use dowse::{SearchHit, Searcher, rebuild_index};
use pdf_extract::{Dictionary, Document, Object, Stream, StringFormat};
use zip::write::SimpleFileOptions;

/// office_extract.rs와 같은 이유로 느린 경로(walkdir)를 강제한다.
static FORCE_SLOW_LANE_INIT: std::sync::Once = std::sync::Once::new();

fn force_slow_lane_for_tests() {
    FORCE_SLOW_LANE_INIT.call_once(|| {
        // Safety: 이 프로세스에서 이 환경 변수를 쓰는 곳은 여기 하나이고 `Once`로 한 번만 실행된다.
        unsafe {
            std::env::set_var("DOWSE_FORCE_SLOW_LANE", "1");
        }
    });
}

fn target_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("dowse-props-")
        .tempdir()
        .unwrap()
}

fn write_zip(path: &Path, entries: &[(&str, &str)]) {
    let file = fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default();
    for (name, content) in entries {
        zip.start_file(*name, options).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

/// 한 쪽짜리 PDF를 만들고 Info 사전에 속성을 넣는다. 작성자는 UTF-16BE(BOM),
/// 제목은 PDFDocEncoding(ASCII)으로 넣어 두 인코딩을 모두 거치게 한다.
fn write_pdf(path: &Path, body: &str, author: &str, title: &str) {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();

    let mut font = Dictionary::new();
    font.set("Type", Object::Name(b"Font".to_vec()));
    font.set("Subtype", Object::Name(b"Type1".to_vec()));
    font.set("BaseFont", Object::Name(b"Helvetica".to_vec()));
    let font_id = doc.add_object(font);
    let mut fonts = Dictionary::new();
    fonts.set("F1", Object::Reference(font_id));
    let mut resources = Dictionary::new();
    resources.set("Font", Object::Dictionary(fonts));
    let resources_id = doc.add_object(resources);

    let content = format!("BT /F1 24 Tf 72 700 Td ({body}) Tj ET");
    let content_id = doc.add_object(Stream::new(Dictionary::new(), content.into_bytes()));

    let mut page = Dictionary::new();
    page.set("Type", Object::Name(b"Page".to_vec()));
    page.set("Parent", Object::Reference(pages_id));
    page.set("Contents", Object::Reference(content_id));
    page.set("Resources", Object::Reference(resources_id));
    page.set(
        "MediaBox",
        Object::Array(vec![0.into(), 0.into(), 595.into(), 842.into()]),
    );
    let page_id = doc.add_object(page);

    let mut pages = Dictionary::new();
    pages.set("Type", Object::Name(b"Pages".to_vec()));
    pages.set("Kids", Object::Array(vec![Object::Reference(page_id)]));
    pages.set("Count", Object::Integer(1));
    doc.objects.insert(pages_id, Object::Dictionary(pages));

    let mut catalog = Dictionary::new();
    catalog.set("Type", Object::Name(b"Catalog".to_vec()));
    catalog.set("Pages", Object::Reference(pages_id));
    let catalog_id = doc.add_object(catalog);
    doc.trailer.set("Root", Object::Reference(catalog_id));

    let mut author_utf16 = vec![0xFE, 0xFF];
    for unit in author.encode_utf16() {
        author_utf16.extend_from_slice(&unit.to_be_bytes());
    }
    let mut info = Dictionary::new();
    info.set(
        "Author",
        Object::String(author_utf16, StringFormat::Hexadecimal),
    );
    info.set(
        "Title",
        Object::String(title.as_bytes().to_vec(), StringFormat::Literal),
    );
    info.set(
        "CreationDate",
        Object::String(b"D:20240306100000+09'00'".to_vec(), StringFormat::Literal),
    );
    info.set(
        "ModDate",
        Object::String(b"D:20240307000000Z".to_vec(), StringFormat::Literal),
    );
    let info_id = doc.add_object(info);
    doc.trailer.set("Info", Object::Reference(info_id));

    doc.save(path).unwrap();
}

fn hits(index_dir: &Path, query: &str) -> Vec<SearchHit> {
    Searcher::open(index_dir)
        .unwrap()
        .search(query, 50)
        .unwrap()
}

fn hit_for<'a>(hits: &'a [SearchHit], file: &str) -> Option<&'a SearchHit> {
    hits.iter().find(|h| h.path.ends_with(file))
}

#[test]
fn office_and_pdf_properties_are_indexed_searchable_and_returned() -> Result<()> {
    force_slow_lane_for_tests();
    let index_dir = tempfile::tempdir()?;
    let target = target_dir();

    let core = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
  xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/"
  xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <dc:title>계통한계가격 분석</dc:title>
  <dc:creator>홍길동</dc:creator>
  <dcterms:created xsi:type="dcterms:W3CDTF">2024-03-05T01:02:03Z</dcterms:created>
  <dcterms:modified xsi:type="dcterms:W3CDTF">2024-03-06T10:00:00+09:00</dcterms:modified>
</cp:coreProperties>"#;
    let body = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>quarterly body text only</w:t></w:r></w:p></w:body></w:document>"#;
    write_zip(
        &target.path().join("report.docx"),
        &[("word/document.xml", body), ("docProps/core.xml", core)],
    );
    write_pdf(
        &target.path().join("paper.pdf"),
        "pdfbodysentinel",
        "김영희",
        "Glimmerfrost Annual Review",
    );
    fs::write(
        target.path().join("plain.txt"),
        "nothing about authors here",
    )?;

    rebuild_index(index_dir.path(), target.path())?;

    // 본문에 없는 작성자 이름(한글)으로 docx가 찾힌다.
    let found = hits(index_dir.path(), "홍길동");
    let docx = hit_for(&found, "report.docx").expect("작성자 이름으로 docx가 찾혀야 함");
    assert_eq!(docx.author.as_deref(), Some("홍길동"));
    assert_eq!(docx.title.as_deref(), Some("계통한계가격 분석"));
    assert_eq!(docx.doc_created, Some(1_709_600_523_000));
    assert_eq!(docx.doc_modified, Some(1_709_686_800_000));
    assert!(hit_for(&found, "plain.txt").is_none());

    // 제목 일부로도 찾힌다.
    assert!(hit_for(&hits(index_dir.path(), "한계가격"), "report.docx").is_some());

    // PDF: UTF-16 작성자, ASCII 제목(입력 중 접두어 포함), 본문, 날짜.
    let found = hits(index_dir.path(), "김영희");
    let pdf = hit_for(&found, "paper.pdf").expect("PDF 작성자로 찾혀야 함");
    assert_eq!(pdf.author.as_deref(), Some("김영희"));
    assert_eq!(pdf.title.as_deref(), Some("Glimmerfrost Annual Review"));
    assert_eq!(pdf.doc_created, Some(1_709_686_800_000));
    assert_eq!(pdf.doc_modified, Some(1_709_769_600_000));
    assert!(hit_for(&hits(index_dir.path(), "glimmerfr"), "paper.pdf").is_some());
    assert!(hit_for(&hits(index_dir.path(), "glimmerfrost annual"), "paper.pdf").is_some());
    assert!(hit_for(&hits(index_dir.path(), "pdfbodysentinel"), "paper.pdf").is_some());

    // 속성이 없는 문서는 None.
    let plain = hits(index_dir.path(), "authors");
    let txt = hit_for(&plain, "plain.txt").expect("본문 검색");
    assert_eq!(txt.author, None);
    assert_eq!(txt.title, None);
    assert_eq!(txt.doc_created, None);
    Ok(())
}
