import { request as httpsRequest } from 'https';
import { request as httpRequest } from 'http';
import { createRemoteJWKSet, customFetch } from 'jose';
import { validateHost } from './hostValidation.js';

// In-memory OIDC discovery cache keyed by issuerUrl. Shared by the login/callback flow
// (oidc.ts) and anything that needs to talk to a provider's token endpoint outside of a
// login (e.g. refreshing a passthrough IMAP/SMTP token).
interface OidcDiscoveryDocument {
  issuer?: string;
  authorization_endpoint?: string;
  token_endpoint?: string;
  jwks_uri?: string;
  end_session_endpoint?: string;
  [field: string]: unknown;
}

interface CachedDiscovery {
  doc: OidcDiscoveryDocument;
  jwks: ReturnType<typeof createRemoteJWKSet>;
  cachedAt: number;
}

const discoveryCache = new Map<string, CachedDiscovery>();
const DISCOVERY_TTL_MS = 5 * 60 * 1000;

// Node's HTTP request options expect a plain header record. undici (jose's customFetch
// path) passes a Headers instance, which would otherwise get forwarded as-is and node
// would serialise it to no headers; forward only plain records.
function toNodeHeaders(headers: RequestInit['headers']): Record<string, string | string[]> {
  if (!headers || Array.isArray(headers) || headers instanceof Headers) return {};
  const out: Record<string, string | string[]> = {};
  for (const [key, value] of Object.entries(headers)) {
    out[key] = typeof value === 'string' ? value : [...(value as string[])];
  }
  return out;
}

// Fetch that skips TLS certificate verification — only used when allow_insecure is set.
export function makeInsecureFetch(signal?: AbortSignal): typeof fetch {
  return function insecureFetch(input: string | URL | Request, init: RequestInit = {}): Promise<Response> {
    const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
    return new Promise<Response>((resolve, reject) => {
      const effectiveSignal = init.signal ?? signal;
      const parsed = new URL(url);
      const isHttps = parsed.protocol === 'https:';
      const port = parsed.port ? parseInt(parsed.port) : (isHttps ? 443 : 80);
      const reqFn = isHttps ? httpsRequest : httpRequest;
      const chunks: Buffer[] = [];
      const req = reqFn(
        { hostname: parsed.hostname, port, path: parsed.pathname + parsed.search, method: init.method ?? 'GET', headers: toNodeHeaders(init.headers), rejectUnauthorized: false },
        res => {
          res.on('data', c => chunks.push(c));
          res.on('end', () => {
            const text = Buffer.concat(chunks).toString('utf8');
            resolve(new Response(text, { status: res.statusCode ?? 200 }));
          });
          res.on('error', reject);
        }
      );
      req.on('error', reject);
      if (effectiveSignal) effectiveSignal.addEventListener('abort', () => req.destroy(), { once: true });
      if (init.body) req.write(init.body as string);
      req.end();
    });
  };
}

export async function getDiscovery(issuerUrl: string, allowInsecure: boolean | null | undefined = false) {
  const parsed = new URL(issuerUrl);
  if (!allowInsecure && parsed.protocol !== 'https:') throw new Error('OIDC issuer URL must use HTTPS');

  // Re-validate at runtime: the saved issuer may predate host validation,
  // or DNS may have changed since the record was saved.
  if (!allowInsecure) {
    const issuerHostErr = await validateHost(parsed.hostname);
    if (issuerHostErr) throw new Error(`OIDC issuer host rejected: ${issuerHostErr}`);
  }

  const cacheKey = `${issuerUrl}:${allowInsecure}`;
  const cached = discoveryCache.get(cacheKey);
  if (cached && Date.now() - cached.cachedAt < DISCOVERY_TTL_MS) {
    return { doc: cached.doc, jwks: cached.jwks };
  }
  const wellKnown = issuerUrl.replace(/\/$/, '') + '/.well-known/openid-configuration';
  const signal = AbortSignal.timeout(10000);
  const fetchFn = allowInsecure ? makeInsecureFetch(signal) : fetch;
  const fetchOpts = allowInsecure ? {} : { signal };
  const res = await fetchFn(wellKnown, fetchOpts);
  if (!res.ok) {
    if (res.status === 401 || res.status === 403) {
      throw new Error(`OIDC discovery blocked (${res.status}): the server cannot reach ${wellKnown} — ensure the well-known endpoint is publicly accessible from the Inboxora server`);
    }
    throw new Error(`OIDC discovery failed for ${issuerUrl}: ${res.status}`);
  }
  const doc = (await res.json()) as OidcDiscoveryDocument;
  const normConfigured = issuerUrl.replace(/\/$/, '');
  const normDiscovered = (doc.issuer || '').replace(/\/$/, '');
  if (normDiscovered !== normConfigured) {
    throw new Error(`OIDC issuer mismatch: configured "${normConfigured}", got "${normDiscovered}"`);
  }
  for (const field of ['authorization_endpoint', 'token_endpoint', 'jwks_uri'] as const) {
    const url = doc[field];
    if (!url) throw new Error(`OIDC discovery missing required field: ${field}`);
    let endpointParsed;
    try { endpointParsed = new URL(url as string); } catch {
      throw new Error(`OIDC discovery returned invalid URL for ${field}: ${url}`);
    }
    if (!allowInsecure) {
      if (endpointParsed.protocol !== 'https:') {
        throw new Error(`OIDC discovery returned non-HTTPS ${field}: ${url}`);
      }
      const hostErr = await validateHost(endpointParsed.hostname);
      if (hostErr) throw new Error(`OIDC discovery ${field} points to a disallowed host: ${hostErr}`);
    }
  }
  const jwksUri = doc.jwks_uri;
  if (!jwksUri) throw new Error('OIDC discovery missing required field: jwks_uri');
  const jwksOptions = allowInsecure ? { [customFetch]: makeInsecureFetch() } : {};
  const jwks = createRemoteJWKSet(new URL(jwksUri), jwksOptions);
  discoveryCache.set(cacheKey, { doc, jwks, cachedAt: Date.now() });
  return { doc, jwks };
}
