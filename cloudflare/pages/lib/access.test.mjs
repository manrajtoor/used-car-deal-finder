// node --test cloudflare/pages/lib/access.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';

import { verifyAccess, clearKeyCache } from './access.js';

const TEAM = 'https://team.cloudflareaccess.com';
const AUD = 'aud-tag';

const b64url = (bytes) => Buffer.from(bytes).toString('base64url');
const enc = (obj) => b64url(new TextEncoder().encode(JSON.stringify(obj)));

async function keyPair(kid) {
  const pair = await crypto.subtle.generateKey(
    { name: 'RSASSA-PKCS1-v1_5', modulusLength: 2048, publicExponent: new Uint8Array([1, 0, 1]), hash: 'SHA-256' },
    true,
    ['sign', 'verify'],
  );
  const jwk = { ...(await crypto.subtle.exportKey('jwk', pair.publicKey)), kid, alg: 'RS256' };
  return { ...pair, jwk };
}

async function sign(privateKey, header, payload) {
  const input = `${enc(header)}.${enc(payload)}`;
  const sig = await crypto.subtle.sign('RSASSA-PKCS1-v1_5', privateKey, new TextEncoder().encode(input));
  return `${input}.${b64url(new Uint8Array(sig))}`;
}

const now = Date.UTC(2026, 8, 30, 12, 0, 0);
const claims = (extra = {}) => ({ aud: [AUD], iss: TEAM, exp: now / 1000 + 600, nbf: now / 1000 - 5, ...extra });
const certs = (...jwks) => async (url) => {
  assert.equal(url, `${TEAM}/cdn-cgi/access/certs`);
  return new Response(JSON.stringify({ keys: jwks }));
};
const req = (headers) => new Request('https://x.pages.dev/api/deals', { headers });

test('a valid Access token passes, by header or by cookie', async () => {
  clearKeyCache();
  const k = await keyPair('k1');
  const t = await sign(k.privateKey, { alg: 'RS256', kid: 'k1' }, claims());
  const opts = { team: TEAM, aud: AUD, fetchImpl: certs(k.jwk), now };
  assert.equal(await verifyAccess(req({ 'Cf-Access-Jwt-Assertion': t }), opts), true);
  assert.equal(await verifyAccess(req({ Cookie: `a=b; CF_Authorization=${t}` }), opts), true);
});

test('missing, forged, foreign or stale tokens are refused', async () => {
  clearKeyCache();
  const k = await keyPair('k1');
  const attacker = await keyPair('k1'); // same kid, different key
  const opts = { team: TEAM, aud: AUD, fetchImpl: certs(k.jwk), now };
  const h = { alg: 'RS256', kid: 'k1' };
  const cases = {
    'no token': req({}),
    garbage: req({ 'Cf-Access-Jwt-Assertion': 'not.a.jwt' }),
    'signed by another key': req({ 'Cf-Access-Jwt-Assertion': await sign(attacker.privateKey, h, claims()) }),
    'other app': req({ 'Cf-Access-Jwt-Assertion': await sign(k.privateKey, h, claims({ aud: ['other'] })) }),
    'other team': req({ 'Cf-Access-Jwt-Assertion': await sign(k.privateKey, h, claims({ iss: 'https://evil.cloudflareaccess.com' })) }),
    expired: req({ 'Cf-Access-Jwt-Assertion': await sign(k.privateKey, h, claims({ exp: now / 1000 - 1 })) }),
    'alg none': req({ 'Cf-Access-Jwt-Assertion': `${enc({ alg: 'none', kid: 'k1' })}.${enc(claims())}.` }),
    'unknown kid': req({ 'Cf-Access-Jwt-Assertion': await sign(k.privateKey, { alg: 'RS256', kid: 'zz' }, claims()) }),
  };
  for (const [name, r] of Object.entries(cases)) {
    assert.equal(await verifyAccess(r, opts), false, name);
  }
  // Tampering with the payload after signing breaks the signature.
  const good = await sign(k.privateKey, h, claims());
  const [hh, , ss] = good.split('.');
  const tampered = `${hh}.${enc(claims({ aud: [AUD, 'x'], exp: now / 1000 + 99999 }))}.${ss}`;
  assert.equal(await verifyAccess(req({ 'Cf-Access-Jwt-Assertion': tampered }), opts), false, 'tampered payload');
});

test('unconfigured team or audience refuses everything', async () => {
  clearKeyCache();
  const k = await keyPair('k1');
  const t = await sign(k.privateKey, { alg: 'RS256', kid: 'k1' }, claims());
  const r = req({ 'Cf-Access-Jwt-Assertion': t });
  assert.equal(await verifyAccess(r, { team: '', aud: AUD, fetchImpl: certs(k.jwk), now }), false);
  assert.equal(await verifyAccess(r, { team: TEAM, aud: '', fetchImpl: certs(k.jwk), now }), false);
});

test('a rotated key is fetched again once', async () => {
  clearKeyCache();
  const oldK = await keyPair('old');
  const newK = await keyPair('new');
  let served = [oldK.jwk];
  let calls = 0;
  const fetchImpl = async () => {
    calls++;
    return new Response(JSON.stringify({ keys: served }));
  };
  const opts = { team: TEAM, aud: AUD, fetchImpl, now };
  assert.equal(await verifyAccess(req({ 'Cf-Access-Jwt-Assertion': await sign(oldK.privateKey, { alg: 'RS256', kid: 'old' }, claims()) }), opts), true);
  served = [newK.jwk];
  assert.equal(await verifyAccess(req({ 'Cf-Access-Jwt-Assertion': await sign(newK.privateKey, { alg: 'RS256', kid: 'new' }, claims()) }), opts), true);
  assert.equal(calls, 2, 'cached once, refetched on the unknown kid');
});
