// /api/contacts
const ALICE = '00000000-0000-4000-8000-0000000000a1';
const BOB = '00000000-0000-4000-8000-0000000000b0';
const BOOK = '00000000-0000-4000-8000-00000000b001';
const DAV_BOOK = '00000000-0000-4000-8000-00000000b002';
const HIDDEN_BOOK = '00000000-0000-4000-8000-00000000b003';
const DAV_CONTACT = '00000000-0000-4000-8000-00000000c002';
const PHOTO_PNG = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==';

async function seed({ sql }) {
  await sql(`INSERT INTO users (id, username, password_hash) VALUES ($1, 'alice@example.test', 'x'), ($2, 'bob@example.test', 'x') ON CONFLICT DO NOTHING`, [ALICE, BOB]);
  await sql(`DELETE FROM address_books WHERE user_id IN ($1, $2)`, [ALICE, BOB]);
  await sql(`INSERT INTO address_books (id, user_id, name, source, visible, created_at) VALUES ($1, $4, 'Personal', 'local', true, '2026-01-01'), ($2, $4, 'Work DAV', 'carddav', true, '2026-01-02'), ($3, $4, 'Hidden', 'local', false, '2026-01-03')`, [BOOK, DAV_BOOK, HIDDEN_BOOK, ALICE]);
  await sql(`INSERT INTO contacts (id, address_book_id, user_id, uid, vcard, etag, display_name, primary_email, emails, photo_data, is_auto, send_count, birthday) VALUES
    ($1, $2, $3, 'dav-1', 'BEGIN:VCARD\r\nVERSION:3.0\r\nUID:dav-1\r\nFN:Dav Person\r\nTITLE:From vCard\r\nURL:https://dav.test\r\nCATEGORIES:A,B\r\nEND:VCARD\r\n', 'e1', 'Dav Person', 'dav@x.test', '[{"value":"dav@x.test","type":"work"}]', 'data:image/png;base64,${PHOTO_PNG}', false, 3, '1980-05-06'),
    (gen_random_uuid(), $4, $3, 'hidden-1', NULL, 'e2', 'Hidden Person', 'hidden@x.test', '[{"value":"hidden@x.test"}]', NULL, false, 0, NULL),
    (gen_random_uuid(), $5, $3, 'auto-1', NULL, 'e3', NULL, 'auto@x.test', '[{"value":"auto@x.test"}]', 'data:image/svg+xml;base64,PHN2Zz4=', true, 9, NULL)`, [DAV_CONTACT, DAV_BOOK, ALICE, HIDDEN_BOOK, BOOK]);
  return { keep: [ALICE, BOB, BOOK, DAV_BOOK, HIDDEN_BOOK, DAV_CONTACT] };
}
const sessions = () => ({ alice: { userId: ALICE }, bob: { userId: BOB } });
const noEtag = (prefix) => [`${prefix}etag`];

export default [
  {
    name: 'address books: list, create, rename, hide, delete, validation',
    seed, sessions, ignoreKeys: ['etag'],
    steps: [
      { path: '/api/contacts/address-books', as: 'alice' },
      { method: 'POST', path: '/api/contacts/address-books', as: 'alice', body: { name: '  Family   Book ' }, capture: (j) => ({ fam: j.id }) },
      { method: 'POST', path: '/api/contacts/address-books', as: 'alice', body: { name: 'Family Book' } },
      { method: 'POST', path: '/api/contacts/address-books', as: 'alice', body: { name: '' } },
      { method: 'POST', path: '/api/contacts/address-books', as: 'alice', body: { name: 'x'.repeat(121) } },
      (c) => ({ method: 'PATCH', path: `/api/contacts/address-books/${c.fam}`, as: 'alice', body: { name: 'Kin', visible: false } }),
      (c) => ({ method: 'PATCH', path: `/api/contacts/address-books/${c.fam}`, as: 'alice', body: {} }),
      (c) => ({ method: 'PATCH', path: `/api/contacts/address-books/${c.fam}`, as: 'alice', body: { visible: 'yes' } }),
      (c) => ({ method: 'PATCH', path: `/api/contacts/address-books/${c.fam}`, as: 'alice', body: { name: 'Personal' } }),
      { method: 'PATCH', path: `/api/contacts/address-books/${DAV_BOOK}`, as: 'alice', body: { name: 'x' } },
      { method: 'PATCH', path: `/api/contacts/address-books/${BOOK}`, as: 'bob', body: { name: 'x' } },
      (c) => ({ method: 'DELETE', path: `/api/contacts/address-books/${c.fam}`, as: 'alice' }),
      { method: 'DELETE', path: `/api/contacts/address-books/${HIDDEN_BOOK}`, as: 'alice' },
      { method: 'DELETE', path: `/api/contacts/address-books/${BOOK}`, as: 'alice' },
      { path: '/api/contacts/address-books', as: 'alice' },
    ],
    after: async ({ sql }) => (await sql(`SELECT name, source, visible FROM address_books WHERE user_id = $1 ORDER BY name`, [ALICE])).rows,
  },
  {
    name: 'contacts: create, list, filter, get, patch, delete',
    seed, sessions, ignoreKeys: ['etag'],
    steps: [
      { method: 'POST', path: '/api/contacts', as: 'alice', body: { displayName: 'Jane Doe', firstName: 'Jane', lastName: 'Doe', emails: [{ value: ' Jane@Example.TEST ', type: 'work', primary: true }], phones: [{ value: '+1 555', type: 'cell' }], organization: 'ACME, Inc', notes: 'a;b\nc', birthday: '1990-02-28', title: ' CEO ', urls: [{ value: 'https://jane.test', type: 'home' }], categories: ['Friends'], addresses: [{ type: 'home', street: '1 Main', locality: 'Town' }] }, ignore: ['etag'], capture: (j) => ({ jane: j.id }) },
      { method: 'POST', path: '/api/contacts', as: 'alice', body: { displayName: 'Dup', emails: [{ value: 'jane@example.test' }] } },
      { method: 'POST', path: '/api/contacts', as: 'alice', body: { emails: null } },
      { method: 'POST', path: '/api/contacts', as: 'alice', body: { phones: 'x' } },
      { method: 'POST', path: '/api/contacts', as: 'alice', body: { firstName: 'Nobody' } },
      { method: 'POST', path: '/api/contacts', as: 'alice', body: { displayName: 'Bad date', birthday: '1990-02-30' } },
      { method: 'POST', path: '/api/contacts', as: 'alice', body: { displayName: 'Bad rich', urls: [{ value: 'ftp://x' }] } },
      { method: 'POST', path: '/api/contacts', as: 'alice', body: { displayName: 'Bad dates', contactDates: [{ label: 'x"y', value: '2020-01-01' }] } },
      { method: 'POST', path: '/api/contacts', as: 'alice', body: { displayName: 'Dated', contactDates: [{ label: 'Birthday', value: '2001-01-01' }, { label: 'Wedding', value: '--06-07' }, { label: 'birthday', value: '2001-01-01' }] }, ignore: ['etag'], capture: (j) => ({ dated: j.id }) },
      { method: 'POST', path: '/api/contacts', as: 'alice', body: { displayName: 'Into DAV', addressBookId: DAV_BOOK } },
      { method: 'POST', path: '/api/contacts', as: 'alice', body: { displayName: 'Into hidden', addressBookId: HIDDEN_BOOK, emails: [{ value: 'h2@x.test' }] }, ignore: ['etag'] },
      { path: '/api/contacts', as: 'alice', ignore: ['contacts.*.etag'] },
      { path: '/api/contacts?q=jane&limit=5', as: 'alice', ignore: ['contacts.*.etag'] },
      { path: '/api/contacts?q=555', as: 'alice', ignore: ['contacts.*.etag'] },
      { path: '/api/contacts?is_auto=true', as: 'alice', ignore: ['contacts.*.etag'] },
      { path: `/api/contacts?addressBookId=${HIDDEN_BOOK}`, as: 'alice', ignore: ['contacts.*.etag'] },
      { path: '/api/contacts?limit=1&offset=1&is_auto=false', as: 'alice', ignore: ['contacts.*.etag'] },
      { path: '/api/contacts?limit=-1', as: 'alice' },
      { path: '/api/contacts', as: 'bob' },
      (c) => ({ path: `/api/contacts/${c.jane}`, as: 'alice', ignore: ['etag'] }),
      { path: `/api/contacts/${DAV_CONTACT}`, as: 'alice' },
      { path: `/api/contacts/${DAV_CONTACT}`, as: 'bob' },
      { path: '/api/contacts/not-a-uuid', as: 'alice' },
      (c) => ({ method: 'PATCH', path: `/api/contacts/${c.jane}`, as: 'alice', body: { displayName: 'Jane Q Doe', emails: [{ value: 'NEW@x.test' }], nickname: 'JQ', birthday: '' }, ignore: ['etag'] }),
      (c) => ({ method: 'PATCH', path: `/api/contacts/${c.jane}`, as: 'alice', body: { contactDates: [{ label: 'Anniversary', value: '2010-10-10' }] }, ignore: ['etag'] }),
      (c) => ({ method: 'PATCH', path: `/api/contacts/${c.dated}`, as: 'alice', body: { notes: 'updated' }, ignore: ['etag'] }),
      (c) => ({ method: 'PATCH', path: `/api/contacts/${c.jane}`, as: 'alice', body: { emails: 'x' } }),
      (c) => ({ method: 'PATCH', path: `/api/contacts/${c.jane}`, as: 'alice', body: { urls: [{ value: 'nope' }] } }),
      { method: 'PATCH', path: `/api/contacts/${DAV_CONTACT}`, as: 'alice', body: { displayName: 'x' } },
      (c) => ({ path: `/api/contacts/${c.jane}`, as: 'alice', ignore: ['etag'] }),
      { method: 'DELETE', path: `/api/contacts/${DAV_CONTACT}`, as: 'alice' },
      (c) => ({ method: 'DELETE', path: `/api/contacts/${c.dated}`, as: 'bob' }),
      (c) => ({ method: 'DELETE', path: `/api/contacts/${c.dated}`, as: 'alice' }),
      (c) => ({ method: 'DELETE', path: `/api/contacts/${c.dated}`, as: 'alice' }),
    ],
    after: async ({ sql }) => (await sql(`SELECT display_name, primary_email, emails, phones, birthday::text, anniversary::text, contact_dates, title, role, nickname, urls, addresses, categories, is_auto, regexp_replace(vcard, 'UID:[^\r]*', 'UID:x') AS vcard FROM contacts WHERE user_id = $1 ORDER BY display_name NULLS LAST, primary_email`, [ALICE])).rows,
  },
  {
    name: 'photo, export and Google CSV import',
    seed, sessions, ignoreKeys: ['etag'],
    steps: [
      { path: '/api/contacts/photo?email=DAV@x.test', as: 'alice', compareBytes: true, compareHeaders: ['content-type', 'cache-control', 'content-security-policy'] },
      { path: '/api/contacts/photo?email=auto@x.test', as: 'alice' },
      { path: '/api/contacts/photo?email=nobody@x.test', as: 'alice' },
      { path: '/api/contacts/photo', as: 'alice' },
      { path: '/api/contacts/gravatar?email=not-an-email', as: 'alice' },
      { path: `/api/contacts/address-books/${DAV_BOOK}/export?format=vcard`, as: 'alice' },
      { path: `/api/contacts/address-books/${DAV_BOOK}/export?format=google-csv`, as: 'alice' },
      { path: `/api/contacts/address-books/${DAV_BOOK}/export?format=outlook-csv`, as: 'alice' },
      { path: `/api/contacts/address-books/${DAV_BOOK}/export?format=pdf`, as: 'alice' },
      { path: `/api/contacts/address-books/${DAV_BOOK}/export?format=vcard`, as: 'bob' },
      { method: 'POST', path: `/api/contacts/address-books/${BOOK}/import/google-csv`, as: 'alice', body: { csv: 'Name,E-mail 1 - Value,Phone 1 - Type,Phone 1 - Value,Birthday,Labels\r\nImported One,ONE@x.test,Mobile,123,1999-09-09,A ::: B\r\nImported Two,two@x.test,,,,\r\n' } },
      { method: 'POST', path: `/api/contacts/address-books/${BOOK}/import/google-csv`, as: 'alice', body: { csv: 'Name,E-mail 1 - Value\r\nImported One Again,one@x.test\r\n' } },
      { method: 'POST', path: `/api/contacts/address-books/${BOOK}/import/google-csv`, as: 'alice', body: { csv: '' } },
      { method: 'POST', path: `/api/contacts/address-books/${BOOK}/import/google-csv`, as: 'alice', body: { csv: 'Name\r\n' } },
      { method: 'POST', path: `/api/contacts/address-books/${DAV_BOOK}/import/google-csv`, as: 'alice', body: { csv: 'Name\r\nX\r\n' } },
    ],
    after: async ({ sql }) => (await sql(`SELECT display_name, primary_email, phones, birthday::text, contact_dates, categories, google_fields FROM contacts WHERE address_book_id = $1 ORDER BY display_name NULLS LAST`, [BOOK])).rows,
  },
];
