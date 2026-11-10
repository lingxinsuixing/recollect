//! C29 的日期尺：只做"从原文认出日期 + 换成 UTC 毫秒"，**不做正文归一**。
//! `normalize_body` 是 `turn_uid`/`mem_id` 的 preimage 尺（P-21），在它里面加日期归一
//! 会让存量 id 全体漂移 ⇒ 日期这一把尺单独住这里（也不引 rc-contract：本 crate 的红线）。
//! 支持的写法：`YYYY-MM-DD` / `YYYY/MM/DD` / `YYYY.MM.DD` / `YYYY年MM月DD日`；月日可一位。

fn is_leap(y: i64) -> bool { (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 }

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 0,
    }
}

/// 消费 1–2 位数字；`after` 必须是分隔符（不是数字）。
fn digits_at(chars: &[char], mut i: usize, max: usize) -> Option<(u32, usize)> {
    let start = i;
    let mut v = 0u32;
    while i < chars.len() && i - start < max && chars[i].is_ascii_digit() {
        v = v * 10 + chars[i].to_digit(10)?;
        i += 1;
    }
    if i == start || (i < chars.len() && chars[i].is_ascii_digit()) { return None; }
    Some((v, i))
}

/// 从 `i` 起解析一个日期；返回归一串与消费到的位置。
fn date_at(chars: &[char], i: usize) -> Option<(String, usize)> {
    if i + 4 > chars.len() || !chars[i..i + 4].iter().all(char::is_ascii_digit) { return None; }
    let y: i64 = chars[i..i + 4].iter().collect::<String>().parse().ok()?;
    if y < 1 { return None; }
    let sep = *chars.get(i + 4)?;
    let chinese = sep == '年';
    if !chinese && !matches!(sep, '-' | '/' | '.') { return None; }
    let (m, mut j) = digits_at(chars, i + 5, 2)?;
    let sep2 = *chars.get(j)?;
    if chinese {
        if sep2 != '月' { return None; }
    } else if sep2 != sep { return None; }
    j += 1;
    let (d, mut k) = digits_at(chars, j, 2)?;
    if chinese && chars.get(k) == Some(&'日') { k += 1; }
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) { return None; }
    Some((format!("{y:04}-{m:02}-{d:02}"), k))
}

pub fn canonical_date(s: &str) -> Option<String> {
    let chars: Vec<char> = s.trim().chars().collect();
    let (out, end) = date_at(&chars, 0)?;
    (end == chars.len()).then_some(out)
}

pub fn dates_in(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if let Some((d, end)) = date_at(&chars, i) {
            if !out.contains(&d) { out.push(d); }
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

/// Howard Hinnant 的 days_from_civil：纯整数、无依赖，负年也成立（本尺只收 y>=1）。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    // `as i64` 必须落在整个 if 表达式外：写在 else 臂上只转 `m + 9`（`u32`），两臂类型不符。
    let mp = (if m > 2 { m - 3 } else { m + 9 }) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn to_epoch_ms(date: &str) -> Option<i64> {
    let canon = canonical_date(date)?;
    let y: i64 = canon[0..4].parse().ok()?;
    let m: u32 = canon[5..7].parse().ok()?;
    let d: u32 = canon[8..10].parse().ok()?;
    Some(days_from_civil(y, m, d) * 86_400_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 四种写法归一成同一个串() {
        for s in ["2026-10-15", "2026/10/15", "2026.10.15", "2026年10月15日"] {
            assert_eq!(canonical_date(s).as_deref(), Some("2026-10-15"), "{s}");
        }
        assert_eq!(canonical_date("2026-2-3").as_deref(), Some("2026-02-03"));
    }

    #[test]
    fn 非法日期一律拒绝_不猜() {
        for s in ["2026-13-01", "2026-00-10", "2026-02-30", "2025-02-29", "2026-10-155", "26-10-15", ""] {
            assert!(canonical_date(s).is_none(), "{s} 不该被认成日期");
        }
        assert!(canonical_date("2024-02-29").is_some(), "闰日必须认");
    }

    #[test]
    fn 文本扫描_去重保序且不截半个日期() {
        let got = dates_in("会上说 2026年10月15日 发布，2026-10-16 复盘，再说一次 2026-10-15；旧串 2026-10-155 不算。");
        assert_eq!(got, vec!["2026-10-15".to_string(), "2026-10-16".to_string()]);
    }

    #[test]
    fn utc_零点毫秒() {
        assert_eq!(to_epoch_ms("1970-01-01"), Some(0));
        assert_eq!(to_epoch_ms("2026-10-15"), Some(1_792_022_400_000));
        assert!(to_epoch_ms("2026-02-30").is_none());
    }
}

/// 行的时刻标签（固定 `+08:00`，分钟级）：只给推导面的**排序**用。
///
/// 它**不是**依据日期：`parse` 仍要求 timeline 的绝对日期逐字出现在依据行正文里
/// （`parse.rs` 的 `date_not_verbatim`）。把行钟当日期写进 timeline 会被拒——这是有意的，
/// 行钟证明的是"哪行更早"，不是"事件发生在哪天"。
pub fn local_ms_label(ms: i64) -> String {
    if ms <= 0 { return "unknown".to_string(); }
    let secs = ms.div_euclid(1000) + 8 * 3600;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}+08:00", sod / 3600, (sod % 3600) / 60)
}

/// Howard Hinnant 的 `civil_from_days`：days 自 1970-01-01 起算。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;                                  // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);            // [0, 365]
    let mp = (5 * doy + 2) / 153;                                  // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;                 // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod local_ms_label_tests {
    use super::local_ms_label;

    #[test]
    fn known_instant_is_utc8_minute_label() {
        // 2026-10-26T14:05+08:00（pass_prompt 测试用的同一时刻）。
        assert_eq!(local_ms_label(1_792_994_700_000), "2026-10-26T14:05+08:00");
        assert_eq!(local_ms_label(0), "unknown");
    }
}
