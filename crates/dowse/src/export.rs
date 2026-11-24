//! Incremental Markdown projection of a committed local index.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::Path,
};
use tantivy::{DocAddress, Index, TantivyDocument, schema::Value};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    name: String,
    digest: String,
}

type Ledger = BTreeMap<String, Entry>;

fn digest(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

fn atomic(path: &Path, bytes: &[u8], replace: bool) -> Result<()> {
    let mut tmp = tempfile::NamedTempFile::new_in(path.parent().context("출력 경로 오류")?)?;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    if replace {
        tmp.persist(path)?;
    } else {
        tmp.persist_noclobber(path)?;
    }
    Ok(())
}

/// Synchronize supported indexed documents into an existing output folder.
pub fn sync(index_dir: &Path, folder: &Path) -> Result<String> {
    let folder = folder
        .canonicalize()
        .context("출력 폴더를 열 수 없습니다")?;
    let roots = crate::registered_roots(index_dir)?;
    for root in &roots {
        let root = root.canonicalize().context("검색 폴더 연결을 확인하세요")?;
        ensure!(
            !folder.starts_with(&root),
            "출력 폴더는 검색 폴더 밖에 지정하세요"
        );
    }
    let local = index_dir.with_file_name("markdown-state");
    fs::create_dir_all(&local)?;
    let key = digest(folder.to_string_lossy().as_bytes());
    let lock = fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(local.join(format!("{key}.lock")))?;
    lock.try_lock()
        .context("다른 내보내기 작업이 실행 중입니다")?;
    let manifest = local.join(format!("{key}.json"));
    let old: Ledger = match fs::read(&manifest) {
        Ok(data) => serde_json::from_slice(&data)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ledger::new(),
        Err(e) => return Err(e.into()),
    };
    let owned: std::collections::HashMap<_, _> = old
        .values()
        .map(|entry| (entry.name.as_str(), entry.digest.as_str()))
        .collect();
    let index = Index::open_in_dir(index_dir)?;
    let schema = index.schema();
    let path_field = schema.get_field("path")?;
    let body_field = schema.get_field("content")?;
    let mtime_field = schema.get_field("mtime")?;
    let title_field = schema.get_field("title")?;
    let author_field = schema.get_field("author")?;
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let mut next = Ledger::new();
    let mut names = std::collections::HashSet::new();
    let mut empty = 0;
    for (ord, segment) in searcher.segment_readers().iter().enumerate() {
        for id in segment.doc_ids_alive() {
            let addr = format!("{}-{id}", segment.segment_id().uuid_string());
            if let Some(entry) = old.get(&addr)
                && folder.join(&entry.name).is_file()
            {
                names.insert(entry.name.clone());
                next.insert(addr, entry.clone());
                continue;
            }
            let doc: TantivyDocument = searcher.doc(DocAddress::new(ord as u32, id))?;
            let raw_path_str = doc
                .get_first(path_field)
                .and_then(|v| v.as_str())
                .context("색인 경로 누락")?;
            let (source_path, message_id) = raw_path_str
                .split_once("::")
                .map_or((raw_path_str, None), |(path, id)| (path, Some(id)));
            let path = Path::new(source_path);
            let ext = path
                .extension()
                .unwrap_or_default()
                .to_string_lossy()
                .to_ascii_lowercase();
            if !matches!(
                ext.as_str(),
                "txt"
                    | "md"
                    | "docx"
                    | "docm"
                    | "pdf"
                    | "pptx"
                    | "pptm"
                    | "xlsx"
                    | "xlsm"
                    | "xls"
                    | "xlsb"
                    | "html"
                    | "htm"
                    | "pst"
                    | "ost"
                    | "msg"
                    | "eml"
            ) {
                continue;
            }
            if message_id.is_some() && !matches!(ext.as_str(), "pst" | "ost") {
                continue;
            }
            let Some(root) = roots
                .iter()
                .filter(|r| path.starts_with(r))
                .max_by_key(|r| r.components().count())
            else {
                continue;
            };
            let relative = path.strip_prefix(root)?.to_string_lossy();
            let filename = path.file_name().unwrap_or_default().to_string_lossy();
            let document_title = doc.get_first(title_field).and_then(|v| v.as_str());
            let title = if message_id.is_some() {
                document_title.unwrap_or(&filename)
            } else {
                &filename
            };
            let name = format!(
                "{}-{}.md",
                filename.chars().take(60).collect::<String>(),
                &digest(raw_path_str.as_bytes())[..24]
            );
            let body = doc
                .get_first(body_field)
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if body.trim().is_empty() {
                empty += 1;
            }
            let ms = doc
                .get_first(mtime_field)
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let date = OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)?
                .format(&Rfc3339)?;
            let root_name = root.file_name().unwrap_or_default().to_string_lossy();
            let mut properties = String::new();
            if let Some(id) = message_id {
                properties.push_str(&format!("message_id: {}\n", serde_json::to_string(id)?));
            }
            for (key, field) in [("document_title", title_field), ("author", author_field)] {
                if let Some(value) = doc.get_first(field).and_then(|v| v.as_str()) {
                    properties.push_str(&format!("{key}: {}\n", serde_json::to_string(value)?));
                }
            }
            let data = format!(
                "---\nschema_version: 1\nsource_root: {}\nsource_path: {}\nmodified_at: {date}\n{properties}---\n\n# {title}\n\n{body}\n",
                serde_json::to_string(&root_name)?,
                serde_json::to_string(&relative)?
            );
            let hash = digest(data.as_bytes());
            let dest = folder.join(&name);
            match fs::read(&dest) {
                Ok(bytes) => {
                    let current = digest(&bytes);
                    if current != hash {
                        ensure!(
                            owned.get(name.as_str()).copied() == Some(current.as_str()),
                            "출력 파일 충돌: {name}"
                        );
                        atomic(&dest, data.as_bytes(), true)?;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    atomic(&dest, data.as_bytes(), false)?
                }
                Err(e) => return Err(e.into()),
            }
            names.insert(name.clone());
            next.insert(addr, Entry { name, digest: hash });
        }
    }
    for entry in old.values().filter(|e| !names.contains(&e.name)) {
        let dest = folder.join(&entry.name);
        match fs::read(&dest) {
            Ok(bytes) if digest(&bytes) == entry.digest => fs::remove_file(dest)?,
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    atomic(&manifest, &serde_json::to_vec(&next)?, true)?;
    Ok(format!(
        "문서 {}개 · 이번 처리 중 빈 본문 {empty}개",
        next.len()
    ))
}
