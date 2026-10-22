// Record the Node DAV clients' XML handling for the DAV corpus (run from backend/).
//   node ../backend-rs/tests/differential/gen-dav.mjs
import { writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { XMLParser, XMLValidator } from 'fast-xml-parser';

const here = path.dirname(fileURLToPath(import.meta.url));
const backend = path.resolve(here, '../../../backend/src');
const imp = (p) => import(pathToFileURL(path.join(backend, p)));
const card = await imp('services/carddavClient.ts');
const cal = await imp('services/caldavClient.ts');
const davXml = await imp('utils/davXml.ts');
const { docs } = await import(pathToFileURL(path.join(here, 'dav-corpus.mjs')));

// The two parser configurations the backend uses (clients / externalCalendarSync).
const clientParser = new XMLParser({ ignoreAttributes: false, removeNSPrefix: true, trimValues: false, processEntities: { maxTotalExpansions: 10_000_000, maxExpansionDepth: 10 } });
const syncParser = new XMLParser({ ignoreAttributes: false, removeNSPrefix: true, trimValues: false });
const BASE = 'https://dav.example.test/base/dir/';
const attempt = (fn) => { try { return { ok: fn() ?? null }; } catch (e) { return { error: String(e.message) }; } };

const out = docs.map((doc) => {
  const syncTree = attempt(() => syncParser.parse(doc));
  return {
    doc,
    valid: XMLValidator.validate(doc) === true,
    client: attempt(() => clientParser.parse(doc)),
    sync: syncTree,
    complete: 'ok' in syncTree ? attempt(() => { davXml.requireCompleteMultistatus(doc, syncTree.ok); return true; }) : null,
    principal: attempt(() => card.extractHref(doc, 'current-user-principal', BASE)),
    bookHome: attempt(() => card.extractHref(doc, 'addressbook-home-set', BASE)),
    calHome: attempt(() => cal.extractHref(doc, 'calendar-home-set', BASE)),
    books: attempt(() => card.parseAddressBooks(doc, BASE)),
    calendars: attempt(() => cal.parseCalendars(doc, BASE)),
    cards: attempt(() => card.parseCards(doc, BASE)),
    charRefs: davXml.decodeDavCharRefs(doc),
  };
});
writeFileSync(path.join(here, 'dav.expected.json'), JSON.stringify(out, null, 1));
console.log(`dav: ${out.length} documents`);
