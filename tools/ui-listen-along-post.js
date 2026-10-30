// Listening along with someone's listening post (listening together, L4): a person's page shows
// what they are listening to with Listen along; it follows their post on your zone, the player
// says "Along with …" with Chat and Leave; Chat shows the post's comments (a suggestion plays from
// there); a song's ⋯ menu suggests it to them; Leave ends it. The node's /v1/profiles/…/listening,
// /v1/presence/post and /v1/comments are mocked here (pm-api's tests cover them on a testnet).
//   node tools/ui-listen-along-post.js <api token> <node URL>
const assert = require('node:assert/strict');
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const TOKEN = process.argv[2] || 'ci-token';
const BASE = process.argv[3] || 'http://127.0.0.1:7788';
const THEM = 'pxnu33x7jtpx9ar1ytsi4yxbp6a5o36gwhffs8zoxmbuptici1jy';
const POST = `pubky://${THEM}/pub/music/v1/posts/0034A0Z4013G0`;
const SONG = 'https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69';
const SUGGESTED = 'https://musicbrainz.org/recording/833f00e1-781f-4edd-90e4-e52712618862';
(async () => {
  const zones = await (await fetch(`${BASE}/v1/zones`, { headers: { authorization: `Bearer ${TOKEN}` } })).json();
  const zone = zones.find((z) => z.id !== 'browser');
  assert.ok(zone, 'the fixture node has a zone');
  const browser = await chromium.launch();
  try {
    for (const width of [1280, 390]) {
      const ctx = await browser.newContext({ viewport: { width, height: 900 }, reducedMotion: 'reduce' });
      await ctx.addInitScript(([t, z]) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', z); }, [TOKEN, zone.id]);
      const page = await ctx.newPage();
      const errors = []; page.on('pageerror', (e) => errors.push(e.message));
      const sent = [];
      let follows = [];
      const following = { zone: zone.id, key: THEM, post: POST, who: 'Ann', name: 'Friday night', song: SONG, status: 'playing', mine: true };
      await page.route(new RegExp(`/v1/profiles/${THEM}$`), (r) => r.fulfill({ json: { id: THEM, name: 'Ann', you: false, following: true, followed_by: false, links: [], followers: 3, following_count: 2, profile_url: `https://pubky.app/profile/${THEM}`, tags: [], tagged: [] } }));
      await page.route(new RegExp(`/v1/shared/${THEM}/playlists$`), (r) => r.fulfill({ json: [] }));
      await page.route(new RegExp(`/v1/profiles/${THEM}/listening$`), (r) => r.fulfill({ json: { live: true, post: POST, name: 'Friday night', song: SONG, title: 'Song', artist: 'Tester', paused: false, position_ms: 5000, duration_ms: 20000 } }));
      await page.route(/\/v1\/presence\/post(\?.*)?$/, async (r) => {
        const req = r.request();
        if (req.method() === 'GET') return r.fulfill({ json: follows });
        if (req.method() === 'POST') { sent.push(req.postDataJSON()); follows = [following]; return r.fulfill({ json: following }); }
        if (req.method() === 'DELETE') { sent.push({ leave: new URL(req.url()).searchParams.get('zone') }); follows = []; return r.fulfill({ status: 204, body: '' }); }
        return r.fallback();
      });
      await page.route(/\/v1\/comments(\?.*)?$/, async (r) => {
        const req = r.request(); const u = new URL(req.url());
        if (req.method() === 'GET') {
          assert.equal(u.searchParams.get('uri'), POST, 'the thread is the post\'s');
          assert.equal(u.searchParams.get('embeds'), '1', 'with the songs suggested');
          return r.fulfill({ json: { parent: POST, indexed: true, comments: [{ post: `pubky://${THEM}/pub/social/v1/posts/0034C`, id: '0034C', author: THEM, name: 'Bo', content: 'Play this next', at: Date.now(), replies: 0, mine: false, parent: POST, embed: SUGGESTED }] } });
        }
        if (req.method() === 'POST') { sent.push({ comment: req.postDataJSON() }); return r.fulfill({ json: { post: 'x', id: 'x', author: THEM, content: 'x', at: Date.now(), replies: 0, mine: true, parent: POST } }); }
        return r.fallback();
      });
      await page.route(/\/v1\/discover\/play\?/, (r) => { sent.push({ play: new URL(r.request().url()).searchParams.get('uri') }); return r.fulfill({ json: { uri: SUGGESTED, connect: [] } }); });
      await page.goto(`${BASE}/person/${THEM}`);
      const live = page.locator('#plive');
      await live.waitFor();
      assert.match(await live.textContent(), /Friday night/);
      assert.match(await live.textContent(), /Song · Tester/);
      // Listen along: on the zone in use
      await live.locator('[data-plive]').click();
      await page.locator('#along:not([hidden])').waitFor();
      assert.deepEqual(sent.shift(), { key: THEM, zone: zone.id });
      assert.equal(await page.locator('#alongtext').textContent(), 'Along with Ann');
      assert.equal(await page.locator('#alongchat').isVisible(), true, 'Chat while following a post');
      // Chat: the post's comments; a suggestion plays from there
      await page.locator('#alongchat').click();
      const dlg = page.locator('dialog[open]');
      await dlg.locator('li.comment').first().waitFor();
      assert.match(await dlg.locator('h2').textContent(), /Friday night/);
      await dlg.locator('[data-cplay]').click();
      // Not on this node's sources (the mock): it says so, as Play on a card does
      const menu = page.locator('.popover.menu:not([hidden])'); await menu.waitFor();
      assert.match(await menu.textContent(), /Not on your services/);
      assert.deepEqual(sent.shift(), { play: SUGGESTED });
      await page.keyboard.press('Escape'); await page.waitForTimeout(200);
      if (await page.locator('dialog[open]').count()) await dlg.locator('[data-alclose]').click();
      assert.equal(await page.locator('dialog[open]').count(), 0);
      // Leave: following ends
      await page.locator('#alongleave').click();
      await page.locator('#along[hidden]').waitFor({ state: 'attached' });
      assert.deepEqual(sent.shift(), { leave: zone.id });
      const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
      assert.ok(overflow <= 0, `no sideways scroll at ${width} px (${overflow})`);
      assert.deepEqual(errors, []);
      await ctx.close();
    }
  } finally {
    await browser.close();
  }
  console.log('listen along with a post: Listening now, Along with, Chat and suggestions, Leave');
})().catch((e) => { console.error(e); process.exit(1); });
