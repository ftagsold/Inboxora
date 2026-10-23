//! `ICAL.Time` (ical.js 2.2): calendar date/time with a zone, lazily normalised.
//!
//! Field assignments are stored raw and normalised on the next read, exactly like ical.js.
//! The recurrence iterator depends on this: it assigns year, month and day in sequence and
//! must not see an intermediate (e.g. 31 February) rolled over between assignments.

use std::cell::Cell;
use std::rc::Rc;

use super::timezone::Timezone;

pub const SUNDAY: i32 = 1;
pub const MONDAY: i32 = 2;
pub const THURSDAY: i32 = 5;
pub const DEFAULT_WEEK_START: i32 = MONDAY;

const DAYS_IN_YEAR_PASSED_MONTH: [[i32; 13]; 2] = [
    [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365],
    [0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335, 366],
];

/// `Math.trunc` for integer division results.
pub fn trunc_div(a: i64, b: i64) -> i64 {
    a / b
}

/// A time's zone.
#[derive(Clone, Debug)]
pub enum Zone {
    Utc,
    /// `Timezone.localTimezone` ("floating").
    Floating,
    Tz(Rc<Timezone>),
}

impl Zone {
    pub fn tzid(&self) -> &str {
        match self {
            Zone::Utc => "UTC",
            Zone::Floating => "floating",
            Zone::Tz(t) => &t.tzid,
        }
    }

    pub fn is_utc(&self) -> bool {
        matches!(self, Zone::Utc)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fields {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
    pub is_date: bool,
}

pub fn is_leap_year(year: i64) -> bool {
    if year <= 1752 { year % 4 == 0 } else { (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 }
}

pub fn days_in_month(month: i64, year: i64) -> i64 {
    const DIM: [i64; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if !(1..=12).contains(&month) {
        return 30;
    }
    DIM[month as usize] + i64::from(month == 2 && is_leap_year(year))
}

pub fn days_in_year_passed_month(leap: bool, month: usize) -> i64 {
    i64::from(DAYS_IN_YEAR_PASSED_MONTH[usize::from(leap)][month])
}

/// `Time.prototype.adjust` on raw fields.
pub fn adjust_fields(t: &mut Fields, extra_days: i64, extra_hours: i64, extra_minutes: i64, extra_seconds: i64) {
    let mut days_overflow = 0;
    if !t.is_date {
        let second = t.second + extra_seconds;
        t.second = second % 60;
        let mut minutes_overflow = trunc_div(second, 60);
        if t.second < 0 {
            t.second += 60;
            minutes_overflow -= 1;
        }
        let minute = t.minute + extra_minutes + minutes_overflow;
        t.minute = minute % 60;
        let mut hours_overflow = trunc_div(minute, 60);
        if t.minute < 0 {
            t.minute += 60;
            hours_overflow -= 1;
        }
        let hour = t.hour + extra_hours + hours_overflow;
        t.hour = hour % 24;
        days_overflow = trunc_div(hour, 24);
        if t.hour < 0 {
            t.hour += 24;
            days_overflow -= 1;
        }
    }
    let years_overflow = if t.month > 12 {
        trunc_div(t.month - 1, 12)
    } else if t.month < 1 {
        trunc_div(t.month, 12) - 1
    } else {
        0
    };
    t.year += years_overflow;
    t.month -= 12 * years_overflow;
    let mut day = t.day + extra_days + days_overflow;
    if day > 0 {
        loop {
            let dim = days_in_month(t.month, t.year);
            if day <= dim {
                break;
            }
            t.month += 1;
            if t.month > 12 {
                t.year += 1;
                t.month = 1;
            }
            day -= dim;
        }
    } else {
        while day <= 0 {
            if t.month == 1 {
                t.year -= 1;
                t.month = 12;
            } else {
                t.month -= 1;
            }
            day += days_in_month(t.month, t.year);
        }
    }
    t.day = day;
}

/// `Date.UTC(y, m - 1, d, h, mi, s)` in seconds, with JS field overflow semantics.
pub fn date_utc_seconds(y: i64, m0: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
    // Normalise month into the year first (JS MakeDay), then add the day/time offsets.
    let ym = y + m0.div_euclid(12);
    let mn = m0.rem_euclid(12) + 1;
    let days = days_from_civil(ym, mn, 1) + (d - 1);
    days * 86_400 + h * 3600 + mi * 60 + s
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date for days since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
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

/// An `ICAL.Time`.
#[derive(Debug)]
pub struct Time {
    raw: Cell<Fields>,
    pending: Cell<bool>,
    pub zone: Zone,
}

impl Clone for Time {
    fn clone(&self) -> Self {
        // `new Time(this._time, this.zone)` copies the raw fields (fromData assigns them,
        // which marks normalisation pending again).
        Time { raw: Cell::new(self.raw.get()), pending: Cell::new(true), zone: self.zone.clone() }
    }
}

impl Time {
    pub fn new(f: Fields, zone: Zone) -> Self {
        Time { raw: Cell::new(f), pending: Cell::new(true), zone }
    }

    pub fn date(year: i64, month: i64, day: i64) -> Self {
        Self::new(Fields { year, month, day, is_date: true, ..Fields::default() }, Zone::Floating)
    }

    fn norm(&self) {
        if self.pending.get() {
            let mut f = self.raw.get();
            if f.is_date {
                f.hour = 0;
                f.minute = 0;
                f.second = 0;
            }
            adjust_fields(&mut f, 0, 0, 0, 0);
            self.raw.set(f);
            self.pending.set(false);
        }
    }

    pub fn fields(&self) -> Fields {
        self.norm();
        self.raw.get()
    }

    pub fn year(&self) -> i64 {
        self.fields().year
    }
    pub fn month(&self) -> i64 {
        self.fields().month
    }
    pub fn day(&self) -> i64 {
        self.fields().day
    }
    pub fn hour(&self) -> i64 {
        self.fields().hour
    }
    pub fn minute(&self) -> i64 {
        self.fields().minute
    }
    pub fn second(&self) -> i64 {
        self.fields().second
    }
    pub fn is_date(&self) -> bool {
        self.fields().is_date
    }

    fn set_with(&self, f: impl FnOnce(&mut Fields)) {
        let mut raw = self.raw.get();
        f(&mut raw);
        self.raw.set(raw);
        self.pending.set(true);
    }

    pub fn set_year(&self, v: i64) {
        self.set_with(|f| f.year = v);
    }
    pub fn set_month(&self, v: i64) {
        self.set_with(|f| f.month = v);
    }
    pub fn set_day(&self, v: i64) {
        self.set_with(|f| f.day = v);
    }
    pub fn set_hour(&self, v: i64) {
        self.set_with(|f| f.hour = v);
    }
    pub fn set_minute(&self, v: i64) {
        self.set_with(|f| f.minute = v);
    }
    pub fn set_second(&self, v: i64) {
        self.set_with(|f| f.second = v);
    }

    /// Setting `isDate` to true first adjusts (normalises) the current fields.
    pub fn set_is_date(&self, v: bool) {
        if v && !self.raw.get().is_date {
            let mut raw = self.raw.get();
            adjust_fields(&mut raw, 0, 0, 0, 0);
            self.raw.set(raw);
        }
        self.set_with(|f| f.is_date = v);
    }

    /// `adjust(d, h, m, s)` applied to the normalised fields.
    pub fn adjust(&self, d: i64, h: i64, m: i64, s: i64) {
        let mut f = self.fields();
        adjust_fields(&mut f, d, h, m, s);
        self.raw.set(f);
        self.pending.set(false);
    }

    /// `dayOfWeek(aWeekStart)`: 1..7 counted from the week start (Sunday by default).
    pub fn day_of_week(&self, week_start: Option<i32>) -> i32 {
        let first = i64::from(week_start.filter(|w| *w != 0).unwrap_or(SUNDAY));
        let f = self.fields();
        let q = f.day;
        let m = f.month + if f.month < 3 { 12 } else { 0 };
        let y = f.year - i64::from(f.month < 3);
        let mut h = q + y + trunc_div((m + 1) * 26, 10) + trunc_div(y, 4);
        h += trunc_div(y, 100) * 6 + trunc_div(y, 400);
        (((h + 7 - first) % 7) + 1) as i32
    }

    pub fn day_of_year(&self) -> i64 {
        let f = self.fields();
        days_in_year_passed_month(is_leap_year(f.year), (f.month - 1).clamp(0, 12) as usize) + f.day
    }

    /// `startDoyWeek(firstDow)`.
    pub fn start_doy_week(&self, first_dow: i32) -> i64 {
        let first = if first_dow == 0 { SUNDAY } else { first_dow };
        let mut delta = i64::from(self.day_of_week(None) - first);
        if delta < 0 {
            delta += 7;
        }
        self.day_of_year() - delta
    }

    /// `nthWeekDay(dow, pos)`: day of month of the pos-th weekday (may be out of range).
    pub fn nth_week_day(&self, dow: i32, pos: i64) -> i64 {
        let dim = days_in_month(self.month(), self.year());
        let mut pos = pos;
        let mut start = 0;
        let other = self.clone();
        let mut weekday;
        if pos >= 0 {
            other.set_day(1);
            if pos != 0 {
                pos -= 1;
            }
            start = other.day();
            let start_dow = other.day_of_week(None);
            let mut offset = i64::from(dow - start_dow);
            if offset < 0 {
                offset += 7;
            }
            start += offset;
            start -= i64::from(dow);
            weekday = i64::from(dow);
        } else {
            other.set_day(dim);
            let end_dow = other.day_of_week(None);
            pos += 1;
            weekday = i64::from(end_dow - dow);
            if weekday < 0 {
                weekday += 7;
            }
            weekday = dim - weekday;
        }
        weekday += pos * 7;
        start + weekday
    }

    pub fn is_nth_week_day(&self, dow: i32, pos: i64) -> bool {
        if pos == 0 && self.day_of_week(None) == dow {
            return true;
        }
        self.nth_week_day(dow, pos) == self.day()
    }

    /// `Time.weekOneStarts(year, wkst)`.
    fn week_one_starts(year: i64, week_start: i32) -> Time {
        let t = Time::date(year, 1, 1);
        let dow = i64::from(t.day_of_week(None));
        let wkst = i64::from(if week_start == 0 { DEFAULT_WEEK_START } else { week_start });
        if dow > i64::from(THURSDAY) {
            t.set_day(t.day() + 7);
        }
        if wkst > i64::from(THURSDAY) {
            t.set_day(t.day() - 7);
        }
        t.set_day(t.day() - (dow - wkst));
        t
    }

    /// `weekNumber(wkst)`.
    pub fn week_number(&self, week_start: i32) -> i64 {
        let dt = self.clone();
        dt.set_is_date(true);
        let mut iso_year = self.year();
        let week1;
        if dt.month() == 12 && dt.day() > 25 {
            let w = Time::week_one_starts(iso_year + 1, week_start);
            if dt.compare(&w) < 0 {
                week1 = Time::week_one_starts(iso_year, week_start);
            } else {
                iso_year += 1;
                week1 = w;
            }
        } else {
            let w = Time::week_one_starts(iso_year, week_start);
            if dt.compare(&w) < 0 {
                iso_year -= 1;
                week1 = Time::week_one_starts(iso_year, week_start);
            } else {
                week1 = w;
            }
        }
        let _ = iso_year;
        let days_between = (dt.subtract_date_seconds(&week1)) as f64 / 86400.0;
        (days_between / 7.0).trunc() as i64 + 1
    }

    /// `Time.fromDayOfYear(doy, year)`.
    pub fn from_day_of_year(doy: i64, year: i64) -> Time {
        let mut year = year;
        let mut doy = doy;
        loop {
            let leap = is_leap_year(year);
            if doy < 1 {
                year -= 1;
                doy += days_in_year_passed_month(is_leap_year(year), 12);
                continue;
            }
            if doy > days_in_year_passed_month(leap, 12) {
                doy -= days_in_year_passed_month(leap, 12);
                year += 1;
                continue;
            }
            let t = Time::new(Fields { year, is_date: true, month: 1, day: 1, ..Fields::default() }, Zone::Floating);
            for month in (0..12).rev() {
                if doy > days_in_year_passed_month(leap, month) {
                    t.set_month(month as i64 + 1);
                    t.set_day(doy - days_in_year_passed_month(leap, month));
                    break;
                }
            }
            return t;
        }
    }

    /// Seconds offset from UTC for this wall time in its zone.
    pub fn utc_offset(&self) -> i64 {
        match &self.zone {
            Zone::Utc | Zone::Floating => 0,
            Zone::Tz(tz) => tz.utc_offset(self),
        }
    }

    /// `toUnixTime()` in seconds.
    pub fn to_unix_time(&self) -> i64 {
        let offset = self.utc_offset();
        let f = self.fields();
        date_utc_seconds(f.year, f.month - 1, f.day, f.hour, f.minute, f.second - offset)
    }

    pub fn compare(&self, other: &Time) -> i32 {
        let (a, b) = (self.to_unix_time(), other.to_unix_time());
        (a > b) as i32 - (b > a) as i32
    }

    /// `subtractDate` (wall-clock difference) in seconds.
    pub fn subtract_date_seconds(&self, other: &Time) -> i64 {
        (self.to_unix_time() + self.utc_offset()) - (other.to_unix_time() + other.utc_offset())
    }

    /// `subtractDateTz` (absolute difference) in seconds.
    pub fn subtract_date_tz_seconds(&self, other: &Time) -> i64 {
        self.to_unix_time() - other.to_unix_time()
    }

    /// `addDuration`: add each part to the wall-clock fields; normalisation happens on the
    /// next read (time parts are discarded for a date-only value, as in ical.js).
    pub fn add_duration(&self, d: &Duration) {
        let mult = if d.negative { -1 } else { 1 };
        let mut f = self.fields();
        f.second += mult * d.seconds;
        f.minute += mult * d.minutes;
        f.hour += mult * d.hours;
        f.day += mult * d.days + mult * 7 * d.weeks;
        self.raw.set(f);
        self.pending.set(true);
    }

    /// `convertToZone(zone)`.
    pub fn convert_to_zone(&self, zone: &Zone) -> Time {
        let copy = self.clone();
        let equal = self.zone.tzid() == zone.tzid();
        if !self.is_date() && !equal && !matches!(self.zone, Zone::Floating) && !matches!(zone, Zone::Floating) {
            let off = self.utc_offset();
            copy.adjust(0, 0, 0, -off);
            let shifted = Time { raw: Cell::new(copy.fields()), pending: Cell::new(false), zone: zone.clone() };
            let off2 = shifted.utc_offset();
            shifted.adjust(0, 0, 0, off2);
            return shifted;
        }
        Time { raw: Cell::new(copy.fields()), pending: Cell::new(false), zone: zone.clone() }
    }

    /// `toString()`: `YYYY-MM-DD[THH:MM:SS[Z]]` (the recurrence-id key).
    pub fn to_js_string(&self) -> String {
        let f = self.fields();
        let mut s = format!("{}-{:02}-{:02}", f.year, f.month, f.day);
        if !f.is_date {
            s.push_str(&format!("T{:02}:{:02}:{:02}", f.hour, f.minute, f.second));
            if self.zone.is_utc() {
                s.push('Z');
            }
        }
        s
    }

    /// `toICALString()`: `YYYYMMDD[THHMMSS[Z]]`.
    pub fn to_ical_string(&self) -> String {
        let f = self.fields();
        let mut s = format!("{}{:02}{:02}", f.year, f.month, f.day);
        if !f.is_date {
            s.push_str(&format!("T{:02}{:02}{:02}", f.hour, f.minute, f.second));
            if self.zone.is_utc() {
                s.push('Z');
            }
        }
        s
    }

    /// A UTC time from unix seconds (`fromUnixTime`).
    pub fn from_unix(seconds: i64, is_date: bool) -> Time {
        let days = seconds.div_euclid(86_400);
        let secs = seconds.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        let mut f = Fields { year: y, month: m, day: d, is_date, ..Fields::default() };
        if !is_date {
            f.hour = secs / 3600;
            f.minute = (secs / 60) % 60;
            f.second = secs % 60;
        }
        Time::new(f, Zone::Utc)
    }
}

/// `Duration`, kept as signed seconds plus the original parts for `toString`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Duration {
    pub negative: bool,
    pub weeks: i64,
    pub days: i64,
    pub hours: i64,
    pub minutes: i64,
    pub seconds: i64,
}

impl Duration {
    pub fn to_seconds(&self) -> i64 {
        let s = self.seconds + 60 * self.minutes + 3600 * self.hours + 86_400 * self.days + 7 * 86_400 * self.weeks;
        if self.negative { -s } else { s }
    }

    pub fn from_seconds(total: i64) -> Self {
        let secs = total.abs();
        let mut d = Duration { negative: total < 0, days: secs / 86_400, ..Default::default() };
        if d.days % 7 == 0 {
            d.weeks = d.days / 7;
            d.days = 0;
        }
        let rest = secs % 86_400;
        d.hours = rest / 3600;
        d.minutes = (rest % 3600) / 60;
        d.seconds = rest % 60;
        d
    }

    /// `Duration.fromString` (lenient chunk parser, throws below two chunks).
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut d = Duration::default();
        let mut chunks = 0;
        let mut rest = s;
        while let Some(pos) = rest.find(['P', 'D', 'W', 'H', 'M', 'S', 'T']) {
            let letter = rest.as_bytes()[pos] as char;
            let number = &rest[..pos];
            rest = &rest[pos + 1..];
            let target = match letter {
                'P' => {
                    d.negative = number == "-";
                    chunks += 1;
                    continue;
                }
                'D' => &mut d.days,
                'W' => &mut d.weeks,
                'H' => &mut d.hours,
                'M' => &mut d.minutes,
                'S' => &mut d.seconds,
                _ => continue,
            };
            if number.is_empty() {
                return Err(format!("invalid duration value: Missing number before \"{letter}\""));
            }
            *target = crate::ical::js_parse_int(number)
                .ok_or_else(|| format!("invalid duration value: Invalid number \"{number}\""))?;
            chunks += 1;
        }
        if chunks < 2 {
            return Err("invalid duration value: Not enough duration components".into());
        }
        Ok(d)
    }
}

impl std::fmt::Display for Duration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.to_seconds() == 0 {
            return f.write_str("PT0S");
        }
        let mut s = String::new();
        if self.negative {
            s.push('-');
        }
        s.push('P');
        let mut has_weeks = false;
        if self.weeks != 0 {
            if self.days != 0 || self.hours != 0 || self.minutes != 0 || self.seconds != 0 {
                s.push_str(&format!("{}D", self.weeks * 7 + self.days));
            } else {
                s.push_str(&format!("{}W", self.weeks));
                has_weeks = true;
            }
        } else if self.days != 0 {
            s.push_str(&format!("{}D", self.days));
        }
        if !has_weeks && (self.hours != 0 || self.minutes != 0 || self.seconds != 0) {
            s.push('T');
            if self.hours != 0 {
                s.push_str(&format!("{}H", self.hours));
            }
            if self.minutes != 0 {
                s.push_str(&format!("{}M", self.minutes));
            }
            if self.seconds != 0 {
                s.push_str(&format!("{}S", self.seconds));
            }
        }
        f.write_str(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lazy_normalisation_matches_ical_js() {
        let t = Time::new(Fields { year: 2026, month: 1, day: 31, ..Fields::default() }, Zone::Floating);
        t.set_month(2);
        t.set_day(5);
        assert_eq!(t.to_js_string(), "2026-02-05T00:00:00");
        t.set_day(t.day() + 30);
        assert_eq!(t.to_js_string(), "2026-03-07T00:00:00");
    }

    #[test]
    fn calendar_helpers() {
        let t = Time::date(2026, 10, 22);
        assert_eq!(t.day_of_week(None), 5); // Thursday, Sunday = 1
        assert_eq!(t.day_of_week(Some(MONDAY)), 4);
        assert_eq!(t.day_of_year(), 295);
        assert_eq!(t.week_number(MONDAY), 43);
        assert_eq!(Time::date(2026, 10, 1).nth_week_day(1, -1), 25); // last Sunday
        assert_eq!(Time::from_day_of_year(60, 2024).to_js_string(), "2024-02-29");
        assert_eq!(Time::from_day_of_year(0, 2024).to_js_string(), "2023-12-31");
        assert_eq!(Duration::parse("P1DT2H").unwrap().to_seconds(), 93_600);
        assert_eq!(Duration::from_seconds(7 * 86_400).to_string(), "P1W");
        assert_eq!(date_utc_seconds(2026, 0, 32, 0, 0, 0), date_utc_seconds(2026, 1, 1, 0, 0, 0));
    }
}
