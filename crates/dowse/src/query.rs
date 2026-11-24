//! Shared GUI/CLI/MCP syntax. Plain input keeps the searcher's typeahead path.
//! Structured expressions bind NOT before AND (including whitespace), then OR.

use anyhow::{Result, bail};

/// 比较符：日期和体积过滤共用同一组。
///
/// 只支持这四种；`mtime:` / `size:` 后面缺了比较符会直接报错，而不是猜一个默认
/// 语义——过滤条件写错却静默生效，比直接报错危险得多（用户会以为过滤起作用了）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cmp {
    /// `>`：严格大于。
    Gt,
    /// `>=`：大于等于。
    Ge,
    /// `<`：严格小于。
    Lt,
    /// `<=`：小于等于。
    Le,
}

/// 一条 `mtime:` 过滤，已把日期归约成"当天 00:00 的毫秒时间戳"和"次日 00:00 的
/// 毫秒时间戳"两个边界（都按 UTC，跟索引里存的 mtime 口径一致——见 indexer 的
/// `file_stat`，取的是 `SystemTime` 相对 `UNIX_EPOCH` 的毫秒数）。
///
/// 为什么按"整天"而不是"某一瞬间"来比：用户写 `mtime:>2026-01-01` 的直觉是
/// "在这一天之后修改的"，把 1 月 1 号一整天都算进去或排除掉，比纠结当天几点几分
/// 更符合预期。所以：
/// - `>D`  → mtime ≥ 次日 0 点（1 号当天不算，从 2 号起）；
/// - `>=D` → mtime ≥ 当天 0 点（含 1 号当天）；
/// - `<D`  → mtime < 当天 0 点（不含 1 号当天）；
/// - `<=D` → mtime < 次日 0 点（含 1 号一整天）。
///
/// `YYYY-MM` 只精确到月：把"当天"换成"当月 1 号 0 点"、"次日"换成"次月 1 号 0 点"，
/// 语义完全同构。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DateBound {
    /// 比较符。
    pub cmp: Cmp,
    /// 日期粒度起点（当天 / 当月 1 号）的 UTC 毫秒时间戳。
    pub start_ms: i64,
    /// 日期粒度终点（次日 / 次月 1 号）的 UTC 毫秒时间戳。
    pub next_ms: i64,
}

/// 一条 `size:` 过滤，已把 `10mb` / `500kb` 这类写法折算成字节数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SizeBound {
    /// 比较符。
    pub cmp: Cmp,
    /// 折算成字节的阈值。
    pub bytes: u64,
}

/// 一条 `path:` 过滤的操作数。
///
/// `phrase` 记录用户是否给操作数加了引号（`path:"my docs"`）：加了就按整体（相邻
/// 短语）匹配路径，没加就按分词后的词/子词各自匹配。真正建查询时用得到这个区分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathTerm {
    /// 剥掉引号后的路径关键词原文（大小写保留，交给分词器时再统一小写）。
    pub operand: String,
    /// 是否是带引号的整体匹配。
    pub phrase: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextField {
    Name,
    Body,
    Path,
    Author,
    Title,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Expr<'a> {
    Term(&'a str),
    Text(TextField, PathTerm),
    Ext(Vec<String>),
    Mtime(DateBound),
    Size(SizeBound),
    Not(Box<Self>),
    And(Vec<Self>),
    Or(Vec<Self>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Token<'a> {
    Term(&'a str),
    And,
    Or,
    Not,
    Open,
    Close,
}

fn split_field(token: &str) -> Option<(&str, &str)> {
    let (field, rest) = token.split_once(':')?;
    (!field.is_empty() && field.bytes().all(|b| b.is_ascii_alphabetic()))
        .then_some((field, rest))
}

fn known_field(field: &str) -> bool {
    ["name", "body", "content", "ext", "path", "author", "title", "mtime", "size"]
        .iter()
        .any(|name| field.eq_ignore_ascii_case(name))
}

fn tokenize(mut input: &str) -> Result<Vec<Token<'_>>> {
    let mut out = Vec::new();
    loop {
        input = input.trim_start();
        if input.is_empty() {
            return Ok(out);
        }
        let punct = match input.as_bytes()[0] {
            b'(' => Some(Token::Open),
            b')' => Some(Token::Close),
            b'-' if input[1..].starts_with(|ch: char| !ch.is_whitespace()) => Some(Token::Not),
            _ => None,
        };
        if let Some(token) = punct {
            out.push(token);
            input = &input[1..];
            continue;
        }

        let mut quoted = false;
        let mut escaped = false;
        let mut end = input.len();
        for (i, ch) in input.char_indices() {
            if escaped {
                escaped = false;
            } else if quoted && ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = !quoted;
            } else if !quoted && (ch.is_whitespace() || matches!(ch, '(' | ')')) {
                end = i;
                break;
            }
        }
        if quoted {
            bail!("닫는 큰따옴표가 필요합니다.");
        }
        out.push(match &input[..end] {
            "AND" => Token::And,
            "OR" => Token::Or,
            "NOT" => Token::Not,
            text => Token::Term(text),
        });
        input = &input[end..];
    }
}

pub(crate) fn parse(query: &str) -> Result<Option<Expr<'_>>> {
    let tokens = tokenize(query)?;
    if tokens.iter().all(|token| match token {
        Token::Term(text) => !split_field(text).is_some_and(|(name, _)| known_field(name)),
        _ => false,
    }) {
        return Ok(None);
    }
    let mut parser = Parser { remaining: &tokens };
    let expr = parser.or(0)?;
    if !parser.remaining.is_empty() {
        bail!("여는 괄호 없이 닫는 괄호가 나왔습니다.");
    }
    Ok(Some(expr))
}

struct Parser<'a, 't> {
    remaining: &'t [Token<'a>],
}

impl<'a> Parser<'a, '_> {
    fn eat(&mut self, token: Token<'_>) -> bool {
        if self.remaining.first().is_some_and(|next| *next == token) {
            self.remaining = &self.remaining[1..];
            true
        } else {
            false
        }
    }

    fn or(&mut self, depth: usize) -> Result<Expr<'a>> {
        let first = self.and(depth)?;
        if !self.eat(Token::Or) {
            return Ok(first);
        }
        let mut terms = vec![first];
        loop {
            terms.push(self.and(depth)?);
            if !self.eat(Token::Or) {
                return Ok(Expr::Or(terms));
            }
        }
    }

    fn and(&mut self, depth: usize) -> Result<Expr<'a>> {
        let first = self.unary(depth)?;
        if matches!(self.remaining.first(), None | Some(Token::Or | Token::Close)) {
            return Ok(first);
        }
        let mut terms = vec![first];
        loop {
            self.eat(Token::And);
            terms.push(self.unary(depth)?);
            if matches!(self.remaining.first(), None | Some(Token::Or | Token::Close)) {
                return Ok(Expr::And(terms));
            }
        }
    }

    fn unary(&mut self, depth: usize) -> Result<Expr<'a>> {
        if depth >= 32 {
            bail!("괄호와 제외 조건은 32단계 미만으로 입력하세요.");
        }
        if self.eat(Token::Not) {
            return Ok(Expr::Not(Box::new(self.unary(depth + 1)?)));
        }
        if self.eat(Token::Open) {
            let expr = self.or(depth + 1)?;
            if !self.eat(Token::Close) {
                bail!("닫는 괄호가 필요합니다.");
            }
            return Ok(expr);
        }
        if let Some((&Token::Term(term), rest)) = self.remaining.split_first() {
            self.remaining = rest;
            return parse_term(term);
        }
        bail!("검색어나 필드 조건이 필요합니다.");
    }
}

fn parse_term(token: &str) -> Result<Expr<'_>> {
    let Some((field, rest)) = split_field(token).filter(|(field, _)| known_field(field)) else {
        return Ok(Expr::Term(token));
    };
    let field = field.to_ascii_lowercase();
    let (operand, phrase) = match rest.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(inner) => (inner, true),
        None => (rest, false),
    };
    if operand.trim().is_empty() {
        bail!("{field}: 뒤에 검색 조건을 입력하세요.");
    }
    let text_field = match field.as_str() {
        "name" => TextField::Name,
        "body" | "content" => TextField::Body,
        "path" => TextField::Path,
        "author" => TextField::Author,
        "title" => TextField::Title,
        "mtime" => return Ok(Expr::Mtime(parse_date_bound(operand)?)),
        "size" => return Ok(Expr::Size(parse_size_bound(operand)?)),
        "ext" => {
            let mut exts = Vec::new();
            for ext in operand.split(',').map(str::trim) {
                let ext = ext.strip_prefix('.').unwrap_or(ext);
                if ext.is_empty() || ext.chars().any(|ch| ch.is_whitespace() || ch == '.') {
                    bail!("확장자를 `ext:pdf` 또는 `ext:pdf,docx`처럼 입력하세요.");
                }
                exts.push(ext.to_ascii_lowercase());
            }
            return Ok(Expr::Ext(exts));
        }
        _ => unreachable!("known field"),
    };
    Ok(Expr::Text(text_field, PathTerm {
        operand: operand.to_owned(),
        phrase,
    }))
}

/// 从 `>=2026-01-01` 这类操作数里切出比较符和后面的实体，比较符缺失直接报错。
/// 注意 `>=` / `<=` 要在 `>` / `<` 之前判，否则会把两字符符号错切成单字符。
fn split_cmp(operand: &str) -> Result<(Cmp, &str)> {
    if let Some(rest) = operand.strip_prefix(">=") {
        Ok((Cmp::Ge, rest))
    } else if let Some(rest) = operand.strip_prefix("<=") {
        Ok((Cmp::Le, rest))
    } else if let Some(rest) = operand.strip_prefix('>') {
        Ok((Cmp::Gt, rest))
    } else if let Some(rest) = operand.strip_prefix('<') {
        Ok((Cmp::Lt, rest))
    } else {
        bail!(
            "过滤条件 \"{operand}\" 缺少比较符，要写成 >、>=、< 或 <=，\
             例如 mtime:>2026-01-01 或 size:<500kb"
        )
    }
}

/// 解析 `mtime:` 操作数，支持 `YYYY-MM-DD` 和 `YYYY-MM` 两种日期粒度。
fn parse_date_bound(operand: &str) -> Result<DateBound> {
    let (cmp, date_str) = split_cmp(operand)?;
    let parts: Vec<&str> = date_str.split('-').collect();
    let bad = || format!("日期 \"{date_str}\" 格式不对，要写成 YYYY-MM-DD 或 YYYY-MM");

    let parse_num =
        |s: &str| -> Result<i64> { s.parse::<i64>().map_err(|_| anyhow::anyhow!(bad())) };

    let (year, month, day, has_day) = match parts.as_slice() {
        [y, m, d] => (parse_num(y)?, parse_num(m)?, parse_num(d)?, true),
        [y, m] => (parse_num(y)?, parse_num(m)?, 1, false),
        _ => bail!(bad()),
    };
    if !(1..=12).contains(&month) {
        bail!("日期 \"{date_str}\" 的月份要在 1..=12 之间");
    }
    let dim = days_in_month(year, month as u32);
    if has_day && !(1..=dim as i64).contains(&day) {
        bail!("日期 \"{date_str}\" 的日要在 1..={dim} 之间");
    }

    let start_ms = date_to_ms(year, month as u32, day as u32);
    // 同一天/同一月的"下一格"起点：有日就 +1 天（UTC 下一天恒为 86_400_000 毫秒，
    // 没有夏令时/闰秒的坑），没日就跳到次月 1 号。
    let next_ms = if has_day {
        start_ms + MS_PER_DAY
    } else {
        let (ny, nm) = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month as u32 + 1)
        };
        date_to_ms(ny, nm, 1)
    };
    Ok(DateBound {
        cmp,
        start_ms,
        next_ms,
    })
}

/// 解析 `size:` 操作数：数字 + 可选单位（kb/mb/gb，大小写不敏感；无单位或 b 按字节）。
fn parse_size_bound(operand: &str) -> Result<SizeBound> {
    let (cmp, rest) = split_cmp(operand)?;
    let rest = rest.trim();
    if rest.is_empty() {
        bail!("size: 后面要跟体积，例如 size:>10mb 或 size:<500kb");
    }

    // 数字部分：数字和小数点连续段；剩下的当单位。允许 `1.5mb` 这种小数写法。
    let split_at = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    let (num_str, unit_str) = rest.split_at(split_at);
    let num: f64 = num_str
        .parse()
        .map_err(|_| anyhow::anyhow!("体积 \"{rest}\" 里的数字部分无法解析"))?;
    if num < 0.0 {
        bail!("体积不能是负数：\"{rest}\"");
    }

    let multiplier: f64 = match unit_str.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        other => bail!("不认识的体积单位 \"{other}\"，只支持 kb / mb / gb（或省略按字节）"),
    };

    let bytes = (num * multiplier).round();
    // f64 乘出来可能超出 u64（比如 `size:>99999999gb`）；夹到 u64::MAX 而不是溢出 panic。
    let bytes = if bytes >= u64::MAX as f64 {
        u64::MAX
    } else {
        bytes as u64
    };
    Ok(SizeBound { cmp, bytes })
}

/// 一天的毫秒数（UTC，无夏令时/闰秒）。
const MS_PER_DAY: i64 = 86_400_000;

/// 某年某月的天数（处理闰年二月）。
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

/// 公历闰年判定。
fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// 把公历 (年,月,日) 折算成 UTC 0 点相对 1970-01-01 的毫秒时间戳，跟索引里 mtime
/// 的口径一致。用 Howard Hinnant 的 days_from_civil 算法直接算"距纪元的天数"，
/// 不引入 chrono/time 依赖——只做日期到时间戳的定点换算，一个纯算术函数足够，
/// 没必要为此多背一个日期库。
fn date_to_ms(year: i64, month: u32, day: u32) -> i64 {
    let m = month as i64;
    let d = day as i64;
    // 把 1、2 月算作上一年的 13、14 月，好让 2 月的闰日落在"年末"，公式更规整。
    let y = if m <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    let days = era * 146097 + doe - 719468; // 距 1970-01-01 的天数
    days * MS_PER_DAY
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_words_and_quoted_operators_use_plain_search() {
        for q in ["限流 中间件", "\"(AND OR NOT)\"", "cats or dogs", "shall not pass"] {
            assert_eq!(parse(q).unwrap(), None, "{q}");
        }
    }

    #[test]
    fn date_to_ms_epoch_and_known_dates() {
        assert_eq!(date_to_ms(1970, 1, 1), 0);
        assert_eq!(date_to_ms(1970, 1, 2), MS_PER_DAY);
        // 2000-01-01 距纪元 10957 天（含 8 个闰年：72,76,...,96 共 8 个 + 无 1900）。
        assert_eq!(date_to_ms(2000, 1, 1), 10957 * MS_PER_DAY);
    }

    #[test]
    fn parse_mtime_day_bounds() {
        let b = parse_date_bound(">2026-01-01").unwrap();
        assert_eq!(b.cmp, Cmp::Gt);
        assert_eq!(b.start_ms, date_to_ms(2026, 1, 1));
        assert_eq!(b.next_ms, date_to_ms(2026, 1, 2));

        let b = parse_date_bound(">=2026-07").unwrap();
        assert_eq!(b.cmp, Cmp::Ge);
        assert_eq!(b.start_ms, date_to_ms(2026, 7, 1));
        // YYYY-MM 的 next 是次月 1 号。
        assert_eq!(b.next_ms, date_to_ms(2026, 8, 1));

        // 12 月的次月要跨年。
        let b = parse_date_bound("<2026-12").unwrap();
        assert_eq!(b.next_ms, date_to_ms(2027, 1, 1));
    }

    #[test]
    fn parse_mtime_rejects_garbage() {
        assert!(parse_date_bound(">abc").is_err());
        assert!(parse_date_bound(">2026-13-01").is_err(), "月份越界应报错");
        assert!(parse_date_bound(">2026-02-30").is_err(), "2 月没有 30 号");
        assert!(parse_date_bound("2026-01-01").is_err(), "缺比较符应报错");
    }

    #[test]
    fn parse_size_units_case_insensitive() {
        assert_eq!(parse_size_bound(">10mb").unwrap().bytes, 10 * 1024 * 1024);
        assert_eq!(parse_size_bound("<500KB").unwrap().bytes, 500 * 1024);
        assert_eq!(parse_size_bound(">=1Gb").unwrap().bytes, 1024 * 1024 * 1024);
        // 无单位按字节。
        assert_eq!(parse_size_bound(">1024").unwrap().bytes, 1024);
        // 小数写法。
        assert_eq!(
            parse_size_bound(">1.5mb").unwrap().bytes,
            (1.5 * 1024.0 * 1024.0) as u64
        );
    }

    #[test]
    fn parse_size_rejects_garbage() {
        assert!(parse_size_bound(">abc").is_err());
        assert!(parse_size_bound(">10tb").is_err(), "不支持的单位应报错");
        assert!(parse_size_bound("10mb").is_err(), "缺比较符应报错");
    }

    #[test]
    fn malformed_structured_queries_do_not_broaden_results() {
        for q in [
            "name:", "body:\"\"", "path:", "author:", "title:\"unfinished", "ext:pdf,",
            "AND alpha", "alpha OR", "alpha OR OR beta", "alpha AND", "NOT",
            "alpha AND (beta OR)", "alpha AND (beta", "alpha )", "()", "NOT )",
        ] {
            assert!(parse(q).is_err(), "{q}");
        }
    }
}
