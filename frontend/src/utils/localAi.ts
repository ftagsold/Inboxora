// Opt-in in-browser AI: a small model (Transformers.js on ONNX Runtime WebGPU) that runs on the
// user's own machine. Nothing is downloaded until the user enables it in settings; the weights
// then live in the browser's cache. While active, api.ai.chat routes here instead of the
// server's /ai/chat proxy, so summarize, AI actions and the compose assistant all work without
// a configured provider. ONNX Runtime needs only WebGPU's default limits, so it runs in every
// browser that ships WebGPU (Chrome, Edge, Safari, Firefox).

export interface LocalAiModel {
  id: string;
  label: string;
  /** Approximate download (q4f16 weights). */
  sizeGB: number;
}

export const LOCAL_AI_MODELS: readonly LocalAiModel[] = [
  { id: 'onnx-community/gemma-4-E2B-it-ONNX', label: 'Gemma 4 E2B', sizeGB: 3.1 },
  { id: 'onnx-community/gemma-4-E4B-it-ONNX', label: 'Gemma 4 E4B', sizeGB: 4.9 },
  { id: 'onnx-community/gemma-3-1b-it-ONNX', label: 'Gemma 3 1B', sizeGB: 0.8 },
];

export const DEFAULT_LOCAL_AI_MODEL = LOCAL_AI_MODELS[0].id;

// Keeps prompt and KV cache within what a browser GPU budget handles comfortably.
const MAX_INPUT_CHARS = 16000;
const MAX_NEW_TOKENS = 1024;

export function isKnownLocalAiModel(id: unknown): id is string {
  return typeof id === 'string' && LOCAL_AI_MODELS.some(m => m.id === id);
}

// ---- Device support ----------------------------------------------------------------------

export type LocalAiSupport = 'checking' | 'supported' | 'no-webgpu' | 'no-gpu';

interface GpuAdapterLike { features: Set<string> }
type GpuNavigator = Navigator & { gpu?: { requestAdapter: (o?: { powerPreference?: string }) => Promise<GpuAdapterLike | null> } };

let support: LocalAiSupport = 'checking';
let supportCheck: Promise<LocalAiSupport> | null = null;
let hasF16 = false;
const supportListeners = new Set<() => void>();

/**
 * Probe once, in the background, whether this device can run the local model: WebGPU (secure
 * contexts of supporting browsers only) and a GPU adapter. Nothing is downloaded.
 */
export function checkLocalAiSupport(): Promise<LocalAiSupport> {
  supportCheck ??= (async (): Promise<LocalAiSupport> => {
    const gpu = typeof navigator === 'undefined' ? undefined : (navigator as GpuNavigator).gpu;
    if (!gpu || globalThis.isSecureContext === false) return 'no-webgpu';
    try {
      const adapter = await gpu.requestAdapter({ powerPreference: 'high-performance' });
      if (!adapter) return 'no-gpu';
      hasF16 = adapter.features.has('shader-f16');
      return 'supported';
    } catch {
      return 'no-gpu';
    }
  })().then(result => {
    support = result;
    for (const fn of supportListeners) fn();
    return result;
  });
  return supportCheck;
}

export function getLocalAiSupport(): LocalAiSupport {
  return support;
}

export function subscribeLocalAiSupport(fn: () => void): () => void {
  supportListeners.add(fn);
  return () => { supportListeners.delete(fn); };
}

/** True only once the background probe found this device able to run the model. */
export function localAiSupported(): boolean {
  return support === 'supported';
}

/** 16-bit weights where the GPU supports shader-f16, else the 32-bit-activation q4 build. */
function dtypeFor(): Dtype {
  return hasF16 ? 'q4f16' : 'q4';
}

// ---- Load state ---------------------------------------------------------------------------

export type LocalAiState =
  | { status: 'idle' }
  | { status: 'loading'; model: string; progress: number }
  | { status: 'ready'; model: string }
  | { status: 'error'; model: string; error: string };

let state: LocalAiState = { status: 'idle' };
const listeners = new Set<(s: LocalAiState) => void>();

function setState(next: LocalAiState) {
  state = next;
  for (const fn of listeners) fn(state);
}

export function getLocalAiState(): LocalAiState {
  return state;
}

export function subscribeLocalAi(fn: (s: LocalAiState) => void): () => void {
  listeners.add(fn);
  return () => { listeners.delete(fn); };
}

// The model chosen in settings while local AI is on; null routes AI to the server.
// It takes effect only on a device the probe found capable.
let activeModel: string | null = null;

export function setLocalAiModel(model: string | null) {
  activeModel = model;
}

export function localAiActive(): boolean {
  return activeModel !== null && localAiSupported();
}

// ---- Worker protocol ----------------------------------------------------------------------

type Dtype = 'q4f16' | 'q4';
export interface LocalChatMessage { role: 'user' | 'assistant'; content: string }
export type LocalAiRequest =
  | { type: 'load'; id: number; model: string; dtype: Dtype }
  | { type: 'generate'; id: number; model: string; dtype: Dtype; messages: LocalChatMessage[]; maxNewTokens: number }
  | { type: 'interrupt' }
  | { type: 'unload'; id: number }
  | { type: 'cached'; id: number; model: string; dtype: Dtype }
  | { type: 'delete'; id: number; model: string; dtype: Dtype };
export type LocalAiResponse =
  | { type: 'progress'; progress: number }
  | { type: 'ready'; id: number }
  | { type: 'error'; id: number; error: string }
  | { type: 'delta'; id: number; text: string }
  | { type: 'done'; id: number; text: string; interrupted: boolean }
  | { type: 'cached'; id: number; cached: boolean };
type IdRequest = Extract<LocalAiRequest, { id: number }>;
type RequestBody = IdRequest extends infer R ? R extends IdRequest ? Omit<R, 'id'> : never : never;

interface Pending {
  resolve: (msg: LocalAiResponse) => void;
  reject: (error: Error) => void;
  onDelta?: (text: string) => void;
  aborted?: boolean;
}

let worker: Worker | null = null;
let seq = 0;
const pending = new Map<number, Pending>();

function getWorker(): Worker {
  if (worker) return worker;
  worker = new Worker(new URL('./localAi.worker.ts', import.meta.url), { type: 'module' });
  worker.onmessage = (event: MessageEvent<LocalAiResponse>) => {
    const msg = event.data;
    if (msg.type === 'progress') {
      if (state.status === 'loading') setState({ ...state, progress: msg.progress });
      return;
    }
    const entry = pending.get(msg.id);
    if (!entry) return;
    if (msg.type === 'delta') {
      // A request aborted before its generation began: stop it as soon as it runs.
      if (entry.aborted) worker?.postMessage({ type: 'interrupt' } satisfies LocalAiRequest);
      else entry.onDelta?.(msg.text);
      return;
    }
    pending.delete(msg.id);
    if (msg.type === 'error') entry.reject(new Error(msg.error));
    else entry.resolve(msg);
  };
  worker.onerror = (event) => {
    const error = new Error(event.message || 'Local AI worker failed');
    for (const entry of pending.values()) entry.reject(error);
    pending.clear();
    worker?.terminate();
    worker = null;
    loading = null;
  };
  return worker;
}

function request(msg: RequestBody, extra: Partial<Pending> = {}): { id: number; done: Promise<LocalAiResponse> } {
  const id = ++seq;
  const done = new Promise<LocalAiResponse>((resolve, reject) => {
    pending.set(id, { resolve, reject, ...extra });
  });
  getWorker().postMessage({ ...msg, id } as LocalAiRequest);
  return { id, done };
}

// ---- Public operations --------------------------------------------------------------------

let loading: { model: string; promise: Promise<void> } | null = null;

/** Download (first time) and load `model` into the GPU; later calls reuse it. */
export async function loadLocalAi(model: string): Promise<void> {
  if (await checkLocalAiSupport() !== 'supported') throw new Error('This device cannot run local AI models');
  if (loading?.model === model) return loading.promise;
  setState({ status: 'loading', model, progress: 0 });
  const promise: Promise<void> = request({ type: 'load', model, dtype: dtypeFor() }).done.then(() => {
    if (loading?.promise === promise) setState({ status: 'ready', model });
  }, (error: Error) => {
    if (loading?.promise === promise) {
      loading = null;
      setState({ status: 'error', model, error: error.message });
    }
    throw error;
  });
  loading = { model, promise };
  return promise;
}

/** Release the GPU memory; the downloaded weights stay cached. */
export async function unloadLocalAi() {
  loading = null;
  setState({ status: 'idle' });
  if (worker) await request({ type: 'unload' }).done.catch(() => {});
}

/** Whether Transformers.js ever stored anything here; checked first so the worker starts only when needed. */
async function hasModelCache(): Promise<boolean> {
  try {
    return await caches.has('transformers-cache');
  } catch {
    return false;
  }
}

export async function isLocalAiModelCached(model: string): Promise<boolean> {
  if (!await hasModelCache() || await checkLocalAiSupport() !== 'supported') return false;
  const reply = await request({ type: 'cached', model, dtype: dtypeFor() }).done.catch(() => null);
  return reply?.type === 'cached' && reply.cached;
}

/** Remove the downloaded weights of `model` from the browser cache. */
export async function deleteLocalAiModel(model: string) {
  if ('model' in state && state.model === model) await unloadLocalAi();
  await request({ type: 'delete', model, dtype: dtypeFor() }).done.catch(() => {});
}

/**
 * Fold system prompts into the first user turn (some chat templates reject a system role) and
 * cap the input to fit the model's budget.
 */
export function toLocalMessages(messages: unknown[]): LocalChatMessage[] {
  const list = messages.filter((m): m is { role: string; content: string } =>
    typeof m === 'object' && m !== null && typeof (m as { content?: unknown }).content === 'string');
  const system = list.filter(m => m.role === 'system').map(m => m.content).join('\n\n');
  const turns = list.filter(m => m.role !== 'system')
    .map(m => ({ role: m.role === 'assistant' ? 'assistant' as const : 'user' as const, content: m.content }));
  if (!turns.length) turns.push({ role: 'user', content: '' });
  const first = turns.findIndex(m => m.role === 'user');
  if (system && first >= 0) turns[first] = { ...turns[first], content: `${system}\n\n${turns[first].content}` };
  const last = turns[turns.length - 1];
  if (last.content.length > MAX_INPUT_CHARS) turns[turns.length - 1] = { ...last, content: last.content.slice(0, MAX_INPUT_CHARS) };
  return turns;
}

/** Drop reasoning blocks some small models emit before the answer. */
export function stripThinking(text: string): string {
  return text.replace(/<think>[\s\S]*?(?:<\/think>|$)/g, '').replace(/^\s+/, '');
}

/** Same contract as streamAiChat: resolves with the full text, reports progress via onDelta. */
export async function streamLocalAiChat(messages: unknown[], { signal, onDelta }: { signal?: AbortSignal; onDelta?: (fullText: string, delta: string) => void } = {}): Promise<string> {
  const model = localAiActive() ? activeModel : null;
  if (!model) throw new Error('Local AI is not enabled');
  const abortError = () => new DOMException('Aborted', 'AbortError');
  if (signal?.aborted) throw abortError();
  await loadLocalAi(model);
  if (signal?.aborted) throw abortError();
  let shown = '';
  const job = request({
    type: 'generate', model, dtype: dtypeFor(), messages: toLocalMessages(messages), maxNewTokens: MAX_NEW_TOKENS,
  }, {
    onDelta: (raw: string) => {
      const next = stripThinking(raw);
      if (next === shown) return;
      const added = next.startsWith(shown) ? next.slice(shown.length) : next;
      shown = next;
      onDelta?.(shown, added);
    },
  });
  const stop = () => {
    const entry = pending.get(job.id);
    if (entry) entry.aborted = true;
    worker?.postMessage({ type: 'interrupt' } satisfies LocalAiRequest);
  };
  signal?.addEventListener('abort', stop, { once: true });
  try {
    const reply = await job.done;
    if (signal?.aborted) throw abortError();
    return reply.type === 'done' ? stripThinking(reply.text) : shown;
  } finally {
    signal?.removeEventListener('abort', stop);
  }
}
