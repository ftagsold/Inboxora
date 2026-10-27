// Opt-in in-browser AI: a small model (WebLLM on WebGPU) that runs on the user's own machine.
// Nothing is downloaded until the user enables it in settings; the weights then live in the
// browser's cache. While active, api.ai.chat routes here instead of the server's /ai/chat proxy,
// so summarize, AI actions and the compose assistant all work without a configured provider.
import type { WebWorkerMLCEngine, InitProgressReport } from '@mlc-ai/web-llm';

export interface LocalAiModel {
  id: string;
  label: string;
  /** GPU memory the model needs, from WebLLM's prebuilt config. */
  vramMB: number;
}

export const LOCAL_AI_MODELS: readonly LocalAiModel[] = [
  { id: 'gemma-4-E2B-it-q4f16_1-MLC', label: 'Gemma 4 E2B', vramMB: 3387 },
  { id: 'gemma3-1b-it-q4f16_1-MLC', label: 'Gemma 3 1B', vramMB: 711 },
  { id: 'Qwen3.5-2B-q4f16_1-MLC', label: 'Qwen 3.5 2B', vramMB: 2245 },
  { id: 'Llama-3.2-3B-Instruct-q4f16_1-MLC', label: 'Llama 3.2 3B', vramMB: 2264 },
  { id: 'Phi-4-mini-instruct-q4f16_1-MLC', label: 'Phi-4 mini', vramMB: 3438 },
];

export const DEFAULT_LOCAL_AI_MODEL = LOCAL_AI_MODELS[0].id;

// Models ship a 4096-token context; keep the prompt well inside it so the answer has room.
const MAX_INPUT_CHARS = 6000;

export function isKnownLocalAiModel(id: unknown): id is string {
  return typeof id === 'string' && LOCAL_AI_MODELS.some(m => m.id === id);
}

/** WebGPU is required; it exists only in secure contexts of supporting browsers. */
export function localAiSupported(): boolean {
  return typeof navigator !== 'undefined' && 'gpu' in navigator && (globalThis.isSecureContext ?? true);
}

export type LocalAiState =
  | { status: 'idle' }
  | { status: 'loading'; model: string; progress: number; text: string }
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
let activeModel: string | null = null;

export function setLocalAiModel(model: string | null) {
  activeModel = model && localAiSupported() ? model : null;
}

export function localAiActive(): boolean {
  return activeModel !== null;
}

let engine: { model: string; promise: Promise<WebWorkerMLCEngine>; token: object } | null = null;

async function f16Supported(): Promise<boolean> {
  try {
    const gpu = (navigator as Navigator & { gpu?: { requestAdapter: () => Promise<{ features: Set<string> } | null> } }).gpu;
    const adapter = await gpu?.requestAdapter();
    return !!adapter?.features.has('shader-f16');
  } catch {
    return false;
  }
}

/** The f32 build of a model when the GPU lacks shader-f16, if WebLLM ships one. */
async function resolveVariant(model: string): Promise<string> {
  if (!model.includes('q4f16_1') || await f16Supported()) return model;
  const { prebuiltAppConfig } = await import('@mlc-ai/web-llm');
  const f32 = model.replace('q4f16_1', 'q4f32_1');
  return prebuiltAppConfig.model_list.some(m => m.model_id === f32) ? f32 : model;
}

/** Download (first time) and load `model`; later calls reuse the loaded engine. */
export function loadLocalAi(model: string): Promise<WebWorkerMLCEngine> {
  if (engine?.model === model) return engine.promise;
  const previous = engine;
  // Identifies this load, so a superseded one neither reports progress nor stays loaded.
  const token = {};
  const current = () => engine?.token === token;
  const promise = (async () => {
    if (previous) await previous.promise.then(e => e.unload()).catch(() => {});
    setState({ status: 'loading', model, progress: 0, text: '' });
    const webllm = await import('@mlc-ai/web-llm');
    const variant = await resolveVariant(model);
    const worker = new Worker(new URL('./localAi.worker.ts', import.meta.url), { type: 'module' });
    const created = await webllm.CreateWebWorkerMLCEngine(worker, variant, {
      initProgressCallback: (report: InitProgressReport) => {
        if (current()) setState({ status: 'loading', model, progress: report.progress, text: report.text });
      },
    }).catch((error: unknown) => { worker.terminate(); throw error; });
    if (!current()) {
      // Superseded by another model or unloaded while downloading.
      await created.unload().catch(() => {});
      worker.terminate();
      throw new DOMException('Aborted', 'AbortError');
    }
    setState({ status: 'ready', model });
    return created;
  })();
  engine = { model, promise, token };
  promise.catch((error: unknown) => {
    if (!current()) return;
    engine = null;
    setState({ status: 'error', model, error: error instanceof Error ? error.message : String(error) });
  });
  return promise;
}

/** Release the GPU memory; the downloaded weights stay cached. */
export async function unloadLocalAi() {
  const current = engine;
  engine = null;
  setState({ status: 'idle' });
  await current?.promise.then(e => e.unload()).catch(() => {});
}

/** Whether WebLLM ever stored anything here; checked first so the library loads only when needed. */
async function hasWebLlmCache(): Promise<boolean> {
  try {
    return (await caches.keys()).some(name => name.startsWith('webllm/'));
  } catch {
    return false;
  }
}

export async function isLocalAiModelCached(model: string): Promise<boolean> {
  if (!await hasWebLlmCache()) return false;
  const webllm = await import('@mlc-ai/web-llm');
  const checks = await Promise.all([model, model.replace('q4f16_1', 'q4f32_1')]
    .map(id => webllm.prebuiltAppConfig.model_list.some(m => m.model_id === id)
      ? webllm.hasModelInCache(id).catch(() => false)
      : false));
  return checks.some(Boolean);
}

/** Remove the downloaded weights of `model` (both precisions) from the browser cache. */
export async function deleteLocalAiModel(model: string) {
  if (engine?.model === model) await unloadLocalAi();
  const webllm = await import('@mlc-ai/web-llm');
  for (const id of new Set([model, model.replace('q4f16_1', 'q4f32_1')])) {
    if (webllm.prebuiltAppConfig.model_list.some(m => m.model_id === id)) {
      await webllm.deleteModelAllInfoInCache(id).catch(() => {});
    }
  }
}

interface ChatMessage { role: string; content: string }

/**
 * Fold system prompts into the first user turn (some chat templates, Gemma's among them,
 * reject a system role) and cap the input to fit the model's context.
 */
export function toLocalMessages(messages: unknown[]): { role: 'user' | 'assistant'; content: string }[] {
  const list = messages.filter((m): m is ChatMessage =>
    typeof m === 'object' && m !== null && typeof (m as ChatMessage).content === 'string');
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

/** Drop reasoning blocks some small models (Qwen) emit before the answer. */
export function stripThinking(text: string): string {
  return text.replace(/<think>[\s\S]*?(?:<\/think>|$)/g, '').replace(/^\s+/, '');
}

/** Same contract as streamAiChat: resolves with the full text, reports progress via onDelta. */
export async function streamLocalAiChat(messages: unknown[], { signal, onDelta }: { signal?: AbortSignal; onDelta?: (fullText: string, delta: string) => void } = {}): Promise<string> {
  const model = activeModel;
  if (!model) throw new Error('Local AI is not enabled');
  const abortError = () => new DOMException('Aborted', 'AbortError');
  if (signal?.aborted) throw abortError();
  const llm = await loadLocalAi(model);
  if (signal?.aborted) throw abortError();
  const stop = () => { void llm.interruptGenerate(); };
  signal?.addEventListener('abort', stop, { once: true });
  try {
    const chunks = await llm.chat.completions.create({ messages: toLocalMessages(messages), stream: true, temperature: 0.4 });
    let raw = '';
    let shown = '';
    for await (const chunk of chunks) {
      if (signal?.aborted) throw abortError();
      const delta = chunk.choices[0]?.delta?.content;
      if (!delta) continue;
      raw += delta;
      const next = stripThinking(raw);
      if (next !== shown) {
        const added = next.startsWith(shown) ? next.slice(shown.length) : next;
        shown = next;
        onDelta?.(shown, added);
      }
    }
    if (signal?.aborted) throw abortError();
    return shown;
  } finally {
    signal?.removeEventListener('abort', stop);
  }
}
