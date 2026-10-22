// TIDAL sign-in with the project's app, no developer credentials (STEWARD-SHARED-APPS.md):
// a node with an empty config signs in through the static return page with PKCE and no
// client secret. A mock TIDAL (authorize + token + users) and a stand-in for the return page
// run here, on one port:
//   * /authorize checks the client id, the PKCE challenge and that the redirect is the return
//     page, then sends the browser there with a code;
//   * /callback (the return page's contract: state = <nonce>.<base64url(callback URL)>) forwards
//     every query parameter to the node's callback named in state, http(s) only;
//   * /v1/oauth2/token refuses any client secret (Authorization header or client_secret) and
//     checks the code, the verifier against the challenge, and the redirect.
// The node runs with an app id and no secret (`[tidal] client_id = "ci-project-app"`, as a build
// with PM_PROJECT_TIDAL_CLIENT_ID would), [auth] return_page = http://127.0.0.1:<port>/callback
// and PM_TIDAL_{AUTH,LOGIN,API}_BASE pointing here (see .github/workflows/ci.yml).
//   node tools/ui-tidal-login.js [node URL, default http://127.0.0.1:7791] [mock port, default 7811]
const http = require('http');
const crypto = require('crypto');
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const NODE = process.argv[2] || 'http://127.0.0.1:7791';
const PORT = Number(process.argv[3] || 7811);
const MOCK = `http://127.0.0.1:${PORT}`;
const CLIENT = 'ci-project-app';
const problems = [];
const check = (ok, what) => { console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`); if (!ok) problems.push(what); };
const b64url = (b) => Buffer.from(b).toString('base64').replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
const codes = new Map(); // code -> { challenge, redirect }
const seen = { authorize: 0, callback: 0, token: [] };

const server = http.createServer((req, res) => {
  const u = new URL(req.url, MOCK);
  if (u.pathname === '/authorize') {
    seen.authorize++;
    const q = u.searchParams;
    const fine = q.get('client_id') === CLIENT && q.get('code_challenge_method') === 'S256' && q.get('code_challenge') && q.get('redirect_uri') === `${MOCK}/callback` && q.get('state');
    if (!fine) { res.writeHead(400); return res.end('bad authorize request: ' + u.search); }
    const code = 'code-' + crypto.randomBytes(8).toString('hex');
    codes.set(code, { challenge: q.get('code_challenge'), redirect: q.get('redirect_uri') });
    res.writeHead(302, { location: `${q.get('redirect_uri')}?code=${code}&state=${encodeURIComponent(q.get('state'))}` });
    return res.end();
  }
  if (u.pathname === '/callback') {
    seen.callback++;
    const st = u.searchParams.get('state') || '';
    const enc = st.split('.')[1] || '';
    let target = '';
    try { target = Buffer.from(enc.replace(/-/g, '+').replace(/_/g, '/'), 'base64').toString('utf8'); } catch { /* not complete */ }
    if (!/^https?:\/\//.test(target)) { res.writeHead(200, { 'content-type': 'text/html' }); return res.end('This sign-in link is not complete'); }
    res.writeHead(302, { location: `${target}${target.includes('?') ? '&' : '?'}${u.searchParams.toString()}` });
    return res.end();
  }
  if (u.pathname === '/v1/oauth2/token' && req.method === 'POST') {
    let body = ''; req.on('data', (d) => { body += d; });
    return req.on('end', () => {
      const f = new URLSearchParams(body);
      seen.token.push({ grant: f.get('grant_type'), auth: !!req.headers.authorization, secret: f.has('client_secret') });
      if (req.headers.authorization || f.has('client_secret')) { res.writeHead(401, { 'content-type': 'application/json' }); return res.end('{"error":"invalid_client","error_description":"this mock refuses secrets"}'); }
      const c = codes.get(f.get('code'));
      const ok = f.get('grant_type') === 'authorization_code' && f.get('client_id') === CLIENT && c && c.redirect === f.get('redirect_uri') && b64url(crypto.createHash('sha256').update(f.get('code_verifier') || '').digest()) === c.challenge;
      if (!ok) { res.writeHead(400, { 'content-type': 'application/json' }); return res.end('{"error":"invalid_grant"}'); }
      codes.delete(f.get('code'));
      res.writeHead(200, { 'content-type': 'application/json' });
      return res.end(JSON.stringify({ access_token: 'mock-access', refresh_token: 'mock-refresh', expires_in: 3600, scope: 'user.read', user_id: 4242, token_type: 'Bearer' }));
    });
  }
  if (/^\/users\//.test(u.pathname)) { res.writeHead(200, { 'content-type': 'application/vnd.api+json' }); return res.end('{"data":{"attributes":{"country":"CH"}}}'); }
  res.writeHead(404); res.end();
});

(async () => {
  await new Promise((r) => server.listen(PORT, '127.0.0.1', r));
  const browser = await chromium.launch();
  const p = await browser.newPage();
  const errors = []; p.on('pageerror', (e) => errors.push(e.message));
  const status = async () => (await fetch(NODE + '/v1/sources/tidal/status')).json();
  const before = await status();
  check(before.configured && !before.signed_in, 'a node with an empty config offers TIDAL (the project app) and is not signed in');
  await p.goto(NODE + '/sources/tidal');
  const btn = await p.waitForSelector('#tidalin', { timeout: 15000 }).catch(() => null);
  check(!!btn, 'Sources › TIDAL shows "Connect TIDAL"');
  if (btn) {
    // The page tidies ?tidal=… away at once, so watch the navigations rather than the final URL
    const landed = [];
    p.on('framenavigated', (f) => { if (f === p.mainFrame()) landed.push(f.url()); });
    await btn.click();
    for (let i = 0; i < 40 && !landed.some((u) => u.startsWith(NODE) && /tidal=/.test(u)); i++) await p.waitForTimeout(500);
    const back = landed.find((u) => u.startsWith(NODE) && /tidal=/.test(u)) || p.url();
    check(/tidal=signed_in/.test(back), `back on the node, signed in (${back.replace(NODE, '')})`);
  }
  check(seen.authorize === 1 && seen.callback === 1, 'the browser went TIDAL › return page › node');
  check(seen.token.length >= 1 && seen.token.every((t) => !t.auth && !t.secret), 'no client secret was ever sent');
  const after = await status();
  check(after.signed_in, 'the node holds the TIDAL session');
  check(after.public_client && after.public_client.outcome === 'accepted', 'the code exchange worked as a public client');
  // A state that does not name an http(s) node is refused by the return page
  const bad = await fetch(`${MOCK}/callback?code=x&state=n.${b64url('javascript:alert(1)')}`, { redirect: 'manual' });
  check(bad.status === 200 && /not complete/.test(await bad.text()), 'the return page refuses a non-http target');
  check(errors.length === 0, `no script errors${errors.length ? ': ' + errors.join(' | ') : ''}`);
  await browser.close(); server.close();
  if (problems.length) { console.log(`${problems.length} problems`); process.exit(1); }
  console.log('TIDAL signs in through the return page with PKCE and no secret');
})();
