// Comments on item pages (TASK-pinakes-social-ux.md B1): a song with a link shows Comments
// between Tags and About; posting sends the song's link, a reply sends the comment's post, only
// your own comments have Delete (after a question); a song without a link has no Comments. The
// node's /v1/comments is mocked here (pm-api's tests cover it against Nexus and a testnet).
//   node tools/ui-comments.js <api token> <node URL>
const assert = require('node:assert/strict');
const { chromium } = require(require.resolve('playwright', { paths: [process.cwd(), __dirname] }));
const TOKEN = process.argv[2] || 'ci-token';
const BASE = process.argv[3] || 'http://127.0.0.1:7788';
const SONG = 'https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69';
const ME = 'o3ftkuxjd9k6zgnc4hhbhbg5jusubr4zqyb1dtzi8hhm3hf5wbmy';
const THEM = 'pxnu33x7jtpx9ar1ytsi4yxbp6a5o36gwhffs8zoxmbuptici1jy';
const post = (k, id) => `pubky://${k}/pub/social/v1/posts/${id}`;
(async () => {
  const real = await (await fetch(`${BASE}/v1/item/song/lib-1`, { headers: { authorization: `Bearer ${TOKEN}` } })).json();
  assert.ok(real.title !== undefined, 'the fixture node has song lib-1');
  const browser = await chromium.launch();
  try {
    for (const width of [1280, 390]) {
      const ctx = await browser.newContext({ viewport: { width, height: 900 }, reducedMotion: 'reduce' });
      await ctx.addInitScript((t) => { localStorage.setItem('pm.token', t); localStorage.setItem('pm.zone', 'browser'); }, TOKEN);
      const page = await ctx.newPage();
      const errors = []; page.on('pageerror', (e) => errors.push(e.message));
      const sent = [];
      let thread = [
        { post: post(THEM, '0034C'), id: '0034C', author: THEM, name: 'Ann', content: 'Newest', at: Date.now() - 60000, replies: 1, mine: false, parent: SONG },
        { post: post(ME, '0034A'), id: '0034A', author: ME, content: 'Mine\non two lines', at: Date.now() - 3600000, replies: 0, mine: true, parent: SONG },
      ];
      await page.route(/\/v1\/item\/song\/lib-1(\?.*)?$/, (r) => r.fulfill({ json: { ...real, song_uri: SONG } }));
      await page.route(/\/v1\/comments(\?.*)?$/, async (r) => {
        const req = r.request(); const u = new URL(req.url());
        if (req.method() === 'GET') {
          if (u.searchParams.get('post')) return r.fulfill({ json: { parent: u.searchParams.get('post'), comments: [{ post: post(ME, '0034D'), id: '0034D', author: ME, content: 'A reply', at: Date.now(), replies: 0, mine: true, parent: u.searchParams.get('post') }], indexed: true } });
          assert.equal(u.searchParams.get('uri'), SONG, 'the thread is asked by the song\'s link');
          return r.fulfill({ json: { parent: SONG, comments: thread, indexed: true } });
        }
        if (req.method() === 'POST') { const b = req.postDataJSON(); sent.push(b); const c = { post: post(ME, `0034N${sent.length}`), id: `0034N${sent.length}`, author: ME, content: b.content, at: Date.now(), replies: 0, mine: true, parent: b.uri || b.reply_to }; if (b.uri) thread = [c, ...thread]; return r.fulfill({ json: c }); }
        if (req.method() === 'DELETE') { sent.push({ delete: u.searchParams.get('post') }); thread = thread.filter((c) => c.post !== u.searchParams.get('post')); return r.fulfill({ json: { deleted: 1 } }); }
        return r.fallback();
      });
      await page.goto(BASE + '/song/lib-1');
      const sec = page.locator('#icomments');
      await sec.locator('li.comment').first().waitFor();
      const order = await page.$$eval('#main section.section', (els) => els.map((e) => e.id).filter((id) => ['itags', 'icomments', 'iabout'].includes(id)));
      assert.deepEqual(order, ['itags', 'icomments', 'iabout'], 'Comments sit between Tags and About');
      assert.equal(await sec.locator('li.comment').count(), 2);
      assert.equal(await sec.locator('li.comment').nth(0).locator('[data-cdel]').count(), 0, 'no Delete on someone else\'s comment');
      assert.equal(await sec.locator('li.comment').nth(1).locator('[data-cdel]').count(), 1, 'Delete on your own');
      assert.match(await sec.locator('li.comment').nth(0).textContent(), /Ann/);
      // Post
      await sec.locator('textarea[aria-label="Add a comment"]').fill('Great song');
      await sec.locator('form.comment-form button').first().click();
      await page.locator('.toast', { hasText: 'Comment posted' }).waitFor();
      assert.deepEqual(sent[0], { uri: SONG, content: 'Great song' });
      await sec.locator('li.comment', { hasText: 'Great song' }).waitFor();
      // Replies, and a reply
      const ann = sec.locator('li.comment', { hasText: 'Newest' });
      await ann.locator('[data-creplies]').click();
      await ann.locator('.comment-sub li.comment', { hasText: 'A reply' }).waitFor();
      await ann.locator(':scope > .toolbar [data-creply]').click();
      await ann.locator('textarea[aria-label="Reply"]').fill('Me too');
      await ann.locator('textarea[aria-label="Reply"]').press('Tab');
      await ann.locator('.comment-sub > form button').click();
      await page.locator('.toast', { hasText: 'Reply posted' }).waitFor();
      assert.deepEqual(sent[1], { reply_to: post(THEM, '0034C'), content: 'Me too' });
      // Delete your own, after a question
      const mine = sec.locator('li.comment', { hasText: 'Mine' });
      await mine.locator('[data-cdel]').click();
      await page.locator('dialog .choice', { hasText: 'Delete' }).click();
      await page.locator('.toast', { hasText: 'Comment deleted' }).waitFor();
      assert.deepEqual(sent[2], { delete: post(ME, '0034A') });
      assert.equal(await sec.locator('li.comment', { hasText: 'Mine' }).count(), 0);
      const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
      assert.ok(overflow <= 0, `no sideways scroll at ${width} px (${overflow})`);
      // A song without a link has no Comments
      await page.unroute(/\/v1\/item\/song\/lib-1(\?.*)?$/);
      await page.route(/\/v1\/item\/song\/lib-1(\?.*)?$/, (r) => r.fulfill({ json: { ...real, song_uri: null, link: null, target: { track_id: 1 } } }));
      await page.goto(BASE + '/song/lib-1');
      await page.locator('#main #iabout').waitFor();
      assert.equal(await page.locator('#icomments').count(), 0);
      assert.deepEqual(errors, []);
      await ctx.close();
    }
  } finally {
    await browser.close();
  }
  console.log('comments: between Tags and About, post, replies, reply, delete your own, none without a link');
})().catch((e) => { console.error(e); process.exit(1); });
