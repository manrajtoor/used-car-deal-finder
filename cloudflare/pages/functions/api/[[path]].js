// Pages Function for /api/*: forwards the dashboard's read-only calls to the
// carbuyer-api Worker through the API service binding (see wrangler.toml).
//
// Only GET/HEAD on the read routes pass. Ingest (POST /api/listings) is not
// reachable through the dashboard: the crawler posts straight to the Worker
// with its bearer token.
//
// Every read also needs a valid Cloudflare Access token for this app
// (lib/access.js, ACCESS_TEAM_DOMAIN / ACCESS_AUD in wrangler.toml), so a
// per-deployment <hash>.pages.dev address that Access does not cover still
// gets no data. Unset vars refuse every read rather than serve them openly.
import { verifyAccess } from '../../lib/access.js';

const READ_ROUTES = new Set(['/api/deals', '/api/snapshots/latest', '/api/alerts', '/api/health']);

const json = (status, body) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json; charset=utf-8', 'Cache-Control': 'no-store' },
  });

export async function onRequest({ request, env }) {
  const url = new URL(request.url);
  const path = url.pathname.replace(/\/+$/, '');
  if (!READ_ROUTES.has(path)) return json(404, { error: 'not found' });
  if (request.method !== 'GET' && request.method !== 'HEAD') return json(405, { error: 'method not allowed' });
  if (!env.ACCESS_TEAM_DOMAIN || !env.ACCESS_AUD) {
    return json(503, { error: 'ACCESS_TEAM_DOMAIN and ACCESS_AUD are not configured' });
  }
  if (!(await verifyAccess(request, { team: env.ACCESS_TEAM_DOMAIN, aud: env.ACCESS_AUD }))) {
    return json(403, { error: 'sign in through Cloudflare Access' });
  }
  if (!env.API) return json(502, { error: 'the API service binding is not configured' });

  // A fresh request: no cookies or Access headers are passed to the Worker.
  // The Worker serves its read routes only to this internal host (a public
  // request cannot carry it), so the reads are reachable only through this
  // Access-protected site.
  const upstream = new URL(path + url.search, 'https://carbuyer-api.internal');
  return env.API.fetch(new Request(upstream, { method: request.method, headers: { Accept: 'application/json' } }));
}
