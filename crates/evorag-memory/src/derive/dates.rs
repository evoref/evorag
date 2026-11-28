//! 日付の表現を決定論に解く (04 §5.1 の「日付」)。
//!
//! 単位の候補の `when` (根拠の文の中の日付の表現の部分文字列、§5.2 の「記録する文字列」) を、同梱の規則
//! (`rules/dates.toml`) とコードの型で `{from, to, basis}` に直す。相対の表現は、発話のターンに記録した UTC からの
//! ずれで、そのターンの時刻を利用者の暦の日に直してから解く。導出の時点のタイムゾーンは使わない (作り直しても
//! 同じ結果にするため)。

use std::collections::BTreeMap;
use std::sync::OnceLock;

use evorag_types::Timestamp;
use serde::Deserialize;

use crate::lexical::find_all;
use crate::rules::fold;
use crate::unit::{UnitWhen, WhenBasis};

/// 1 日のミリ秒。
const DAY_MS: i64 = 86_400_000;

/// `rules/dates.toml` の言語ごとの部分。語は NFKC と casefold で変わらない形で書く。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanguageDates {
    /// 相対の日 (語 → 日のずれ)。
    pub day: BTreeMap<String, i32>,
    /// 相対の週 (語 → 週のずれ。週は月曜から日曜)。
    pub week: BTreeMap<String, i32>,
    /// 相対の月 (語 → 月のずれ)。
    pub month: BTreeMap<String, i32>,
    /// 相対の年 (語 → 年のずれ)。
    pub year: BTreeMap<String, i32>,
    /// 月の名前 (1 月から順に 12 個。無い言語は空)。
    pub month_names: Vec<String>,
}

/// `rules/dates.toml` の全体。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DateRules {
    /// 規則と読み方の版 (導出の指紋の成分、04 §5.3)。
    pub version: u32,
    pub ja: LanguageDates,
    pub en: LanguageDates,
}

impl DateRules {
    /// 言語ごとの部分と、その語を語の境界で当てるか (英語だけ)。
    fn languages(&self) -> [(&LanguageDates, bool); 2] {
        [(&self.ja, false), (&self.en, true)]
    }

    /// 規則の検査。問題の一覧を返す (空なら問題なし)。
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (lang, _) in self.languages() {
            let words = lang
                .day
                .keys()
                .chain(lang.week.keys())
                .chain(lang.month.keys())
                .chain(lang.year.keys())
                .chain(lang.month_names.iter());
            for w in words {
                if w.is_empty() || fold(w) != *w {
                    out.push(format!("`{w}` is not in the folded form"));
                }
            }
            if !(lang.month_names.is_empty() || lang.month_names.len() == 12) {
                out.push("month_names must list 12 names".to_owned());
            }
        }
        out
    }
}

/// 同梱の日付の規則。
///
/// # Panics
///
/// 同梱の `rules/dates.toml` が読めないとき (試験 `bundled_date_rules_are_valid` で防ぐ)。
#[must_use]
pub fn date_rules() -> &'static DateRules {
    static CELL: OnceLock<DateRules> = OnceLock::new();
    CELL.get_or_init(|| {
        toml::from_str(include_str!("../../rules/dates.toml"))
            .unwrap_or_else(|e| panic!("the bundled rules/dates.toml parses: {e}"))
    })
}

/// 発話の時点 (ターンの時刻と、そのターンに記録した UTC からのずれ)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spoken {
    /// ターンの時刻 (UTC)。
    pub at: Timestamp,
    /// UTC からのずれ (分。記録の無いターンは `None`、05 §3.6 の `tz_offset`)。
    pub offset_minutes: Option<i32>,
}

impl Spoken {
    /// 利用者の暦の日 (1970-01-01 からの日数)。ずれが無ければ UTC の日。
    fn local_day(self) -> i64 {
        let shift = i64::from(self.offset_minutes.unwrap_or(0)) * 60_000;
        (self.at.as_millis() + shift).div_euclid(DAY_MS)
    }
}

/// 発話の日 (`basis = utterance`。出来事で日付の表現が無いか読めないとき、04 §5.1)。ずれで直した暦の日 (ずれが
/// 無ければ UTC の日)。年が 4 桁に収まらない時刻は `None`。
#[must_use]
pub fn utterance_day(spoken: Spoken) -> Option<UnitWhen> {
    let d = day_string(spoken.local_day())?;
    Some(UnitWhen {
        from: Some(d.clone()),
        to: Some(d),
        basis: WhenBasis::Utterance,
    })
}

/// 読んだ表現。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reading {
    /// 絶対の日付 (年の無い形は `None`)。
    Day(Option<i64>, u32, u32),
    Month(Option<i64>, u32),
    Year(i64),
    /// 相対の表現 (単位とずれ)。
    Relative(Unit, i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Day,
    Week,
    Month,
    Year,
}

/// 日付の表現 `expr` を解く (04 §5.1)。読めない表現は `None`。相対の表現で、ずれの記録の無いターンは
/// `{from: null, to: null, basis: no_offset}`。
#[must_use]
pub fn resolve(rules: &DateRules, expr: &str, spoken: Spoken) -> Option<UnitWhen> {
    let folded = fold(expr);
    let reading = read(rules, &folded)?;
    let today = spoken.local_day();
    let (year, _, _) = civil_from_days(today);
    let stated = |from: String, to: String| UnitWhen {
        from: Some(from),
        to: Some(to),
        basis: WhenBasis::Stated,
    };
    match reading {
        Reading::Day(y, m, d) => {
            let y = y.unwrap_or(year);
            (d <= days_in_month(y, m)).then_some(())?;
            let s = day_string(days_from_civil(y, m, d))?;
            Some(stated(s.clone(), s))
        }
        Reading::Month(y, m) => {
            let s = month_string(y.unwrap_or(year), m)?;
            Some(stated(s.clone(), s))
        }
        Reading::Year(y) => {
            let s = year_string(y)?;
            Some(stated(s.clone(), s))
        }
        Reading::Relative(unit, n) => {
            if spoken.offset_minutes.is_none() {
                return Some(UnitWhen {
                    from: None,
                    to: None,
                    basis: WhenBasis::NoOffset,
                });
            }
            let n = i64::from(n);
            let (from, to) = match unit {
                Unit::Day => {
                    let s = day_string(today + n)?;
                    (s.clone(), s)
                }
                Unit::Week => {
                    let monday = today - weekday(today) + 7 * n;
                    (day_string(monday)?, day_string(monday + 6)?)
                }
                Unit::Month => {
                    let (y, m, _) = civil_from_days(today);
                    let index = y * 12 + i64::from(m) - 1 + n;
                    let m = u32::try_from(index.rem_euclid(12) + 1).ok()?;
                    let s = month_string(index.div_euclid(12), m)?;
                    (s.clone(), s)
                }
                Unit::Year => {
                    let s = year_string(year + n)?;
                    (s.clone(), s)
                }
            };
            Some(UnitWhen {
                from: Some(from),
                to: Some(to),
                basis: WhenBasis::Relative,
            })
        }
    }
}

/// 表現の中で最も前から始まる一致 (同じ位置なら長い方、数字の形を語より先) を読む。
fn read(rules: &DateRules, folded: &str) -> Option<Reading> {
    // (始まり, 長さの逆, 語か, 読み)。小さいものを採る。
    let mut best: Option<(usize, std::cmp::Reverse<usize>, bool, Reading)> = None;
    let mut offer = |start: usize, len: usize, word: bool, r: Reading| {
        let cand = (start, std::cmp::Reverse(len), word, r);
        if best
            .as_ref()
            .is_none_or(|b| (cand.0, cand.1, cand.2) < (b.0, b.1, b.2))
        {
            best = Some(cand);
        }
    };
    let chars: Vec<(usize, char)> = folded.char_indices().collect();
    let month_names: Vec<&str> = rules
        .languages()
        .iter()
        .flat_map(|(l, _)| l.month_names.iter().map(String::as_str))
        .collect();
    for i in 0..chars.len() {
        if let Some((end, r)) = absolute_at(&chars, i, &month_names) {
            let start = chars[i].0;
            let end_byte = chars.get(end).map_or(folded.len(), |c| c.0);
            offer(start, end_byte - start, false, r);
            break;
        }
    }
    for (lang, bounded) in rules.languages() {
        let tables = [
            (&lang.day, Unit::Day),
            (&lang.week, Unit::Week),
            (&lang.month, Unit::Month),
            (&lang.year, Unit::Year),
        ];
        for (table, unit) in tables {
            for (word, n) in table {
                if let Some(&at) = find_all(folded, word, bounded).first() {
                    offer(at, word.len(), true, Reading::Relative(unit, *n));
                }
            }
        }
    }
    best.map(|b| b.3)
}

/// 文字の並びを読む位置。
#[derive(Debug, Clone, Copy)]
struct Cursor<'c> {
    s: &'c [(usize, char)],
    i: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).map(|c| c.1)
    }

    /// `min`〜`max` 桁の数字 (後ろに数字が続かないもの)。
    fn num(&mut self, min: usize, max: usize) -> Option<u32> {
        let start = self.i;
        let mut v: u32 = 0;
        while let Some(d) = self.peek().and_then(|c| c.to_digit(10)) {
            if self.i - start == max {
                return None;
            }
            v = v * 10 + d;
            self.i += 1;
        }
        (self.i - start >= min).then_some(v)
    }

    fn lit(&mut self, l: &str) -> bool {
        let mut at = self.i;
        for c in l.chars() {
            if self.s.get(at).map(|x| x.1) != Some(c) {
                return false;
            }
            at += 1;
        }
        self.i = at;
        true
    }

    /// `set` のどれか 1 文字。
    fn sep(&mut self, set: &[char]) -> Option<char> {
        let c = self.peek().filter(|c| set.contains(c))?;
        self.i += 1;
        Some(c)
    }

    /// 空白 (1 つ以上を求めるなら `at_least_one`)。
    fn spaces(&mut self, at_least_one: bool) -> bool {
        let start = self.i;
        while self.peek().is_some_and(char::is_whitespace) {
            self.i += 1;
        }
        !at_least_one || self.i > start
    }

    /// 月の名前 (語の境界で)。1 から。
    fn month_name(&mut self, names: &[&str]) -> Option<u32> {
        for (k, name) in names.iter().enumerate() {
            let mut c = *self;
            if c.lit(name) && !c.peek().is_some_and(char::is_alphanumeric) {
                *self = c;
                return u32::try_from(k % 12 + 1).ok();
            }
        }
        None
    }
}

fn month_ok(m: u32) -> bool {
    (1..=12).contains(&m)
}

fn day_ok(d: u32) -> bool {
    (1..=31).contains(&d)
}

/// 位置 `i` から始まる絶対の日付の形を読む (読めたら、終わりの文字の位置と読み)。前が数字か英数字の語の途中なら
/// 読まない。
fn absolute_at(s: &[(usize, char)], i: usize, months: &[&str]) -> Option<(usize, Reading)> {
    let prev = i.checked_sub(1).map(|p| s[p].1);
    if prev.is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    let start = Cursor { s, i };
    let year = |n: u32| i64::from(n);
    // 数字で始まる形 (長いものから)。
    type Shape = fn(&mut Cursor<'_>) -> Option<Reading>;
    let numeric: [Shape; 7] = [
        |c| {
            let y = c.num(4, 4)?;
            c.lit("年").then_some(())?;
            let m = c.num(1, 2).filter(|&m| month_ok(m))?;
            c.lit("月").then_some(())?;
            let d = c.num(1, 2).filter(|&d| day_ok(d))?;
            c.lit("日").then_some(())?;
            Some(Reading::Day(Some(i64::from(y)), m, d))
        },
        |c| {
            let y = c.num(4, 4)?;
            let sep = c.sep(&['-', '/', '.'])?;
            let m = c.num(1, 2).filter(|&m| month_ok(m))?;
            c.sep(&[sep])?;
            let d = c.num(1, 2).filter(|&d| day_ok(d))?;
            Some(Reading::Day(Some(i64::from(y)), m, d))
        },
        |c| {
            let y = c.num(4, 4)?;
            c.lit("年").then_some(())?;
            let m = c.num(1, 2).filter(|&m| month_ok(m))?;
            c.lit("月").then_some(())?;
            Some(Reading::Month(Some(i64::from(y)), m))
        },
        |c| {
            let y = c.num(4, 4)?;
            c.sep(&['-', '/'])?;
            let m = c.num(1, 2).filter(|&m| month_ok(m))?;
            Some(Reading::Month(Some(i64::from(y)), m))
        },
        |c| {
            let m = c.num(1, 2).filter(|&m| month_ok(m))?;
            c.lit("月").then_some(())?;
            let d = c.num(1, 2).filter(|&d| day_ok(d))?;
            c.lit("日").then_some(())?;
            Some(Reading::Day(None, m, d))
        },
        |c| {
            let m = c.num(1, 2).filter(|&m| month_ok(m))?;
            c.sep(&['/'])?;
            let d = c.num(1, 2).filter(|&d| day_ok(d))?;
            Some(Reading::Day(None, m, d))
        },
        |c| {
            let y = c.num(4, 4)?;
            c.lit("年").then_some(())?;
            Some(Reading::Year(i64::from(y)))
        },
    ];
    for shape in numeric {
        let mut c = start;
        if let Some(r) = shape(&mut c) {
            return Some((c.i, r));
        }
    }
    // 「3月」(年の無い月)。
    {
        let mut c = start;
        if let Some(m) = c.num(1, 2).filter(|&m| month_ok(m))
            && c.lit("月")
        {
            return Some((c.i, Reading::Month(None, m)));
        }
    }
    // 英語の月の名前の形。前が英数字なら読まない。
    if prev.is_some_and(char::is_alphanumeric) {
        return None;
    }
    // 「14 March 2026」「14 March」。
    {
        let mut c = start;
        if let Some(d) = c.num(1, 2).filter(|&d| day_ok(d))
            && c.spaces(true)
            && let Some(m) = c.month_name(months)
        {
            let mut y_cursor = c;
            if y_cursor.spaces(true)
                && let Some(y) = y_cursor.num(4, 4)
            {
                return Some((y_cursor.i, Reading::Day(Some(year(y)), m, d)));
            }
            return Some((c.i, Reading::Day(None, m, d)));
        }
    }
    // 「March 14, 2026」「March 14」「March 2026」。
    let mut c = start;
    let m = c.month_name(months)?;
    c.spaces(true).then_some(())?;
    let mut y_cursor = c;
    if let Some(y) = y_cursor.num(4, 4) {
        return Some((y_cursor.i, Reading::Month(Some(year(y)), m)));
    }
    let d = c.num(1, 2).filter(|&d| day_ok(d))?;
    let mut y_cursor = c;
    y_cursor.sep(&[',']);
    if y_cursor.spaces(true)
        && let Some(y) = y_cursor.num(4, 4)
    {
        return Some((y_cursor.i, Reading::Day(Some(year(y)), m, d)));
    }
    Some((c.i, Reading::Day(None, m, d)))
}

/// 曜日 (月曜が 0)。1970-01-01 は木曜。
fn weekday(days: i64) -> i64 {
    (days + 3).rem_euclid(7)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// 暦の日から 1970-01-01 からの日数へ (H. Hinnant の `days_from_civil`)。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 1970-01-01 からの日数から暦の日へ (H. Hinnant の `civil_from_days`)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (
        y,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

fn year_string(y: i64) -> Option<String> {
    (0..=9999).contains(&y).then(|| format!("{y:04}"))
}

fn month_string(y: i64, m: u32) -> Option<String> {
    Some(format!("{}-{m:02}", year_string(y)?))
}

fn day_string(days: i64) -> Option<String> {
    let (y, m, d) = civil_from_days(days);
    Some(format!("{}-{m:02}-{d:02}", year_string(y)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-05-20T01:00:00Z (日本時間の 10:00)。
    fn at() -> i64 {
        days_from_civil(2026, 5, 20) * DAY_MS + 3_600_000
    }

    fn spoken(offset: Option<i32>) -> Spoken {
        Spoken {
            at: Timestamp::from_millis(at()),
            offset_minutes: offset,
        }
    }

    fn when(
        expr: &str,
        offset: Option<i32>,
    ) -> Option<(Option<String>, Option<String>, WhenBasis)> {
        resolve(date_rules(), expr, spoken(offset)).map(|w| (w.from, w.to, w.basis))
    }

    fn range(
        from: &str,
        to: &str,
        basis: WhenBasis,
    ) -> Option<(Option<String>, Option<String>, WhenBasis)> {
        Some((Some(from.to_owned()), Some(to.to_owned()), basis))
    }

    #[test]
    fn bundled_date_rules_are_valid() {
        assert_eq!(date_rules().problems(), Vec::<String>::new());
        assert_eq!(date_rules().version, 1);
    }

    #[test]
    fn civil_round_trips() {
        for days in -800_000..800_000 {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(
            day_string(at().div_euclid(DAY_MS)).as_deref(),
            Some("2026-05-20")
        );
        // 2026-05-20 は水曜。
        assert_eq!(weekday(at().div_euclid(DAY_MS)), 2);
    }

    /// verifies: AC-FR-315-4 (相対の日付は、発話のターンに記録した UTC からのずれで解く。導出の時点のタイムゾーンは
    /// 使わない)
    #[test]
    fn relative_expressions_use_the_recorded_offset() {
        let jst = Some(540);
        assert_eq!(
            when("昨日", jst),
            range("2026-05-19", "2026-05-19", WhenBasis::Relative)
        );
        assert_eq!(
            when("一昨日", jst),
            range("2026-05-18", "2026-05-18", WhenBasis::Relative)
        );
        assert_eq!(
            when("先週", jst),
            range("2026-05-11", "2026-05-17", WhenBasis::Relative)
        );
        assert_eq!(
            when("来週", jst),
            range("2026-05-25", "2026-05-31", WhenBasis::Relative)
        );
        assert_eq!(
            when("来月", jst),
            range("2026-06", "2026-06", WhenBasis::Relative)
        );
        assert_eq!(
            when("先々月", jst),
            range("2026-03", "2026-03", WhenBasis::Relative)
        );
        assert_eq!(
            when("去年の夏", jst),
            range("2025", "2025", WhenBasis::Relative)
        );
        assert_eq!(
            when("Yesterday", jst),
            range("2026-05-19", "2026-05-19", WhenBasis::Relative)
        );
        assert_eq!(
            when("last week", jst),
            range("2026-05-11", "2026-05-17", WhenBasis::Relative)
        );
        // 前の日の 23:30 (UTC) は、ずれで直すと次の日になる。
        let late = Spoken {
            at: Timestamp::from_millis(at() - 2 * 3_600_000),
            offset_minutes: Some(-600),
        };
        assert_eq!(
            resolve(date_rules(), "今日", late)
                .and_then(|w| w.from)
                .as_deref(),
            Some("2026-05-19")
        );
    }

    #[test]
    fn relative_expressions_without_an_offset_are_not_resolved() {
        assert_eq!(when("先週", None), Some((None, None, WhenBasis::NoOffset)));
        // 絶対の日付は、ずれが無くても解く (年の無い形は UTC の年)。
        assert_eq!(
            when("3/14", None),
            range("2026-03-14", "2026-03-14", WhenBasis::Stated)
        );
    }

    #[test]
    fn absolute_dates_are_stated() {
        let jst = Some(540);
        assert_eq!(
            when("2026年3月", jst),
            range("2026-03", "2026-03", WhenBasis::Stated)
        );
        assert_eq!(
            when("２０２６年３月１４日", jst),
            range("2026-03-14", "2026-03-14", WhenBasis::Stated)
        );
        assert_eq!(
            when("2025-12-01", jst),
            range("2025-12-01", "2025-12-01", WhenBasis::Stated)
        );
        assert_eq!(
            when("2025/7", jst),
            range("2025-07", "2025-07", WhenBasis::Stated)
        );
        assert_eq!(
            when("7月22日", jst),
            range("2026-07-22", "2026-07-22", WhenBasis::Stated)
        );
        assert_eq!(
            when("2024年", jst),
            range("2024", "2024", WhenBasis::Stated)
        );
        assert_eq!(
            when("March 14, 2026", jst),
            range("2026-03-14", "2026-03-14", WhenBasis::Stated)
        );
        assert_eq!(
            when("14 March", jst),
            range("2026-03-14", "2026-03-14", WhenBasis::Stated)
        );
        assert_eq!(
            when("in May 2025", jst),
            range("2025-05", "2025-05", WhenBasis::Stated)
        );
        assert_eq!(when("2月30日", jst), None, "no such day");
        assert_eq!(when("13/40", jst), None);
        assert_eq!(when("いつか", jst), None);
        assert_eq!(when("12345年", jst), None, "digits continue");
    }

    #[test]
    fn the_earliest_expression_wins() {
        let jst = Some(540);
        assert_eq!(
            when("再来週", jst),
            range("2026-06-01", "2026-06-07", WhenBasis::Relative)
        );
        assert_eq!(
            when("2026年3月の先週", jst),
            range("2026-03", "2026-03", WhenBasis::Stated)
        );
        assert_eq!(
            when("先月の3日", jst),
            range("2026-04", "2026-04", WhenBasis::Relative)
        );
    }

    #[test]
    fn utterance_day_uses_the_offset_or_utc() {
        let late = Spoken {
            at: Timestamp::from_millis(at() - 2 * 3_600_000),
            offset_minutes: None,
        };
        assert_eq!(
            utterance_day(late).and_then(|w| w.from).as_deref(),
            Some("2026-05-19")
        );
        assert_eq!(
            utterance_day(spoken(Some(540))).map(|w| w.basis),
            Some(WhenBasis::Utterance)
        );
    }
}
