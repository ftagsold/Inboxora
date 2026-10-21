import { useEffect, useState } from 'react';

/**
 * `true` only once `flag` has stayed true for `delayMs` without interruption, and `false`
 * as soon as it turns false. Used for progress notices, so a request that finishes quickly
 * never flashes one.
 */
export function useDelayedFlag(flag: boolean, delayMs = 500): boolean {
  const [shown, setShown] = useState(false);
  useEffect(() => {
    if (!flag) { setShown(false); return undefined; }
    const timer = setTimeout(() => setShown(true), delayMs);
    return () => clearTimeout(timer);
  }, [flag, delayMs]);
  return flag && shown;
}
