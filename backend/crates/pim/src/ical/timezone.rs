//! `ICAL.Timezone` from a VTIMEZONE definition (ical.js 2.2 `utcOffset` algorithm), and the
//! per-calendar zone lookup used when decorating date-time values.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use super::component::{Component, PValue, Property};
use super::recur::{Recur, RecurIterator};
use super::time::{Fields, Time, Zone, adjust_fields};

#[derive(Debug, Clone, Copy)]
struct Change {
    f: Fields,
    utc_offset: i64,
    prev_utc_offset: i64,
    is_daylight: bool,
}

fn cmp_change(a: &Fields, b: &Fields) -> i32 {
    for (x, y) in [
        (a.year, b.year),
        (a.month, b.month),
        (a.day, b.day),
        (a.hour, b.hour),
        (a.minute, b.minute),
        (a.second, b.second),
    ] {
        if x < y {
            return -1;
        }
        if x > y {
            return 1;
        }
    }
    0
}

fn adjust_change(c: &mut Change, seconds: i64) {
    adjust_fields(&mut c.f, 0, 0, 0, seconds);
}

/// `binsearchInsert`.
fn binsearch_insert(list: &[Change], seek: &Fields) -> usize {
    if list.is_empty() {
        return 0;
    }
    let (mut low, mut high) = (0i64, list.len() as i64 - 1);
    let mut mid = 0i64;
    let mut cmpval = 0;
    while low <= high {
        mid = low + (high - low) / 2;
        cmpval = cmp_change(seek, &list[mid as usize].f);
        if cmpval < 0 {
            high = mid - 1;
        } else if cmpval > 0 {
            low = mid + 1;
        } else {
            break;
        }
    }
    if cmpval > 0 { (mid + 1) as usize } else { mid as usize }
}

/// UTC offset text (`+HH:MM[:SS]` jCal form) → seconds; seconds are ignored like ical.js.
fn offset_seconds(v: &str) -> Option<i64> {
    let factor = if v.starts_with('+') { 1 } else { -1 };
    let hours = super::js_parse_int(v.get(1..3)?)?;
    let minutes = super::js_parse_int(v.get(4..6)?)?;
    Some(factor * (hours * 3600 + minutes * 60))
}

#[derive(Debug)]
pub struct Timezone {
    pub tzid: String,
    component: Component,
    changes: RefCell<Vec<Change>>,
    expanded_until: Cell<i64>,
}

fn current_year() -> i64 {
    use chrono::Datelike;
    i64::from(chrono::Utc::now().year())
}

impl Timezone {
    pub fn new(tzid: &str, component: Component) -> Self {
        Timezone { tzid: tzid.to_owned(), component, changes: RefCell::new(Vec::new()), expanded_until: Cell::new(0) }
    }

    /// Whether the definition has STANDARD/DAYLIGHT children (calendarZoneResolver's test).
    pub fn has_rules(&self) -> bool {
        self.component.comps.iter().any(|c| c.name == "standard" || c.name == "daylight")
    }

    fn ensure_coverage(&self, year: i64) {
        let end = year.max(current_year()) + 5;
        let needs = self.changes.borrow().is_empty() || self.expanded_until.get() < year;
        if needs {
            let mut changes = self.changes.borrow_mut();
            for sub in &self.component.comps {
                expand_component(sub, end, &mut changes);
            }
            changes.sort_by(|a, b| cmp_change(&a.f, &b.f).cmp(&0));
            self.expanded_until.set(end);
        }
    }

    /// `utcOffset(tt)` in seconds.
    pub fn utc_offset(&self, tt: &Time) -> i64 {
        self.ensure_coverage(tt.year());
        let changes = self.changes.borrow();
        if changes.is_empty() {
            return 0;
        }
        let f = tt.fields();
        let tt_change = Fields { is_date: false, ..f };
        let mut change_num = binsearch_insert(&changes, &tt_change) as i64;
        if change_num >= changes.len() as i64 {
            change_num = changes.len() as i64 - 1;
        }
        let mut to_use: i64 = -1;
        let mut step = 1;
        loop {
            let mut change = changes[change_num as usize];
            if change.utc_offset < change.prev_utc_offset {
                let o = change.utc_offset;
                adjust_change(&mut change, o);
            } else {
                let o = change.prev_utc_offset;
                adjust_change(&mut change, o);
            }
            if cmp_change(&tt_change, &change.f) >= 0 {
                to_use = change_num;
            } else {
                step = -1;
            }
            if step == -1 && to_use != -1 {
                break;
            }
            change_num += step;
            if change_num < 0 {
                return 0;
            }
            if change_num >= changes.len() as i64 {
                break;
            }
        }
        if to_use < 0 {
            return 0;
        }
        let mut zone_change = changes[to_use as usize];
        if zone_change.utc_offset - zone_change.prev_utc_offset < 0 && to_use > 0 {
            let mut tmp = zone_change;
            let o = tmp.prev_utc_offset;
            adjust_change(&mut tmp, o);
            if cmp_change(&tt_change, &tmp.f) < 0 {
                let prev = changes[to_use as usize - 1];
                let want_daylight = false;
                if zone_change.is_daylight != want_daylight && prev.is_daylight == want_daylight {
                    zone_change = prev;
                }
            }
        }
        zone_change.utc_offset
    }
}

fn first_str(c: &Component, name: &str) -> Option<String> {
    c.prop(name).and_then(Property::first_str)
}

fn expand_component(c: &Component, year: i64, changes: &mut Vec<Change>) {
    let (Some(dt), Some(to), Some(from)) =
        (first_str(c, "dtstart"), first_str(c, "tzoffsetto"), first_str(c, "tzoffsetfrom"))
    else {
        return;
    };
    let (Some(to), Some(from)) = (offset_seconds(&to), offset_seconds(&from)) else { return };
    // DTSTART inside STANDARD/DAYLIGHT is floating.
    let Ok(dtstart) = super::recur::time_from_string(&dt) else { return };
    let dtstart = Time::new(dtstart.fields(), Zone::Floating);
    let base = |f: Fields| Change { f, utc_offset: to, prev_utc_offset: from, is_daylight: c.name == "daylight" };
    if !c.has("rrule") && !c.has("rdate") {
        let mut change = base(Fields { is_date: false, ..dtstart.fields() });
        adjust_change(&mut change, -from);
        changes.push(change);
        return;
    }
    for rdate in c.props_named("rdate") {
        let Some(v) = rdate.first_str() else { continue };
        let Ok(time) = super::recur::time_from_string(&v) else { continue };
        let tf = time.fields();
        let mut change = if time.is_date() {
            let d = dtstart.fields();
            base(Fields {
                year: tf.year,
                month: tf.month,
                day: tf.day,
                hour: d.hour,
                minute: d.minute,
                second: d.second,
                is_date: false,
            })
        } else {
            base(Fields { is_date: false, ..tf })
        };
        let utc = if time.is_date() { dtstart.zone.is_utc() } else { time.zone.is_utc() };
        if !utc {
            adjust_change(&mut change, -from);
        }
        changes.push(change);
    }
    if let Some(PValue::Recur(data)) = c.prop("rrule").and_then(|p| p.values.first()) {
        let mut rule = Recur::from_data(data);
        if let Some(until) = rule.until.take() {
            if until.zone.is_utc() {
                until.adjust(0, 0, 0, from);
                rule.until = Some(Time::new(until.fields(), Zone::Floating));
            } else {
                rule.until = Some(until);
            }
        }
        let Ok(mut it) = RecurIterator::new(rule, &dtstart) else { return };
        while let Ok(Some(occ)) = it.next() {
            if occ.year() > year {
                break;
            }
            let mut change = base(occ.fields());
            adjust_change(&mut change, -from);
            changes.push(change);
        }
    }
}

/// A parsed calendar with `getTimeZoneByID` caching (only a VCALENDAR root resolves zones).
pub struct Calendar {
    pub root: Component,
    zones: RefCell<HashMap<String, Option<Rc<Timezone>>>>,
}

impl Calendar {
    pub fn new(root: Component) -> Self {
        Calendar { root, zones: RefCell::new(HashMap::new()) }
    }

    /// `component.getTimeZoneByID(tzid)` on the root.
    pub fn zone_by_id(&self, tzid: &str) -> Option<Rc<Timezone>> {
        if self.root.name != "vcalendar" {
            return None;
        }
        if let Some(z) = self.zones.borrow().get(tzid) {
            return z.clone();
        }
        let found = self
            .root
            .subcomponents("vtimezone")
            .find(|z| first_str(z, "tzid").as_deref() == Some(tzid))
            .map(|z| Rc::new(Timezone::new(tzid, z.clone())));
        self.zones.borrow_mut().insert(tzid.to_owned(), found.clone());
        found
    }

    /// Decorate a date/date-time property value (`Time.fromString(value, prop)`).
    pub fn time_value(&self, prop: &Property, value: &str, in_timezone_rule: bool) -> Result<Time, String> {
        let int = |s: &str| super::js_parse_int(s).ok_or_else(|| format!("Could not extract integer from \"{s}\""));
        let sl = |a: usize, b: usize| value.get(a..b.min(value.len())).unwrap_or("");
        if value.len() <= 10 {
            let f = Fields {
                year: int(sl(0, 4))?,
                month: int(sl(5, 7))?,
                day: int(sl(8, 10))?,
                is_date: true,
                ..Fields::default()
            };
            return Ok(Time::new(f, Zone::Floating));
        }
        if value.len() < 19 {
            return Err(format!("invalid date-time value: \"{value}\""));
        }
        let f = Fields {
            year: int(sl(0, 4))?,
            month: int(sl(5, 7))?,
            day: int(sl(8, 10))?,
            hour: int(sl(11, 13))?,
            minute: int(sl(14, 16))?,
            second: int(sl(17, 19))?,
            is_date: false,
        };
        let zone = if value.ends_with('Z') {
            Zone::Utc
        } else if in_timezone_rule {
            Zone::Floating
        } else {
            match prop.param("tzid") {
                Some(tzid) => match self.zone_by_id(tzid) {
                    Some(tz) => Zone::Tz(tz),
                    None if matches!(tzid, "Z" | "UTC" | "GMT") => Zone::Utc,
                    None => Zone::Floating,
                },
                None => Zone::Floating,
            }
        };
        Ok(Time::new(f, zone))
    }

    /// First value of a date/date-time property of `component`.
    pub fn first_time(&self, component: &Component, name: &str) -> Option<Result<Time, String>> {
        let prop = component.prop(name)?;
        let value = prop.first_str()?;
        Some(self.time_value(prop, &value, matches!(component.name.as_str(), "standard" | "daylight")))
    }
}
