use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};
use rand::{Rng, distributions::Alphanumeric};
use sha2::{Digest, Sha256};

pub fn now() -> i64 {
    Utc::now().timestamp()
}

pub fn random_alnum(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill(&mut buf[..]);
    hex::encode(buf)
}

pub fn sha256_hex(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}

fn offset(tz_hours: i32) -> FixedOffset {
    FixedOffset::east_opt(tz_hours * 3600).unwrap_or_else(|| FixedOffset::east_opt(0).unwrap())
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "Mei", "Jun", "Jul", "Agu", "Sep", "Okt", "Nov", "Des",
];

/// "25 Okt 2026, 09:12"
pub fn fmt_time(ts: i64, tz_hours: i32) -> String {
    match DateTime::from_timestamp(ts, 0) {
        Some(t) => {
            let t = t.with_timezone(&offset(tz_hours));
            use chrono::Datelike;
            format!(
                "{} {} {}, {}",
                t.day(),
                MONTHS[t.month0() as usize],
                t.year(),
                t.format("%H:%M")
            )
        }
        None => "—".into(),
    }
}

pub fn fmt_opt_time(ts: Option<i64>, tz_hours: i32) -> String {
    ts.map(|t| fmt_time(t, tz_hours))
        .unwrap_or_else(|| "—".into())
}

/// Parses an `<input type="datetime-local">` value in the dashboard's time zone.
pub fn parse_local_datetime(value: &str, tz_hours: i32) -> Option<i64> {
    let naive = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M")
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S"))
        .ok()?;
    offset(tz_hours)
        .from_local_datetime(&naive)
        .single()
        .map(|t| t.timestamp())
}

/// Indonesian thousands separator: 128402 -> "128.402"
pub fn fmt_num(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// "andi@contoh.co.id" -> "an***@contoh.co.id"
pub fn mask_email(email: &str) -> String {
    match email.split_once('@') {
        Some((local, domain)) => {
            let keep: String = local.chars().take(2).collect();
            format!("{keep}***@{domain}")
        }
        None => "***".into(),
    }
}

pub fn domain_of(email: &str) -> Option<String> {
    email
        .rsplit_once('@')
        .map(|(_, d)| d.trim().to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_numbers_indonesian_style() {
        assert_eq!(fmt_num(0), "0");
        assert_eq!(fmt_num(999), "999");
        assert_eq!(fmt_num(1000), "1.000");
        assert_eq!(fmt_num(128402), "128.402");
        assert_eq!(fmt_num(-1234567), "-1.234.567");
    }

    #[test]
    fn masks_email() {
        assert_eq!(mask_email("andi@contoh.co.id"), "an***@contoh.co.id");
        assert_eq!(mask_email("a@b.c"), "a***@b.c");
    }

    #[test]
    fn parses_local_datetime_in_wib() {
        let ts = parse_local_datetime("2026-10-26T08:00", 7).unwrap();
        assert_eq!(
            ts,
            Utc.with_ymd_and_hms(2026, 10, 26, 1, 0, 0)
                .unwrap()
                .timestamp()
        );
        assert_eq!(fmt_time(ts, 7), "26 Okt 2026, 08:00");
    }
}
