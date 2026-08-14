// Template for a browser test of the Cookbook Library UI. Copy to the
// scratchpad, replace the "feature under test" section, and run:
//
//   env BASE=http://127.0.0.1:8099 CB_USER=pwtest CB_PASS=... node uitest.js
//
// Runs under phone emulation by default — that is the layout most likely to
// break, and the one the desktop pass never exercises. `UI_DEVICE=desktop`
// gives the old 1280×800 window; run both when a change touches app.css.
//
// Exits non-zero if any check fails, so it can gate a change.
const { chromium, devices } = require('playwright');

const BASE = process.env.BASE || 'http://127.0.0.1:8099';
const USER = process.env.CB_USER;          // a throwaway account, not the user's
const PASS = process.env.CB_PASS;          // from the environment, never in this file
const SHOT = process.env.SHOTDIR || '.';
const MOBILE = (process.env.UI_DEVICE || 'mobile') !== 'desktop';

// iPhone 13 is 390×844. Its own deviceScaleFactor is 3, which makes every
// screenshot a 1170px-wide PNG for no extra detail — pin it to 2.
const CONTEXT = MOBILE
  ? { ...devices['iPhone 13'], deviceScaleFactor: 2 }
  : { viewport: { width: 1280, height: 800 } };

const log = [];
const ok = (name, cond, extra = '') =>
  log.push(`${cond ? 'PASS' : 'FAIL'}  ${name}${extra ? ' — ' + extra : ''}`);

// Every paged view paints `<p class="loading">Loading…</p>` before its results,
// so `waitForSelector('.rcard, .loading')` returns against the placeholder and
// the count that follows is zero. Wait for the placeholder to clear instead.
const settled = page => page.waitForFunction(() => {
  const l = document.querySelector('p.loading');
  return !l || !/Loading/.test(l.textContent);
}, null, { timeout: 15000 });

// The frontend replaces #view in place, so the previous view's nodes stay on
// screen until the fetch resolves. Wait for the heading the new view renders.
const heading = (page, text) => page.waitForFunction(
  t => (document.querySelector('#view h1')?.textContent || '').includes(t),
  text, { timeout: 15000 });

// `.navlinks` is `display: none` below 820px (app.css) and there is no burger
// menu, so on a phone the only way to reach a view is its hash. That is a
// same-document navigation: the browser restores the old scroll offset, hence
// the explicit scrollTo. On desktop, click the nav the way a user would.
const go = async (page, hash, head) => {
  if (MOBILE) await page.goto(`${BASE}/#/${hash}`);
  else await page.click(`.navlinks a[href="#/${hash}"]`);
  await heading(page, head);
  await settled(page);
  await page.evaluate(() => window.scrollTo(0, 0));
};

// A green run proves behaviour, not layout: always read these back. Park the
// mouse in a dead corner first — it sits wherever the last click left it, and a
// `:hover` row photographs exactly like a selected one. Scroll to the top and
// give thumbnails a beat, or every cover shoots as a blank placeholder.
const shot = async (page, name) => {
  if (!MOBILE) await page.mouse.move(1270, 10);
  await page.evaluate(() => window.scrollTo(0, 0));
  await page.waitForTimeout(800);
  await page.screenshot({ path: `${SHOT}/${name}.png` });
};

// Horizontal overflow is the classic phone break and is invisible in a
// screenshot — the page just looks cropped. Assert it on every view.
const noOverflow = async (page, where) => {
  const px = await page.evaluate(() =>
    document.documentElement.scrollWidth - window.innerWidth);
  ok(`no horizontal overflow on ${where}`, px <= 1, `${px}px`);
};

(async () => {
  const browser = await chromium.launch();
  const ctx = await browser.newContext(CONTEXT);
  const page = await ctx.newPage();

  // The frontend renders fetch failures into the view rather than throwing, so
  // a quiet console is the only evidence nothing went wrong underneath.
  const errors = [];
  page.on('console', m => { if (m.type() === 'error') errors.push(m.text()); });
  page.on('pageerror', e => errors.push(String(e)));

  // ---- sign in
  await page.goto(`${BASE}/login`);
  await page.fill('input[name=username]', USER);
  await page.fill('input[name=password]', PASS);
  await page.click('button[type=submit]');
  await page.waitForURL(u => !u.pathname.startsWith('/login'), { timeout: 10000 });
  ok('sign in', (await page.locator('#whoami').textContent()) !== '');

  // ---- pick a recipe from the global index rather than hard-coding an id:
  // ids are reassigned by every re-index.
  await go(page, 'recipes', 'All recipes');
  await page.waitForSelector('.rcard');
  await noOverflow(page, 'recipes');
  const recipeId = await page.evaluate(() =>
    Number(document.querySelector('.rcard').getAttribute('href').split('/').pop()));
  await page.goto(`${BASE}/#/recipe/${recipeId}`);
  await page.waitForSelector('#cookbtn');
  const title = (await page.locator('#view h1').textContent()).trim();

  // ------------------------------------------------------ feature under test
  // Wait on the state that should change, not on a timeout. Every marks
  // endpoint answers with the recipe's updated marks and the panel redraws.
  // Use tap() under phone emulation: the context has touch, and tap is what a
  // phone user does — a handler bound to click still fires.

  const press = (sel) => MOBILE ? page.tap(sel) : page.click(sel);

  const before = await page.locator('.cooknote').textContent();
  await press('#cookbtn');
  await page.waitForFunction(t => document.querySelector('.cooknote').textContent !== t, before);
  ok('cook button records a session',
    /Cooked \d+×/.test(await page.locator('.cooknote').textContent()));

  // Notes autosave is debounced; blur forces it. Reload to prove it persisted
  // rather than only reaching the DOM.
  // NB: clicking `.notes summary` TOGGLES the panel — force it open instead.
  await page.evaluate(() => { document.querySelector('.notes').open = true; });
  await page.fill('#comment', 'test note · ½ lemon');
  await page.locator('#comment').blur();
  // Wait for exactly "Saved" — the box shows "Editing…" first, so a
  // "has any text" wait resolves before the POST and the reload races it.
  await page.waitForFunction(
    () => document.getElementById('savedmsg')?.textContent === 'Saved',
    null, { timeout: 5000 });
  await page.reload();
  await page.waitForSelector('#comment');
  ok('note persists across reload',
    (await page.locator('#comment').inputValue()) === 'test note · ½ lemon');

  await noOverflow(page, 'recipe');
  await shot(page, '1-recipe');

  // ---------------------------------------------------------------- teardown
  // This writes into the shared auth.db, so put the recipe back as it was.
  // Going through the API is steadier than clicking every × away.
  const left = await page.evaluate(async id => {
    const m = (await fetch(`/api/recipes/${id}`).then(r => r.json())).marks;
    for (const e of (m.cooked || []))
      await fetch(`/api/recipes/${id}/cook/${e.id}`, { method: 'DELETE' });
    if (m.favorite)
      await fetch(`/api/recipes/${id}/favorite`, { method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ on: false }) });
    for (const t of (m.tags || []))
      await fetch(`/api/recipes/${id}/tags/${encodeURIComponent(t)}`, { method: 'DELETE' });
    if (m.comment)
      await fetch(`/api/recipes/${id}/comment`, { method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ text: '' }) });
    const after = (await fetch(`/api/recipes/${id}`).then(r => r.json())).marks;
    return { fav: after.favorite, tags: (after.tags || []).length,
             cooks: (after.cooked || []).length, note: after.comment || '' };
  }, recipeId);
  ok('teardown left no marks behind',
    !left.fav && left.tags === 0 && left.cooks === 0 && left.note === '',
    JSON.stringify(left));

  ok('no console errors', errors.length === 0, errors.join(' | '));

  console.log(`${MOBILE ? 'mobile' : 'desktop'} · recipe used: ${title} (#${recipeId})`);
  console.log(log.join('\n'));
  await browser.close();
  process.exit(log.some(l => l.startsWith('FAIL')) ? 1 : 0);
})();
