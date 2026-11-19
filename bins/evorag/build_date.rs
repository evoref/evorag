//! build.rs が使う、`EVORAG_RELEASE_DATE` の確かめ (12 §5.3・§7.4)。
//!
//! build.rs から evorag-update の依存の木を引き込まないように、ここに小さく持つ。evorag-update の試験
//! (`tests/build_date.rs`) が、この関数と `ReleaseDate` の読み方が同じであることを確かめる。

/// `YYYY-MM-DD` の実在する日付か (evorag-update の `ReleaseDate` と同じ条件。build.rs から evorag-update の
/// 依存の木を引き込まないように、ここに小さく持つ)。
pub fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    let shape = b.len() == 10
        && b.iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        });
    if !shape {
        return false;
    }
    let num = |r: std::ops::Range<usize>| s[r].parse::<u32>().unwrap_or(0);
    let (year, month, day) = (num(0..4), num(5..7), num(8..10));
    let leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    };
    year >= 1 && (1..=days).contains(&day)
}
