//! VTIMEZONE components for outgoing iCalendar data, built from the IANA database: the
//! year's offset transitions become yearly STANDARD/DAYLIGHT rules (the form Exchange and
//! Outlook emit and expect), so a receiver never has to guess what a local time means.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc, Weekday};
use chrono_tz::Tz;

/// The IANA zone named `name`, if it is one.
pub fn zone(name: &str) -> Option<Tz> {
    name.parse().ok()
}

/// `20260105T100000`: the wall-clock time of `t` in `tz` (for `DTSTART;TZID=…`).
pub fn local_stamp(t: &DateTime<Utc>, tz: Tz) -> String {
    t.with_timezone(&tz).format("%Y%m%dT%H%M%S").to_string()
}

fn offset_at(tz: Tz, t: DateTime<Utc>) -> i32 {
    tz.offset_from_utc_datetime(&t.naive_utc()).fix().local_minus_utc()
}

fn format_offset(secs: i32) -> String {
    let sign = if secs < 0 { '-' } else { '+' };
    let abs = secs.abs();
    format!("{sign}{:02}{:02}", abs / 3600, abs % 3600 / 60)
}

fn weekday_code(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "MO",
        Weekday::Tue => "TU",
        Weekday::Wed => "WE",
        Weekday::Thu => "TH",
        Weekday::Fri => "FR",
        Weekday::Sat => "SA",
        Weekday::Sun => "SU",
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (y, m) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    NaiveDate::from_ymd_opt(y, m, 1).and_then(|d| d.pred_opt()).map_or(28, |d| d.day())
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Transition {
    at: DateTime<Utc>,
    from: i32,
    to: i32,
}

/// The offset changes during `year`, found hourly and narrowed to the minute.
fn transitions(tz: Tz, year: i32) -> Vec<Transition> {
    let Some(start) = NaiveDate::from_ymd_opt(year, 1, 1).and_then(|d| d.and_hms_opt(0, 0, 0)) else {
        return Vec::new();
    };
    let start = start.and_utc();
    let end = start + Duration::days(366);
    let mut out = Vec::new();
    let mut prev = offset_at(tz, start);
    let mut t = start;
    while t < end {
        let next = t + Duration::hours(1);
        let off = offset_at(tz, next);
        if off != prev {
            let (mut lo, mut hi) = (t, next);
            while hi - lo > Duration::minutes(1) {
                let mid = lo + (hi - lo) / 2;
                if offset_at(tz, mid) == prev { lo = mid } else { hi = mid }
            }
            if hi.year() == year {
                out.push(Transition { at: hi, from: prev, to: off });
            }
            prev = off;
        }
        t = next;
    }
    out
}

/// `BYDAY` for the day a transition falls on: `-1SU` in a month's last week, else `2SU` etc.
fn byday(date: NaiveDate) -> (i32, Weekday) {
    let d = date.day();
    let n = if d + 7 > days_in_month(date.year(), date.month()) { -1 } else { ((d - 1) / 7 + 1) as i32 };
    (n, date.weekday())
}

/// The first date in `year` matching `BYMONTH=month;BYDAY=<n><weekday>`.
fn rule_date(year: i32, month: u32, n: i32, weekday: Weekday) -> Option<NaiveDate> {
    if n < 0 {
        let last = NaiveDate::from_ymd_opt(year, month, days_in_month(year, month))?;
        let back = (last.weekday().num_days_from_monday() + 7 - weekday.num_days_from_monday()) % 7;
        last.checked_sub_signed(Duration::days(back as i64))
    } else {
        NaiveDate::from_weekday_of_month_opt(year, month, weekday, n as u8)
    }
}

fn observance(kind: &str, dtstart: NaiveDateTime, from: i32, to: i32, rrule: Option<String>) -> Vec<String> {
    let mut lines = vec![
        format!("BEGIN:{kind}"),
        format!("DTSTART:{}", dtstart.format("%Y%m%dT%H%M%S")),
        format!("TZOFFSETFROM:{}", format_offset(from)),
        format!("TZOFFSETTO:{}", format_offset(to)),
    ];
    lines.extend(rrule.map(|r| format!("RRULE:{r}")));
    lines.push(format!("END:{kind}"));
    lines
}

/// The VTIMEZONE lines for IANA zone `name` as it applies in `year`, or `None` for an unknown
/// zone. A zone without daylight saving gets one STANDARD observance; a zone with the usual
/// two yearly changes gets RRULE-based observances; anything else (rules changing that year)
/// gets the year's transitions as one-off observances.
pub fn vtimezone(name: &str, year: i32) -> Option<Vec<String>> {
    let tz = zone(name)?;
    let base = NaiveDate::from_ymd_opt(1970, 1, 1)?.and_hms_opt(0, 0, 0)?;
    let mut lines = vec!["BEGIN:VTIMEZONE".to_owned(), format!("TZID:{name}")];
    let ts = transitions(tz, year);
    let kind = |t: &Transition| if t.to > t.from { "DAYLIGHT" } else { "STANDARD" };
    if ts.is_empty() {
        let off = offset_at(tz, NaiveDate::from_ymd_opt(year, 1, 1)?.and_hms_opt(0, 0, 0)?.and_utc());
        lines.extend(observance("STANDARD", base, off, off, None));
    } else if ts.len() == 2 && ts[0].to == ts[1].from && ts[1].to == ts[0].from {
        for t in &ts {
            // The rule is stated in the wall-clock time before the change.
            let wall = (t.at + Duration::seconds(t.from as i64)).naive_utc();
            let (n, weekday) = byday(wall.date());
            let first = rule_date(1970, wall.month(), n, weekday)?.and_time(wall.time());
            let rrule = format!("FREQ=YEARLY;BYMONTH={};BYDAY={n}{}", wall.month(), weekday_code(weekday));
            lines.extend(observance(kind(t), first, t.from, t.to, Some(rrule)));
        }
    } else {
        lines.extend(observance("STANDARD", base, ts[0].from, ts[0].from, None));
        for t in &ts {
            let wall = (t.at + Duration::seconds(t.from as i64)).naive_utc();
            lines.extend(observance(kind(t), wall, t.from, t.to, None));
        }
    }
    lines.push("END:VTIMEZONE".into());
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn berlin_has_yearly_rules() {
        let v = vtimezone("Europe/Berlin", 2026).unwrap().join("\n");
        assert!(v.contains("TZID:Europe/Berlin"));
        assert!(v.contains(
            "BEGIN:DAYLIGHT\nDTSTART:19700329T020000\nTZOFFSETFROM:+0100\nTZOFFSETTO:+0200\nRRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU"
        ));
        assert!(v.contains(
            "BEGIN:STANDARD\nDTSTART:19701025T030000\nTZOFFSETFROM:+0200\nTZOFFSETTO:+0100\nRRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU"
        ));
    }

    #[test]
    fn new_york_uses_nth_sundays() {
        let v = vtimezone("America/New_York", 2026).unwrap().join("\n");
        assert!(v.contains("RRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=2SU"));
        assert!(v.contains("RRULE:FREQ=YEARLY;BYMONTH=11;BYDAY=1SU"));
        assert!(v.contains("TZOFFSETFROM:-0400\nTZOFFSETTO:-0500"));
    }

    #[test]
    fn zones_without_dst_and_unknown_zones() {
        let v = vtimezone("Asia/Kolkata", 2026).unwrap().join("\n");
        assert!(v.contains("BEGIN:STANDARD\nDTSTART:19700101T000000\nTZOFFSETFROM:+0530\nTZOFFSETTO:+0530\nEND:STANDARD"));
        assert!(!v.contains("DAYLIGHT"));
        assert!(vtimezone("Mars/Olympus", 2026).is_none());
    }

    #[test]
    fn local_stamps() {
        let t = Utc.with_ymd_and_hms(2026, 7, 1, 8, 0, 0).unwrap();
        assert_eq!(local_stamp(&t, zone("Europe/Berlin").unwrap()), "20260701T100000");
    }
}
