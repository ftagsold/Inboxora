/**
 * Whether another page of the message list exists. A short page means the end of what the
 * server holds locally, even when the folder's reported `total` (the mail server's count,
 * which can include messages not cached yet) is larger — otherwise "Load more" would stay
 * visible forever and fetch nothing.
 */
export function hasMorePages(received: number, limit: number, loaded: number, total: number): boolean {
  return received >= limit && loaded < total;
}
