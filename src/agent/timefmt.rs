//! Dates without a dependency: UTC timestamps as `2026-11-22T10:30:00Z`.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian, Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn format_iso(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

pub fn now_iso() -> String {
    format_iso(now_secs())
}

/// `2026-11-22T10:30:00Z`, also with fractions of a second and a numeric offset (`+01:00`).
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let part = s.get(from..from + len)?;
        if part.bytes().all(|c| c.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !(b[10] == b'T' || b[10] == b't' || b[10] == b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (y, mo, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (h, mi, sec) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    let offset = match b.get(i) {
        Some(b'Z' | b'z') if i + 1 == b.len() => 0,
        Some(sign @ (b'+' | b'-')) if b.len() == i + 6 && b[i + 3] == b':' => {
            let off = num(i + 1, 2)? * 3600 + num(i + 4, 2)? * 60;
            if *sign == b'+' { off } else { -off }
        }
        _ => return None,
    };
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec - offset)
}

/// Whole days from `now` until `iso` (negative when it has passed). None when `iso` cannot be read.
pub fn days_until(iso: &str, now: i64) -> Option<i64> {
    parse_iso(iso).map(|t| (t - now).div_euclid(86_400))
}

/// Just the date of a timestamp, for screens: `2026-11-22`.
pub fn date_of(iso: &str) -> String {
    iso.chars().take(10).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_known_values() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(format_iso(951_868_800), "2000-03-01T00:00:00Z");
        for t in [0, 86_399, 1_000_000_000, 1_795_000_000, 4_102_444_800] {
            assert_eq!(parse_iso(&format_iso(t)), Some(t));
        }
        assert_eq!(format_iso(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn fractions_and_offsets() {
        assert_eq!(
            parse_iso("2026-11-22T10:00:00.123Z"),
            parse_iso("2026-11-22T10:00:00Z")
        );
        assert_eq!(
            parse_iso("2026-11-22T11:00:00+01:00"),
            parse_iso("2026-11-22T10:00:00Z")
        );
    }

    #[test]
    fn rejects_rubbish() {
        for bad in [
            "",
            "tomorrow",
            "2026-13-01T00:00:00Z",
            "2026-11-22T25:00:00Z",
            "2026-11-22T10:00:00",
            "2026-11-22T10:00:00.Z",
            "2026-11-22T10:00:00+0100",
        ] {
            assert_eq!(parse_iso(bad), None, "{bad}");
        }
    }

    #[test]
    fn days_left() {
        let now = parse_iso("2026-11-22T10:00:00Z").unwrap();
        assert_eq!(days_until("2026-12-06T10:00:00Z", now), Some(14));
        assert_eq!(days_until("2026-12-06T09:00:00Z", now), Some(13));
        assert_eq!(days_until("2026-11-21T10:00:00Z", now), Some(-1));
        assert_eq!(days_until("x", now), None);
    }
}
