// A mock OpenID Connect provider for contract scenarios. The authorization `code` is the
// base64url JSON of the claims to issue (plus `nonce`), so a scenario controls the id_token
// exactly; the token endpoint signs it with an RS256 key published at /jwks.
import http from 'node:http';
import { generateKeyPair, exportJWK, SignJWT } from 'jose';

const idps = new Map();

export async function idp(port) {
  if (idps.has(port)) return idps.get(port);
  const { publicKey, privateKey } = await generateKeyPair('RS256');
  const jwk = { ...(await exportJWK(publicKey)), kid: 'k1', alg: 'RS256', use: 'sig' };
  const issuer = `http://127.0.0.1:${port}`;
  const state = { port, issuer, tokenRequests: [] };
  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, issuer);
    const send = (status, body) => { res.writeHead(status, { 'content-type': 'application/json' }); res.end(JSON.stringify(body)); };
    if (url.pathname === '/.well-known/openid-configuration') {
      return send(200, { issuer, authorization_endpoint: `${issuer}/authorize`, token_endpoint: `${issuer}/token`, jwks_uri: `${issuer}/jwks`, end_session_endpoint: `${issuer}/logout` });
    }
    if (url.pathname === '/jwks') return send(200, { keys: [jwk] });
    if (url.pathname === '/token' && req.method === 'POST') {
      let body = '';
      for await (const chunk of req) body += chunk;
      const form = new URLSearchParams(body);
      state.tokenRequests.push(Object.fromEntries(form));
      let claims;
      try { claims = JSON.parse(Buffer.from(form.get('code') || '', 'base64url').toString()); } catch { return send(400, { error: 'invalid_grant' }); }
      if (claims.fail) return send(400, { error: 'invalid_grant' });
      const { audience = form.get('client_id'), issuerOverride, expired, ...rest } = claims;
      const token = await new SignJWT(rest)
        .setProtectedHeader({ alg: 'RS256', kid: 'k1' })
        .setIssuer(issuerOverride || issuer)
        .setAudience(audience)
        .setIssuedAt()
        .setExpirationTime(expired ? Math.floor(Date.now() / 1000) - 60 : '5m')
        .sign(privateKey);
      return send(200, { id_token: token, access_token: 'at-' + (rest.sub || 'x'), refresh_token: 'rt', expires_in: 600, token_type: 'Bearer' });
    }
    send(404, { error: 'not found' });
  });
  await new Promise(resolve => server.listen(port, '127.0.0.1', resolve));
  server.unref();
  idps.set(port, state);
  return state;
}

/** The `code` a scenario passes to the callback to receive these claims. */
export const code = (claims) => Buffer.from(JSON.stringify(claims)).toString('base64url');
