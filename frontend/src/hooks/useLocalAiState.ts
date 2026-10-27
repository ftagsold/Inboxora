import { useSyncExternalStore } from 'react';
import { getLocalAiState, subscribeLocalAi } from '../utils/localAi.ts';

/** The in-browser model's download / load state. */
export function useLocalAiState() {
  return useSyncExternalStore(subscribeLocalAi, getLocalAiState, getLocalAiState);
}
