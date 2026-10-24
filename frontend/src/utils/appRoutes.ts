// URL ⇄ view mapping for the signed-in app. The store stays the source of truth; these paths
// only mirror it so a view can be bookmarked, reloaded, or reached with browser Back/Forward.
//   /mail/<accountId>/<folder>   one account's folder (folder path is a single encoded segment)
//   /mail/all/<folder>           the unified inbox (no selected account)
//   /calendar, /contacts, /files

export type AppView =
  | { kind: 'mail'; accountId: string | null; folder: string }
  | { kind: 'calendar' }
  | { kind: 'contacts' }
  | { kind: 'files' };

export interface ViewState {
  showCalendar: boolean;
  showContacts: boolean;
  showFiles?: boolean;
  selectedAccountId: string | null;
  selectedFolder: string;
}

const ALL_ACCOUNTS = 'all';

export function viewFromState(state: ViewState): AppView {
  if (state.showCalendar) return { kind: 'calendar' };
  if (state.showContacts) return { kind: 'contacts' };
  if (state.showFiles) return { kind: 'files' };
  return { kind: 'mail', accountId: state.selectedAccountId, folder: state.selectedFolder || 'INBOX' };
}

export function pathForView(view: AppView): string {
  if (view.kind !== 'mail') return `/${view.kind}`;
  return `/mail/${encodeURIComponent(view.accountId ?? ALL_ACCOUNTS)}/${encodeURIComponent(view.folder)}`;
}

/** The view a pathname names, or null for any other path (e.g. "/"). */
export function viewFromPath(pathname: string): AppView | null {
  let segments: string[];
  try {
    segments = pathname.split('/').filter(Boolean).map(decodeURIComponent);
  } catch {
    return null; // malformed percent-encoding
  }
  const [root, account, folder, ...rest] = segments;
  if (segments.length === 1 && (root === 'calendar' || root === 'contacts' || root === 'files')) return { kind: root };
  if (root !== 'mail' || !account || rest.length) return null;
  return { kind: 'mail', accountId: account === ALL_ACCOUNTS ? null : account, folder: folder || 'INBOX' };
}
