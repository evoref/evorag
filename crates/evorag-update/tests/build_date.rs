//! bins/evorag の build.rs が埋め込む `EVORAG_RELEASE_DATE` の確かめが、`ReleaseDate` の読み方と同じで
//! あること (12 §5.3・§7.4)。build.rs は evorag-update に依存しないので、同じ条件を小さく持っている。

#[path = "../../../bins/evorag/build_date.rs"]
mod build_date;

use evorag_update::ReleaseDate;

#[test]
fn the_build_script_accepts_exactly_the_dates_release_date_reads() {
    let mut cases: Vec<String> = [
        "2026-11-18",
        "2024-02-29",
        "2000-02-29",
        "1900-02-29",
        "2026-02-29",
        "2026-04-31",
        "2026-13-01",
        "2026-00-10",
        "0000-01-01",
        "0001-01-01",
        "9999-12-31",
        "2026/11/18",
        "2026-1-18",
        "2026-11-18 ",
        "",
        "２０２６-11-18",
        "+026-11-18",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    for month in 0..=13 {
        for day in [0, 1, 28, 29, 30, 31, 32] {
            cases.push(format!("2023-{month:02}-{day:02}"));
            cases.push(format!("2024-{month:02}-{day:02}"));
        }
    }
    for case in &cases {
        assert_eq!(
            build_date::is_date(case),
            case.parse::<ReleaseDate>().is_ok(),
            "{case:?}"
        );
    }
}
