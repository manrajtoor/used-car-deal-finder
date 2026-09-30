// Cloudflare Access check for the Pages Function.
//
// Access in front of the site is the first gate, but Pages also serves every
// deployment on its own <hash>.<project>.pages.dev address, which an Access
// app does not always cover. So /api/* also verifies the Access login token
// itself: a request without a valid token for this Access app gets no data,
// whichever address it came in on.
//
// The token is a JWT signed by the team's Access keys (RS256), sent as the
// Cf-Access-Jwt-Assertion header, or the CF_Authorization cookie. It is valid
// when its signature matches one of the team's published keys, its `aud`
// contains this app's audience tag, `iss` is the team domain, and it has not
// expired. Nothing is trusted from the token until the signature checks out.

const certCache = new Map(); // team domain -> { keys, fetchedAt }
const CERT_TTL_MS = 60 * 60 * 1000;

function b64urlBytes(s) {
  const b64 = s.replace(/-/g, '+').replace(/_/g, '/') + '='.repeat((4 - (s.length % 4)) % 4);
  const bin = atob(b64);
  return Uint8Array.from(bin, (c) => c.charCodeAt(0));
}

function b64urlJson(s) {
  return JSON.parse(new TextDecoder().decode(b64urlBytes(s)));
}

function tokenFrom(request) {
  const header = request.headers.get('Cf-Access-Jwt-Assertion');
  if (header) return header;
  const cookie = request.headers.get('Cookie') || '';
  const m = cookie.match(/(?:^|;\s*)CF_Authorization=([^;]+)/);
  return m ? m[1] : null;
}

async function teamKeys(team, fetchImpl, now, fresh = false) {
  const hit = certCache.get(team);
  if (!fresh && hit && now - hit.fetchedAt < CERT_TTL_MS) return hit.keys;
  const res = await fetchImpl(`${team}/cdn-cgi/access/certs`);
  if (!res.ok) throw new Error(`Access certs: HTTP ${res.status}`);
  const { keys } = await res.json();
  certCache.set(team, { keys: keys || [], fetchedAt: now });
  return keys || [];
}

/**
 * True when `request` carries a valid Access token for `aud` issued by `team`
 * (e.g. "https://yourteam.cloudflareaccess.com").
 */
export async function verifyAccess(request, { team, aud, fetchImpl = fetch, now = Date.now() }) {
  if (!team || !aud) return false;
  const token = tokenFrom(request);
  if (!token) return false;
  const parts = token.split('.');
  if (parts.length !== 3) return false;
  let header, payload;
  try {
    header = b64urlJson(parts[0]);
    payload = b64urlJson(parts[1]);
  } catch {
    return false;
  }
  if (header.alg !== 'RS256' || !header.kid) return false;

  // A key rotation shows up as an unknown kid: refetch once before refusing.
  let jwk = (await teamKeys(team, fetchImpl, now)).find((k) => k.kid === header.kid);
  if (!jwk) jwk = (await teamKeys(team, fetchImpl, now, true)).find((k) => k.kid === header.kid);
  if (!jwk) return false;

  const key = await crypto.subtle.importKey('jwk', jwk, { name: 'RSASSA-PKCS1-v1_5', hash: 'SHA-256' }, false, ['verify']);
  const signed = new TextEncoder().encode(`${parts[0]}.${parts[1]}`);
  const ok = await crypto.subtle.verify('RSASSA-PKCS1-v1_5', key, b64urlBytes(parts[2]), signed);
  if (!ok) return false;

  const auds = Array.isArray(payload.aud) ? payload.aud : [payload.aud];
  const seconds = now / 1000;
  return (
    auds.includes(aud) &&
    payload.iss === team &&
    typeof payload.exp === 'number' &&
    payload.exp > seconds &&
    (payload.nbf === undefined || payload.nbf <= seconds + 60)
  );
}

// For tests: forget cached keys.
export function clearKeyCache() {
  certCache.clear();
}
