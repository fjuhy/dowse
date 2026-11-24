use std::fs;
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::rules::IndexRules;

/// 按扩展名白名单认定的纯文本文件。
const TEXT_EXTS: &[&str] = &[
    "txt", "md", "markdown", "mdc", "mmd", "mermaid", "drawio", "log", "csv", "tsv", "json",
    "toml", "yaml", "yml", "ini", "cfg", "conf", "xml", "html", "htm", "sql", "sh", "ps1", "bat",
    "rs", "py", "go", "java", "js", "ts", "jsx", "tsx", "c", "h", "cpp", "hpp", "cs", "rb", "php",
    "lua", "vue",
];

/// 지원하는 Office 및 메일 아카이브 포맷(OOXML, `.xls`/`.xlsb`, 매크로 포함 `.docm`/`.xlsm`/`.pptm`, 아웃룩 `.pst`/`.ost`/`.msg`/`.eml`).
const OFFICE_EXTS: &[&str] = &[
    "docx", "docm", "xlsx", "xlsm", "xls", "xlsb", "pptx", "pptm", "pst", "ost", "msg", "eml",
];

pub(crate) fn builtin_extensions() -> impl Iterator<Item = &'static str> {
    TEXT_EXTS
        .iter()
        .chain(OFFICE_EXTS)
        .chain(crate::ocr::IMAGE_EXTS)
        .copied()
        .chain(["pdf"])
}

/// 这个文件是否属于能抽取文本的类型（只看扩展名，不读文件，很便宜）。
/// 启动对账用它过滤：非文本类型的文件从来不会进索引，没必要参与三态比对——
/// 否则每次对账都把它们当"新增"白跑一趟、还把对账统计数字撑虚。
///
/// 读进程级当前生效规则：追加扩展名（`extra_text_exts`）里的类型也算可抽取。
pub(crate) fn is_extractable(path: &Path) -> bool {
    is_extractable_with(path, &crate::rules::active_rules())
}

/// [`is_extractable`] 的纯函数版：判定逻辑接收显式规则，不碰进程级全局，便于
/// 单测。零参的 `is_extractable` 只是用当前生效规则调它。
pub(crate) fn is_extractable_with(path: &Path, rules: &IndexRules) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    if !rules.allows_extension(&ext) {
        return false;
    }
    ext == "pdf"
        || TEXT_EXTS.contains(&ext.as_str())
        || OFFICE_EXTS.contains(&ext.as_str())
        || rules.is_extra_text_ext(&ext)
}

/// 从文件里抽出可索引的纯文本。
/// 返回 Option 而不是 Result：抽不出来（不支持的格式/太大/损坏）不算错误，
/// 调用方只需要知道"这个文件没有文本"，跳过即可。
///
/// 体积上限和追加文本扩展名取自进程级当前生效规则（见 `rules` 模块）。
///
/// 색인 경로는 [`extract_document`]로 옮겨 가서 지금은 테스트에서만 쓴다
/// (테스트 밖에서는 dead_code → Windows CI의 `clippy -D warnings` 실패).
#[cfg(test)]
pub fn extract_text(path: &Path) -> Option<String> {
    extract_text_with(path, &crate::rules::active_rules())
}

/// 문서 속성(작성자·제목·작성일·수정일). docx/xlsx/pptx는 `docProps/core.xml`의
/// `dc:creator`/`dc:title`/`dcterms:created`/`dcterms:modified`, PDF는 trailer의
/// Info 사전(`/Author`/`/Title`/`/CreationDate`/`/ModDate`)에서 읽는다.
/// 날짜는 UTC 기준 밀리초 Unix 타임스탬프(파일 mtime 필드와 같은 단위).
/// 값이 없거나 형식을 못 읽으면 `None` — 속성은 있으면 좋은 부가 정보라
/// 하나 못 읽었다고 본문 색인을 포기하지 않는다.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct DocProps {
    pub author: Option<String>,
    pub title: Option<String>,
    pub created: Option<i64>,
    pub modified: Option<i64>,
}

/// 본문 + 문서 속성. 색인 경로는 이걸 써서 PDF를 한 번만 연다.
#[derive(Debug, Default)]
pub(crate) struct ExtractedDoc {
    pub text: String,
    pub props: DocProps,
}

/// 현재 적용 중인 색인 규칙과 같은 panic 방어로 본문과 문서 속성을 함께 뽑는다.
pub(crate) fn extract_document(path: &Path) -> Option<ExtractedDoc> {
    extract_document_with(path, &crate::rules::active_rules())
}

/// [`extract_text`] 的纯函数版：体积上限、追加扩展名都从显式规则读，不碰进程级
/// 全局，便于单测。零参的 `extract_text` 只是用当前生效规则调它。
#[cfg(test)]
fn extract_text_with(path: &Path, rules: &IndexRules) -> Option<String> {
    extract_document_with(path, rules).map(|doc| doc.text)
}

fn extract_document_with(path: &Path, rules: &IndexRules) -> Option<ExtractedDoc> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();

    if !rules.allows_extension(&ext) {
        return None;
    }
    let meta = fs::metadata(path).ok()?;
    let size_limit = match ext.as_str() {
        // PST/OST archives are read via bounded random-access seeks, never loaded whole into RAM.
        "pst" | "ost" => 32 * 1024 * 1024 * 1024,
        // Standalone .msg/.eml often carry large binary attachments while body text is tiny.
        "msg" | "eml" => rules.max_file_bytes().max(100 * 1024 * 1024),
        _ => rules.max_file_bytes(),
    };
    if meta.len() > size_limit {
        return None;
    }

    // 各格式的抽取都可能在底层库里 panic：PDF 走的 `pdf_extract`（及其底层
    // `lopdf`）对某些畸形/深层嵌套的 PDF 不是返回 `Err`，而是内部 `unwrap`/`panic!`；
    // docx/xlsx/pptx 共用的 zip + XML 解析同样可能对畸形输入 panic。而抽取是在
    // 监听/OCR 线程持有共享 `IndexUpdater` 锁时调的：panic 一旦穿过持有的
    // `MutexGuard`，就会把这把共享锁**毒化**，之后所有 `.lock()` 都 panic，连锁
    // 拖垮整条监听 + OCR 管线（对常驻托盘程序是灾难性的）。
    //
    // 这里用 `catch_unwind` 把**任意**格式抽取过程中的 panic 兜成 `None`，让畸形
    // 文件跟其它抽不出文本的文件一样安静跳过。（`updater.rs`/`watch.rs`/
    // `ocr_worker.rs` 那侧的锁也已改成抗毒化的 `unwrap_or_else(|e| e.into_inner())`，
    // 两层防御各自都能兜住。）
    //
    // 注意 `catch_unwind` 只能接住**栈展开式**的 panic（`unwrap`/`panic!` 这类）。
    // 若畸形输入触发深度递归把线程栈撑爆，那是栈溢出，走的是 SIGABRT/进程 abort，
    // `catch_unwind` 接不住，进程仍会整体退出——这一类只能靠上游解析库自身的
    // 递归深度限制来防，不在本函数的兜底范围内。
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let plain = |text: Option<String>| {
            text.map(|text| ExtractedDoc {
                text,
                props: DocProps::default(),
            })
        };
        let office = |text: Option<String>| {
            text.map(|text| ExtractedDoc {
                text,
                props: fs::File::open(path)
                    .ok()
                    .and_then(|file| zip::ZipArchive::new(file).ok())
                    .map(|mut zip| office_props(&mut zip))
                    .unwrap_or_default(),
            })
        };
        match ext.as_str() {
            "pdf" => extract_pdf(path),
            "docx" | "docm" => office(extract_docx(fs::File::open(path).ok()?)),
            "xlsx" | "xlsm" | "xls" | "xlsb" => {
                office(extract_spreadsheet(BufReader::new(fs::File::open(path).ok()?), &ext))
            }
            "pptx" | "pptm" => office(extract_pptx(fs::File::open(path).ok()?)),
            "pst" | "ost" => crate::pst::extract_pst_archive(path).map(|a| ExtractedDoc {
                text: a.summary_text,
                props: DocProps::default(),
            }),
            "msg" => crate::pst::extract_msg(path),
            "eml" => crate::pst::extract_eml(path),
            "html" | "htm" => extract_html(path),
            "csv" | "tsv" => plain(extract_delimited_text(path)),
            e if TEXT_EXTS.contains(&e) => {
                plain(read_text_smart(path).map(|s| truncate_utf8(s, 512 * 1024)))
            }
            // 追加白名单里的扩展名当纯文本读（自动探测编码），跟内建文本类型同路。
            e if rules.is_extra_text_ext(e) => {
                plain(read_text_smart(path).map(|s| truncate_utf8(s, 512 * 1024)))
            }
            _ => None,
        }
    }))
    .ok()
    .flatten()
}

const MAX_OFFICE_EXPANDED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_OFFICE_XML_BYTES: u64 = 128 * 1024 * 1024;

fn office_archive<R: Read + Seek>(input: R, limit: u64) -> Option<zip::ZipArchive<R>> {
    let archive = zip::ZipArchive::new(input).ok()?;
    // The central directory has final sizes even when entries use data descriptors.
    let mut remaining = limit;
    for i in 0..archive.len() {
        remaining = remaining.checked_sub(archive.by_index_data(i).ok()?.size())?;
    }
    Some(archive)
}

fn read_office_xml(entry: impl Read) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    entry.take(MAX_OFFICE_XML_BYTES + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= MAX_OFFICE_XML_BYTES).then_some(bytes)
}

pub(crate) fn attachment_extension(name: &str) -> Option<String> {
    let ext = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "pdf" | "docx" | "docm" | "xlsx" | "xlsm" | "xls" | "xlsb" | "pptx" | "pptm" | "txt" | "md"
    ).then_some(ext)
}

pub(crate) fn extract_attachment(
    ext: &str,
    bytes: &[u8],
    max_text_bytes: usize,
) -> Option<ExtractedDoc> {
    std::panic::catch_unwind(|| {
        let mut props = DocProps::default();
        if matches!(ext, "docx" | "docm" | "xlsx" | "xlsm" | "xlsb" | "pptx" | "pptm") {
            let mut zip = office_archive(Cursor::new(bytes), 128 * 1024 * 1024)?;
            props = office_props(&mut zip);
        }
        let text = match ext {
            "pdf" => {
                let mut doc = pdf_extract::Document::load_mem(bytes).ok()?;
                if doc.is_encrypted() {
                    doc.decrypt("").ok()?;
                }
                props = pdf_props(&doc);
                extract_pdf_pages(&doc, max_text_bytes)
            }
            "docx" | "docm" => extract_docx(Cursor::new(bytes)).unwrap_or_default(),
            "xlsx" | "xlsm" | "xls" | "xlsb" => {
                extract_spreadsheet(Cursor::new(bytes), ext).unwrap_or_default()
            }
            "pptx" | "pptm" => extract_pptx(Cursor::new(bytes)).unwrap_or_default(),
            "txt" | "md" => decode_bytes_with_codepage(bytes, 0),
            _ => return None,
        };
        Some(ExtractedDoc { text: truncate_utf8(text, max_text_bytes), props })
    }).ok().flatten()
}

fn truncate_utf8(mut s: String, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s;
    }
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s
}

/// 从 PDF 抽文本和 Info 속성。单页解析失败时仅跳过该页，保留其余正常页面的文本。
fn extract_pdf(path: &Path) -> Option<ExtractedDoc> {
    let mut doc = pdf_extract::Document::load(path).ok()?;
    if doc.is_encrypted() {
        doc.decrypt("").ok()?;
    }
    Some(ExtractedDoc {
        text: extract_pdf_pages(&doc, usize::MAX),
        props: pdf_props(&doc),
    })
}

fn extract_pdf_pages(doc: &pdf_extract::Document, max_text_bytes: usize) -> String {
    let mut text = String::new();
    for number in doc.get_pages().keys() {
        let mut page = String::new();
        if pdf_extract::output_doc_page(
            doc,
            &mut pdf_extract::PlainTextOutput::new(&mut page),
            *number,
        )
        .is_ok()
            && !page.trim().is_empty()
        {
            text.push_str(&format!("## 페이지 {number}\n{page}\n\n"));
            if text.len() >= max_text_bytes {
                break;
            }
        }
    }
    text
}

/// PDF trailer의 `/Info` 사전에서 문서 속성을 읽는다.
fn pdf_props(doc: &pdf_extract::Document) -> DocProps {
    let Ok(info) = doc
        .trailer
        .get_deref(b"Info", doc)
        .and_then(|o| o.as_dict())
    else {
        return DocProps::default();
    };
    let text = |key: &[u8]| {
        info.get_deref(key, doc)
            .ok()
            .and_then(|o| o.as_str().ok())
            .map(decode_pdf_text_string)
            .and_then(non_empty)
    };
    DocProps {
        author: text(b"Author"),
        title: text(b"Title"),
        created: text(b"CreationDate").and_then(|s| parse_pdf_date(&s)),
        modified: text(b"ModDate").and_then(|s| parse_pdf_date(&s)),
    }
}

/// PDF text string 디코딩: UTF-16BE(BOM `FE FF`), UTF-8(BOM `EF BB BF`, PDF 2.0),
/// 그 외는 PDFDocEncoding — ASCII 범위는 같고 나머지는 Latin-1로 근사한다.
fn decode_pdf_text_string(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_be_bytes(pair))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// docx/xlsx/pptx 공통: `docProps/core.xml`에서 속성을 읽는다. 항목이 없으면 기본값.
fn office_props(archive: &mut zip::ZipArchive<impl Read + Seek>) -> DocProps {
    let Some(xml) = (|| {
        let entry = archive.by_name("docProps/core.xml").ok()?;
        let mut xml = Vec::new();
        entry.take(256 * 1024 + 1).read_to_end(&mut xml).ok()?;
        (xml.len() <= 256 * 1024).then_some(xml)
    })() else {
        return DocProps::default();
    };
    let field = |tag: &[u8]| non_empty(extract_tagged_text(&xml, tag, None));
    DocProps {
        author: field(b"creator"),
        title: field(b"title"),
        created: field(b"created").and_then(|s| parse_w3c_datetime(&s)),
        modified: field(b"modified").and_then(|s| parse_w3c_datetime(&s)),
    }
}

fn non_empty(s: String) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// 그레고리력 날짜 → 1970-01-01 기준 일수 (Howard Hinnant의 days_from_civil).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 각 구성요소 범위를 확인하고 UTC 밀리초로 바꾼다. `offset_min`은 현지 시각의
/// UTC 오프셋(분)이라 빼 준다.
fn to_epoch_millis(
    (y, mo, d, h, mi, s): (i64, i64, i64, i64, i64, i64),
    millis: i64,
    offset_min: i64,
) -> Option<i64> {
    if !(1..=12).contains(&mo)
        || !(1..=31).contains(&d)
        || !(0..=23).contains(&h)
        || !(0..=59).contains(&mi)
        || !(0..=60).contains(&s)
    {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + h * 3_600 + mi * 60 + s - offset_min * 60;
    Some(secs * 1_000 + millis)
}

/// 앞에서 숫자 `n`자리를 읽는다.
fn take_digits(s: &mut &str, n: usize) -> Option<i64> {
    let head = s.get(..n)?;
    if !head.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    *s = &s[n..];
    head.parse().ok()
}

/// W3CDTF(`dcterms:created`/`modified`의 형식, ISO 8601의 부분집합):
/// `YYYY`, `YYYY-MM`, `YYYY-MM-DD`, `YYYY-MM-DDThh:mm[:ss[.s+]](Z|±hh:mm)`.
/// 시간대가 없으면 UTC로 본다.
fn parse_w3c_datetime(s: &str) -> Option<i64> {
    let mut rest = s.trim();
    let y = take_digits(&mut rest, 4)?;
    let mut parts = (y, 1, 1, 0, 0, 0);
    let mut millis = 0;
    let mut offset = 0;
    if let Some(r) = rest.strip_prefix('-') {
        rest = r;
        parts.1 = take_digits(&mut rest, 2)?;
        if let Some(r) = rest.strip_prefix('-') {
            rest = r;
            parts.2 = take_digits(&mut rest, 2)?;
            if let Some(r) = rest.strip_prefix(['T', 't', ' ']) {
                rest = r;
                parts.3 = take_digits(&mut rest, 2)?;
                rest = rest.strip_prefix(':')?;
                parts.4 = take_digits(&mut rest, 2)?;
                if let Some(r) = rest.strip_prefix(':') {
                    rest = r;
                    parts.5 = take_digits(&mut rest, 2)?;
                    if let Some(r) = rest.strip_prefix('.') {
                        let frac_len = r.bytes().take_while(u8::is_ascii_digit).count();
                        if frac_len == 0 {
                            return None;
                        }
                        let frac = &r[..frac_len];
                        let padded = format!("{:0<3}", &frac[..frac.len().min(3)]);
                        millis = padded.parse().ok()?;
                        rest = &r[frac_len..];
                    }
                }
                if let Some(r) = rest.strip_prefix(['Z', 'z']) {
                    rest = r;
                } else if let Some(sign) = rest.chars().next().filter(|c| *c == '+' || *c == '-') {
                    rest = &rest[1..];
                    let oh = take_digits(&mut rest, 2)?;
                    rest = rest.strip_prefix(':').unwrap_or(rest);
                    let om = take_digits(&mut rest, 2)?;
                    offset = (oh * 60 + om) * if sign == '-' { -1 } else { 1 };
                }
            }
        }
    }
    if !rest.is_empty() {
        return None;
    }
    to_epoch_millis(parts, millis, offset)
}

/// PDF 날짜 문자열(PDF 32000-1 §7.9.4): `D:YYYYMMDDHHmmSSOHH'mm'`.
/// `D:` 접두어와 연 이후의 모든 부분은 생략 가능하고, `O`는 `Z`/`+`/`-`.
/// 오프셋이 없으면 UTC로 본다. 일부 생성기가 붙이는 끝의 `'`도 허용.
fn parse_pdf_date(s: &str) -> Option<i64> {
    let mut rest = s.trim();
    rest = rest.strip_prefix("D:").unwrap_or(rest);
    let y = take_digits(&mut rest, 4)?;
    let mut parts = (y, 1, 1, 0, 0, 0);
    let mut offset = 0;
    let fields: [&mut i64; 5] = [
        &mut parts.1,
        &mut parts.2,
        &mut parts.3,
        &mut parts.4,
        &mut parts.5,
    ];
    for slot in fields {
        if !rest.starts_with(|c: char| c.is_ascii_digit()) {
            break;
        }
        *slot = take_digits(&mut rest, 2)?;
    }
    if let Some(sign) = rest.chars().next() {
        match sign {
            'Z' | 'z' => rest = &rest[1..],
            '+' | '-' => {
                rest = &rest[1..];
                let oh = take_digits(&mut rest, 2)?;
                rest = rest.strip_prefix('\'').unwrap_or(rest);
                let om = take_digits(&mut rest, 2).unwrap_or(0);
                offset = (oh * 60 + om) * if sign == '-' { -1 } else { 1 };
            }
            _ => return None,
        }
        rest = rest.trim_end_matches('\'');
    }
    if !rest.is_empty() {
        return None;
    }
    to_epoch_millis(parts, 0, offset)
}

/// docx/docm 본문(`word/document.xml`)과 부속 파트(머리글·바닥글·각주·미주·메모)를 추출한다.
fn extract_docx(input: impl Read + Seek) -> Option<String> {
    let mut archive = office_archive(input, MAX_OFFICE_EXPANDED_BYTES)?;
    let doc_bytes = read_office_xml(archive.by_name("word/document.xml").ok()?)?;
    let mut parts = Vec::new();
    let main_text = extract_tagged_text(&doc_bytes, b"t", Some(b"p"));
    let main_trimmed = main_text.trim();
    if !main_trimmed.is_empty() {
        parts.push(main_trimmed.to_string());
    }

    let extra_names: Vec<String> = archive
        .file_names()
        .filter_map(Result::ok)
        .filter(|n| {
            n.ends_with(".xml")
                && (n.starts_with("word/header")
                    || n.starts_with("word/footer")
                    || *n == "word/footnotes.xml"
                    || *n == "word/endnotes.xml"
                    || *n == "word/comments.xml")
        })
        .map(std::borrow::Cow::into_owned)
        .collect();

    for name in extra_names {
        if let Some(buf) = archive.by_name(&name).ok().and_then(read_office_xml) {
            let extra = extract_tagged_text(&buf, b"t", Some(b"p"));
            let extra_trimmed = extra.trim();
            if !extra_trimmed.is_empty() {
                parts.push(extra_trimmed.to_string());
            }
        }
    }

    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn is_numeric_or_timestamp_str(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return true;
    }
    if trimmed.parse::<f64>().is_ok() {
        return true;
    }
    // Common numeric/timestamp/hex/unit forms like "2026-11-19 10:20:30", "98.4%", "#N/A"
    let mut digits = 0usize;
    let mut letters = 0usize;
    for ch in trimmed.chars() {
        if ch.is_ascii_digit() {
            digits += 1;
        } else if ch.is_alphabetic() {
            letters += 1;
        }
    }
    digits > 0 && letters <= 1 && trimmed.len() <= 32
}

/// 시트 단위로 '문서형 시트'와 'RAW 데이터 테이블 시트'를 자동 판별해 추출한다.
/// - 문서형 시트(소규모 표, 회의록, 체크리스트, 사양서 등): 빈 셀을 제거하고 본문 전체를 보존.
/// - RAW 데이터 시트(수백~수십만 행의 측정치·로그·DB 테이블): 시트명 + 헤더 행 + 고유 문자열 범주 + 상위 표본 행만 요약 색인.
fn extract_spreadsheet(mut input: impl std::io::BufRead + Seek, ext: &str) -> Option<String> {
    use calamine::{Data, Reader, Sheets, Xls, Xlsb, Xlsx};
    use std::collections::HashSet;

    const MAX_SCAN_ROWS: usize = 50_000;
    const DOC_SHEET_MAX_BYTES: usize = 200_000;
    const MAX_UNIQUE_CATEGORIES: usize = 120;

    if ext != "xls" {
        office_archive(&mut input, MAX_OFFICE_EXPANDED_BYTES)?;
        input.rewind().ok()?;
    }

    let book = match ext {
        "xls" => Xls::new(&mut input).map(Sheets::Xls).ok(),
        "xlsb" => Xlsb::new(&mut input).map(Sheets::Xlsb).ok(),
        _ => Xlsx::new(&mut input).map(Sheets::Xlsx).ok(),
    };
    if let Some(mut book) = book {
        let mut out = String::new();
        for sheet in book.sheet_names() {
            let Ok(range) = book.worksheet_range(&sheet) else {
                continue;
            };

            let mut non_empty_rows = 0usize;
            let mut max_cols = 0usize;
            let mut numeric_cells = 0usize;
            let mut prose_cells = 0usize;

            // Sample up to 350 non-empty rows first to classify cleanly while scanning.
            for row in range.rows().take(MAX_SCAN_ROWS) {
                let mut row_non_empty = 0usize;
                for cell in row {
                    match cell {
                        Data::Empty => {}
                        Data::Int(_)
                        | Data::Float(_)
                        | Data::Bool(_)
                        | Data::DateTime(_)
                        | Data::DateTimeIso(_)
                        | Data::DurationIso(_)
                        | Data::Error(_) => {
                            row_non_empty += 1;
                            numeric_cells += 1;
                        }
                        Data::String(s) => {
                            let t = s.trim();
                            if !t.is_empty() {
                                row_non_empty += 1;
                                if is_numeric_or_timestamp_str(t) {
                                    numeric_cells += 1;
                                } else {
                                    prose_cells += 1;
                                }
                            }
                        }
                    }
                }
                if row_non_empty > 0 {
                    non_empty_rows += 1;
                    max_cols = max_cols.max(row_non_empty);
                }
            }

            if non_empty_rows == 0 {
                continue;
            }

            let total_cells = (numeric_cells + prose_cells).max(1);
            let numeric_ratio = numeric_cells as f64 / total_cells as f64;
            let is_raw_table = non_empty_rows > 800
                || (non_empty_rows > 150 && numeric_ratio >= 0.65)
                || (total_cells > 4_000 && numeric_ratio >= 0.70);

            if !is_raw_table {
                out.push_str(&format!("\n## {sheet}\n"));
                let sheet_start = out.len();
                for row in range.rows() {
                    let cells: Vec<String> = row
                        .iter()
                        .filter_map(|c| match c {
                            Data::Empty => None,
                            other => {
                                let s = other.to_string();
                                let t = s.trim();
                                (!t.is_empty()).then(|| t.to_string())
                            }
                        })
                        .collect();
                    if !cells.is_empty() {
                        out.push_str(&cells.join("\t"));
                        out.push('\n');
                        if out.len() - sheet_start >= DOC_SHEET_MAX_BYTES {
                            break;
                        }
                    }
                }
            } else {
                out.push_str(&format!(
                    "\n## {sheet} [데이터 시트 요약: {}행 × {}열, 수치 비중 {:.0}%]\n",
                    non_empty_rows,
                    max_cols,
                    numeric_ratio * 100.0
                ));

                let mut header_rows = Vec::new();
                let mut sample_rows = Vec::new();
                let mut categories = Vec::new();
                let mut seen_cat = HashSet::new();

                for row in range.rows().take(5_000) {
                    let non_empty: Vec<&Data> =
                        row.iter().filter(|c| !matches!(c, Data::Empty)).collect();
                    if non_empty.is_empty() {
                        continue;
                    }
                    if header_rows.len() < 3 {
                        let line = non_empty
                            .iter()
                            .take(64)
                            .map(|c| c.to_string())
                            .collect::<Vec<_>>()
                            .join("\t");
                        header_rows.push(line);
                    } else if sample_rows.len() < 5 {
                        let line = non_empty
                            .iter()
                            .take(16)
                            .map(|c| c.to_string())
                            .collect::<Vec<_>>()
                            .join("\t");
                        sample_rows.push(line);
                    }

                    if categories.len() < MAX_UNIQUE_CATEGORIES {
                        for c in non_empty {
                            if let Data::String(s) = c {
                                let t = s.trim();
                                let char_cnt = t.chars().count();
                                if (2..=80).contains(&char_cnt)
                                    && !is_numeric_or_timestamp_str(t)
                                    && seen_cat.insert(t.to_lowercase())
                                {
                                    categories.push(t.to_string());
                                    if categories.len() >= MAX_UNIQUE_CATEGORIES {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }

                if !header_rows.is_empty() {
                    out.push_str("컬럼/헤더:\n");
                    for h in header_rows {
                        out.push_str(&h);
                        out.push('\n');
                    }
                }
                if !categories.is_empty() {
                    out.push_str("주요 항목: ");
                    out.push_str(&categories.join(", "));
                    out.push('\n');
                }
                if !sample_rows.is_empty() {
                    out.push_str("표본 행:\n");
                    for sr in sample_rows {
                        out.push_str(&sr);
                        out.push('\n');
                    }
                }
            }
        }
        let trimmed = out.trim();
        if !trimmed.is_empty() {
            return Some(out);
        }
    }

    if !matches!(ext, "xlsx" | "xlsm") {
        return None;
    }

    // Minimal/corrupted OOXML zip fallback when calamine workbook metadata is incomplete.
    input.seek(SeekFrom::Start(0)).ok()?;
    let mut archive = office_archive(input, MAX_OFFICE_EXPANDED_BYTES)?;
    let mut text = String::new();

    if let Some(bytes) = archive.by_name("xl/sharedStrings.xml").ok().and_then(read_office_xml) {
        text.push_str(&extract_tagged_text(&bytes, b"t", None));
        text.push('\n');
    }

    let sheet_names: Vec<String> = archive
        .file_names()
        .filter_map(Result::ok)
        .filter(|n| n.starts_with("xl/worksheets/sheet") && n.ends_with(".xml"))
        .map(std::borrow::Cow::into_owned)
        .collect();
    for name in sheet_names {
        let Some(bytes) = archive.by_name(&name).ok().and_then(read_office_xml) else {
            continue;
        };
        text.push_str(&extract_tagged_text(&bytes, b"t", None));
        text.push('\n');
    }

    let text = text.trim();
    (!text.is_empty()).then(|| truncate_utf8(text.to_string(), 256 * 1024))
}

fn extract_delimited_text(path: &Path) -> Option<String> {
    use std::collections::HashSet;

    let raw = read_text_smart(path)?;
    let non_empty_lines: Vec<&str> = raw.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if non_empty_lines.is_empty() {
        return None;
    }
    if non_empty_lines.len() <= 200 && raw.len() <= 64 * 1024 {
        return Some(raw);
    }

    let delim = if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("tsv"))
    {
        '\t'
    } else {
        ','
    };

    let mut num_cells = 0usize;
    let mut txt_cells = 0usize;
    for line in non_empty_lines.iter().take(300) {
        for tok in line.split(delim) {
            let t = tok.trim().trim_matches('"');
            if t.is_empty() {
                continue;
            }
            if is_numeric_or_timestamp_str(t) {
                num_cells += 1;
            } else {
                txt_cells += 1;
            }
        }
    }
    let ratio = num_cells as f64 / (num_cells + txt_cells).max(1) as f64;
    let is_raw_table = non_empty_lines.len() > 800 || (non_empty_lines.len() > 200 && ratio >= 0.65);
    if !is_raw_table {
        return Some(truncate_utf8(raw, 256 * 1024));
    }

    let mut out = format!(
        "[데이터 파일 요약: {}행, 수치 비중 {:.0}%]\n",
        non_empty_lines.len(),
        ratio * 100.0
    );
    out.push_str("컬럼/헤더:\n");
    for h in non_empty_lines.iter().take(3) {
        out.push_str(&truncate_utf8((*h).to_string(), 1024));
        out.push('\n');
    }

    let mut categories = Vec::new();
    let mut seen = HashSet::new();
    for line in non_empty_lines.iter().skip(1).take(5_000) {
        for tok in line.split(delim) {
            let t = tok.trim().trim_matches('"');
            let char_cnt = t.chars().count();
            if (2..=80).contains(&char_cnt)
                && !is_numeric_or_timestamp_str(t)
                && seen.insert(t.to_lowercase())
            {
                categories.push(t.to_string());
                if categories.len() >= 120 {
                    break;
                }
            }
        }
        if categories.len() >= 120 {
            break;
        }
    }
    if !categories.is_empty() {
        out.push_str("주요 항목: ");
        out.push_str(&categories.join(", "));
        out.push('\n');
    }
    out.push_str("표본 행:\n");
    for s in non_empty_lines.iter().skip(3).take(5) {
        out.push_str(&truncate_utf8((*s).to_string(), 512));
        out.push('\n');
    }
    Some(out)
}

pub(crate) fn clean_html_fragment(raw: &str) -> String {
    extract_html_str(raw).map(|(text, _)| text).unwrap_or_default()
}

fn extract_html(path: &Path) -> Option<ExtractedDoc> {
    let raw = read_text_smart(path)?;
    let (text, title) = extract_html_str(&raw)?;
    Some(ExtractedDoc {
        text,
        props: DocProps {
            title,
            ..DocProps::default()
        },
    })
}

fn extract_html_str(raw: &str) -> Option<(String, Option<String>)> {
    let bytes = raw.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(raw.len().min(128 * 1024));
    let mut title_buf = String::new();
    let mut in_title = false;
    let mut i = 0usize;

    while i < len {
        if bytes[i] == b'<' {
            if raw[i..].starts_with("<!--") {
                if let Some(end) = raw[i + 4..].find("-->") {
                    i += 4 + end + 3;
                    continue;
                }
                break;
            }
            let Some(rel_close) = raw[i + 1..].find('>') else {
                break;
            };
            let tag_inner = raw[i + 1..i + 1 + rel_close].trim();
            let is_closing = tag_inner.starts_with('/');
            let name_slice = tag_inner.trim_start_matches('/');
            let tag_name: String = name_slice
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .map(|c| c.to_ascii_lowercase())
                .collect();
            let self_closing = tag_inner.ends_with('/');
            i += 1 + rel_close + 1;

            if !is_closing
                && !self_closing
                && matches!(tag_name.as_str(), "script" | "style" | "noscript" | "svg")
            {
                let needle = format!("</{tag_name}");
                let rest_lower = raw[i..].to_ascii_lowercase();
                if let Some(pos) = rest_lower.find(&needle) {
                    i += pos;
                    if let Some(gt) = raw[i..].find('>') {
                        i += gt + 1;
                    }
                    continue;
                }
                break;
            }

            if tag_name == "title" {
                in_title = !is_closing && !self_closing;
                continue;
            }

            if matches!(
                tag_name.as_str(),
                "p" | "div"
                    | "br"
                    | "tr"
                    | "li"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "section"
                    | "article"
                    | "header"
                    | "footer"
                    | "blockquote"
                    | "pre"
                    | "hr"
            ) {
                if !out.ends_with('\n') && !out.is_empty() {
                    out.push('\n');
                }
            } else if matches!(tag_name.as_str(), "td" | "th")
                && is_closing
                && !out.ends_with(['\t', '\n'])
            {
                out.push('\t');
            }
            continue;
        }

        let next_lt = raw[i..].find('<').map(|p| i + p).unwrap_or(len);
        let decoded = decode_html_entities(&raw[i..next_lt]);
        if in_title {
            title_buf.push_str(&decoded);
        } else {
            for ch in decoded.chars() {
                if ch == '\r' {
                    continue;
                }
                if ch == '\n' {
                    if !out.ends_with("\n\n") && !out.is_empty() {
                        out.push('\n');
                    }
                } else if ch.is_whitespace() {
                    if !out.ends_with([' ', '\t', '\n']) && !out.is_empty() {
                        out.push(' ');
                    }
                } else {
                    out.push(ch);
                }
            }
        }
        i = next_lt;
        if out.len() >= 384 * 1024 {
            break;
        }
    }

    let text = out.trim().to_string();
    if text.is_empty() && title_buf.trim().is_empty() {
        return None;
    }
    let final_text = if text.is_empty() {
        title_buf.trim().to_string()
    } else {
        text
    };
    Some((final_text, non_empty(title_buf)))
}

fn decode_html_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        if let Some(semi) = after.bytes().take(12).position(|b| b == b';') {
            let ent = &after[..semi];
            let replaced = match ent {
                "nbsp" | "#160" | "#xa0" | "#xA0" => Some(' '),
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                    u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
                }
                _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
                _ => None,
            };
            if let Some(ch) = replaced {
                out.push(ch);
                rest = &after[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = after;
    }
    out.push_str(rest);
    out
}

/// pptx/pptm 슬라이드 본문(`ppt/slides/slideN.xml`), 발표자 노트(`notesSlideN.xml`), 다이어그램(`diagrams/data*.xml`) 추출.
fn extract_pptx(input: impl Read + Seek) -> Option<String> {
    let mut archive = office_archive(input, MAX_OFFICE_EXPANDED_BYTES)?;

    let mut slide_nums: Vec<(u32, String)> = archive
        .file_names()
        .filter_map(Result::ok)
        .filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"))
        .map(|n| {
            let num = n
                .trim_start_matches("ppt/slides/slide")
                .trim_end_matches(".xml")
                .parse::<u32>()
                .unwrap_or(u32::MAX);
            (num, n.into_owned())
        })
        .collect();
    slide_nums.sort_by_key(|(num, _)| *num);

    let diagram_names: Vec<String> = archive
        .file_names()
        .filter_map(Result::ok)
        .filter(|n| n.starts_with("ppt/diagrams/data") && n.ends_with(".xml"))
        .map(std::borrow::Cow::into_owned)
        .collect();

    let mut text = String::new();
    for (idx, (slide_num, name)) in slide_nums.into_iter().enumerate() {
        text.push_str(&format!("\n## 슬라이드 {}\n", idx + 1));
        if let Some(bytes) = archive.by_name(&name).ok().and_then(read_office_xml) {
            text.push_str(&extract_tagged_text(&bytes, b"t", Some(b"p")));
            text.push('\n');
        }
        if slide_num != u32::MAX {
            let note_name = format!("ppt/notesSlides/notesSlide{slide_num}.xml");
            if let Some(note_bytes) = archive.by_name(&note_name).ok().and_then(read_office_xml) {
                let note_txt = extract_tagged_text(&note_bytes, b"t", Some(b"p"));
                let trimmed = note_txt.trim();
                if !trimmed.is_empty() {
                    text.push_str(trimmed);
                    text.push('\n');
                }
            }
        }
    }

    for diag in diagram_names {
        if let Some(bytes) = archive.by_name(&diag).ok().and_then(read_office_xml) {
            let diag_txt = extract_tagged_text(&bytes, b"t", Some(b"p"));
            let trimmed = diag_txt.trim();
            if !trimmed.is_empty() {
                text.push('\n');
                text.push_str(trimmed);
                text.push('\n');
            }
        }
    }

    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// 流式扫一遍 XML 事件，抠出本地标签名匹配 text_tag 的节点里的文本
/// （按本地名匹配、忽略命名空间前缀，`w:t`/`a:t`/无前缀的 `t` 都能命中）。
/// 不建 DOM 树：Office XML 展开后可能几十万字符，没必要整棵放内存。
/// para_tag 给定时，每闭合一个该标签就补换行，用来标记段落/幻灯片内的换行边界。
/// XML 本身损坏时中途跳出循环，返回已经攒到的文本——调用方那层再统一按
/// "抽不出内容就是 None" 处理，不在这里 panic 或者往外抛错误。
fn extract_tagged_text(xml: &[u8], text_tag: &[u8], para_tag: Option<&[u8]>) -> String {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);

    let mut out = String::new();
    let mut in_text = false;

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                if e.local_name().as_ref() == text_tag {
                    in_text = true;
                }
            }
            Ok(Event::Text(t)) => {
                if in_text
                    && let Ok(decoded) = t.decode()
                    && let Ok(unescaped) = quick_xml::escape::unescape(&decoded)
                {
                    out.push_str(&unescaped);
                }
            }
            // quick-xml 0.38 起，`&amp;` / `&#54637;` 这类实体与字符引用不再并进
            // Text 事件，而是单独作为 GeneralRef 事件抛出。不处理的话 `R&D` 会变成
            // `RD`，而 openpyxl 等工具把非 ASCII 字符一律写成数字字符引用时，
            // 整格的中文/韩文正文会全部丢失。
            Ok(Event::GeneralRef(r)) => {
                if in_text {
                    if let Ok(Some(ch)) = r.resolve_char_ref() {
                        out.push(ch);
                    } else if let Ok(name) = r.decode()
                        && let Some(s) = quick_xml::escape::resolve_predefined_entity(&name)
                    {
                        out.push_str(s);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = e.local_name();
                if name.as_ref() == text_tag {
                    in_text = false;
                } else if para_tag.is_some_and(|p| name.as_ref() == p) {
                    out.push('\n');
                }
            }
            Ok(Event::Empty(e)) => {
                if para_tag.is_some_and(|p| e.local_name().as_ref() == p) {
                    out.push('\n');
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    out
}

/// 读文本文件，自动探测编码。
/// Windows 上的中文 txt 很多是 GBK 而不是 UTF-8，直接按 UTF-8 读会变乱码，
/// 所以先用 chardetng 猜编码，再用 encoding_rs 解码成 UTF-8。
pub(crate) fn decode_bytes_with_codepage(bytes: &[u8], codepage: u32) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let explicit_enc = match codepage {
        65001 => Some(encoding_rs::UTF_8),
        949 | 51949 => Some(encoding_rs::EUC_KR),
        936 | 54936 => Some(encoding_rs::GB18030),
        950 => Some(encoding_rs::BIG5),
        932 => Some(encoding_rs::SHIFT_JIS),
        1252 | 28591 => Some(encoding_rs::WINDOWS_1252),
        _ => None,
    };
    if let Some(enc) = explicit_enc {
        let (cow, _, _) = enc.decode(bytes);
        return cow.into_owned();
    }

    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
    detector.feed(bytes, true);
    let encoding = detector.guess(None, chardetng::Utf8Detection::Allow);
    let (text, _, had_errors) = encoding.decode(bytes);
    if had_errors && encoding != encoding_rs::UTF_8 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    text.into_owned()
}

fn read_text_smart(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(decode_bytes_with_codepage(&bytes, 0))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::write::SimpleFileOptions;

    use super::*;

    /// 程序化拼一个最小合法 zip 包，条目名到内容的映射即够——
    /// Office 文件本质就是 zip，抽取只关心我们读的那几个条目存在且是合法 XML。
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

    #[test]
    fn office_archive_limits_total_expanded_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.docx");
        let parts = [
            ("word/document.xml", "<t>Document</t>"),
            ("word/header1.xml", "<t>Header</t>"),
        ];
        write_zip(&path, &parts);
        let size = parts.iter().map(|(_, text)| text.len() as u64).sum::<u64>();
        assert!(office_archive(fs::File::open(&path).unwrap(), size - 1).is_none());
        assert!(office_archive(fs::File::open(&path).unwrap(), size).is_some());
    }

    #[test]
    fn extracts_docx_paragraph_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.docx");
        let document_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:r><w:t>季度对账哨兵词</w:t></w:r></w:p>
    <w:p><w:r><w:t>quarterly-sentinel</w:t></w:r></w:p>
  </w:body>
</w:document>"#;
        write_zip(&path, &[("word/document.xml", document_xml)]);

        let text = extract_text(&path).expect("docx 应能抽出文本");
        assert!(text.contains("季度对账哨兵词"));
        assert!(text.contains("quarterly-sentinel"));
    }

    #[test]
    fn extracts_xlsx_shared_and_inline_strings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.xlsx");
        let shared_strings = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="1" uniqueCount="1">
  <si><t>季度对账哨兵词</t></si>
</sst>"#;
        let sheet1 = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1" t="s"><v>0</v></c>
      <c r="B1" t="inlineStr"><is><t>quarterly-sentinel</t></is></c>
    </row>
  </sheetData>
</worksheet>"#;
        write_zip(
            &path,
            &[
                ("xl/sharedStrings.xml", shared_strings),
                ("xl/worksheets/sheet1.xml", sheet1),
            ],
        );

        let text = extract_text(&path).expect("xlsx 应能抽出文本");
        assert!(text.contains("季度对账哨兵词"));
        assert!(text.contains("quarterly-sentinel"));
    }

    /// openpyxl 写内联字符串时把非 ASCII 字符全部转成数字字符引用；
    /// 预定义实体（`&amp;` 等）在任何 Office 文件里都很常见。两者都必须还原。
    #[test]
    fn xlsx_numeric_char_refs_and_entities_are_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refs.xlsx");
        let sheet1 = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1" t="inlineStr"><is><t>&#45257;&#44033;&#49688;&#50728;&#46020;</t></is></c>
      <c r="B1" t="inlineStr"><is><t>R&amp;D &lt;x&gt; &#x41;</t></is></c>
    </row>
  </sheetData>
</worksheet>"#;
        write_zip(&path, &[("xl/worksheets/sheet1.xml", sheet1)]);

        let text = extract_text(&path).expect("xlsx 应能抽出文本");
        assert!(text.contains("냉각수온도"), "数字字符引用应还原: {text:?}");
        assert!(text.contains("R&D <x> A"), "预定义实体应还原: {text:?}");
    }

    #[test]
    fn docx_entities_inside_text_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refs.docx");
        let document_xml = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>R&amp;D 배터리&#54057;</w:t></w:r></w:p></w:body></w:document>"#;
        write_zip(&path, &[("word/document.xml", document_xml)]);

        let text = extract_text(&path).expect("docx 应能抽出文本");
        assert_eq!(text, "R&D 배터리팩");
    }

    #[test]
    fn extracts_pptx_slide_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.pptx");
        let slide1 = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:sp><p:txBody><a:p><a:r><a:t>季度对账哨兵词</a:t></a:r></a:p></p:txBody></p:sp>
      <p:sp><p:txBody><a:p><a:r><a:t>quarterly-sentinel</a:t></a:r></a:p></p:txBody></p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;
        write_zip(&path, &[("ppt/slides/slide1.xml", slide1)]);

        let text = extract_text(&path).expect("pptx 应能抽出文本");
        assert!(text.contains("季度对账哨兵词"));
        assert!(text.contains("quarterly-sentinel"));
    }

    #[test]
    fn corrupted_docx_is_skipped_without_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.docx");
        fs::write(&path, b"this is not a zip file at all").unwrap();

        assert!(extract_text(&path).is_none());
    }

    // --- 编码探测（read_text_smart）---

    #[test]
    fn gbk_encoded_chinese_txt_is_decoded_to_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gbk_sample.txt");
        // 一段足够长的中文，探测器才有把握判成 GBK 而不是别的单字节编码。
        let content = "这是一段用于编码探测的中文测试文本，\
包含足够多的汉字以便字符集识别器把它判定为国标编码而不是别的编码，\
季度对账哨兵词也在这里出现。";
        let (gbk_bytes, _, unmappable) = encoding_rs::GBK.encode(content);
        assert!(!unmappable, "测试文本应能完整编码成 GBK");
        fs::write(&path, &gbk_bytes).unwrap();

        let text = read_text_smart(&path).expect("GBK 文件应能解码出文本");
        assert!(text.contains("季度对账哨兵词"), "应还原出原始中文: {text}");
        assert!(text.contains("国标编码"));
    }

    #[test]
    fn utf8_bom_file_is_decoded_without_leading_bom_char() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bom_sample.txt");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("带 BOM 的内容 marker-bom".as_bytes());
        fs::write(&path, &bytes).unwrap();

        let text = read_text_smart(&path).expect("带 BOM 的 UTF-8 文件应能解码");
        assert!(text.contains("marker-bom"));
        assert!(text.contains("带 BOM 的内容"));
        assert!(!text.starts_with('\u{FEFF}'), "BOM 应被剥掉，不进正文");
    }

    #[test]
    fn empty_text_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty_sample.txt");
        fs::write(&path, b"").unwrap();
        assert!(read_text_smart(&path).is_none());
    }

    #[test]
    fn file_over_size_limit_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized_sample.txt");
        let rules = IndexRules::default();
        // 用 set_len 造一个刚超过上限的稀疏文件，只撑大小、不真写 20MB 数据。
        let f = fs::File::create(&path).unwrap();
        f.set_len(rules.max_file_bytes() + 1).unwrap();
        drop(f);
        assert!(
            extract_text_with(&path, &rules).is_none(),
            "超过体积上限的文件应被跳过"
        );
    }

    #[test]
    fn custom_lower_size_limit_skips_file_default_would_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mid.txt");
        // 造一个 2MB 的文件：默认 20MB 上限下能抽取，但把上限调到 1MB 就该跳过。
        let f = fs::File::create(&path).unwrap();
        f.set_len(2 * 1024 * 1024).unwrap();
        drop(f);

        let default_rules = IndexRules::default();
        assert!(
            extract_text_with(&path, &default_rules).is_some(),
            "默认 20MB 上限下 2MB 文件应能抽取"
        );

        let tight = IndexRules {
            max_file_mb: 1,
            ..IndexRules::default()
        };
        assert!(
            extract_text_with(&path, &tight).is_none(),
            "上限收紧到 1MB 后 2MB 文件应被跳过"
        );
    }

    #[test]
    fn extra_text_ext_becomes_extractable_and_reads_as_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.rst");
        fs::write(&path, "追加扩展名内容 extra-ext-marker").unwrap();

        let default_rules = IndexRules::default();
        assert!(
            !is_extractable_with(&path, &default_rules),
            "默认规则下 .rst 不是可抽取类型"
        );
        assert!(
            extract_text_with(&path, &default_rules).is_none(),
            "默认规则下 .rst 抽不出文本"
        );

        let with_rst = IndexRules {
            extra_text_exts: vec!["rst".into()],
            ..IndexRules::default()
        };
        assert!(
            is_extractable_with(&path, &with_rst),
            "追加 rst 后 .rst 应算可抽取"
        );
        let text = extract_text_with(&path, &with_rst).expect("追加 rst 后应能抽出文本");
        assert!(text.contains("extra-ext-marker"));
    }

    #[test]
    fn misdetected_encoding_falls_back_to_lossy_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("misdetect_sample.txt");
        // 先放一大段合法 GBK 中文，让探测器有把握判成 GBK；结尾附一个非法的 GBK
        // 多字节序列（0x81 是前导字节，0x20 不是合法后继），逼 encoding_rs 解码时
        // had_errors=true，从而走 read_text_smart 里"猜错后有损回退"那条分支。
        let (gbk, _, _) = encoding_rs::GBK
            .encode("编码探测有损回退分支覆盖用例需要一段足够长的国标中文来稳定命中 GBK 判定");
        let mut buf = b"FALLBACK_MARKER_".to_vec();
        buf.extend_from_slice(&gbk);
        buf.extend_from_slice(&[0x81, 0x20]);
        fs::write(&path, &buf).unwrap();

        let text = read_text_smart(&path).expect("回退分支也要返回 Some");
        // 有损回退把整段字节当 UTF-8 读：ASCII 前缀存活，GBK 中文变成替换符，
        // 因此不会出现正确解出来的中文——以此证明走的是回退而非正常 GBK 解码。
        assert!(text.contains("FALLBACK_MARKER_"));
        assert!(
            !text.contains("国标中文"),
            "若走了正常 GBK 解码就会出现真中文，说明没命中回退分支: {text}"
        );
    }

    // --- 畸形 Office 文件 ---

    #[test]
    fn docx_missing_document_entry_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no_document.docx");
        // 合法 zip，但缺少 word/document.xml 这个正文条目。
        write_zip(&path, &[("docProps/core.xml", "<coreProperties/>")]);
        assert!(extract_text(&path).is_none());
    }

    #[test]
    fn docx_with_truncated_xml_keeps_text_before_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.docx");
        // 正文先给一段合法内容，再接一个对不上的结束标签，触发 XML 解析中途出错
        // （extract_tagged_text 里的 Err(_) => break 兜底分支）。
        let document_xml = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>partial-before-corruption</w:t></w:r></w:p></w:bogusEnd>"#;
        write_zip(&path, &[("word/document.xml", document_xml)]);

        let text = extract_text(&path).expect("损坏前已抽到的文本应保留");
        assert!(text.contains("partial-before-corruption"));
    }

    #[test]
    fn whitespace_only_docx_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blank.docx");
        let document_xml = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>    </w:t></w:r></w:p></w:body></w:document>"#;
        write_zip(&path, &[("word/document.xml", document_xml)]);
        assert!(extract_text(&path).is_none());
    }

    #[test]
    fn xlsx_skips_unreadable_sheet_and_processes_the_rest() {
        use zip::CompressionMethod;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial_sheets.xlsx");

        // sheet1 用 Stored（不压缩）写入，稍后直接改坏它的明文字节，读取时 CRC
        // 校验失败 → read_to_end 返回 Err → 命中 extract_xlsx 里的 continue 分支。
        // sheet2 正常，验证坏掉一个 sheet 不影响其它 sheet 的抽取。
        {
            let file = fs::File::create(&path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            zip.start_file("xl/worksheets/sheet1.xml", stored).unwrap();
            zip.write_all(
                br#"<worksheet><sheetData><row><c t="inlineStr"><is><t>CORRUPTME_SHEET1_UNIQUE</t></is></c></row></sheetData></worksheet>"#,
            )
            .unwrap();
            zip.start_file("xl/worksheets/sheet2.xml", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(
                br#"<worksheet><sheetData><row><c t="inlineStr"><is><t>survivor-sheet2-text</t></is></c></row></sheetData></worksheet>"#,
            )
            .unwrap();
            zip.finish().unwrap();
        }

        // 把 sheet1 的 Stored 明文里某个字节改掉，让它和存档记录的 CRC 对不上。
        let mut raw = fs::read(&path).unwrap();
        let needle = b"CORRUPTME_SHEET1_UNIQUE";
        let pos = raw
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("应能在 zip 明文里找到 sheet1 的标记");
        raw[pos] ^= 0xFF;
        fs::write(&path, &raw).unwrap();

        let text = extract_text(&path).expect("坏掉一个 sheet 后其它 sheet 仍应产出文本");
        assert!(
            text.contains("survivor-sheet2-text"),
            "健康 sheet 的文本应被抽出"
        );
        assert!(
            !text.contains("CORRUPTME_SHEET1_UNIQUE"),
            "坏掉的 sheet 内容不应出现在结果里"
        );
    }

    // --- 문서 속성 (schema v8) ---

    const CORE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
  xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/"
  xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <dc:title>R&amp;D 분기 보고서</dc:title>
  <dc:creator>&#54861;길동</dc:creator>
  <cp:lastModifiedBy>someone-else</cp:lastModifiedBy>
  <dcterms:created xsi:type="dcterms:W3CDTF">2024-03-05T01:02:03Z</dcterms:created>
  <dcterms:modified xsi:type="dcterms:W3CDTF">2024-03-06T10:00:00+09:00</dcterms:modified>
</cp:coreProperties>"#;

    const DOCX_BODY: &str = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>본문</w:t></w:r></w:p></w:body></w:document>"#;

    #[test]
    fn docx_core_properties_are_extracted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("props.docx");
        write_zip(
            &path,
            &[
                ("word/document.xml", DOCX_BODY),
                ("docProps/core.xml", CORE_XML),
            ],
        );
        let doc = extract_document_with(&path, &IndexRules::default()).expect("본문이 있어야 함");
        assert_eq!(doc.text, "본문");
        assert_eq!(doc.props.title.as_deref(), Some("R&D 분기 보고서"));
        assert_eq!(doc.props.author.as_deref(), Some("홍길동"));
        assert_eq!(doc.props.created, Some(1_709_600_523_000));
        // 2024-03-06T10:00:00+09:00 == 2024-03-06T01:00:00Z
        assert_eq!(doc.props.modified, Some(1_709_686_800_000));
    }

    #[test]
    fn xlsx_and_pptx_read_the_same_core_properties() {
        let dir = tempfile::tempdir().unwrap();
        let xlsx = dir.path().join("props.xlsx");
        write_zip(
            &xlsx,
            &[
                ("xl/sharedStrings.xml", "<sst><si><t>셀</t></si></sst>"),
                ("docProps/core.xml", CORE_XML),
            ],
        );
        let pptx = dir.path().join("props.pptx");
        write_zip(
            &pptx,
            &[
                (
                    "ppt/slides/slide1.xml",
                    "<p:sld xmlns:a=\"a\" xmlns:p=\"p\"><a:p><a:r><a:t>슬라이드</a:t></a:r></a:p></p:sld>",
                ),
                ("docProps/core.xml", CORE_XML),
            ],
        );
        for path in [xlsx, pptx] {
            let doc = extract_document_with(&path, &IndexRules::default()).unwrap();
            assert_eq!(doc.props.author.as_deref(), Some("홍길동"), "{path:?}");
            assert_eq!(doc.props.title.as_deref(), Some("R&D 분기 보고서"));
        }
    }

    #[test]
    fn missing_or_empty_core_properties_yield_none() {
        let dir = tempfile::tempdir().unwrap();
        let no_core = dir.path().join("nocore.docx");
        write_zip(&no_core, &[("word/document.xml", DOCX_BODY)]);
        let doc = extract_document_with(&no_core, &IndexRules::default()).unwrap();
        assert_eq!(doc.props, DocProps::default());

        let empty = dir.path().join("empty.docx");
        let core = r#"<cp:coreProperties xmlns:cp="c" xmlns:dc="d" xmlns:dcterms="t"><dc:creator>  </dc:creator><dc:title/><dcterms:created>not a date</dcterms:created></cp:coreProperties>"#;
        write_zip(
            &empty,
            &[
                ("word/document.xml", DOCX_BODY),
                ("docProps/core.xml", core),
            ],
        );
        let doc = extract_document_with(&empty, &IndexRules::default()).unwrap();
        assert_eq!(doc.props, DocProps::default());

        // 텍스트 파일은 속성이 없다.
        let txt = dir.path().join("a.txt");
        fs::write(&txt, "plain").unwrap();
        let doc = extract_document_with(&txt, &IndexRules::default()).unwrap();
        assert_eq!(doc.props, DocProps::default());
    }

    #[test]
    fn w3c_datetime_variants() {
        assert_eq!(parse_w3c_datetime("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_w3c_datetime("1970-01-01"), Some(0));
        assert_eq!(parse_w3c_datetime("1970"), Some(0));
        assert_eq!(parse_w3c_datetime("1970-02"), Some(31 * 86_400_000));
        assert_eq!(
            parse_w3c_datetime("2000-02-29T12:00Z"),
            Some(951_825_600_000)
        );
        assert_eq!(
            parse_w3c_datetime("2024-03-05T01:02:03.5Z"),
            Some(1_709_600_523_500)
        );
        assert_eq!(
            parse_w3c_datetime("2024-03-05T01:02:03.123456-05:00"),
            Some(1_709_600_523_123 + 5 * 3_600_000)
        );
        assert_eq!(
            parse_w3c_datetime("2024-03-05T01:02:03"),
            Some(1_709_600_523_000)
        );
        for bad in [
            "",
            "abc",
            "2024-13-01",
            "2024-03-05T25:00Z",
            "2024-03-05Tjunk",
            "2024-03-05T01:02:03Zx",
        ] {
            assert_eq!(parse_w3c_datetime(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn pdf_date_variants() {
        assert_eq!(parse_pdf_date("D:19700101000000Z"), Some(0));
        assert_eq!(parse_pdf_date("D:1970"), Some(0));
        assert_eq!(parse_pdf_date("19700101"), Some(0));
        assert_eq!(
            parse_pdf_date("D:20240306100000+09'00'"),
            Some(1_709_686_800_000)
        );
        assert_eq!(
            parse_pdf_date("D:20240306100000+09'00"),
            Some(1_709_686_800_000)
        );
        assert_eq!(
            parse_pdf_date("D:20240305200000-05'00'"),
            Some(1_709_686_800_000)
        );
        for bad in ["", "D:", "D:20241301", "D:2024xx", "hello"] {
            assert_eq!(parse_pdf_date(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn pdf_text_string_encodings() {
        let mut utf16 = vec![0xFE, 0xFF];
        for u in "홍길동".encode_utf16() {
            utf16.extend_from_slice(&u.to_be_bytes());
        }
        assert_eq!(decode_pdf_text_string(&utf16), "홍길동");
        let mut utf8 = vec![0xEF, 0xBB, 0xBF];
        utf8.extend_from_slice("보고서".as_bytes());
        assert_eq!(decode_pdf_text_string(&utf8), "보고서");
        assert_eq!(decode_pdf_text_string(b"Jane Doe"), "Jane Doe");
    }

    #[test]
    fn csv_raw_numeric_table_is_summarized_while_preserving_headers_and_categories() {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("telemetry.csv");
        let mut content = String::from("Timestamp,Rack_ID,Cell_V,Temp_C,Current_A\n");
        for i in 0..300 {
            let rack = if i == 180 { "RACK_FAULT_07" } else { "RACK_NORMAL_01" };
            content.push_str(&format!("2026-11-19,{rack},3.7124,25.4,12.{i:03}\n"));
        }
        fs::write(&csv, content).unwrap();

        let text = extract_text(&csv).expect("CSV should extract summary");
        assert!(text.contains("데이터 파일 요약"), "Summary header missing: {text}");
        assert!(text.contains("Cell_V"), "Column header must remain searchable");
        assert!(text.contains("RACK_FAULT_07"), "Unique string category at row 180 must remain searchable");
        assert!(!text.contains("12.299"), "Trailing bulk numeric rows must not bloat index");
    }

    #[test]
    fn html_strips_scripts_styles_and_extracts_title() {
        let dir = tempfile::tempdir().unwrap();
        let html = dir.path().join("confluence_page.html");
        fs::write(
            &html,
            r#"<!DOCTYPE html><html><head><title>배터리 시뮬레이터 설계안 &amp; 검증</title>
            <style>.hidden-css-token { color: red; }</style>
            <script>const secretJsNoise = 99999;</script></head>
            <body><h1>아키텍처 결정</h1><p>실행 캐시를 도입한다.</p></body></html>"#,
        )
        .unwrap();

        let doc = extract_document_with(&html, &IndexRules::default()).unwrap();
        assert_eq!(
            doc.props.title.as_deref(),
            Some("배터리 시뮬레이터 설계안 & 검증")
        );
        assert!(doc.text.contains("아키텍처 결정"));
        assert!(doc.text.contains("실행 캐시를 도입한다."));
        assert!(!doc.text.contains("hidden-css-token"));
        assert!(!doc.text.contains("secretJsNoise"));
    }
}
