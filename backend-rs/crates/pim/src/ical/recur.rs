//! `ICAL.Recur` and `ICAL.RecurIterator` (ical.js 2.2), ported statement by statement.
//!
//! The iterator is libical's algorithm operating on a lazily normalised `Time`; several of its
//! quirks (carries happening through normalisation, an `indexOf` truthiness slip in the
//! BYWEEKNO+BYDAY branch, a reused loop index in the BYMONTH+BYWEEKNO branch) are reproduced
//! because occurrence dates and recurrence ids must match what the Node backend produced.

use std::collections::HashMap;

use super::component::{RecurVal, ical_day_to_numeric, numeric_to_ical_day};
use super::time::{Fields, MONDAY, SUNDAY, Time, Zone, days_in_month, is_leap_year, trunc_div};

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Int(i64),
    Day(String),
}

impl Part {
    fn int(&self) -> i64 {
        match self {
            Part::Int(i) => *i,
            Part::Day(_) => 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Recur {
    pub freq: String,
    pub count: Option<i64>,
    pub interval: i64,
    pub until: Option<Time>,
    pub wkst: i32,
    /// Uppercase BYxxx name → values, in rule order.
    pub parts: Vec<(String, Vec<Part>)>,
}

const PART_NAMES: [&str; 9] =
    ["BYSECOND", "BYMINUTE", "BYHOUR", "BYDAY", "BYMONTHDAY", "BYYEARDAY", "BYWEEKNO", "BYMONTH", "BYSETPOS"];

fn to_parts(v: &RecurVal) -> Vec<Part> {
    let one = |v: &RecurVal| match v {
        RecurVal::Int(i) => Part::Int(*i),
        RecurVal::Str(s) => Part::Day(s.clone()),
        RecurVal::List(_) => Part::Int(0),
    };
    match v {
        RecurVal::List(items) => items.iter().map(one).collect(),
        other => vec![one(other)],
    }
}

impl Recur {
    /// `Recur.fromData(jCalData)`.
    pub fn from_data(data: &[(String, RecurVal)]) -> Recur {
        let mut r =
            Recur { freq: String::new(), count: None, interval: 1, until: None, wkst: MONDAY, parts: Vec::new() };
        for (k, v) in data {
            let uc = k.to_uppercase();
            if PART_NAMES.contains(&uc.as_str()) {
                r.parts.push((uc, to_parts(v)));
                continue;
            }
            match (k.as_str(), v) {
                ("freq", RecurVal::Str(s)) => r.freq = s.clone(),
                ("count", RecurVal::Int(i)) => r.count = Some(*i),
                ("interval", RecurVal::Int(i)) => r.interval = (*i).max(1),
                ("wkst", RecurVal::Int(i)) => r.wkst = *i as i32,
                ("until", RecurVal::Str(s)) => r.until = time_from_string(s).ok(),
                _ => {}
            }
        }
        r
    }

    pub fn has_part(&self, name: &str) -> bool {
        self.parts.iter().any(|(k, _)| k == name)
    }

    fn part(&self, name: &str) -> Option<&Vec<Part>> {
        self.parts.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    /// `Recur.toJSON()` key order, as data for serialisation.
    pub fn to_data(&self) -> Vec<(String, RecurVal)> {
        let mut out = vec![("freq".to_owned(), RecurVal::Str(self.freq.clone()))];
        if let Some(c) = self.count.filter(|c| *c != 0) {
            out.push(("count".into(), RecurVal::Int(c)));
        }
        if self.interval > 1 {
            out.push(("interval".into(), RecurVal::Int(self.interval)));
        }
        for (k, parts) in &self.parts {
            let vals: Vec<RecurVal> = parts
                .iter()
                .map(|p| match p {
                    Part::Int(i) => RecurVal::Int(*i),
                    Part::Day(d) => RecurVal::Str(d.clone()),
                })
                .collect();
            let v = if vals.len() == 1 {
                vals.into_iter().next().unwrap_or(RecurVal::Int(0))
            } else {
                RecurVal::List(vals)
            };
            out.push((k.to_lowercase(), v));
        }
        if let Some(u) = &self.until {
            out.push(("until".into(), RecurVal::Str(u.to_js_string())));
        }
        if self.wkst != MONDAY {
            out.push(("wkst".into(), RecurVal::Int(i64::from(self.wkst))));
        }
        out
    }
}

/// `Time.fromString(value)` without a property (Z → UTC, otherwise floating).
pub fn time_from_string(v: &str) -> Result<Time, String> {
    let int = |s: &str| super::js_parse_int(s).ok_or_else(|| format!("Could not extract integer from \"{s}\""));
    let sl = |a: usize, b: usize| v.get(a..b.min(v.len())).unwrap_or("");
    if v.len() > 10 {
        if v.len() < 19 {
            return Err(format!("invalid date-time value: \"{v}\""));
        }
        let zone = if v.ends_with('Z') { Zone::Utc } else { Zone::Floating };
        let f = Fields {
            year: int(sl(0, 4))?,
            month: int(sl(5, 7))?,
            day: int(sl(8, 10))?,
            hour: int(sl(11, 13))?,
            minute: int(sl(14, 16))?,
            second: int(sl(17, 19))?,
            is_date: false,
        };
        Ok(Time::new(f, zone))
    } else {
        let f = Fields {
            year: int(sl(0, 4))?,
            month: int(sl(5, 7))?,
            day: int(sl(8, 10))?,
            is_date: true,
            ..Fields::default()
        };
        Ok(Time::new(f, Zone::Floating))
    }
}

/// Failures that ical.js signals by throwing out of the iterator.
#[derive(Debug, Clone, PartialEq)]
pub struct RecurError(pub String);

#[derive(Clone, Copy, PartialEq)]
enum Attr {
    Second,
    Minute,
    Hour,
    MonthDay,
}

const CONTRACT: u8 = 1;

/// Inner iterations allowed per occurrence (see `next_inner`).
pub const STEP_BUDGET: u64 = 1_000_000;

fn expand_map(freq: &str) -> [u8; 8] {
    match freq {
        "SECONDLY" => [1, 1, 1, 1, 1, 1, 1, 1],
        "MINUTELY" => [2, 1, 1, 1, 1, 1, 1, 1],
        "HOURLY" => [2, 2, 1, 1, 1, 1, 1, 1],
        "DAILY" => [2, 2, 2, 1, 1, 1, 1, 1],
        "WEEKLY" => [2, 2, 2, 2, 3, 3, 1, 1],
        "MONTHLY" => [2, 2, 2, 2, 2, 3, 3, 1],
        _ => [2, 2, 2, 2, 2, 2, 2, 2],
    }
}

fn index_map(rule: &str) -> usize {
    match rule {
        "BYSECOND" => 0,
        "BYMINUTE" => 1,
        "BYHOUR" => 2,
        "BYDAY" => 3,
        "BYMONTHDAY" => 4,
        "BYYEARDAY" => 5,
        "BYWEEKNO" => 6,
        "BYMONTH" => 7,
        _ => 8,
    }
}

/// `ruleDayOfWeek`: first match of `/([+-]?[0-9])?(MO|TU|WE|TH|FR|SA|SU)/`.
fn rule_day_of_week(dow: &str, week_start: i32) -> (i64, i32) {
    const DAYS: [&str; 7] = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];
    let b = dow.as_bytes();
    let day_at = |i: usize| DAYS.iter().find(|d| b.get(i..i + 2) == Some(d.as_bytes())).copied();
    for i in 0..b.len() {
        // With the optional group: [+-]?[0-9] then a day name.
        let (num_end, num) = if matches!(b[i], b'+' | b'-') && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            (i + 2, Some(&dow[i..i + 2]))
        } else if b[i].is_ascii_digit() {
            (i + 1, Some(&dow[i..i + 1]))
        } else {
            (i, None)
        };
        if num.is_some()
            && let Some(day) = day_at(num_end)
        {
            let pos = num.and_then(|n| n.parse::<i64>().ok()).unwrap_or(0);
            return (pos, ical_day_to_numeric(day, week_start));
        }
        if let Some(day) = day_at(i) {
            return (0, ical_day_to_numeric(day, week_start));
        }
    }
    (0, 0)
}

pub struct RecurIterator {
    pub rule: Recur,
    pub dtstart: Time,
    pub last: Time,
    pub occurrence_number: i64,
    pub completed: bool,
    by_data: HashMap<String, Vec<Part>>,
    by_indices: HashMap<String, i64>,
    days: Vec<i64>,
    days_index: usize,
}

impl RecurIterator {
    /// `rule.iterator(dtstart)`. `Err` when ical.js throws from the constructor.
    pub fn new(rule: Recur, dtstart: &Time) -> Result<Self, RecurError> {
        let by_data: HashMap<String, Vec<Part>> = rule.parts.iter().cloned().collect();
        let by_indices: HashMap<String, i64> =
            ["BYSECOND", "BYMINUTE", "BYHOUR", "BYDAY", "BYMONTH", "BYWEEKNO", "BYMONTHDAY"]
                .iter()
                .map(|k| ((*k).to_owned(), 0))
                .collect();
        let mut it = RecurIterator {
            rule,
            dtstart: dtstart.clone(),
            last: dtstart.clone(),
            occurrence_number: 0,
            completed: false,
            by_data,
            by_indices,
            days: Vec::new(),
            days_index: 0,
        };
        match it.init() {
            Ok(()) => Ok(it),
            Err(InitError::Invalid) => {
                it.completed = true;
                Ok(it)
            }
            Err(InitError::Fatal(e)) => Err(RecurError(e)),
        }
    }

    fn has_by_data(&self, name: &str) -> bool {
        self.rule.has_part(name)
    }

    fn by(&self, name: &str) -> &[Part] {
        self.by_data.get(name).map_or(&[], Vec::as_slice)
    }

    fn idx(&self, name: &str) -> i64 {
        self.by_indices.get(name).copied().unwrap_or(0)
    }

    fn set_idx(&mut self, name: &str, v: i64) {
        self.by_indices.insert(name.to_owned(), v);
    }

    fn init(&mut self) -> Result<(), InitError> {
        self.last = self.dtstart.clone();
        if let Some(mut byday) = self.by_data.remove("BYDAY") {
            self.sort_byday_rules(&mut byday);
            self.by_data.insert("BYDAY".into(), byday);
        }
        let has = |k: &str| self.by_data.contains_key(k);
        if has("BYYEARDAY") && (has("BYMONTH") || has("BYWEEKNO") || has("BYMONTHDAY")) {
            return Err(InitError::Fatal("Invalid BYYEARDAY rule".into()));
        }
        if has("BYWEEKNO") && has("BYMONTHDAY") {
            return Err(InitError::Fatal("BYWEEKNO does not fit to BYMONTHDAY".into()));
        }
        if self.rule.freq == "MONTHLY" && (has("BYYEARDAY") || has("BYWEEKNO")) {
            return Err(InitError::Fatal("For MONTHLY recurrences neither BYYEARDAY nor BYWEEKNO may appear".into()));
        }
        if self.rule.freq == "WEEKLY" && (has("BYYEARDAY") || has("BYMONTHDAY")) {
            return Err(InitError::Fatal("For WEEKLY recurrences neither BYMONTHDAY nor BYYEARDAY may appear".into()));
        }
        if self.rule.freq != "YEARLY" && has("BYYEARDAY") {
            return Err(InitError::Fatal("BYYEARDAY may only appear in YEARLY rules".into()));
        }
        let s = self.setup_defaults("BYSECOND", "SECONDLY", self.dtstart.second());
        self.last.set_second(s);
        let m = self.setup_defaults("BYMINUTE", "MINUTELY", self.dtstart.minute());
        self.last.set_minute(m);
        let h = self.setup_defaults("BYHOUR", "HOURLY", self.dtstart.hour());
        self.last.set_hour(h);
        let d = self.setup_defaults("BYMONTHDAY", "DAILY", self.dtstart.day());
        self.last.set_day(d);
        let mo = self.setup_defaults("BYMONTH", "MONTHLY", self.dtstart.month());
        self.last.set_month(mo);

        if self.rule.freq == "WEEKLY" {
            if let Some(first) = self.by_data.get("BYDAY").and_then(|v| v.first()).cloned() {
                let Part::Day(code) = first else { return Ok(()) };
                let (_, dow) = rule_day_of_week(&code, self.rule.wkst);
                let wkdy = i64::from(dow - self.last.day_of_week(Some(self.rule.wkst)));
                if (i64::from(self.last.day_of_week(Some(self.rule.wkst))) < i64::from(dow) && wkdy >= 0) || wkdy < 0 {
                    self.last.set_day(self.last.day() + wkdy);
                }
            } else {
                let day_name = numeric_to_ical_day(self.dtstart.day_of_week(None), SUNDAY);
                self.by_data.insert("BYDAY".into(), vec![Part::Day(day_name.to_owned())]);
            }
        }

        if self.rule.freq == "YEARLY" {
            let until_year = self.rule.until.as_ref().map_or(20_000, Time::year);
            while self.last.year() <= until_year {
                self.expand_year_days(self.last.year());
                if !self.days.is_empty() {
                    break;
                }
                self.increment_year(self.rule.interval);
            }
            if self.days.is_empty() {
                return Err(InitError::Invalid);
            }
            if !self.next_by_year_day() && !self.next_year() && !self.next_year() && !self.next_year() {
                return Err(InitError::Invalid);
            }
        }

        if self.rule.freq == "MONTHLY" {
            if self.has_by_data("BYDAY") {
                let mut temp_last: Option<Time> = None;
                let init_last = self.last.clone();
                let mut dim = days_in_month(self.last.month(), self.last.year());
                for bydow in self.by("BYDAY").to_vec() {
                    let Part::Day(code) = bydow else { continue };
                    self.last = init_last.clone();
                    let (pos, dow) = rule_day_of_week(&code, 0);
                    let mut day_of_month = self.last.nth_week_day(dow, pos);
                    if !(-6 < pos && pos < 6) {
                        return Err(InitError::Fatal("Malformed values in BYDAY part".into()));
                    }
                    if day_of_month > dim || day_of_month <= 0 {
                        if temp_last.as_ref().is_some_and(|t| t.month() == init_last.month()) {
                            continue;
                        }
                        while day_of_month > dim || day_of_month <= 0 {
                            self.increment_month();
                            dim = days_in_month(self.last.month(), self.last.year());
                            day_of_month = self.last.nth_week_day(dow, pos);
                        }
                    }
                    self.last.set_day(day_of_month);
                    if temp_last.as_ref().is_none_or(|t| self.last.compare(t) < 0) {
                        temp_last = Some(self.last.clone());
                    }
                }
                if let Some(t) = temp_last {
                    self.last = t.clone();
                }
                if self.has_by_data("BYMONTHDAY") {
                    self.by_day_and_month_day(true).map_err(InitError::Fatal)?;
                }
                if self.last.day() > dim || self.last.day() == 0 {
                    return Err(InitError::Fatal("Malformed values in BYDAY part".into()));
                }
            } else if self.has_by_data("BYMONTHDAY") {
                self.last.set_day(1);
                let rules = self.rule.part("BYMONTHDAY").cloned().unwrap_or_default();
                let normalized: Vec<i64> = self
                    .normalize_by_month_day_rules(self.last.year(), self.last.month(), &rules)
                    .map_err(InitError::Fatal)?
                    .into_iter()
                    .filter(|d| *d >= self.last.day())
                    .collect();
                if let Some(first) = normalized.first() {
                    self.last.set_day(*first);
                    self.by_data.insert("BYMONTHDAY".into(), normalized.into_iter().map(Part::Int).collect());
                } else if !self.next_month().map_err(InitError::Fatal)?
                    && !self.next_month().map_err(InitError::Fatal)?
                    && !self.next_month().map_err(InitError::Fatal)?
                {
                    return Err(InitError::Invalid);
                }
            }
        }
        Ok(())
    }

    /// `next()`: the next occurrence, `Ok(None)` when finished.
    pub fn next(&mut self) -> Result<Option<Time>, RecurError> {
        self.next_inner(false)
    }

    fn next_inner(&mut self, again: bool) -> Result<Option<Time>, RecurError> {
        let before = self.last.clone();
        let count = self.rule.count.filter(|c| *c != 0);
        if count.is_some_and(|c| self.occurrence_number >= c)
            || self.rule.until.as_ref().is_some_and(|u| self.last.compare(u) > 0)
        {
            self.completed = true;
        }
        if self.completed {
            return Ok(None);
        }
        if self.occurrence_number == 0 && self.last.compare(&self.dtstart) >= 0 {
            self.occurrence_number += 1;
            return Ok(Some(self.last.clone()));
        }
        let mut invalid_count = 0;
        // ical.js has no bound here and spins forever on rules whose contracting parts can
        // never match (e.g. FREQ=DAILY;BYMONTH=2;BYMONTHDAY=30); stop with an error instead.
        let mut steps: u64 = 0;
        loop {
            steps += 1;
            if steps > STEP_BUDGET {
                return Err(RecurError("Recurrence rule exceeded the iteration step budget".into()));
            }
            let mut valid = true;
            match self.rule.freq.as_str() {
                "SECONDLY" => {
                    self.next_second();
                }
                "MINUTELY" => {
                    self.next_minute();
                }
                "HOURLY" => {
                    self.next_hour();
                }
                "DAILY" => {
                    self.next_day();
                }
                "WEEKLY" => {
                    self.next_week();
                }
                "MONTHLY" => {
                    valid = self.next_month().map_err(RecurError)?;
                    if valid {
                        invalid_count = 0;
                    } else {
                        invalid_count += 1;
                        if invalid_count == 336 {
                            self.completed = true;
                            return Ok(None);
                        }
                    }
                }
                "YEARLY" => {
                    valid = self.next_year();
                    if valid {
                        invalid_count = 0;
                    } else {
                        invalid_count += 1;
                        if invalid_count == 28 {
                            self.completed = true;
                            return Ok(None);
                        }
                    }
                }
                _ => return Ok(None),
            }
            if self.check_contracting_rules() && self.last.compare(&self.dtstart) >= 0 && valid {
                break;
            }
        }
        if self.last.compare(&before) == 0 {
            if again {
                return Err(RecurError("Same occurrence found twice, protecting you from death by recursion".into()));
            }
            self.next_inner(true)?;
        }
        if self.rule.until.as_ref().is_some_and(|u| self.last.compare(u) > 0) {
            self.completed = true;
            Ok(None)
        } else {
            self.occurrence_number += 1;
            Ok(Some(self.last.clone()))
        }
    }

    fn get_attr(&self, a: Attr) -> i64 {
        match a {
            Attr::Second => self.last.second(),
            Attr::Minute => self.last.minute(),
            Attr::Hour => self.last.hour(),
            Attr::MonthDay => self.last.day(),
        }
    }

    fn set_attr(&self, a: Attr, v: i64) {
        match a {
            Attr::Second => self.last.set_second(v),
            Attr::Minute => self.last.set_minute(v),
            Attr::Hour => self.last.set_hour(v),
            Attr::MonthDay => self.last.set_day(v),
        }
    }

    fn increment(&mut self, a: Attr, inc: i64) {
        match a {
            Attr::Second => self.increment_generic(inc, Attr::Second, 60, Attr::Minute),
            Attr::Minute => self.increment_generic(inc, Attr::Minute, 60, Attr::Hour),
            Attr::Hour => self.increment_generic(inc, Attr::Hour, 24, Attr::MonthDay),
            Attr::MonthDay => self.increment_monthday(inc),
        }
    }

    fn next_generic(
        &mut self,
        rule_type: &str,
        interval: &str,
        attr: Attr,
        following: Attr,
        previous: Option<Attr>,
    ) -> i64 {
        let has_by_rule = self.by_data.contains_key(rule_type);
        let this_freq = self.rule.freq == interval;
        let mut end_of_data = 0;
        if let Some(p) = previous {
            let r = match p {
                Attr::Second => self.next_second(),
                Attr::Minute => self.next_minute(),
                _ => 1,
            };
            if r == 0 {
                return end_of_data;
            }
        }
        if has_by_rule {
            let mut i = self.idx(rule_type) + 1;
            let len = self.by(rule_type).len() as i64;
            if i == len {
                i = 0;
                end_of_data = 1;
            }
            self.set_idx(rule_type, i);
            let v = self.by(rule_type).get(i as usize).map_or(0, Part::int);
            self.set_attr(attr, v);
        } else if this_freq {
            self.increment(attr, self.rule.interval);
        }
        if has_by_rule && end_of_data == 1 && this_freq {
            self.increment(following, 1);
        }
        end_of_data
    }

    fn next_second(&mut self) -> i64 {
        self.next_generic("BYSECOND", "SECONDLY", Attr::Second, Attr::Minute, None)
    }

    fn next_minute(&mut self) -> i64 {
        self.next_generic("BYMINUTE", "MINUTELY", Attr::Minute, Attr::Hour, Some(Attr::Second))
    }

    fn next_hour(&mut self) -> i64 {
        self.next_generic("BYHOUR", "HOURLY", Attr::Hour, Attr::MonthDay, Some(Attr::Minute))
    }

    fn next_day(&mut self) {
        let this_freq = self.rule.freq == "DAILY";
        if self.next_hour() == 0 {
            return;
        }
        if this_freq {
            self.increment_monthday(self.rule.interval);
        } else {
            self.increment_monthday(1);
        }
    }

    fn next_week(&mut self) -> i64 {
        let mut end_of_data = 0;
        if self.next_weekday_by_week() == 0 {
            return end_of_data;
        }
        if self.has_by_data("BYWEEKNO") {
            let mut i = self.idx("BYWEEKNO") + 1;
            if i as usize == self.by("BYWEEKNO").len() {
                i = 0;
                end_of_data = 1;
            }
            self.set_idx("BYWEEKNO", i);
            self.last.set_month(1);
            self.last.set_day(1);
            let week_no = self.by("BYWEEKNO").get(i as usize).map_or(0, Part::int);
            self.last.set_day(self.last.day() + 7 * week_no);
            if end_of_data == 1 {
                self.increment_year(1);
            }
        } else {
            self.increment_monthday(7 * self.rule.interval);
        }
        end_of_data
    }

    fn normalize_by_month_day_rules(&self, year: i64, month: i64, rules: &[Part]) -> Result<Vec<i64>, String> {
        let dim = days_in_month(month, year);
        let mut out: Vec<i64> = Vec::new();
        for r in rules {
            let mut rule = match r {
                Part::Int(i) => *i,
                Part::Day(_) => return Err("Invalid BYMONTHDAY value".into()),
            };
            if rule.abs() > dim {
                continue;
            }
            if rule < 0 {
                rule = dim + (rule + 1);
            } else if rule == 0 {
                continue;
            }
            if !out.contains(&rule) {
                out.push(rule);
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    fn by_day_and_month_day(&mut self, is_init: bool) -> Result<bool, String> {
        let by_day: Vec<Part> = self.by("BYDAY").to_vec();
        let mut date_idx = 0usize;
        let mut data_is_valid = false;
        let mut last_day = self.last.day();
        let mut by_month_day: Vec<i64>;
        let mut days_in_month_v;
        let mut date_len;

        let init_month =
            |this: &mut Self, date_idx: &mut usize, last_day: i64| -> Result<(Vec<i64>, i64, usize), String> {
                let dim = days_in_month(this.last.month(), this.last.year());
                let rules = this.by("BYMONTHDAY").to_vec();
                let bmd = this.normalize_by_month_day_rules(this.last.year(), this.last.month(), &rules)?;
                let len = bmd.len();
                while bmd.get(*date_idx).is_some_and(|d| *d <= last_day)
                    && !(is_init && bmd.get(*date_idx) == Some(&last_day))
                    && *date_idx + 1 < len
                {
                    *date_idx += 1;
                }
                Ok((bmd, dim, len))
            };

        (by_month_day, days_in_month_v, date_len) = init_month(self, &mut date_idx, last_day)?;
        if is_init {
            last_day -= 1;
        }
        let mut months_counter = 48;
        while !data_is_valid && months_counter > 0 {
            months_counter -= 1;
            let date = last_day + 1;
            if date > days_in_month_v {
                last_day = 0;
                self.increment_month();
                date_idx = 0;
                (by_month_day, days_in_month_v, date_len) = init_month(self, &mut date_idx, last_day)?;
                continue;
            }
            let next = by_month_day.get(date_idx).copied();
            date_idx += 1;
            match next {
                Some(n) if n >= date => last_day = n,
                _ => {
                    last_day = 0;
                    self.increment_month();
                    date_idx = 0;
                    (by_month_day, days_in_month_v, date_len) = init_month(self, &mut date_idx, last_day)?;
                    continue;
                }
            }
            for d in &by_day {
                let Part::Day(code) = d else { continue };
                let (pos, dow) = rule_day_of_week(code, 0);
                self.last.set_day(last_day);
                if self.last.is_nth_week_day(dow, pos) {
                    data_is_valid = true;
                    break;
                }
            }
            if !data_is_valid && date_idx == date_len {
                last_day = 0;
                self.increment_month();
                date_idx = 0;
                (by_month_day, days_in_month_v, date_len) = init_month(self, &mut date_idx, last_day)?;
                continue;
            }
        }
        if months_counter <= 0 {
            return Err("Malformed values in BYDAY combined with BYMONTHDAY parts".into());
        }
        Ok(data_is_valid)
    }

    fn next_month(&mut self) -> Result<bool, String> {
        let mut data_valid = true;
        if self.next_hour() == 0 {
            return Ok(data_valid);
        }
        if self.has_by_data("BYDAY") && self.has_by_data("BYMONTHDAY") {
            data_valid = self.by_day_and_month_day(false)?;
        } else if self.has_by_data("BYDAY") {
            let dim = days_in_month(self.last.month(), self.last.year());
            let mut setpos = 0i64;
            let mut setpos_total = 0i64;
            if self.has_by_data("BYSETPOS") {
                let last_day = self.last.day();
                for day in 1..=dim {
                    self.last.set_day(day);
                    if self.is_day_in_byday(&self.last) {
                        setpos_total += 1;
                        if day <= last_day {
                            setpos += 1;
                        }
                    }
                }
                self.last.set_day(last_day);
            }
            data_valid = false;
            let mut day = self.last.day() + 1;
            while day <= dim {
                self.last.set_day(day);
                if self.is_day_in_byday(&self.last) {
                    setpos += 1;
                    if !self.has_by_data("BYSETPOS")
                        || self.check_set_position(setpos)
                        || self.check_set_position(setpos - setpos_total - 1)
                    {
                        data_valid = true;
                        break;
                    }
                }
                day += 1;
            }
            if day > dim {
                self.last.set_day(1);
                self.increment_month();
                if self.is_day_in_byday(&self.last) {
                    if !self.has_by_data("BYSETPOS") || self.check_set_position(1) {
                        data_valid = true;
                    }
                } else {
                    data_valid = false;
                }
            }
        } else if self.has_by_data("BYMONTHDAY") {
            let mut i = self.idx("BYMONTHDAY") + 1;
            if i as usize >= self.by("BYMONTHDAY").len() {
                i = 0;
                self.set_idx("BYMONTHDAY", 0);
                self.increment_month();
                if self.by("BYMONTHDAY").is_empty() {
                    return Ok(false);
                }
            }
            self.set_idx("BYMONTHDAY", i);
            let dim = days_in_month(self.last.month(), self.last.year());
            let mut day = self.by("BYMONTHDAY").get(i as usize).map_or(0, Part::int);
            if day < 0 {
                day = dim + day + 1;
            }
            if day > dim {
                self.last.set_day(1);
                data_valid = self.is_day_in_byday(&self.last);
            } else {
                self.last.set_day(day);
            }
        } else {
            self.increment_month();
            let dim = days_in_month(self.last.month(), self.last.year());
            let first = self.by("BYMONTHDAY").first().map_or(0, Part::int);
            if first > dim {
                data_valid = false;
            } else {
                self.last.set_day(first);
            }
        }
        Ok(data_valid)
    }

    fn next_weekday_by_week(&mut self) -> i64 {
        let mut end_of_data = 0;
        if self.next_hour() == 0 {
            return end_of_data;
        }
        if !self.has_by_data("BYDAY") {
            return 1;
        }
        loop {
            let mut i = self.idx("BYDAY") + 1;
            if i as usize == self.by("BYDAY").len() {
                i = 0;
                end_of_data = 1;
            }
            self.set_idx("BYDAY", i);
            let code = match self.by("BYDAY").get(i as usize) {
                Some(Part::Day(c)) => c.clone(),
                _ => String::new(),
            };
            let (_, mut dow) = rule_day_of_week(&code, 0);
            dow -= self.rule.wkst;
            if dow < 0 {
                dow += 7;
            }
            let tt = Time::new(
                Fields { year: self.last.year(), month: self.last.month(), day: self.last.day(), ..Fields::default() },
                Zone::Floating,
            );
            let start_of_week = tt.start_doy_week(self.rule.wkst);
            if i64::from(dow) + start_of_week < 1 && end_of_data == 0 {
                continue;
            }
            let next = Time::from_day_of_year(start_of_week + i64::from(dow), self.last.year());
            self.last.set_year(next.year());
            self.last.set_month(next.month());
            self.last.set_day(next.day());
            return end_of_data;
        }
    }

    fn next_year(&mut self) -> bool {
        if self.next_hour() == 0 {
            return false;
        }
        // `days.length == 0 || ++days_index == days.length` (short-circuit: no increment when empty)
        let exhausted = self.days.is_empty() || {
            self.days_index += 1;
            self.days_index == self.days.len()
        };
        if exhausted {
            self.days_index = 0;
            self.increment_year(self.rule.interval);
            if self.has_by_data("BYMONTHDAY") {
                let rules = self.rule.part("BYMONTHDAY").cloned().unwrap_or_default();
                if let Ok(v) = self.normalize_by_month_day_rules(self.last.year(), self.last.month(), &rules) {
                    self.by_data.insert("BYMONTHDAY".into(), v.into_iter().map(Part::Int).collect());
                }
            }
            self.expand_year_days(self.last.year());
            if self.days.is_empty() {
                return false;
            }
        }
        self.next_by_year_day()
    }

    fn next_by_year_day(&mut self) -> bool {
        let Some(&doy0) = self.days.get(self.days_index) else { return false };
        let mut doy = doy0;
        let mut year = self.last.year();
        if doy.abs() == 366 && !is_leap_year(self.last.year()) {
            return false;
        }
        if doy < 1 {
            doy += 1;
            year += 1;
        }
        let next = Time::from_day_of_year(doy, year);
        self.last.set_day(next.day());
        self.last.set_month(next.month());
        true
    }

    fn increment_generic(&mut self, inc: i64, attr: Attr, factor: i64, next: Attr) {
        self.set_attr(attr, self.get_attr(attr) + inc);
        let nextunit = trunc_div(self.get_attr(attr), factor);
        self.set_attr(attr, self.get_attr(attr) % factor);
        if nextunit != 0 {
            self.increment(next, nextunit);
        }
    }

    fn increment_monthday(&mut self, inc: i64) {
        for _ in 0..inc {
            let dim = days_in_month(self.last.month(), self.last.year());
            self.last.set_day(self.last.day() + 1);
            if self.last.day() > dim {
                self.last.set_day(self.last.day() - dim);
                self.increment_month();
            }
        }
    }

    fn increment_month(&mut self) {
        self.last.set_day(1);
        if self.has_by_data("BYMONTH") {
            let mut i = self.idx("BYMONTH") + 1;
            if i as usize == self.by("BYMONTH").len() {
                i = 0;
                self.increment_year(1);
            }
            self.set_idx("BYMONTH", i);
            let m = self.by("BYMONTH").get(i as usize).map_or(1, Part::int);
            self.last.set_month(m);
        } else {
            if self.rule.freq == "MONTHLY" {
                self.last.set_month(self.last.month() + self.rule.interval);
            } else {
                self.last.set_month(self.last.month() + 1);
            }
            self.last.set_month(self.last.month() - 1);
            let years = trunc_div(self.last.month(), 12);
            self.last.set_month(self.last.month() % 12);
            self.last.set_month(self.last.month() + 1);
            if years != 0 {
                self.increment_year(years);
            }
        }
        if self.has_by_data("BYMONTHDAY") {
            let rules = self.rule.part("BYMONTHDAY").cloned().unwrap_or_default();
            if let Ok(v) = self.normalize_by_month_day_rules(self.last.year(), self.last.month(), &rules) {
                self.by_data.insert("BYMONTHDAY".into(), v.into_iter().map(Part::Int).collect());
            }
        }
    }

    fn increment_year(&mut self, inc: i64) {
        self.last.set_day(1);
        self.last.set_year(self.last.year() + inc);
    }

    fn expand_year_days(&mut self, year: i64) {
        let t = Time::new(Fields::default(), Zone::Floating);
        self.days = Vec::new();
        let mut parts: Vec<&str> = ["BYDAY", "BYWEEKNO", "BYMONTHDAY", "BYMONTH", "BYYEARDAY"]
            .into_iter()
            .filter(|p| self.rule.has_part(p))
            .collect();
        if parts.contains(&"BYMONTH") && parts.contains(&"BYWEEKNO") {
            let mut valid = 1i64;
            let mut valid_weeks: HashMap<i64, i64> = HashMap::new();
            t.set_year(year);
            t.set_is_date(true);
            let months = self.by("BYMONTH").to_vec();
            let mut month_idx: i64 = 0;
            while (month_idx as usize) < months.len() {
                let month = months.get(month_idx as usize).map_or(1, Part::int);
                t.set_month(month);
                t.set_day(1);
                let first_week = t.week_number(self.rule.wkst);
                t.set_day(days_in_month(month, year));
                let last_week = t.week_number(self.rule.wkst);
                month_idx = first_week;
                while month_idx < last_week {
                    valid_weeks.insert(month_idx, 1);
                    month_idx += 1;
                }
                month_idx += 1;
            }
            let weeks = self.by("BYWEEKNO").to_vec();
            let mut week_idx = 0;
            while week_idx < weeks.len() && valid != 0 {
                let weekno = weeks[week_idx].int();
                if weekno < 52 {
                    valid &= valid_weeks.get(&(week_idx as i64)).copied().unwrap_or(0);
                } else {
                    valid = 0;
                }
                week_idx += 1;
            }
            if valid != 0 {
                parts.retain(|p| *p != "BYMONTH");
            } else {
                parts.retain(|p| *p != "BYWEEKNO");
            }
        }
        let has = |p: &str| parts.contains(&p);
        let count = parts.len();
        let ints = |v: &[Part]| v.iter().map(Part::int).collect::<Vec<i64>>();
        if count == 0 {
            let t1 = self.dtstart.clone();
            t1.set_year(self.last.year());
            self.days.push(t1.day_of_year());
        } else if count == 1 && has("BYMONTH") {
            for month in ints(self.by("BYMONTH")) {
                let t2 = self.dtstart.clone();
                t2.set_year(year);
                t2.set_month(month);
                t2.set_is_date(true);
                self.days.push(t2.day_of_year());
            }
        } else if count == 1 && has("BYMONTHDAY") {
            for mut monthday in ints(self.by("BYMONTHDAY")) {
                let t3 = self.dtstart.clone();
                if monthday < 0 {
                    monthday += days_in_month(t3.month(), year) + 1;
                }
                t3.set_day(monthday);
                t3.set_year(year);
                t3.set_is_date(true);
                self.days.push(t3.day_of_year());
            }
        } else if count == 2 && has("BYMONTHDAY") && has("BYMONTH") {
            for month in ints(self.by("BYMONTH")) {
                let dim = days_in_month(month, year);
                for mut monthday in ints(self.by("BYMONTHDAY")) {
                    if monthday < 0 {
                        monthday += dim + 1;
                    }
                    t.set_day(monthday);
                    t.set_month(month);
                    t.set_year(year);
                    t.set_is_date(true);
                    self.days.push(t.day_of_year());
                }
            }
        } else if (count == 1 && has("BYWEEKNO")) || (count == 2 && has("BYWEEKNO") && has("BYMONTHDAY")) {
            // not implemented in ical.js
        } else if count == 1 && has("BYDAY") {
            let d = self.expand_by_day(year);
            self.days.extend(d);
        } else if count == 2 && has("BYDAY") && has("BYMONTH") {
            for month in ints(self.by("BYMONTH")) {
                let dim = days_in_month(month, year);
                t.set_year(year);
                t.set_month(month);
                t.set_day(1);
                t.set_is_date(true);
                let first_dow = i64::from(t.day_of_week(None));
                let doy_offset = t.day_of_year() - 1;
                t.set_day(dim);
                let last_dow = i64::from(t.day_of_week(None));
                if self.has_by_data("BYSETPOS") {
                    let mut by_month_day = Vec::new();
                    for day in 1..=dim {
                        t.set_day(day);
                        if self.is_day_in_byday(&t) {
                            by_month_day.push(day);
                        }
                    }
                    let len = by_month_day.len() as i64;
                    for (sp, day) in by_month_day.iter().enumerate() {
                        let sp = sp as i64;
                        if self.check_set_position(sp + 1) || self.check_set_position(sp - len) {
                            self.days.push(doy_offset + day);
                        }
                    }
                } else {
                    for code in self.by("BYDAY").to_vec() {
                        let Part::Day(code) = code else { continue };
                        let (pos, dow) = rule_day_of_week(&code, 0);
                        let dow = i64::from(dow);
                        let first_matching = ((dow + 7 - first_dow) % 7) + 1;
                        let last_matching = dim - ((last_dow + 7 - dow) % 7);
                        if pos == 0 {
                            let mut day = first_matching;
                            while day <= dim {
                                self.days.push(doy_offset + day);
                                day += 7;
                            }
                        } else if pos > 0 {
                            let md = first_matching + (pos - 1) * 7;
                            if md <= dim {
                                self.days.push(doy_offset + md);
                            }
                        } else {
                            let md = last_matching + (pos + 1) * 7;
                            if md > 0 {
                                self.days.push(doy_offset + md);
                            }
                        }
                    }
                }
            }
            self.days.sort_unstable();
        } else if count == 2 && has("BYDAY") && has("BYMONTHDAY") {
            let monthdays = ints(self.by("BYMONTHDAY"));
            for day in self.expand_by_day(year) {
                let tt = Time::from_day_of_year(day, year);
                if monthdays.contains(&tt.day()) {
                    self.days.push(day);
                }
            }
        } else if count == 3 && has("BYDAY") && has("BYMONTHDAY") && has("BYMONTH") {
            let (months, monthdays) = (ints(self.by("BYMONTH")), ints(self.by("BYMONTHDAY")));
            for day in self.expand_by_day(year) {
                let tt = Time::from_day_of_year(day, year);
                if months.contains(&tt.month()) && monthdays.contains(&tt.day()) {
                    self.days.push(day);
                }
            }
        } else if count == 2 && has("BYDAY") && has("BYWEEKNO") {
            let weeks = ints(self.by("BYWEEKNO"));
            for day in self.expand_by_day(year) {
                let tt = Time::from_day_of_year(day, year);
                let weekno = tt.week_number(self.rule.wkst);
                // `if (BYWEEKNO.indexOf(weekno))`: true unless found at index 0.
                if weeks.iter().position(|w| *w == weekno) != Some(0) {
                    self.days.push(day);
                }
            }
        } else if count == 3 && has("BYDAY") && has("BYWEEKNO") && has("BYMONTHDAY") {
            // not implemented in ical.js
        } else if count == 1 && has("BYYEARDAY") {
            let d = ints(self.by("BYYEARDAY"));
            self.days.extend(d);
        } else if count == 2 && has("BYYEARDAY") && has("BYDAY") {
            let diy = if is_leap_year(year) { 366 } else { 365 };
            let expanded: std::collections::HashSet<i64> = self.expand_by_day(year).into_iter().collect();
            for mut doy in ints(self.by("BYYEARDAY")) {
                if doy < 0 {
                    doy += diy + 1;
                }
                if expanded.contains(&doy) {
                    self.days.push(doy);
                }
            }
        } else {
            self.days = Vec::new();
        }
        let diy = if is_leap_year(year) { 366 } else { 365 };
        let key = |a: i64| if a < 0 { a + diy + 1 } else { a };
        self.days.sort_by_key(|a| key(*a));
    }

    fn expand_by_day(&self, year: i64) -> Vec<i64> {
        let mut out = Vec::new();
        let tmp = self.last.clone();
        tmp.set_year(year);
        tmp.set_month(1);
        tmp.set_day(1);
        tmp.set_is_date(true);
        let start_dow = i64::from(tmp.day_of_week(None));
        tmp.set_month(12);
        tmp.set_day(31);
        tmp.set_is_date(true);
        let end_dow = i64::from(tmp.day_of_week(None));
        let end_year_day = tmp.day_of_year();
        for code in self.by("BYDAY") {
            let Part::Day(code) = code else { continue };
            let (mut pos, dow) = rule_day_of_week(code, 0);
            let dow = i64::from(dow);
            if pos == 0 {
                let mut doy = ((dow + 7 - start_dow) % 7) + 1;
                while doy <= end_year_day {
                    out.push(doy);
                    doy += 7;
                }
            } else if pos > 0 {
                let first = if dow >= start_dow { dow - start_dow + 1 } else { dow - start_dow + 8 };
                out.push(first + (pos - 1) * 7);
            } else {
                pos = -pos;
                let last = if dow <= end_dow { end_year_day - end_dow + dow } else { end_year_day - end_dow + dow - 7 };
                out.push(last - (pos - 1) * 7);
            }
        }
        out
    }

    fn is_day_in_byday(&self, tt: &Time) -> bool {
        for code in self.by("BYDAY") {
            let Part::Day(code) = code else { continue };
            let (pos, dow) = rule_day_of_week(code, 0);
            let this_dow = tt.day_of_week(None);
            if (pos == 0 && dow == this_dow) || tt.nth_week_day(dow, pos) == tt.day() {
                return true;
            }
        }
        false
    }

    fn check_set_position(&self, pos: i64) -> bool {
        self.has_by_data("BYSETPOS") && self.by("BYSETPOS").iter().any(|p| p.int() == pos)
    }

    fn sort_byday_rules(&self, rules: &mut [Part]) {
        let wkst = self.rule.wkst;
        let dow = |p: &Part| match p {
            Part::Day(c) => rule_day_of_week(c, wkst).1,
            Part::Int(_) => 0,
        };
        for i in 0..rules.len() {
            for j in 0..i {
                if dow(&rules[j]) > dow(&rules[i]) {
                    rules.swap(i, j);
                }
            }
        }
    }

    fn check_contract_restriction(&self, rule_type: &str, v: &Part) -> bool {
        let rule_map = expand_map(&self.rule.freq)[index_map(rule_type).min(7)];
        match self.by_data.get(rule_type) {
            Some(values) if rule_map == CONTRACT => values.iter().any(|b| match (b, v) {
                (Part::Int(a), Part::Int(b)) => a == b,
                (Part::Day(a), Part::Day(b)) => a == b,
                // `bydata == v` compares a BYDAY code like "1MO" with "MO": no match.
                _ => false,
            }),
            _ => true,
        }
    }

    fn check_contracting_rules(&self) -> bool {
        let dow = self.last.day_of_week(None);
        let week_no = self.last.week_number(self.rule.wkst);
        let doy = self.last.day_of_year();
        self.check_contract_restriction("BYSECOND", &Part::Int(self.last.second()))
            && self.check_contract_restriction("BYMINUTE", &Part::Int(self.last.minute()))
            && self.check_contract_restriction("BYHOUR", &Part::Int(self.last.hour()))
            && self.check_contract_restriction("BYDAY", &Part::Day(numeric_to_ical_day(dow, 0).to_owned()))
            && self.check_contract_restriction("BYWEEKNO", &Part::Int(week_no))
            && self.check_contract_restriction("BYMONTHDAY", &Part::Int(self.last.day()))
            && self.check_contract_restriction("BYMONTH", &Part::Int(self.last.month()))
            && self.check_contract_restriction("BYYEARDAY", &Part::Int(doy))
    }

    fn setup_defaults(&mut self, rule_type: &str, req: &str, deftime: i64) -> i64 {
        let rule_map = expand_map(&self.rule.freq)[index_map(rule_type).min(7)];
        if rule_map != CONTRACT {
            if !self.by_data.contains_key(rule_type) {
                self.by_data.insert(rule_type.to_owned(), vec![Part::Int(deftime)]);
            }
            if self.rule.freq != req {
                return self.by_data.get(rule_type).and_then(|v| v.first()).map_or(deftime, Part::int);
            }
        }
        deftime
    }
}

enum InitError {
    Invalid,
    Fatal(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ical::component::parse_recur;

    fn occurrences(rule: &str, start: &str, n: usize) -> Vec<String> {
        let r = Recur::from_data(&parse_recur(rule).unwrap());
        let s = time_from_string(start).unwrap();
        let mut it = RecurIterator::new(r, &s).unwrap();
        let mut out = Vec::new();
        while out.len() < n {
            match it.next().unwrap() {
                Some(t) => out.push(t.to_js_string()),
                None => break,
            }
        }
        out
    }

    #[test]
    fn rfc5545_basics() {
        assert_eq!(
            occurrences("FREQ=DAILY;COUNT=3", "2026-01-30T09:00:00", 10),
            vec!["2026-01-30T09:00:00", "2026-01-31T09:00:00", "2026-02-01T09:00:00"]
        );
        assert_eq!(
            occurrences("FREQ=WEEKLY;BYDAY=TU,TH", "2026-01-06T09:00:00", 4),
            vec!["2026-01-06T09:00:00", "2026-01-08T09:00:00", "2026-01-13T09:00:00", "2026-01-15T09:00:00"]
        );
        assert_eq!(
            occurrences("FREQ=MONTHLY;BYDAY=-1FR", "2026-01-30T09:00:00", 3),
            vec!["2026-01-30T09:00:00", "2026-02-27T09:00:00", "2026-03-27T09:00:00"]
        );
        assert_eq!(
            occurrences("FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU", "2026-03-29T01:00:00", 2),
            vec!["2026-03-29T01:00:00", "2027-03-28T01:00:00"]
        );
        assert_eq!(
            occurrences("FREQ=MONTHLY;BYMONTHDAY=31", "2026-01-31", 3),
            vec!["2026-01-31", "2026-03-31", "2026-05-31"]
        );
    }
}
