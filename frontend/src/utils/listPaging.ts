/**
 * Whether another page of the message list exists. A short page means the end of what the
 * server holds locally, even when the folder's reported `total` (the mail server's count,
 * which can include messages not cached yet) is larger — otherwise "Load more" would stay
 * visible forever and fetch nothing.
 */
export function hasMorePages(received: number, limit: number, loaded: number, total: number): boolean {
  return received >= limit && loaded < total;
}

interface DatedRow { id?: unknown; date?: unknown }

const rowTime = (row: DatedRow) => {
  const value = new Date(String(row.date ?? '')).getTime();
  return Number.isNaN(value) ? null : value;
};

/**
 * The rows a live refresh keeps, so the bottom of an already-loaded list stays put: new mail
 * appears on top without pushing the oldest loaded row out (and back in once it is deleted).
 * The refresh fetches with headroom; this cuts the result at the previously oldest row, or at
 * its date when that row itself is gone.
 */
export function anchorRefreshedRows<Row extends DatedRow>(previous: readonly DatedRow[], fetched: readonly Row[]): Row[] {
  const oldest = previous.at(-1);
  if (!oldest) return [...fetched];
  const index = fetched.findIndex(row => row.id === oldest.id);
  if (index >= 0) return fetched.slice(0, index + 1);
  const cutoff = rowTime(oldest);
  if (cutoff == null) return fetched.slice(0, previous.length);
  return fetched.filter(row => (rowTime(row) ?? Infinity) >= cutoff);
}
