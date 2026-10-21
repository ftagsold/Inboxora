import { useStore } from '../store/index.ts';

// When the open message leaves the list (moved, archived, deleted, marked spam…), nothing is
// selected afterwards: the reading pane closes instead of jumping to a neighbouring message the
// user did not choose. Store-only (no component state), so core row-actions and plugins (e.g. GTD
// "done") share it.
// selectedWithinRemovedRow: when true, clear even if the selected id isn't exactly removedId —
// used by threaded archive, where the open message may be a child of the removed thread head
// (a different id) but still leaves the list.
export function clearSelectionAfterRemoval<T extends [] | [selectedWithinRemovedRow: boolean]>(
  removedId: string,
  ...args: T
): void {
  const { selectedMessageId, setSelectedMessage } = useStore.getState();
  const selectedWithinRemovedRow = args.length === 1 && args[0];
  if (!selectedWithinRemovedRow && selectedMessageId !== removedId) return;
  if (selectedMessageId !== null) setSelectedMessage(null);
}
