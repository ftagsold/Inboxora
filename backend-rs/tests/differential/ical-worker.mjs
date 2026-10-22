// Worker: evaluate one recurrence rule with ical.js (posted back as { out } or { error }).
import { parentPort, workerData } from 'node:worker_threads';
import { pathToFileURL } from 'node:url';
const ICAL = (await import(pathToFileURL(workerData.icalPath))).default;
const origNext = ICAL.RecurIterator.prototype.next;
const origCheck = ICAL.RecurIterator.prototype.check_contracting_rules;
ICAL.RecurIterator.prototype.next = function (again) { if (!again) this._steps = 0; return origNext.call(this, again); };
ICAL.RecurIterator.prototype.check_contracting_rules = function () { if (++this._steps > 1_000_000) throw new Error('Recurrence rule exceeded the iteration step budget'); return origCheck.call(this); };
parentPort.on('message', ({ rule, start }) => {
  try {
    const r = ICAL.Recur.fromData(ICAL.design.icalendar.value.recur.fromICAL(rule));
    const it = r.iterator(ICAL.Time.fromString(start));
    const out = [];
    for (let n = 0; n < 30; n++) { const o = it.next(); if (!o) break; out.push(o.toString()); }
    parentPort.postMessage({ out });
  } catch (e) { parentPort.postMessage({ error: String(e.message) }); }
});
