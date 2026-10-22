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
writeFileSync(path.join(here, 'text.expected.json'), JSON.stringify(textOut, null, 1));
console.log(`text: ${Object.values(textOut).reduce((n, a) => n + a.length, 0)} cases`);
