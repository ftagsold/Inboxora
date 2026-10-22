// Record the Node backend's outputs for the differential corpora (run from backend/).
//   node ../backend-rs/tests/differential/gen.mjs
import { writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = process.env.DIFF_DIR || path.dirname(fileURLToPath(import.meta.url));
const backend = path.resolve(here, '../../../backend/src');
const imp = (p) => import(pathToFileURL(path.join(backend, p)));
const san = await imp('services/emailSanitizer.ts');
const mp = await imp('services/messageParser.ts');
const html = (await import(pathToFileURL(path.join(here, 'html-corpus.mjs')))).default;

// Seeded random documents built from hostile fragments (mulberry32 PRNG).
function rng(seed) { return () => { seed |= 0; seed = seed + 0x6D2B79F5 | 0; let t = Math.imul(seed ^ seed >>> 15, 1 | seed); t = t + Math.imul(t ^ t >>> 7, 61 | t) ^ t; return ((t ^ t >>> 14) >>> 0) / 4294967296; }; }
const FRAGMENTS = ['<p>', '</p>', '<div class="x" style="color:red">', '</div>', '<b>', '</b>', '<i>', '</i>', '<a href="http://x.test/?a=1&b=2">', '<a href="javascript:alert(1)">', '</a>',
  '<img src="http://x.test/i.png" width="10">', '<img src=x onerror=alert(1)>', '<table>', '<tr>', '<td bgcolor="#eee">', '</td>', '</tr>', '</table>', '<ul>', '<li>', '</ul>',
  '<style>.a{background:url(http://x.test/b.png)}</style>', '<style>', '</style>', '<script>alert(1)</script>', '<script>', '<!-- c -->', '<!--', '-->', '<head>', '</head>', '<title>T</title>',
  '<svg>', '</svg>', '<math>', '<textarea>', '</textarea>', '<iframe>', '</iframe>', '<noscript>', '</noscript>', '<xmp>', '</xmp>', '<option>', '<select>', '&amp;', '&lt;', '&nbsp;', '&#x3c;',
  'text', ' ', '\n', 'é', '"', "'", '<', '>', '=', '<br>', '<hr/>', '<span style="font: 12px \'A\'; x">', '</span>', '<font color=red>', '<center>', '<h1>', '</h1>', '<pre>', '</pre>',
  '<blockquote>', '</blockquote>', '<td width="">', '<a name=n>', '<img srcset="a.png 1x, http://b.test/b.png 2x">', '<form>', '<input value=x>', '<!--[if mso]>', '<![endif]-->', '<base href=x>', '<p/>'];
const random = rng(20261022);
for (let n = 0; n < 600; n++) {
  let doc = '';
  const len = 1 + Math.floor(random() * 14);
  for (let k = 0; k < len; k++) doc += FRAGMENTS[Math.floor(random() * FRAGMENTS.length)];
  html.push(doc);
}

const sanitizer = html.map(input => ({
  input,
  sanitizeEmail: san.sanitizeEmail(input),
  sanitizeComposeBody: san.sanitizeComposeBody(input) ?? null,
  sanitizeSignature: san.sanitizeSignature(input) ?? null,
  stripEmailHead: san.stripEmailHead(input) ?? null,
  rewriteAnchorHrefs: san.rewriteAnchorHrefs(input) ?? null,
  rewriteEbayImageserUrls: san.rewriteEbayImageserUrls(input) ?? null,
  hasRemoteImages: san.hasRemoteImages(input),
  blockRemoteImages: san.blockRemoteImages(input) ?? null,
  blockRemoteImagesSanitized: san.blockRemoteImages(san.sanitizeEmail(input)) ?? null,
  buildSnippetFromHtml: mp.buildSnippetFromHtml(input),
}));
writeFileSync(path.join(here, 'sanitizer.expected.json'), JSON.stringify(sanitizer, null, 1));
console.log(`sanitizer: ${sanitizer.length} cases`);

const text = await import(pathToFileURL(path.join(here, 'text-corpus.mjs')));
const textOut = {
  snippetFromBody: text.snippets.map(([t, h]) => ({ text: t, html: h, out: mp.snippetFromBody(t, h) })),
  decodeMimeWords: text.mimeWords.map(input => ({ input, out: mp.decodeMimeWords(input) })),
  headers: text.rawHeaders.map(input => {
    const parsed = mp.parseRawHeaders(input);
    return {
      input,
      parsed,
      to: parsed.to ? mp.parseMailboxList(parsed.to) : [],
      category: mp.detectCategoryFromHeaders(parsed),
      bulk: mp.detectBulkFromParsedHeaders(parsed),
      delivery: mp.parseDeliveryAddresses(parsed),
      raw: mp.headersToRawString(parsed),
    };
  }),
  buildHeaders: text.storedMessages.map(input => ({ input, out: mp.buildHeadersFromMessage(input) })),
};
const cd = await imp('utils/contentDisposition.ts');
const rt = await imp('utils/richText.ts');
const nm = await imp('services/threading/normalizeMessageId.ts');
const names = ['report.pdf', 'ré\u202eport (1).pdf', '../etc/passwd', '\u0001\u0002', '', '  spaced  ', 'a"b\\c.txt', '😀.png', 'x'.repeat(300), "it's (*).doc"];
const ids = ['<a@b>', ' <a@b> ', 'a@b', '<a b>', '<é@b>', '', '<>', '<<a@b>>', 'x'.repeat(1000), '<a@b>\r\n'];
const idLists = ['<a@b> <c@d>\n<a@b>', 'junk <x@y> more <bad id> <z@w>', '', '<a@b><c@d>'];
textOut.utils = {
  disposition: names.map(input => ({ input, safe: cd.safeFilename(input), header: cd.attachmentDisposition(input) })),
  messageId: ids.map(input => ({ input, out: nm.normalizeMessageId(input) })),
  messageIdList: idLists.map(input => ({ input, out: nm.normalizeMessageIdList(input) })),
  plainText: html.slice(0, 106).map(input => ({ input, isHtml: rt.isHtmlDescription(input), plain: rt.htmlToPlainText(input) })),
};
writeFileSync(path.join(here, 'text.expected.json'), JSON.stringify(textOut, null, 1));

const vc = await imp('utils/vcard.ts');
const cf = await imp('utils/contactFields.ts');
const ct = await imp('utils/contactTransfer.ts');
const pim = await import(pathToFileURL(path.join(here, 'pim-corpus.mjs')));
writeFileSync(path.join(here, 'vcard.expected.json'), JSON.stringify({
  parse: pim.vcards.map(input => ({ input, out: vc.parseVCard(input) })),
  generate: pim.contacts.map(input => ({ input, out: vc.generateVCard(input) })),
  merge: pim.merges.map(([raw, contact]) => ({ raw, contact, out: vc.mergeVCard(raw, contact) })),
  rich: pim.richBodies.map(input => ({ input, out: cf.normalizeRichContactFields(input) ?? null })),
  exports: { google: ct.contactsToGoogleCsv(pim.exportRows), outlook: ct.contactsToOutlookCsv(pim.exportRows), vcard: ct.contactsToVCard(pim.exportRows) },
  googleCsv: pim.googleCsv.map(input => ({ input, out: ct.parseGoogleCsv(input) })),
  dates: ['2020-02-29', '2021-02-29', '20200101', '--02-29', '--0230', ' 1999-12-31 ', '1999-1-1', '0000-01-01'].map(input => ({ input, out: vc.normalizeVCardDate(input), strict: vc.normalizeVCardDate(input, false) })),
}, null, 1));
console.log('text/utils: written');

const ICAL = (await import(pathToFileURL(path.resolve(backend, '../node_modules/ical.js/dist/ical.js')))).default;
// ical.js can loop forever in RecurIterator.next() for rules whose contracting parts never
// match; the Rust port stops after STEP_BUDGET inner steps with this error, so apply the same
// budget here (identical results for every rule that terminates).
const STEP_BUDGET = 1_000_000;
const origNext = ICAL.RecurIterator.prototype.next;
const origCheck = ICAL.RecurIterator.prototype.check_contracting_rules;
ICAL.RecurIterator.prototype.next = function (again) { if (!again) this._steps = 0; return origNext.call(this, again); };
ICAL.RecurIterator.prototype.check_contracting_rules = function () { if (++this._steps > STEP_BUDGET) throw new Error('Recurrence rule exceeded the iteration step budget'); return origCheck.call(this); };
const rec = await imp('utils/calendarRecurrence.ts');
const ic = await imp('utils/ical.ts');
const icorp = await import(pathToFileURL(path.join(here, 'ical-corpus.mjs')));
// Each rule runs in a worker with a watchdog: ical.js never returns for some rules
// (e.g. FREQ=WEEKLY;BYWEEKNO=-1); those are recorded as timeouts.
const { Worker } = await import('node:worker_threads');
const icalPath = path.resolve(backend, '../node_modules/ical.js/dist/ical.js');
const iterOut = [];
let worker = null;
for (const [rule, start] of icorp.rules) {
  worker ??= new Worker(new URL('./ical-worker.mjs', pathToFileURL(path.join(here, 'x'))), { workerData: { icalPath } });
  const result = await new Promise((resolve) => {
    const timer = setTimeout(() => { worker.terminate(); worker = null; resolve({ timeout: true }); }, 5000);
    worker.once('message', (m) => { clearTimeout(timer); resolve(m); });
    worker.postMessage({ rule, start });
  });
  iterOut.push({ rule, start, ...result });
}
await worker?.terminate();
const ser = (v) => JSON.parse(JSON.stringify(v));
const projections = [];
icorp.docs.forEach((raw, d) => icorp.windows.forEach(([from, to]) => {
  const row = { id: `row-${d}`, summary: 'meta summary', description: null, location: null, organizer: null, attendees: null, starts_at: '2026-01-01T00:00:00.000Z', ends_at: '2026-01-01T01:00:00.000Z', all_day: false, raw_ical: raw };
  try { projections.push({ doc: d, from, to, out: ser(rec.projectCalendarResourceWithStatus(row, new Date(from), new Date(to), { maxIterations: 5000 })) }); }
  catch (e) { projections.push({ doc: d, from, to, error: String(e.message) }); }
}));
const parsed = icorp.docs.map(raw => ser(ic.parseCalendarEvent(raw)));
const truncated = icorp.truncations.map(([d, id]) => ({ doc: d, id, out: ser(rec.truncateSeriesBefore(icorp.docs[d], id)) }));
const merged = icorp.merges.map(([d, repl, id, cancel]) => { try { return { doc: d, repl, id, cancel, out: rec.mergeCalendarResource(icorp.docs[d], repl, id, cancel) }; } catch (e) { return { doc: d, repl, id, cancel, error: String(e.message) }; } });
const resources = icorp.docs.map(raw => { try { return { out: rec.calendarResources(raw) }; } catch (e) { return { error: String(e.message) }; } });
writeFileSync(path.join(here, 'ical.docs.json'), JSON.stringify(icorp.docs, null, 1));
writeFileSync(path.join(here, 'ical.expected.json'), JSON.stringify({ iterator: iterOut, projections, parsed, truncated, merged, resources }, null, 1));
console.log(`ical: ${iterOut.length} rules, ${projections.length} projections`);
