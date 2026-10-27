// Runs the in-browser model (Transformers.js on ONNX Runtime WebGPU) off the main thread so
// downloads and generation never block the UI. Talks to localAi.ts through LocalAiRequest /
// LocalAiResponse messages.
import {
  AutoModelForCausalLM, AutoTokenizer, InterruptableStoppingCriteria, ModelRegistry, TextStreamer, env,
} from '@huggingface/transformers';
import type { PreTrainedModel, PreTrainedTokenizer, ProgressInfo } from '@huggingface/transformers';
// ONNX Runtime's WebAssembly core, served from this origin instead of a CDN.
import ortMjs from 'onnxruntime-web/ort-wasm-simd-threaded.asyncify.mjs?url';
import ortWasm from 'onnxruntime-web/ort-wasm-simd-threaded.asyncify.wasm?url';
import type { LocalAiRequest, LocalAiResponse } from './localAi.ts';

if (env.backends.onnx.wasm) env.backends.onnx.wasm.wasmPaths = { mjs: ortMjs, wasm: ortWasm };
// The wasm cache re-serves the loader from a blob: URL, which the CSP (script-src 'self')
// refuses to import; the hashed same-origin assets are browser-cached anyway.
env.useWasmCache = false;
env.allowLocalModels = false;

let loaded: { key: string; tokenizer: PreTrainedTokenizer; model: PreTrainedModel } | null = null;
const stopping = new InterruptableStoppingCriteria();

const post = (message: LocalAiResponse) => self.postMessage(message);
const errorText = (error: unknown) => error instanceof Error ? error.message : String(error);

async function load(model: string, dtype: 'q4f16' | 'q4') {
  const key = `${model}:${dtype}`;
  if (loaded?.key === key) return loaded;
  if (loaded) {
    await loaded.model.dispose().catch(() => {});
    loaded = null;
  }
  const progress_callback = (info: ProgressInfo) => {
    if (info.status === 'progress_total') post({ type: 'progress', progress: info.progress / 100 });
  };
  const tokenizer = await AutoTokenizer.from_pretrained(model, { progress_callback });
  const llm = await AutoModelForCausalLM.from_pretrained(model, { dtype, device: 'webgpu', progress_callback });
  loaded = { key, tokenizer, model: llm };
  return loaded;
}

async function handle(msg: LocalAiRequest) {
  switch (msg.type) {
    case 'load':
      try {
        await load(msg.model, msg.dtype);
        post({ type: 'ready', id: msg.id });
      } catch (error) {
        post({ type: 'error', id: msg.id, error: errorText(error) });
      }
      return;
    case 'generate':
      try {
        const { tokenizer, model } = await load(msg.model, msg.dtype);
        const inputs = tokenizer.apply_chat_template(msg.messages, {
          add_generation_prompt: true,
          return_dict: true,
          // Template variable (Gemma, Qwen): answer directly, no reasoning preamble.
          enable_thinking: false,
        } as Parameters<PreTrainedTokenizer['apply_chat_template']>[1]) as Record<string, unknown>;
        let text = '';
        const streamer = new TextStreamer(tokenizer, {
          skip_prompt: true,
          skip_special_tokens: true,
          callback_function: (chunk: string) => {
            text += chunk;
            post({ type: 'delta', id: msg.id, text });
          },
        });
        stopping.reset();
        await model.generate({
          ...inputs,
          max_new_tokens: msg.maxNewTokens,
          do_sample: false,
          streamer,
          stopping_criteria: stopping,
        });
        post({ type: 'done', id: msg.id, text, interrupted: stopping.interrupted });
      } catch (error) {
        post({ type: 'error', id: msg.id, error: errorText(error) });
      }
      return;
    case 'interrupt':
      stopping.interrupt();
      return;
    case 'unload':
      await loaded?.model.dispose().catch(() => {});
      loaded = null;
      post({ type: 'ready', id: msg.id });
      return;
    case 'cached':
      try {
        post({ type: 'cached', id: msg.id, cached: await ModelRegistry.is_cached(msg.model, { dtype: msg.dtype }) });
      } catch {
        post({ type: 'cached', id: msg.id, cached: false });
      }
      return;
    case 'delete':
      try {
        await ModelRegistry.clear_cache(msg.model, { dtype: msg.dtype });
      } catch { /* nothing cached */ }
      post({ type: 'ready', id: msg.id });
      return;
  }
}

// Messages run one at a time, except interrupts, which must reach a running generation.
let queue = Promise.resolve();
self.onmessage = (event: MessageEvent<LocalAiRequest>) => {
  if (event.data.type === 'interrupt') {
    stopping.interrupt();
    return;
  }
  queue = queue.then(() => handle(event.data));
};
