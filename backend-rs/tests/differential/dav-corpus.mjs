// DAV client XML corpus: realistic multistatus responses from common servers plus the parser
// edge cases the Node clients depend on (fast-xml-parser 5 value typing, whitespace, CDATA,
// entities, namespaces, repeated elements, malformed documents).

const ms = (inner, ns = 'xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:ic="http://apple.com/ns/ical/"') =>
  `<?xml version="1.0" encoding="UTF-8"?>\n<d:multistatus ${ns}>${inner}</d:multistatus>`;
const resp = (href, props, status = 'HTTP/1.1 200 OK') => `<d:response><d:href>${href}</d:href><d:propstat><d:prop>${props}</d:prop><d:status>${status}</d:status></d:propstat></d:response>`;
const VCARD = 'BEGIN:VCARD\r\nVERSION:3.0\r\nUID:abc\r\nFN:Ann &amp; Bob\r\nEMAIL:ann@x.test\r\nEND:VCARD\r\n';
const ICS = 'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:e1\r\nDTSTART:20261101T090000Z\r\nSUMMARY:A &lt;b&gt; &#233; &amp;#233;\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n';

export const docs = [
  // Nextcloud-style principal discovery
  ms(resp('/remote.php/dav/', '<d:current-user-principal><d:href>/remote.php/dav/principals/users/ann/</d:href></d:current-user-principal>')),
  ms(resp('/remote.php/dav/principals/users/ann/', '<card:addressbook-home-set><d:href>/remote.php/dav/addressbooks/users/ann/</d:href></card:addressbook-home-set>')),
  ms(resp('/p/', '<cal:calendar-home-set><d:href>https://other.test/cal/ann/</d:href></cal:calendar-home-set>')),
  ms(resp('/p/', '<d:current-user-principal><d:href> /spaced/ </d:href></d:current-user-principal>')),
  ms(resp('/p/', '<d:current-user-principal>\n  <d:href>/nl/</d:href>\n</d:current-user-principal>')),
  ms(resp('/p/', '<d:current-user-principal><d:unauthenticated/></d:current-user-principal>')),
  ms(resp('/p/', '<d:current-user-principal/>')),
  ms(resp('/p/', '<d:current-user-principal>/bare/text/</d:current-user-principal>')),
  ms(resp('/p/', '<d:current-user-principal><d:href>/a/</d:href><d:href>/b/</d:href></d:current-user-principal>')),
  ms(resp('/p/', '<d:current-user-principal><d:href>/not/</d:href></d:current-user-principal>', 'HTTP/1.1 404 Not Found')),
  ms(resp('/p/', '<d:current-user-principal><d:href x="1">/attr/</d:href></d:current-user-principal>')),
  ms(resp('/p/', '<d:current-user-principal><d:href>12345</d:href></d:current-user-principal>')),
  ms(''),
  ms('\n'),
  '<?xml version="1.0"?><multistatus xmlns="DAV:"><response><href>/x/</href><propstat><prop><current-user-principal><href>/default-ns/</href></current-user-principal></prop><status>HTTP/1.1 200 OK</status></propstat></response></multistatus>',
  // address book listings
  ms(
    resp('/ab/', '<d:resourcetype><d:collection/></d:resourcetype><d:displayname>Home</d:displayname>')
    + resp('/ab/contacts/', '<d:resourcetype><d:collection/><card:addressbook/></d:resourcetype><d:displayname>Contacts</d:displayname><cs:getctag>"c1"</cs:getctag>')
    + resp('/ab/work/', '<d:resourcetype>\n<d:collection/>\n<card:addressbook/>\n</d:resourcetype><d:displayname>Work &amp; Co</d:displayname>')
    + resp('/ab/numeric/', '<d:resourcetype><card:addressbook/></d:resourcetype><d:displayname>2024</d:displayname>')
    + resp('/ab/bool/', '<d:resourcetype><card:addressbook/></d:resourcetype><d:displayname>true</d:displayname>')
    + resp('/ab/empty/', '<d:resourcetype><card:addressbook/></d:resourcetype><d:displayname/>')
    + resp('https://abs.test/ab/x/', '<d:resourcetype><card:addressbook/></d:resourcetype>'),
  ),
  ms(resp('/ab/', '<d:resourcetype/><d:displayname>Item</d:displayname>') + resp('/ab/b/', '<d:resourcetype><card:addressbook/></d:resourcetype><d:displayname>B</d:displayname>')),
  ms(resp('/ab/', '<d:resourcetype> </d:resourcetype>')),
  ms(`<d:response><d:href>/ab/split/</d:href><d:propstat><d:prop><d:resourcetype><card:addressbook/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat><d:propstat><d:prop><d:displayname>Hidden</d:displayname></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat></d:response>`),
  ms(`<d:response><d:href>/ab/two/</d:href><d:propstat><d:prop><d:displayname>First</d:displayname></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat><d:propstat><d:prop><d:resourcetype><card:addressbook/></d:resourcetype><d:displayname>Second</d:displayname></d:prop></d:propstat></d:response>`),
  // calendar listings
  ms(
    resp('/cal/', '<d:resourcetype><d:collection/></d:resourcetype>')
    + resp('/cal/work/', '<d:resourcetype><d:collection/><cal:calendar/></d:resourcetype><d:displayname>Work</d:displayname><ic:calendar-color>#FF0000FF</ic:calendar-color>')
    + resp('/cal/home/', '<d:resourcetype><cal:calendar/></d:resourcetype><d:displayname>Home</d:displayname><ic:calendar-color symbolic-color="custom">#00ff00</ic:calendar-color>')
    + resp('/cal/inbox/', '<d:resourcetype><cal:schedule-inbox/><cal:calendar/></d:resourcetype>')
    + resp('/cal/outbox/', '<d:resourcetype><cal:schedule-outbox/><cal:calendar/></d:resourcetype>')
    + resp('/cal/nocolor/', '<d:resourcetype><cal:calendar/></d:resourcetype><ic:calendar-color>red</ic:calendar-color>')
    + resp('/cal/sp%20ace/', '<d:resourcetype><cal:calendar/></d:resourcetype><ic:calendar-color> #123456 </ic:calendar-color>')
    + resp('/cal/hex/', '<d:resourcetype><cal:calendar/></d:resourcetype><d:displayname>0x1F</d:displayname>'),
  ),
  // cards
  ms(
    resp('/ab/contacts/', '<d:getetag/>')
    + resp('/ab/contacts/a.vcf', `<d:getetag>"e1"</d:getetag><card:address-data>${VCARD}</card:address-data>`)
    + resp('/ab/contacts/b.vcf', `<d:getetag>"e2"</d:getetag><card:address-data><![CDATA[${VCARD.replace('&amp;', '&')}]]></card:address-data>`)
    + resp('/ab/contacts/c.vcf', '<d:getetag>12345</d:getetag><card:address-data>\n  BEGIN:VCARD&#13;\nUID:c&#x41;\nEND:VCARD\n  </card:address-data>')
    + resp('/ab/contacts/d.vcf', '<d:getetag>W/"weak"</d:getetag><card:address-data>   </card:address-data>')
    + resp('/ab/contacts/e.vcf', '<card:address-data>BEGIN:VCARD&#x1F600;&#xD800;&#1114112;END:VCARD</card:address-data>'),
  ),
  ms(resp('/ab/contacts/a.vcf', `<card:address-data>${VCARD}</card:address-data>`) + '<d:response><d:href>/ab/contacts/</d:href><d:status>HTTP/1.1 507 Insufficient Storage</d:status></d:response>'),
  ms(resp('/ab/contacts/a.vcf', `<card:address-data>${VCARD}</card:address-data>`, 'HTTP/1.1 403 Forbidden')),
  ms(resp('/ab/contacts/a.vcf', `<card:address-data>${VCARD}</card:address-data>`, 'HTTP/1.1 503 Service Unavailable')),
  ms('<d:response><d:href>/x</d:href><d:status code="1">HTTP/1.1 500 Oops</d:status></d:response>'),
  ms('<d:response><d:href>/x</d:href><d:status>HTTP/1.1 5000 Odd</d:status></d:response>'),
  // calendar data
  ms(
    resp('/cal/work/', '<d:getetag>"ctag"</d:getetag>')
    + resp('/cal/work/e1.ics', `<d:getetag>"abc"</d:getetag><cal:calendar-data>${ICS}</cal:calendar-data>`)
    + resp('/cal/work/e2.ics', '<d:getetag>"q"</d:getetag><cal:calendar-data><![CDATA[BEGIN:VCALENDAR\r\nX:&amp;\r\nEND:VCALENDAR]]></cal:calendar-data>')
    + resp('e3.ics', '<cal:calendar-data>123</cal:calendar-data>')
    + resp('', '<cal:calendar-data>BEGIN:VCALENDAR</cal:calendar-data>'),
  ),
  ms('<d:response><d:href>/one/</d:href><d:propstat><d:prop><cal:calendar-data>X</cal:calendar-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>'),
  // malformed / unusual documents
  '',
  'not xml at all',
  '<d:multistatus xmlns:d="DAV:"><d:response>',
  '<d:multistatus xmlns:d="DAV:"><d:response></d:multistatus>',
  '<a></a><b></b>',
  '<multistatus>text only</multistatus>',
  '<multistatus>42</multistatus>',
  '<multistatus/>',
  '<?xml version="1.0"?><!DOCTYPE x [<!ENTITY e "expanded">]><multistatus><response><href>&e;</href></response></multistatus>',
  '<multistatus><response><href>a &lt; b &gt; c &quot; &apos; &amp;amp; &#65; &#x42;</href></response></multistatus>',
  '<multistatus><response><href>&unknown; &amp</href></response></multistatus>',
  '<multistatus><!-- comment --><response><href>/c/<!-- inner -->d</href></response></multistatus>',
  '<multistatus><response a="1" b=\'two\' xmlns:z="x" z:c="3"><href>/attrs/</href></response></multistatus>',
  '<x:y:multistatus xmlns:x="a"><response/></x:y:multistatus>',
  '<multistatus><response><href>-0</href><status>+1.50</status><x>007</x><y>1e3</y><z>.5</z><w>-0x10</w><v> 1 </v><u>Infinity</u><t>12345678901234567890</t><s>false</s></response></multistatus>',
  '<multistatus><response><href>/r1/</href></response><response><href>/r2/</href></response><response><href>/r3/</href></response></multistatus>',
  '<multistatus><response>text<href>/mixed/</href>tail</response></multistatus>',
  '<multistatus><response><href>/t/</href><propstat><prop>  </prop><status>HTTP/1.1 200 OK</status></propstat></response></multistatus>',
  '<multistatus>\r\n  <response>\r\n    <href>/crlf/</href>\r\n  </response>\r\n</multistatus>\r\n',
  '<MULTISTATUS><response><href>/case/</href></response></MULTISTATUS>',
  '<multistatus><response><href><![CDATA[/cdata/ <x>]]></href></response></multistatus>',
  '<multistatus><response><href>/unclosed-attr/</href><x a="1></response></multistatus>',
  '<multistatus><response><href>/é/ünïcode/</href></response></multistatus>',
];

// Seeded random documents assembled from DAV fragments.
function rng(seed) { return () => { seed |= 0; seed = seed + 0x6D2B79F5 | 0; let t = Math.imul(seed ^ seed >>> 15, 1 | seed); t = t + Math.imul(t ^ t >>> 7, 61 | t) ^ t; return ((t ^ t >>> 14) >>> 0) / 4294967296; }; }
const FRAGMENTS = ['<d:response>', '</d:response>', '<d:href>/x/</d:href>', '<d:href>', '</d:href>', '<d:propstat>', '</d:propstat>', '<d:prop>', '</d:prop>',
  '<d:status>HTTP/1.1 200 OK</d:status>', '<d:status>HTTP/1.1 404 Not Found</d:status>', '<d:status>HTTP/1.1 403 Forbidden</d:status>', '<d:resourcetype><card:addressbook/></d:resourcetype>',
  '<d:resourcetype><cal:calendar/></d:resourcetype>', '<d:resourcetype/>', '<d:displayname>N</d:displayname>', '<d:displayname>7</d:displayname>', '<d:getetag>"e"</d:getetag>',
  `<card:address-data>${VCARD}</card:address-data>`, `<cal:calendar-data>${ICS}</cal:calendar-data>`, '<ic:calendar-color>#abcdef</ic:calendar-color>', ' ', '\n', 'text', '&amp;', '&#65;', '<![CDATA[c]]>', '<!-- c -->'];
const random = rng(20261022);
for (let n = 0; n < 300; n++) {
  let inner = '';
  const len = 1 + Math.floor(random() * 16);
  for (let k = 0; k < len; k++) inner += FRAGMENTS[Math.floor(random() * FRAGMENTS.length)];
  docs.push(ms(inner));
}
