// Runs the in-browser model (WebLLM) off the main thread so generation never blocks the UI.
import { WebWorkerMLCEngineHandler } from '@mlc-ai/web-llm';

const handler = new WebWorkerMLCEngineHandler();
self.onmessage = (msg: MessageEvent) => { handler.onmessage(msg); };
