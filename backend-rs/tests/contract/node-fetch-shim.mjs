// Preloaded into the Node backend by the harness when the local Node is newer than the
// backend's supported major (22). Node's bundled fetch (undici 7+) rejects the npm undici 6
// Agent that safeFetch passes as its dispatcher ("invalid onError method"); Node 22 bundles a
// compatible undici. Using npm undici's own fetch restores the production pairing.
import { fetch, Headers, Request, Response } from 'undici';

Object.assign(globalThis, { fetch, Headers, Request, Response });
