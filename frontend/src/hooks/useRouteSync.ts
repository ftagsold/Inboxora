import { useEffect, useRef } from 'react';
import { useStore } from '../store/index.ts';
import { pathForView, viewFromPath, viewFromState } from '../utils/appRoutes.ts';

const currentPath = () => pathForView(viewFromState(useStore.getState()));

/** Open the view a URL names. Returns false when the path names none (or an unknown account). */
function applyPath(pathname: string): boolean {
  const target = viewFromPath(pathname);
  if (!target) return false;
  const state = useStore.getState();
  if (pathForView(target) === currentPath()) return true;
  if (target.kind === 'calendar') state.setShowCalendar(true);
  else if (target.kind === 'contacts') state.setShowContacts(true);
  else {
    if (target.accountId && state.accountsReady && !state.accounts.some(account => account.id === target.accountId)) return false;
    state.setSelectedAccount(target.accountId, target.folder);
  }
  return true;
}

/**
 * Mirror the selected account folder / calendar / contacts view in the URL.
 *
 * Desktop: each navigation is its own history entry, and Back/Forward restore the view.
 * Mobile: Back is owned by useBackNavigation (it closes calendar, contacts and overlays through
 * its own history entries), so the URL is only updated in place and popstate is left to it.
 */
export function useRouteSync(isMobile: boolean) {
  const path = useStore(state => pathForView(viewFromState(state)));
  const accountsReady = useStore(state => state.accountsReady);
  const mobile = useRef(isMobile);
  mobile.current = isMobile;
  const initialized = useRef(false);

  // URL → store: a deep link on load, then desktop Back/Forward.
  useEffect(() => {
    applyPath(window.location.pathname);
    const onPop = () => { if (!mobile.current) applyPath(window.location.pathname); };
    window.addEventListener('popstate', onPop);
    return () => window.removeEventListener('popstate', onPop);
  }, []);

  // A deep link to an account that only becomes known once accounts load.
  useEffect(() => {
    if (accountsReady) applyPath(window.location.pathname);
  }, [accountsReady]);

  // Store → URL. Read the live state, not `path`: the URL → store effect above may have
  // changed it during this same commit.
  useEffect(() => {
    const next = currentPath();
    const first = !initialized.current;
    initialized.current = true;
    if (window.location.pathname === next) return;
    if (first || mobile.current || !useStore.getState().accountsReady) {
      // Keep the query: on load, mount effects still read ?m=, ?mailto= and OAuth results.
      window.history.replaceState(window.history.state, '', next + window.location.search + window.location.hash);
    } else {
      window.history.pushState(null, '', next);
    }
  }, [path]);
}
