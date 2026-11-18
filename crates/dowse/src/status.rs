//! 只读索引状态查询。[`index_status`] 汇总文档总数、已注册根目录、索引落盘
//! 体积、最近更新时间（[`IndexStatus`]），供 CLI/MCP 一类查询命令使用，
//! 不修改索引任何状态。

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use walkdir::WalkDir;

use crate::rules::IndexRules;

/// 索引的只读状态快照，给 CLI/MCP 的 `index_status` 一类查询用。
pub struct IndexStatus {
    /// 索引里的文档总数。
    pub num_docs: u64,
    /// 已注册的索引根目录。
    pub roots: Vec<PathBuf>,
    /// 索引目录落盘体积（字节），递归求和索引目录下所有文件大小。
    pub disk_size_bytes: u64,
    /// 索引目录下所有文件里最新的 mtime——近似"最近一次更新时间"。
    /// 目录为空（理论上不该发生，schema 校验已保证至少有 meta 文件）时是 None。
    pub last_updated: Option<SystemTime>,
    /// 当前生效的索引规则（排除目录、追加文本扩展名、单文件体积上限）。读取
    /// 索引目录旁的 rules.json，无规则文件时是默认规则。让 `dowse status`
    /// 能把当前规则展示给用户，也让"某些文件因体积上限被挡"这类现象有据可查。
    pub rules: IndexRules,
    /// 색인을 만들 때 한국어 형태소 사전(ko-dic)을 썼는지. 모든 근이 같은 값으로
    /// 기록돼 있을 때만 `Some`; 기록 이전 색인이거나 근마다 다르면 `None`.
    pub korean_morphology_indexed: Option<bool>,
    /// 지금 이 프로세스에서 ko-dic을 쓸 수 있는지.
    pub korean_morphology_available: bool,
    /// 색인할 때의 ko-dic 사용 여부가 지금과 달라 다시 색인하는 게 좋은 근들
    /// (`roots`에 적힌 그대로). 기록이 없는 근은 판단하지 않아 들어가지 않는다.
    pub korean_reindex_roots: Vec<PathBuf>,
}

impl IndexStatus {
    /// 색인 시점과 지금의 ko-dic 사용 가능 여부가 달라 재색인이 권장되는지.
    /// 기록이 없는 옛 색인(None)은 판단하지 않는다(false).
    pub fn korean_dictionary_mismatch(&self) -> bool {
        !self.korean_reindex_roots.is_empty()
    }
}

/// (근별 기록 요약, 지금과 달라 다시 색인할 근들)
fn korean_morphology_summary(
    meta: &crate::meta::IndexMeta,
    available: bool,
) -> (Option<bool>, Vec<PathBuf>) {
    let per_root: Vec<Option<bool>> = meta
        .roots
        .iter()
        .map(|r| meta.korean_morphology_of(r))
        .collect();
    let summary = match per_root.first() {
        // 근이 하나도 없으면 전체 재구축 기록을 그대로 보여 준다.
        None => meta.korean_morphology,
        Some(first) => per_root
            .iter()
            .all(|v| v == first)
            .then_some(*first)
            .flatten(),
    };
    let stale = meta
        .roots
        .iter()
        .zip(&per_root)
        .filter(|(_, v)| v.is_some_and(|indexed| indexed != available))
        .map(|(r, _)| r.clone())
        .collect();
    (summary, stale)
}

/// 색인할 때의 한국어 사전(ko-dic) 사용 여부가 지금과 달라 다시 색인하는 게 좋은
/// 근 목록. [`index_status`]의 `korean_reindex_roots`와 같지만 색인 크기를 세지 않아
/// 가볍다(앱 화면이 창을 열 때마다 부른다). 처음 부르면 ko-dic 사전을 연다.
///
/// # Errors
///
/// meta.json을 읽을 수 없거나 schema 버전이 맞지 않으면 `Err`.
pub fn korean_reindex_roots(index_dir: &Path) -> Result<Vec<PathBuf>> {
    let meta = crate::meta::ensure_schema_version(index_dir)?;
    Ok(korean_morphology_summary(&meta, crate::korean_morphology_available()).1)
}

/// 读索引的只读状态：文档数、已注册根、落盘体积、最近更新时间。
///
/// 先走 `registered_roots` 做存在性 + schema 版本校验——索引不存在或版本不对时
/// 复用它已有的报错文案（提示重建），不在这里另造一套错误信息。
///
/// # Examples
///
/// ```no_run
/// # fn main() -> anyhow::Result<()> {
/// use std::path::Path;
/// use dowse::index_status;
///
/// let status = index_status(Path::new("./my-index"))?;
/// println!("{} 篇文档，{} 个根", status.num_docs, status.roots.len());
/// for root in &status.roots {
///     println!("  {}", root.display());
/// }
/// # Ok(())
/// # }
/// ```
pub fn index_status(index_dir: &Path) -> Result<IndexStatus> {
    let meta = crate::meta::ensure_schema_version(index_dir)?;
    let korean_morphology_available = crate::korean_morphology_available();
    let (korean_morphology_indexed, korean_reindex_roots) =
        korean_morphology_summary(&meta, korean_morphology_available);
    let roots = meta.roots;

    let mut disk_size_bytes = 0u64;
    let mut last_updated: Option<SystemTime> = None;
    for entry in WalkDir::new(index_dir).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        disk_size_bytes += meta.len();
        if let Ok(modified) = meta.modified() {
            last_updated = Some(match last_updated {
                Some(prev) if prev >= modified => prev,
                _ => modified,
            });
        }
    }

    // 只读打开一次 Searcher 拿文档总数：这是唯一需要真正读 tantivy 索引的部分,
    // 复用 Searcher::open 而不是自己再开一遍 Index，保证 schema 校验、分词器
    // 注册这些细节只有一处实现。
    let num_docs = crate::Searcher::open(index_dir)?.num_docs();

    // 只读展示当前规则，不动进程级全局（那是写入类入口的职责）。
    let rules = crate::rules::load_rules(index_dir);

    Ok(IndexStatus {
        num_docs,
        roots,
        disk_size_bytes,
        last_updated,
        rules,
        korean_morphology_indexed,
        korean_morphology_available,
        korean_reindex_roots,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn korean_dictionary_mismatch_only_when_recorded_and_different() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;
        std::fs::write(target_dir.path().join("note.md"), "배터리팩의 내용")?;
        crate::rebuild_index(index_dir.path(), target_dir.path())?;

        let status = index_status(index_dir.path())?;
        let available = crate::korean_morphology_available();
        assert_eq!(status.korean_morphology_indexed, Some(available));
        assert_eq!(status.korean_morphology_available, available);
        assert!(!status.korean_dictionary_mismatch());
        assert!(korean_reindex_roots(index_dir.path())?.is_empty());

        let mut meta = crate::meta::load_meta(index_dir.path())?;
        meta.korean_morphology = Some(!available);
        crate::meta::save_meta(index_dir.path(), &meta)?;
        let status = index_status(index_dir.path())?;
        assert_eq!(status.korean_morphology_indexed, Some(!available));
        assert!(status.korean_dictionary_mismatch());
        assert_eq!(status.korean_reindex_roots, status.roots);
        assert_eq!(korean_reindex_roots(index_dir.path())?, status.roots);

        meta.korean_morphology = None;
        crate::meta::save_meta(index_dir.path(), &meta)?;
        let status = index_status(index_dir.path())?;
        assert_eq!(status.korean_morphology_indexed, None);
        assert!(!status.korean_dictionary_mismatch());
        Ok(())
    }

    #[test]
    fn index_status_on_missing_index_errors() {
        let index_dir = tempfile::tempdir().unwrap();
        // 目录存在但从没建过索引：没有 meta.json，应该报错而不是返回空状态。
        let missing = index_dir.path().join("no-such-index");
        assert!(index_status(&missing).is_err());
    }

    #[test]
    fn index_status_reports_docs_roots_and_nonzero_disk_size() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;
        std::fs::write(target_dir.path().join("note.md"), "内容")?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;

        let status = index_status(index_dir.path())?;
        assert_eq!(status.num_docs, 1);
        assert_eq!(status.roots, vec![target_dir.path().to_path_buf()]);
        assert!(status.disk_size_bytes > 0, "刚建的索引落盘体积不应为 0");
        assert!(status.last_updated.is_some());
        Ok(())
    }
}
