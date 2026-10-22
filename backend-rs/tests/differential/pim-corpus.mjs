// vCard inputs for differential tests.
const C = (...lines) => lines.join('\r\n') + '\r\n';
export const vcards = [
  C('BEGIN:VCARD', 'VERSION:3.0', 'UID:abc-1', 'FN:Jane Doe', 'N:Doe;Jane;;;', 'EMAIL;TYPE=INTERNET,WORK:Jane@Example.com', 'EMAIL;TYPE=HOME;PREF=1:jane@home.test', 'TEL;TYPE=CELL:+1 555 0100', 'ORG:ACME\\, Inc;Dept', 'NOTE:Line1\\nLine2\; semi', 'END:VCARD'),
  C('BEGIN:VCARD', 'VERSION:3.0', 'FN:Apple', 'item1.EMAIL;type=INTERNET;type=pref:a@x.test', 'item1.X-ABLabel:_$!<Other>!$_', 'item2.X-ABDATE;type=pref:2010-06-12', 'item2.X-ABLabel:Wedding', 'BDAY:1990-02-28', 'ANNIVERSARY:20100612', 'END:VCARD'),
  C('BEGIN:VCARD', 'VERSION:3.0', 'FN:Android', 'X-ANDROID-CUSTOM:vnd.android.cursor.item/contact_event;2001-01-01;0;Graduation;;;;;;;;;;;;', 'X-ANDROID-CUSTOM:vnd.android.cursor.item/contact_event;;0;Launch;2002-02-02;;;', 'BDAY:--03-15', 'END:VCARD'),
  C('BEGIN:VCARD', 'VERSION:3.0', 'FN:Bad dates', 'BDAY:2023-02-30', 'X-ABDATE;TYPE="Unterminated:2020-01-01', 'X-ABDATE;TYPE="Fine";X=1:2020-01-02', 'X-ABDATE;LABEL=Custom\\, label:2020-01-03', 'END:VCARD'),
  C('BEGIN:VCARD', 'VERSION:4.0', 'FN:Rich', 'TITLE:CEO', 'ROLE:Boss', 'NICKNAME:Bossy', 'URL;TYPE=WORK:https://acme.test', 'URL:javascript:alert(1)', 'IMPP:xmpp:me@jabber.test', 'IMPP;TYPE=SKYPE:live:me', 'CATEGORIES:Friends,Work\\,Stuff,,', 'ADR;TYPE=HOME:;;1 Main St;Springfield;IL;62704;USA', 'END:VCARD'),
  C('BEGIN:VCARD', 'VERSION:3.0', 'FN:Photo', 'PHOTO;ENCODING=b;TYPE=PNG:iVBORw0KGgo=', 'END:VCARD'),
  C('BEGIN:VCARD', 'VERSION:3.0', 'FN:Photo2', 'PHOTO:data:image/svg+xml;base64,PHN2Zz4=', 'END:VCARD'),
  C('BEGIN:VCARD', 'VERSION:3.0', 'FN:Photo3', 'PHOTO;VALUE=uri:https://x.test/p.jpg', 'END:VCARD'),
  'BEGIN:VCARD\nVERSION:3.0\nFN:Folded long name that goes on and on and on and on and on and on and on and on and on\n  continued\nEMAIL:a@b.test\nTEL:tel:+15550101\nEND:VCARD\n',
  C('BEGIN:VCARD', 'FN:No version', 'EMAIL;PREF:pref@x.test', 'EMAIL:other@x.test', 'END:VCARD'),
  C('BEGIN:VCARD', 'FN:Pref rank', 'EMAIL;PREF=5:five@x.test', 'EMAIL;PREF=2:two@x.test', 'EMAIL;TYPE=pref:typepref@x.test', 'END:VCARD'),
  C('BEGIN:VCARD', 'VERSION:3.0', 'FN;CHARSET=UTF-8:Jürgen Müller 😀', 'N:Müller;Jürgen;;;', 'X-SOCIALPROFILE;type=twitter:https://twitter.com/x', 'X-CUSTOM:keep me', 'END:VCARD'),
  'garbage without vcard markers',
  '',
  C('BEGIN:VCARD', 'VERSION:3.0', 'FN:Quoted', 'X-ABDATE;TYPE="Wed;ding":2020-05-05', 'EMAIL;TYPE="WORK":q@x.test', 'END:VCARD'),
];
export const contacts = [
  { uid: 'u1', displayName: 'Jane Doe', firstName: 'Jane', lastName: 'Doe', emails: [{ value: 'jane@x.test', type: 'work', primary: true }, { value: 'j2@x.test' }], phones: [{ value: '+1 555', type: 'cell' }, { value: '123' }], organization: 'ACME, Inc', notes: 'a;b\nc' },
  { firstName: 'Only' },
  { emails: [{ value: 'fallback@x.test' }] },
  { uid: 'u2', displayName: 'Dates', contactDates: [{ label: 'Birthday', value: '1990-01-01' }, { label: 'birthday', value: '1990-01-01' }, { label: 'Wedding; party', value: '20200505' }, { label: 'Bad', value: '2020-13-01' }, { label: 'Partial', value: '--05-06' }] },
  { uid: 'u3', birthday: '1991-02-03', anniversary: 'bad', title: 'CEO', role: 'r', nickname: 'n', urls: [{ value: 'https://x.test', type: 'home' }, { value: '' }], instantMessages: [{ value: 'xmpp:a@b' }], categories: ['A', 'B,C'], addresses: [{ type: 'work', street: '1 Main;St', locality: 'Town' }] },
  { uid: 'long', displayName: 'Ä'.repeat(60), notes: 'x'.repeat(200) },
];
export const merges = [
  [vcards[0], { displayName: 'Jane Q Doe', emails: [{ value: 'new@x.test', type: 'home' }] }],
  [vcards[11], { uid: 'keep', displayName: 'Changed', title: 'New title' }],
  [vcards[1], { displayName: 'Apple', contactDates: [{ label: 'Anniversary', value: '2011-01-01' }] }],
  ['not a vcard', { displayName: 'Fresh' }],
];
