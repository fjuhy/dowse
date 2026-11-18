//! 内容分词器：把文本按"汉字 / 非汉字"切成一段段，汉字段交给 jieba 继续
//! 按中文习惯切词，非汉字段（拉丁字母、数字、标点、空白等）按字母数字连续
//! 段切成词；两侧产出的 token 一律小写归一。
//!
//! 为什么要在 jieba 外面套这层：`tantivy_jieba` 直接把整串喂给 jieba 的
//! 中文 HMM 统计分词，遇到英文/连字符串（如 `glimmerfrost-9931-unique-marker`）
//! 会按中文习惯乱切，而且"整篇一起切"和"单独查询词切"两次结果不一致，
//! 导致子词搜不到。这里先把汉字和非汉字分开：只有真正的汉字才进 jieba，
//! 其余走确定性的字母数字切分，两侧口径一致，`api` 能搜到 `API`、
//! `covid` 能搜到 `covid-19`。
//!
//! offset 语义是硬约束：每个 token 的 `offset_from`/`offset_to` 必须是**原始**
//! 输入串里的精确字节下标——搜索侧的摘要/高亮会拿这些区间去切原文，错一个
//! 字节就是切串 panic，不只是搜错。所以汉字段喂给 jieba 后要把它给出的
//! 段内 offset 加回该段在原串里的字节基址。

use std::borrow::Cow;
use std::sync::OnceLock;

use tantivy::tokenizer::{Token, TokenStream, Tokenizer};
use unicode_script::{Script, UnicodeScript};

/// 索引期为普通字母数字词生成的最长前缀。更长的极端输入在查询侧改走
/// Tantivy 的正则自动机，不让超长 URL/哈希制造二次方级别的索引膨胀。
pub(crate) const AUTOCOMPLETE_MAX_PREFIX_CHARS: usize = 64;

/// autocomplete 使用的 CJK script 判定，对齐 Lucene CJKBigramFilter 覆盖的
/// Han / Hiragana / Katakana / Hangul，而不是手写几个迟早漏掉的码点区间。
pub(crate) fn is_cjk(ch: char) -> bool {
    matches!(
        ch.script(),
        Script::Han | Script::Hiragana | Script::Katakana | Script::Hangul
    )
}

/// 主相关性字段仍只有 Han 交给 jieba；日文假名不改变原有分词路径。
fn is_jieba_han(ch: char) -> bool {
    ch.script() == Script::Han
}

/// 한글(Hangul script) 구간은 lindera + ko-dic 형태소 분석으로 넘긴다.
fn is_hangul(ch: char) -> bool {
    ch.script() == Script::Hangul
}

/// 主分词器眼里的三类 run：汉字（jieba）、한글（lindera ko-dic）、其余（字母数字切分）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RunKind {
    Han,
    Hangul,
    Other,
}

fn run_kind(ch: char) -> RunKind {
    if is_jieba_han(ch) {
        RunKind::Han
    } else if is_hangul(ch) {
        RunKind::Hangul
    } else {
        RunKind::Other
    }
}

/// ko-dic 사전 디렉터리를 찾을 때 쓰는 환경 변수.
pub(crate) const KO_DIC_ENV: &str = "DOWSE_KO_DIC";

/// 실행 파일 옆에 둘 ko-dic 사전 디렉터리 이름(`<exe 폴더>/ko-dic/`).
pub(crate) const KO_DIC_DIR_NAME: &str = "ko-dic";

/// ko-dic 사전 후보 위치. 순서: `DOWSE_KO_DIC` → 실행 파일 옆 `ko-dic/` →
/// macOS .app 번들의 `Contents/Resources/ko-dic/` → 바이너리에 내장된 사전
/// (`embed-ko-dic` feature로 빌드했을 때만 존재).
///
/// 사전을 기본으로 내장하지 않는 이유: 컴파일된 ko-dic은 약 82MB이고 설치 파일에는
/// 앱과 CLI sidecar가 둘 다 들어가 엔진이 두 번 링크된다. 내장하면 release CLI가
/// 10,069,392 → 96,609,312 bytes로 커져 README의 설치 파일 예산(<50MB)을 넘는다.
/// 별도 디렉터리로 두면 앱과 CLI가 한 벌을 공유하고 mmap으로 읽는다.
fn ko_dic_candidates() -> Vec<String> {
    let mut out = Vec::new();
    if let Some(dir) = std::env::var_os(KO_DIC_ENV).filter(|v| !v.is_empty()) {
        out.push(dir.to_string_lossy().into_owned());
    }
    if let Ok(exe) = std::env::current_exe() {
        out.extend(
            ko_dic_dirs_for_exe(&exe)
                .into_iter()
                .filter(|dir| dir.join("metadata.json").is_file())
                .map(|dir| dir.to_string_lossy().into_owned()),
        );
    }
    out.push("embedded://ko-dic".to_string());
    out
}

/// 실행 파일 위치로부터 ko-dic 디렉터리 후보를 우선순위대로 돌려준다(존재 여부는
/// 보지 않음). 1) 실행 파일 옆 `ko-dic/` — Windows 설치본과 개발 빌드.
/// 2) 실행 파일이 `<X>.app/Contents/MacOS/` 안에 있으면 `Contents/Resources/ko-dic/`
/// — Tauri는 macOS에서 bundle resources를 실행 파일 옆이 아니라 Resources에 넣는다.
/// 앱과 CLI sidecar가 둘 다 `Contents/MacOS/`에 있으므로 한 벌을 같이 쓴다.
/// 판별은 디렉터리 배치(`Contents/MacOS`)로만 하므로 플랫폼과 무관하게 시험할 수 있다.
fn ko_dic_dirs_for_exe(exe: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Some(exe_dir) = exe.parent() else {
        return out;
    };
    out.push(exe_dir.join(KO_DIC_DIR_NAME));
    if exe_dir.file_name().is_some_and(|n| n == "MacOS")
        && let Some(contents) = exe_dir.parent()
        && contents.file_name().is_some_and(|n| n == "Contents")
    {
        out.push(contents.join("Resources").join(KO_DIC_DIR_NAME));
    }
    out
}

/// ko-dic 세그멘터는 처음 한글을 만났을 때 한 번만 만든다(lazy 로딩: 한국어를
/// 안 쓰는 사용자는 사전을 열지도 않는다). 어디서도 사전을 못 찾으면 `None`을
/// 기억해 두고 이후 한글 구간은 모두 어절 통째로 1 token 처리로 대체한다
/// (형태소 분석 도입 전과 같은 동작).
fn ko_segmenter() -> Option<&'static lindera::segmenter::Segmenter> {
    static SEGMENTER: OnceLock<Option<lindera::segmenter::Segmenter>> = OnceLock::new();
    SEGMENTER
        .get_or_init(|| {
            let dictionary = ko_dic_candidates().into_iter().find_map(|uri| {
                lindera::dictionary::load_dictionary_with_options(&uri, true).ok()
            })?;
            Some(lindera::segmenter::Segmenter::new(
                lindera::mode::Mode::Normal,
                dictionary,
                None,
            ))
        })
        .as_ref()
}

/// 한국어 형태소 분석 사전을 쓸 수 있는지(없으면 한글은 어절 단위로 색인됨).
pub fn korean_morphology_available() -> bool {
    ko_segmenter().is_some()
}

/// lindera가 돌려준 run 내부 byte 구간들을 검증한다. 비어 있지 않고, 서로 겹치지
/// 않게 앞에서 뒤로 진행하며, run 범위 안에서 char boundary에 걸려야 한다.
/// 하나라도 어긋나면 `None` — 호출 측이 그 run만 기존 방식으로 대체한다.
fn ko_spans(run: &str) -> Option<Vec<(usize, usize)>> {
    let segmenter = ko_segmenter()?;
    let tokens = segmenter.segment(Cow::Borrowed(run)).ok()?;
    let mut spans = Vec::with_capacity(tokens.len());
    let mut prev_end = 0usize;
    for tok in &tokens {
        let (from, to) = (tok.byte_start, tok.byte_end);
        if from < prev_end
            || from >= to
            || to > run.len()
            || !run.is_char_boundary(from)
            || !run.is_char_boundary(to)
        {
            return None;
        }
        spans.push((from, to));
        prev_end = to;
    }
    if spans.is_empty() { None } else { Some(spans) }
}

/// 混合分词器：汉字段走 jieba，非汉字段按字母数字切，全部小写归一。
#[derive(Clone)]
pub(crate) struct MixedTokenizer {
    jieba: tantivy_jieba::JiebaTokenizer,
}

impl MixedTokenizer {
    pub(crate) fn new() -> Self {
        Self {
            jieba: tantivy_jieba::JiebaTokenizer::new(),
        }
    }

    /// 汉字段：喂给 jieba，把段内 offset 加上基址后原样收下（保持 jieba 的
    /// 中文切词质量不变），position 用外部的顺序计数器覆盖。
    fn emit_cjk(&mut self, run: &str, base: usize, out: &mut Vec<Token>, position: &mut usize) {
        let mut stream = self.jieba.token_stream(run);
        while stream.advance() {
            let tok = stream.token();
            out.push(Token {
                offset_from: base + tok.offset_from,
                offset_to: base + tok.offset_to,
                position: *position,
                text: tok.text.to_lowercase(),
                position_length: 1,
            });
            *position += 1;
        }
    }

    /// 한글 구간: lindera ko-dic으로 형태소를 나누고, run 안 byte 구간에 원문
    /// base를 더해 정확한 offset을 만든다. token 텍스트는 lindera의 surface가
    /// 아니라 원문 조각을 그대로 쓴다(정규화가 끼어들어도 offset과 어긋나지 않게).
    /// lindera 결과가 검증을 통과하지 못하면 이 run만 어절 통째로 1 token으로 대체.
    fn emit_hangul(run: &str, base: usize, out: &mut Vec<Token>, position: &mut usize) {
        let spans = ko_spans(run).unwrap_or_else(|| vec![(0, run.len())]);
        for (from, to) in spans {
            out.push(Token {
                offset_from: base + from,
                offset_to: base + to,
                position: *position,
                text: run[from..to].to_lowercase(),
                position_length: 1,
            });
            *position += 1;
        }
    }

    /// 非汉字段：每一段极大连续的 `char::is_alphanumeric()` 字符成一个词
    /// （对齐 tantivy `SimpleTokenizer` 的口径），连字符/点/空白/ASCII 标点
    /// 都是分隔符、不产 token。offset 是 `base + 段内字节下标`。
    fn emit_non_cjk(run: &str, base: usize, out: &mut Vec<Token>, position: &mut usize) {
        let mut span_start: Option<usize> = None;
        for (i, ch) in run.char_indices() {
            if ch.is_alphanumeric() {
                if span_start.is_none() {
                    span_start = Some(i);
                }
            } else if let Some(start) = span_start.take() {
                out.push(Token {
                    offset_from: base + start,
                    offset_to: base + i,
                    position: *position,
                    text: run[start..i].to_lowercase(),
                    position_length: 1,
                });
                *position += 1;
            }
        }
        if let Some(start) = span_start {
            out.push(Token {
                offset_from: base + start,
                offset_to: base + run.len(),
                position: *position,
                text: run[start..].to_lowercase(),
                position_length: 1,
            });
            *position += 1;
        }
    }

    fn emit_run(
        &mut self,
        kind: RunKind,
        run: &str,
        base: usize,
        out: &mut Vec<Token>,
        position: &mut usize,
    ) {
        match kind {
            RunKind::Han => self.emit_cjk(run, base, out, position),
            RunKind::Hangul => Self::emit_hangul(run, base, out, position),
            RunKind::Other => Self::emit_non_cjk(run, base, out, position),
        }
    }
}

impl Tokenizer for MixedTokenizer {
    type TokenStream<'a> = VecTokenStream;

    fn token_stream(&mut self, text: &str) -> VecTokenStream {
        let mut tokens: Vec<Token> = Vec::new();
        let mut position: usize = 0;

        // 单趟扫描：把原串切成一段段极大的"同类"run（同为汉字或同为非汉字），
        // 记住每段在原串里的字节基址，逐段产 token。position 是贯穿全程的
        // 顺序计数器，每产一个 token +1，跟 run 类型/跨了几个字无关——tantivy
        // 的短语查询要求相邻词位置差 1，不能用 jieba 的段内字符偏移。
        let mut run_start = 0usize;
        let mut run_kind_cur: Option<RunKind> = None;
        for (idx, ch) in text.char_indices() {
            let kind = run_kind(ch);
            match run_kind_cur {
                None => run_kind_cur = Some(kind),
                Some(prev) if prev != kind => {
                    let run = &text[run_start..idx];
                    self.emit_run(prev, run, run_start, &mut tokens, &mut position);
                    run_start = idx;
                    run_kind_cur = Some(kind);
                }
                _ => {}
            }
        }
        if let Some(prev) = run_kind_cur {
            let run = &text[run_start..];
            self.emit_run(prev, run, run_start, &mut tokens, &mut position);
        }

        VecTokenStream { tokens, index: 0 }
    }
}

/// 把整段分词结果先算成一个拥有所有权的 `Vec<Token>`，再按下标顺序吐出来。
/// 这样就不用把 jieba 借用原串的零拷贝 token 穿过自己的生命周期，简单稳妥。
pub(crate) struct VecTokenStream {
    tokens: Vec<Token>,
    index: usize,
}

/// CJK 输入即搜专用分词器：每个 CJK 字符成为一个带原文位置的 token，非 CJK
/// 字符不产 token 但仍占位置，因此 `北-A-极` 不会被误认成连续的 `北极`。
#[derive(Clone, Default)]
pub(crate) struct CjkCharTokenizer;

impl Tokenizer for CjkCharTokenizer {
    type TokenStream<'a> = VecTokenStream;

    fn token_stream(&mut self, text: &str) -> VecTokenStream {
        let mut tokens = Vec::new();
        for (position, (offset_from, ch)) in text.char_indices().enumerate() {
            if !is_cjk(ch) {
                continue;
            }
            tokens.push(Token {
                offset_from,
                offset_to: offset_from + ch.len_utf8(),
                position,
                text: ch.to_string(),
                position_length: 1,
            });
        }
        VecTokenStream { tokens, index: 0 }
    }
}

/// autocomplete 的非 CJK edge-ngram 分词器。每个连续字母数字词在同一 position
/// 发出从 1 到 64 字符的前缀，等价于成熟搜索引擎的索引期 edge-ngram；CJK 由上面
/// 的逐字位置字段单独负责。
#[derive(Clone, Default)]
pub(crate) struct AutocompletePrefixTokenizer;

impl AutocompletePrefixTokenizer {
    fn emit_prefixes(
        text: &str,
        offset_from: usize,
        offset_to: usize,
        position: usize,
        tokens: &mut Vec<Token>,
    ) {
        let word = &text[offset_from..offset_to];
        for (index, (relative, ch)) in word.char_indices().enumerate() {
            if index >= AUTOCOMPLETE_MAX_PREFIX_CHARS {
                break;
            }
            let prefix_to = relative + ch.len_utf8();
            tokens.push(Token {
                offset_from,
                offset_to: offset_from + prefix_to,
                position,
                text: word[..prefix_to].to_lowercase(),
                position_length: 1,
            });
        }
    }
}

impl Tokenizer for AutocompletePrefixTokenizer {
    type TokenStream<'a> = VecTokenStream;

    fn token_stream(&mut self, text: &str) -> VecTokenStream {
        let mut tokens = Vec::new();
        let mut start: Option<usize> = None;
        let mut position = 0usize;

        for (offset, ch) in text.char_indices() {
            if ch.is_alphanumeric() && !is_cjk(ch) {
                start.get_or_insert(offset);
            } else if let Some(offset_from) = start.take() {
                Self::emit_prefixes(text, offset_from, offset, position, &mut tokens);
                position += 1;
            }
        }
        if let Some(offset_from) = start {
            Self::emit_prefixes(text, offset_from, text.len(), position, &mut tokens);
        }

        VecTokenStream { tokens, index: 0 }
    }
}

impl TokenStream for VecTokenStream {
    fn advance(&mut self) -> bool {
        if self.index < self.tokens.len() {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn token(&self) -> &Token {
        &self.tokens[self.index - 1]
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.index - 1]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把分词结果收成 Vec 方便断言。
    fn collect(text: &str) -> Vec<Token> {
        let mut tokenizer = MixedTokenizer::new();
        let mut stream = tokenizer.token_stream(text);
        let mut out = Vec::new();
        while stream.advance() {
            out.push(stream.token().clone());
        }
        out
    }

    #[test]
    fn cjk_then_latin_offsets_are_byte_based_not_char_based() {
        // "报告" 两个汉字在 UTF-8 里各占 3 字节，后面的拉丁段基址必须落在
        // 字节 6 之后，而不是字符数 2 之后——这是最容易把字节/字符搞混的地方。
        let text = "报告api";
        let tokens = collect(text);

        // 非汉字段 "api" 的 offset 必须能切回原串的那三个字节。
        let api = tokens
            .iter()
            .find(|t| t.text == "api")
            .expect("应该切出 api");
        assert_eq!(
            api.offset_from,
            "报告".len(),
            "拉丁段基址应在汉字段字节数之后"
        );
        assert_eq!(api.offset_to, text.len());
        assert_eq!(&text[api.offset_from..api.offset_to], "api");

        // position 是顺序递增的，不是 jieba 的段内字符偏移。
        for (i, t) in tokens.iter().enumerate() {
            assert_eq!(t.position, i, "position 应顺序递增: {tokens:?}");
            assert_eq!(t.position_length, 1);
        }
    }

    #[test]
    fn hyphenated_string_splits_into_subwords_with_exact_offsets() {
        let text = "glimmerfrost-9931-unique-marker";
        let tokens = collect(text);
        let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["glimmerfrost", "9931", "unique", "marker"]);

        // 每个子词的 offset 都要能精确切回原串对应的子串。
        for t in &tokens {
            assert_eq!(&text[t.offset_from..t.offset_to], t.text);
        }
    }

    #[test]
    fn dotted_version_and_case_folding() {
        assert_eq!(
            collect("v1.2.3")
                .iter()
                .map(|t| t.text.clone())
                .collect::<Vec<_>>(),
            vec!["v1", "2", "3"]
        );
        assert_eq!(
            collect("GPT-4")
                .iter()
                .map(|t| t.text.clone())
                .collect::<Vec<_>>(),
            vec!["gpt", "4"]
        );
    }

    #[test]
    fn empty_input_produces_no_tokens() {
        assert!(collect("").is_empty());
    }

    #[test]
    fn punctuation_and_whitespace_only_produce_no_tokens() {
        assert!(collect("   \t\r\n").is_empty());
        assert!(collect(" !@#$%^&*()_+-=[]{};:'\",.<>/?\\|`~ ").is_empty());
    }

    #[test]
    fn cjk_punctuation_only_produces_no_tokens() {
        // 全角/CJK 标点不算汉字（不进 jieba），也不是字母数字（非汉字段不产 token）。
        assert!(collect("，。！？；：、（）《》「」【】").is_empty());
    }

    #[test]
    fn pure_digits_form_a_single_token() {
        let tokens = collect("1234567890");
        let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["1234567890"]);
        assert_eq!(tokens[0].offset_from, 0);
        assert_eq!(tokens[0].offset_to, "1234567890".len());
    }

    #[test]
    fn cjk_char_tokenizer_emits_stable_character_positions() {
        let text = "North北极-星2026";
        let mut tokenizer = CjkCharTokenizer;
        let mut stream = tokenizer.token_stream(text);
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().clone());
        }
        assert_eq!(
            tokens
                .iter()
                .map(|token| token.text.as_str())
                .collect::<Vec<_>>(),
            vec!["北", "极", "星"]
        );
        assert_eq!(
            tokens
                .iter()
                .map(|token| token.position)
                .collect::<Vec<_>>(),
            vec![5, 6, 8],
            "非 CJK 字符必须留下位置间隔，不能把不连续文本拼成假短语"
        );
        for token in &tokens {
            assert_eq!(&text[token.offset_from..token.offset_to], token.text);
        }
    }

    #[test]
    fn cjk_detection_uses_unicode_scripts_instead_of_partial_ranges() {
        assert!(is_cjk('北'));
        assert!(is_cjk('𠀀'), "CJK 扩展 B 也属于 Han script");
        assert!(is_cjk('あ'));
        assert!(is_cjk('한'));
        assert!(!is_cjk('A'));
        assert!(!is_cjk('。'));
    }

    #[test]
    fn autocomplete_prefix_tokenizer_ignores_cjk_and_normalizes_words() {
        let mut tokenizer = AutocompletePrefixTokenizer;
        let mut stream = tokenizer.token_stream("北极 NorthStar-2026 搜索");
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().clone());
        }
        assert_eq!(
            tokens
                .iter()
                .map(|token| token.text.as_str())
                .collect::<Vec<_>>(),
            vec![
                "n",
                "no",
                "nor",
                "nort",
                "north",
                "norths",
                "northst",
                "northsta",
                "northstar",
                "2",
                "20",
                "202",
                "2026"
            ]
        );
        assert!(tokens[..9].iter().all(|token| token.position == 0));
        assert!(tokens[9..].iter().all(|token| token.position == 1));
    }

    /// 모든 token에 대해 offset이 원문 조각과 정확히 일치하고 position이 0부터 1씩 증가하는지.
    fn assert_offsets_and_positions(text: &str, tokens: &[Token]) {
        for (i, t) in tokens.iter().enumerate() {
            assert_eq!(t.position, i, "position은 1씩 증가해야 함: {tokens:?}");
            assert_eq!(t.position_length, 1);
            assert_eq!(
                text[t.offset_from..t.offset_to].to_lowercase(),
                t.text,
                "offset이 원문 byte 위치와 어긋남: {t:?}"
            );
        }
    }

    #[test]
    fn hangul_eojeol_is_split_into_morphemes() {
        let text = "배터리팩의";
        let tokens = collect(text);
        let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        assert!(texts.len() >= 2, "어절이 형태소로 나뉘어야 함: {texts:?}");
        assert_eq!(texts.first(), Some(&"배터리"), "{texts:?}");
        assert_eq!(texts.last(), Some(&"의"), "조사가 분리돼야 함: {texts:?}");
        assert_eq!(texts.concat(), text, "형태소를 이어 붙이면 원래 어절");
        assert_offsets_and_positions(text, &tokens);
    }

    #[test]
    fn hangul_query_term_matches_indexed_morpheme() {
        // 검색어 "배터리"와 색인된 "배터리팩의"가 같은 token을 공유해야 주 필드에서 맞는다.
        let query: Vec<String> = collect("배터리").into_iter().map(|t| t.text).collect();
        assert_eq!(query, vec!["배터리"]);
        let doc: Vec<String> = collect("배터리팩의 수명")
            .into_iter()
            .map(|t| t.text)
            .collect();
        assert!(doc.contains(&"배터리".to_string()), "{doc:?}");
    }

    #[test]
    fn mixed_korean_latin_han_offsets_are_exact_bytes() {
        let text = "SMP가 계통한계가격(報告)을 결정한다. API-2026 테스트";
        let tokens = collect(text);
        assert_offsets_and_positions(text, &tokens);
        let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        assert!(texts.contains(&"smp"), "{texts:?}");
        assert!(texts.contains(&"가"), "{texts:?}");
        assert!(texts.contains(&"api"), "{texts:?}");
        assert!(texts.contains(&"2026"), "{texts:?}");
        // 한글 token은 공백·라틴·한자 경계를 넘지 않는다.
        for t in &tokens {
            let s = &text[t.offset_from..t.offset_to];
            if s.chars().any(is_hangul) {
                assert!(
                    s.chars().all(is_hangul),
                    "한글 token이 run 경계를 넘음: {s:?}"
                );
            }
        }
    }

    #[test]
    fn hangul_compat_jamo_and_long_text_do_not_panic() {
        let text = "ㅋㅋㅋ ㄱㄴㄷ 한국어 형태소 분석기 ".repeat(2_000);
        let tokens = collect(&text);
        assert!(!tokens.is_empty());
        assert_offsets_and_positions(&text, &tokens);
    }

    #[test]
    fn ko_spans_cover_run_without_gaps_for_plain_words() {
        let run = "급속충전";
        let spans = ko_spans(run).expect("ko-dic 사전이 로딩돼야 함");
        assert_eq!(spans.first().map(|s| s.0), Some(0));
        assert_eq!(spans.last().map(|s| s.1), Some(run.len()));
        for w in spans.windows(2) {
            assert!(w[0].1 <= w[1].0);
        }
    }

    #[test]
    fn ko_dic_dirs_for_plain_exe_is_only_next_to_exe() {
        let base = std::path::Path::new("/opt/dowse");
        let dirs = ko_dic_dirs_for_exe(&base.join("dowse"));
        assert_eq!(dirs, vec![base.join("ko-dic")]);
    }

    #[test]
    fn ko_dic_dirs_for_macos_app_bundle_add_resources_after_exe_dir() {
        let contents = std::path::Path::new("/Applications/dowse.app/Contents");
        for exe in ["dowse-app", "dowse"] {
            let dirs = ko_dic_dirs_for_exe(&contents.join("MacOS").join(exe));
            assert_eq!(
                dirs,
                vec![
                    contents.join("MacOS").join("ko-dic"),
                    contents.join("Resources").join("ko-dic"),
                ]
            );
        }
    }

    #[test]
    fn ko_dic_dirs_need_both_contents_and_macos_names() {
        // `MacOS` 폴더라도 상위가 `Contents`가 아니면 번들로 보지 않는다.
        let dirs = ko_dic_dirs_for_exe(std::path::Path::new("/tmp/MacOS/dowse"));
        assert_eq!(dirs, vec![std::path::PathBuf::from("/tmp/MacOS/ko-dic")]);
    }

    #[test]
    fn very_long_single_line_does_not_panic_and_tokenizes_each_run() {
        let unit = "alpha-99 ";
        let reps = 20_000;
        let text = unit.repeat(reps);
        let tokens = collect(&text);
        // 每个重复单元产出两个 token：字母段 alpha 和数字段 99。
        assert_eq!(tokens.len(), reps * 2);
        assert_eq!(tokens.first().unwrap().text, "alpha");
        assert_eq!(tokens.last().unwrap().text, "99");
        // position 仍是全程顺序递增的。
        for (i, t) in tokens.iter().enumerate() {
            assert_eq!(t.position, i);
        }
    }
}
