// Collections with web links and posts (TASK-pinakes-social-ux.md B3): in the playlist page's one
// list, in order, a song is a song row and a web page or a post is its Pinakes card with Open
// instead of Play; the hero counts songs and links apart. /v1/cards is mocked here (pm-api's tests
// cover it against a mock Pinakes). Makes one collection on the node and deletes it again.
//   node tools/ui-collection-cards.js <api token> <node URL>
const assert = require('node:assert/strict');
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const TOKEN = process.argv[2] || 'ci-token';
const BASE = process.argv[3] || 'http://127.0.0.1:7788';
const KEY = 'o3ftkuxjd9k6zgnc4hhbhbg5jusubr4zqyb1dtzi8hhm3hf5wbmy';
const LINK = 'https://example.com/liner-notes';
const POST = `pubky://${KEY}/pub/social/v1/posts/0033SCZXVEPNG`;
const api = async (method, path, body) => {
  const r = await fetch(`${BASE}/v1${path}`, { method, headers: { authorization: `Bearer ${TOKEN}`, 'content-type': 'application/json' }, body: body ? JSON.stringify(body) : undefined });
  const j = await r.json().catch(() => null);
  assert.ok(r.ok, `${method} ${path}: ${r.status} ${JSON.stringify(j)}`);
  return j;
};
(async () => {
  const SONG = 'https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69';
  const c = await api('POST', '/collections', { name: 'Cards check', uris: [SONG, LINK, POST] });
  const id = c.summary.id;
  const browser = await chromium.launch();
  try {
    for (const width of [1280, 390]) {
      const ctx = await browser.newContext({ viewport: { width, height: 900 }, reducedMotion: 'reduce' });
      await ctx.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); HTMLMediaElement.prototype.play = async function () {}; }, TOKEN);
      const page = await ctx.newPage();
      const errors = []; page.on('pageerror', (e) => errors.push(e.message));
      const asked = [];
      await page.route(/\/v1\/cards\?/, (r) => {
        const uri = new URL(r.request().url()).searchParams.get('uri'); asked.push(uri);
        if (uri === LINK) return r.fulfill({ json: { uri, status: 'found', kind: 'Article', name: 'Liner notes', site: 'example.com', description: 'How it was made', image: 'https://example.com/cover.jpg' } });
        return r.fulfill({ json: { uri, status: 'unknown' } });
      });
      await page.route('https://example.com/cover.jpg', (r) => r.fulfill({ status: 404, body: '' }));
      await page.goto(`${BASE}/playlist/${encodeURIComponent(id)}`);
      const list = page.locator('#plsongs .list');
      await list.locator('.card-row .name', { hasText: 'Liner notes' }).waitFor();
      const rows = await list.locator(':scope > .list-row').evaluateAll((els) => els.map((e) => (e.classList.contains('card-row') ? 'card' : 'song')));
      assert.deepEqual(rows, ['song', 'card', 'card'], 'one list, in the collection\'s order');
      const card = list.locator('.card-row').first();
      assert.match(await card.locator('.card-sub').textContent(), /example\.com · How it was made/);
      assert.equal(await card.locator('a', { hasText: 'Open' }).getAttribute('href'), LINK);
      assert.equal(await card.locator('[data-splay]').count(), 0, 'a card has no Play');
      assert.deepEqual([...new Set(asked)].sort(), [LINK, POST].sort(), 'both cards are asked for');
      assert.match(await list.locator('.card-row').nth(1).locator('.card-sub').textContent(), /A link/);
      assert.match(await page.locator('.hero .sub').textContent(), /1 song · 2 links/);
      // Clicking a card's row plays nothing and breaks nothing
      await card.locator('.grow').click();
      const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
      assert.ok(overflow <= 0, `no sideways scroll at ${width} px (${overflow})`);
      assert.deepEqual(errors, []);
      await ctx.close();
    }
  } finally {
    await browser.close();
    await api('DELETE', `/collections/${encodeURIComponent(id)}`).catch(() => {});
  }
  // A card first (review of #155, M1): Play and Shuffle skip it; the web page is never sent to
  // be played
  const first = await api('POST', '/collections', { name: 'Card first', uris: [LINK, SONG] });
  const b2 = await chromium.launch();
  try {
    const ctx = await b2.newContext({ viewport: { width: 1280, height: 900 }, reducedMotion: 'reduce' });
    await ctx.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); HTMLMediaElement.prototype.play = async function () {}; }, TOKEN);
    const page = await ctx.newPage();
    const errors = []; page.on('pageerror', (e) => errors.push(e.message));
    const sentToPlay = [];
    await page.route(/\/v1\/discover\/play\?/, (r) => { sentToPlay.push(new URL(r.request().url()).searchParams.get('uri')); return r.continue(); });
    await page.route(/\/v1\/cards\?/, (r) => r.fulfill({ json: { uri: new URL(r.request().url()).searchParams.get('uri'), status: 'unknown' } }));
    await page.goto(`${BASE}/playlist/${encodeURIComponent(first.summary.id)}`);
    await page.locator('#plsongs .card-row').first().waitFor();
    for (const b of ['#plplay', '#plshuffle']) { await page.click(b); await page.waitForTimeout(800); }
    assert.ok(!sentToPlay.includes(LINK), `the web page was sent to be played: ${JSON.stringify(sentToPlay)}`);
    assert.deepEqual(errors, []);
    await ctx.close();
  } finally {
    await b2.close();
    await api('DELETE', `/collections/${encodeURIComponent(first.summary.id)}`).catch(() => {});
  }
  console.log('collection cards: one list in order, cards with Open, counts apart; a card first plays nothing of it');
})().catch((e) => { console.error(e); process.exit(1); });
