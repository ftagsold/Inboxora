//! `ICAL.Event` and `ICAL.RecurExpansion` (ical.js 2.2).

use std::collections::HashMap;

use super::component::{Component, PValue};
use super::recur::{Recur, RecurIterator};
use super::time::{Duration, Fields, Time, Zone};
use super::timezone::Calendar;

pub type IcalResult<T> = Result<T, String>;

/// An event component with its related recurrence exceptions.
pub struct Event<'a> {
    pub cal: &'a Calendar,
    pub comp: &'a Component,
    exceptions: HashMap<String, Event<'a>>,
    range_exceptions: Vec<(i64, String)>,
}

impl<'a> Event<'a> {
    /// `new ICAL.Event(component)`: exceptions in the same calendar are related automatically
    /// unless this component is itself an exception.
    pub fn new(cal: &'a Calendar, comp: &'a Component, siblings: &'a [Component]) -> IcalResult<Self> {
        let mut ev = Event { cal, comp, exceptions: HashMap::new(), range_exceptions: Vec::new() };
        if !ev.is_recurrence_exception() {
            for other in siblings.iter().filter(|c| c.name == "vevent" && c.has("recurrence-id")) {
                ev.relate_exception(Event {
                    cal,
                    comp: other,
                    exceptions: HashMap::new(),
                    range_exceptions: Vec::new(),
                })?;
            }
        }
        Ok(ev)
    }

    /// An event without exception relation (an exception itself, or a standalone component).
    pub fn standalone(cal: &'a Calendar, comp: &'a Component) -> Self {
        Event { cal, comp, exceptions: HashMap::new(), range_exceptions: Vec::new() }
    }

    pub fn is_recurrence_exception(&self) -> bool {
        self.comp.has("recurrence-id")
    }

    fn relate_exception(&mut self, ex: Event<'a>) -> IcalResult<()> {
        let rid = ex.recurrence_id()?.ok_or("missing recurrence-id value")?;
        let id = rid.to_js_string();
        if ex.modifies_future() {
            let item = (rid.to_unix_time(), id.clone());
            let keys: Vec<i64> = self.range_exceptions.iter().map(|(t, _)| *t).collect();
            let idx = binsearch_keys(&keys, item.0);
            self.range_exceptions.insert(idx, item);
        }
        self.exceptions.insert(id, ex);
        Ok(())
    }

    fn modifies_future(&self) -> bool {
        self.comp.prop("recurrence-id").and_then(|p| p.param("range")) == Some("THISANDFUTURE")
    }

    fn time(&self, name: &str) -> IcalResult<Option<Time>> {
        self.cal.first_time(self.comp, name).transpose()
    }

    pub fn start_date(&self) -> IcalResult<Option<Time>> {
        self.time("dtstart")
    }

    pub fn recurrence_id(&self) -> IcalResult<Option<Time>> {
        self.time("recurrence-id")
    }

    fn duration_prop(&self) -> IcalResult<Option<Duration>> {
        match self.comp.prop("duration").and_then(|p| p.first_str()) {
            Some(v) => Duration::parse(&v).map(Some),
            None => Ok(None),
        }
    }

    /// `endDate`: DTEND, else DTSTART + DURATION, else DTSTART (+1 day for dates).
    pub fn end_date(&self) -> IcalResult<Option<Time>> {
        if let Some(end) = self.time("dtend")? {
            return Ok(Some(end));
        }
        let Some(start) = self.start_date()? else { return Ok(None) };
        let end = start.clone();
        if let Some(d) = self.duration_prop()? {
            end.add_duration(&d);
        } else if end.is_date() {
            end.set_day(end.day() + 1);
        }
        Ok(Some(end))
    }

    /// `duration`: DURATION, else end − start (absolute).
    pub fn duration(&self) -> IcalResult<Duration> {
        if let Some(d) = self.duration_prop()? {
            return Ok(d);
        }
        let start = self.start_date()?.ok_or("missing DTSTART")?;
        let end = self.end_date()?.ok_or("missing DTSTART")?;
        Ok(Duration::from_seconds(end.subtract_date_tz_seconds(&start)))
    }

    fn find_range_exception(&self, time: &Time) -> Option<&str> {
        if self.range_exceptions.is_empty() {
            return None;
        }
        let utc = time.to_unix_time();
        let keys: Vec<i64> = self.range_exceptions.iter().map(|(t, _)| *t).collect();
        let idx = binsearch_keys(&keys, utc) as i64 - 1;
        if idx < 0 {
            return None;
        }
        let item = &self.range_exceptions[idx as usize];
        if utc < item.0 { None } else { Some(&item.1) }
    }

    /// `getOccurrenceDetails(occurrence)` → (item, start, end).
    pub fn occurrence_details(&self, occ: &Time) -> IcalResult<(&Event<'a>, Time, Time)> {
        let id = occ.to_js_string();
        let utc_id = occ.convert_to_zone(&Zone::Utc).to_js_string();
        if let Some(ex) = self.exceptions.get(&id).or_else(|| self.exceptions.get(&utc_id)) {
            let s = ex.start_date()?.ok_or("missing DTSTART")?;
            let e = ex.end_date()?.ok_or("missing DTSTART")?;
            return Ok((ex, s, e));
        }
        if let Some(rid) = self.find_range_exception(occ).map(str::to_owned) {
            let ex = &self.exceptions[&rid];
            let new_start = ex.start_date()?.ok_or("missing DTSTART")?;
            let original = ex.recurrence_id()?.ok_or("missing RECURRENCE-ID")?;
            let original = Time::new(original.fields(), new_start.zone.clone());
            let diff = Duration::from_seconds(new_start.subtract_date_seconds(&original));
            let start = Time::new(occ.fields(), new_start.zone.clone());
            start.add_duration(&diff);
            let end = start.clone();
            end.add_duration(&ex.duration()?);
            return Ok((ex, start, end));
        }
        let end = occ.clone();
        end.add_duration(&self.duration()?);
        Ok((self, occ.clone(), end))
    }

    /// `iterator()`.
    pub fn iterator(&self) -> IcalResult<RecurExpansion> {
        let start = self.start_date()?.ok_or(".dtstart (ICAL.Time) must be given")?;
        RecurExpansion::new(self.cal, self.comp, &start)
    }
}

/// `binsearchInsert` over numeric keys.
fn binsearch_keys(list: &[i64], seek: i64) -> usize {
    if list.is_empty() {
        return 0;
    }
    let (mut low, mut high) = (0i64, list.len() as i64 - 1);
    let (mut mid, mut c) = (0i64, 0i64);
    while low <= high {
        mid = low + (high - low) / 2;
        c = (seek - list[mid as usize]).signum();
        if c < 0 {
            high = mid - 1;
        } else if c > 0 {
            low = mid + 1;
        } else {
            break;
        }
    }
    if c > 0 { (mid + 1) as usize } else { mid as usize }
}

/// `_compare_special`: a date-time compared with a date uses only its date.
fn compare_special(a: &Time, b: &Time) -> i32 {
    if !a.is_date() && b.is_date() {
        let d = Time::new(
            Fields { year: a.year(), month: a.month(), day: a.day(), is_date: true, ..Fields::default() },
            Zone::Floating,
        );
        return d.compare(b);
    }
    a.compare(b)
}

fn binsearch_insert_by(list: &[Time], seek: &Time, cmp: fn(&Time, &Time) -> i32) -> usize {
    if list.is_empty() {
        return 0;
    }
    let (mut low, mut high) = (0i64, list.len() as i64 - 1);
    let (mut mid, mut c) = (0i64, 0);
    while low <= high {
        mid = low + (high - low) / 2;
        c = cmp(seek, &list[mid as usize]);
        if c < 0 {
            high = mid - 1;
        } else if c > 0 {
            low = mid + 1;
        } else {
            break;
        }
    }
    if c > 0 { (mid + 1) as usize } else { mid as usize }
}

fn plain_compare(a: &Time, b: &Time) -> i32 {
    a.compare(b)
}

pub struct RecurExpansion {
    pub complete: bool,
    iters: Vec<(u64, RecurIterator)>,
    /// An iterator ical.js removed from its list while it was already the chosen one this round.
    detached: Option<(u64, RecurIterator)>,
    rule_dates: Vec<Time>,
    rule_date_inc: usize,
    ex_dates: Vec<Time>,
    ex_date_inc: usize,
    last: Time,
}

impl RecurExpansion {
    fn new(cal: &Calendar, comp: &Component, dtstart: &Time) -> IcalResult<Self> {
        let mut e = RecurExpansion {
            complete: false,
            iters: Vec::new(),
            detached: None,
            rule_dates: Vec::new(),
            rule_date_inc: 0,
            ex_dates: Vec::new(),
            ex_date_inc: 0,
            last: dtstart.clone(),
        };
        if !comp.has("rdate") && !comp.has("rrule") && !comp.has("recurrence-id") {
            e.rule_dates = vec![dtstart.clone()];
            e.complete = true;
            return Ok(e);
        }
        if comp.has("rdate") {
            e.rule_dates = extract_dates(cal, comp, "rdate", plain_compare)?;
            if e.rule_dates.first().is_some_and(|d| d.compare(dtstart) < 0) {
                e.rule_date_inc = 0;
                e.last = e.rule_dates[0].clone();
            } else {
                e.rule_date_inc = binsearch_insert_by(&e.rule_dates, &e.last, plain_compare);
            }
        }
        for (n, prop) in comp.props_named("rrule").enumerate() {
            let Some(PValue::Recur(data)) = prop.values.first() else { continue };
            let mut it = RecurIterator::new(Recur::from_data(data), dtstart).map_err(|e| e.0)?;
            it.next().map_err(|e| e.0)?;
            e.iters.push((n as u64, it));
        }
        if comp.has("exdate") {
            e.ex_dates = extract_dates(cal, comp, "exdate", plain_compare)?;
            e.ex_date_inc = binsearch_insert_by(&e.ex_dates, &e.last, compare_special);
        }
        Ok(e)
    }

    fn rule_date(&self) -> Option<&Time> {
        self.rule_dates.get(self.rule_date_inc)
    }

    fn ex_date(&self) -> Option<&Time> {
        self.ex_dates.get(self.ex_date_inc)
    }

    /// `_nextRecurrenceIter`, including ical.js's off-by-one when dropping a completed iterator
    /// (it decrements the index before splicing, so the *previous* iterator is removed). The
    /// chosen iterator is returned by id; it may have been detached from the list this round.
    fn next_iter_id(&mut self) -> Option<u64> {
        self.detached = None;
        let mut len = self.iters.len();
        let mut i = 0usize;
        let mut chosen: Option<u64> = None;
        while i < len {
            if self.iters[i].1.completed {
                len -= 1;
                if i != 0 {
                    i -= 1;
                }
                let removed = self.iters.remove(i);
                if chosen == Some(removed.0) {
                    self.detached = Some(removed);
                }
                i += 1;
                continue;
            }
            let better = match chosen {
                None => true,
                Some(c) => self.iter_by_id(c).is_some_and(|it| it.last.compare(&self.iters[i].1.last) > 0),
            };
            if better {
                chosen = Some(self.iters[i].0);
            }
            i += 1;
        }
        chosen
    }

    fn iter_by_id(&self, id: u64) -> Option<&RecurIterator> {
        self.iters
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, it)| it)
            .or_else(|| self.detached.as_ref().filter(|(i, _)| *i == id).map(|(_, it)| it))
    }

    fn iter_by_id_mut(&mut self, id: u64) -> Option<&mut RecurIterator> {
        if let Some(pos) = self.iters.iter().position(|(i, _)| *i == id) {
            return Some(&mut self.iters[pos].1);
        }
        self.detached.as_mut().filter(|(i, _)| *i == id).map(|(_, it)| it)
    }

    /// `next()`: the next occurrence start, `Ok(None)` when complete.
    pub fn next(&mut self) -> IcalResult<Option<Time>> {
        let mut tries = 0;
        loop {
            tries += 1;
            if tries > 501 {
                return Err("max tries have occurred, rule may be impossible to fulfill.".into());
            }
            let iter = if self.iters.is_empty() { None } else { self.next_iter_id() };
            let rule_date = self.rule_date().cloned();
            if rule_date.is_none() && iter.is_none() {
                self.complete = true;
                return Ok(None);
            }
            let iter_last = iter.and_then(|id| self.iter_by_id(id)).map(|it| it.last.clone());
            let next: Time;
            let mut from_rule_date = true;
            match (rule_date, iter_last) {
                (Some(rd), Some(il)) if rd.compare(&il) <= 0 => next = rd,
                (Some(rd), None) => next = rd,
                (_, Some(il)) => {
                    next = il;
                    if let Some(it) = iter.and_then(|id| self.iter_by_id_mut(id)) {
                        it.next().map_err(|e| e.0)?;
                    }
                    from_rule_date = false;
                }
                (None, None) => return Ok(None),
            }
            if from_rule_date {
                self.rule_date_inc += 1;
            }
            self.last = next;
            if let Some(ex) = self.ex_date().cloned() {
                let c = compare_special(&self.last, &ex);
                if c > 0 {
                    self.ex_date_inc += 1;
                }
                if c == 0 {
                    self.ex_date_inc += 1;
                    continue;
                }
            }
            return Ok(Some(self.last.clone()));
        }
    }
}

/// `_extractDates`: all values of all `name` properties, sorted (periods use their start).
fn extract_dates(cal: &Calendar, comp: &Component, name: &str, cmp: fn(&Time, &Time) -> i32) -> IcalResult<Vec<Time>> {
    let mut out: Vec<Time> = Vec::new();
    for prop in comp.props_named(name) {
        for v in &prop.values {
            let s = match v {
                PValue::Str(s) => s.clone(),
                PValue::Period(a, _) => a.clone(),
                _ => continue,
            };
            let t = cal.time_value(prop, &s, false)?;
            let idx = binsearch_insert_by(&out, &t, cmp);
            out.insert(idx, t);
        }
    }
    Ok(out)
}
