use std::fs;
use std::io::Read;
use std::path::Path;

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::rules::IndexRules;

/// 按扩展名白名单认定的纯文本文件。
const TEXT_EXTS: &[&str] = &[
    "txt", "md", "markdown", "log", "csv", "tsv", "json", "toml", "yaml", "yml", "ini", "cfg",
    "conf", "xml", "html", "htm", "sql", "sh", "ps1", "bat", "rs", "py", "go", "java", "js", "ts",
    "jsx", "tsx", "c", "h", "cpp", "hpp", "cs", "rb", "php", "lua", "vue",
];

/// 支持抽取的 Office Open XML 格式（docx/xlsx/pptx，本质都是 zip 包）。
/// 老式二进制格式 doc/xls/ppt 不在白名单里，天然被跳过——不做兼容。
const OFFICE_EXTS: &[&str] = &["docx", "xlsx", "pptx"];

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

/// [`extract_text`]와 같은 규칙·같은 panic 방어로 본문과 문서 속성을 함께 뽑는다.
pub(crate) fn extract_document(path: &Path) -> Option<ExtractedDoc> {
    extract_document_with(path, &crate::rules::active_rules())
}

/// [`extract_text`] 的纯函数版：体积上限、追加扩展名都从显式规则读，不碰进程级
/// 全局，便于单测。零参的 `extract_text` 只是用当前生效规则调它。
fn extract_text_with(path: &Path, rules: &IndexRules) -> Option<String> {
    extract_document_with(path, rules).map(|doc| doc.text)
}

fn extract_document_with(path: &Path, rules: &IndexRules) -> Option<ExtractedDoc> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();

    let meta = fs::metadata(path).ok()?;
    if meta.len() > rules.max_file_bytes() {
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
                props: office_props(path),
            })
        };
        match ext.as_str() {
            "pdf" => extract_pdf(path),
            "docx" => office(extract_docx(path)),
            "xlsx" => office(extract_xlsx(path)),
            "pptx" => office(extract_pptx(path)),
            e if TEXT_EXTS.contains(&e) => plain(read_text_smart(path)),
            // 追加白名单里的扩展名当纯文本读（自动探测编码），跟内建文本类型同路。
            e if rules.is_extra_text_ext(e) => plain(read_text_smart(path)),
            _ => None,
        }
    }))
    .ok()
    .flatten()
}

/// 从 PDF 抽文本和 Info 속성。畸形 PDF 触发的 panic 由 `extract_text` 那层统一的
/// `catch_unwind` 兜底（见其文档），这里只走正常路径：出错时落成 `None`。
/// 跟 `pdf_extract::extract_text` 同一流程（加载 → 空密码解密 → 输出纯文本），
/// 只是自己持有 `Document`，好顺手读 trailer 里的 Info 사전，不必把文件读两遍。
fn extract_pdf(path: &Path) -> Option<ExtractedDoc> {
    let mut doc = pdf_extract::Document::load(path).ok()?;
    if doc.is_encrypted() {
        doc.decrypt("").ok()?;
    }
    let mut text = String::new();
    {
        let mut output = pdf_extract::PlainTextOutput::new(&mut text);
        pdf_extract::output_doc(&doc, &mut output).ok()?;
    }
    Some(ExtractedDoc {
        text,
        props: pdf_props(&doc),
    })
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
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// docx/xlsx/pptx 공통: `docProps/core.xml`에서 속성을 읽는다. 항목이 없으면 기본값.
fn office_props(path: &Path) -> DocProps {
    let Some(xml) = read_zip_entry(path, "docProps/core.xml") else {
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

/// 打开一个 zip 包里的单个条目，读成字节。条目不存在/包本身打不开（损坏、
/// 加密）都走 None——密码保护的 Office 文件会在这里打不开 zip 结构，
/// 自然跳过，不会 panic。
fn read_zip_entry(path: &Path, entry_name: &str) -> Option<Vec<u8>> {
    let file = fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let mut entry = archive.by_name(entry_name).ok()?;
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// docx 是一个 zip 包，正文全部在 word/document.xml 里，文本节点是 `<w:t>`。
/// 段落 `<w:p>` 闭合时补一个换行，避免不同段落的文字连成一整块无法阅读。
fn extract_docx(path: &Path) -> Option<String> {
    let xml = read_zip_entry(path, "word/document.xml")?;
    let text = extract_tagged_text(&xml, b"t", Some(b"p"));
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// xlsx 的单元格文本绝大多数不直接存在 sheet 里，而是去重后存进
/// xl/sharedStrings.xml，sheet 里只留索引号；覆盖不了共享字符串表的是
/// 内联字符串（`<c t="inlineStr"><is><t>...</t></is></c>`），直接写在各
/// xl/worksheets/sheet*.xml 里，两处都要读。两处的文本节点本地名都是 `<t>`。
fn extract_xlsx(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;

    let mut text = String::new();

    if let Ok(mut entry) = archive.by_name("xl/sharedStrings.xml") {
        let mut bytes = Vec::new();
        if entry.read_to_end(&mut bytes).is_ok() {
            drop(entry);
            text.push_str(&extract_tagged_text(&bytes, b"t", None));
            text.push('\n');
        }
    }

    let sheet_names: Vec<String> = archive
        .file_names()
        .filter(|n| n.starts_with("xl/worksheets/sheet") && n.ends_with(".xml"))
        .map(|n| n.to_string())
        .collect();
    for name in sheet_names {
        let Ok(mut entry) = archive.by_name(&name) else {
            continue;
        };
        let mut bytes = Vec::new();
        if entry.read_to_end(&mut bytes).is_err() {
            continue;
        }
        drop(entry);
        text.push_str(&extract_tagged_text(&bytes, b"t", None));
        text.push('\n');
    }

    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// pptx 每页幻灯片是独立的 ppt/slides/slideN.xml，文本 run 是 `<a:t>`，
/// 本地名同样是 `t`。幻灯片之间补空行分隔。
fn extract_pptx(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;

    let mut slide_names: Vec<String> = archive
        .file_names()
        .filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"))
        .map(|n| n.to_string())
        .collect();
    slide_names.sort();

    let mut text = String::new();
    for name in slide_names {
        let Ok(mut entry) = archive.by_name(&name) else {
            continue;
        };
        let mut bytes = Vec::new();
        if entry.read_to_end(&mut bytes).is_err() {
            continue;
        }
        drop(entry);
        text.push_str(&extract_tagged_text(&bytes, b"t", Some(b"p")));
        text.push('\n');
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
fn read_text_smart(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    if bytes.is_empty() {
        return None;
    }

    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
    detector.feed(&bytes, true);
    let encoding = detector.guess(None, chardetng::Utf8Detection::Allow);

    let (text, _, had_errors) = encoding.decode(&bytes);
    if had_errors && encoding != encoding_rs::UTF_8 {
        // 猜错编码时宁可退回 UTF-8 有损解码，也不索引一堆乱码词条
        return Some(String::from_utf8_lossy(&bytes).into_owned());
    }
    Some(text.into_owned())
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
}
