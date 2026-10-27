import { useEffect, useSyncExternalStore } from 'react';
import { checkLocalAiSupport, getLocalAiState, getLocalAiSupport, subscribeLocalAi, subscribeLocalAiSupport } from '../utils/localAi.ts';

/** The in-browser model's download / load state. */
export function useLocalAiState() {
  return useSyncExternalStore(subscribeLocalAi, getLocalAiState, getLocalAiState);
}

/** Whether this device can run the local model; starts the background probe on first use. */
export function useLocalAiSupport() {
  useEffect(() => { void checkLocalAiSupport(); }, []);
  return useSyncExternalStore(subscribeLocalAiSupport, getLocalAiSupport, getLocalAiSupport);
}
