// Template for a browser test of the Cookbook Library UI. Copy to the
// scratchpad, replace the "feature under test" section, and run:
//
//   env BASE=http://127.0.0.1:8099 CB_USER=pwtest CB_PASS=... node uitest.js
//
// Exits non-zero if any check fails, so it can gate a change.
const { chromium } = require('playwright');

const BASE = process.env.BASE || 'http://127.0.0.1:8099';
const USER = process.env.CB_USER;          // a throwaway account, not the user's
const PASS = process.env.CB_PASS;          // from the environment, never in this file
const SHOT = process.env.SHOTDIR || '.';

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

(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });

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
  await page.goto(`${BASE}/#/recipes`);
  await page.waitForSelector('.rcard');
  const href = await page.locator('.rcard').first().getAttribute('href');
  const recipeId = href.split('/').pop();
  await page.goto(`${BASE}/${href}`);
  await page.waitForSelector('#cookbtn');
  const title = (await page.locator('h1').textContent()).trim();

  // ------------------------------------------------------ feature under test
  // Wait on the state that should change, not on a timeout. Every marks
  // endpoint answers with the recipe's updated marks and the panel redraws.

  const before = await page.locator('.cooknote').textContent();
  await page.click('#cookbtn');
  await page.waitForFunction(t => document.querySelector('.cooknote').textContent !== t, before);
  ok('cook button records a session',
    /Cooked \d+×/.test(await page.locator('.cooknote').textContent()));

  // Notes autosave is debounced; blur forces it. Reload to prove it persisted
  // rather than only reaching the DOM.
  // NB: clicking `.notes summary` TOGGLES the panel — force it open instead.
  await page.evaluate(() => { document.querySelector('.notes').open = true; });
  await page.fill('#comment', 'test note · ½ lemon');
  await page.locator('#comment').blur();
  await page.waitForFunction(
    () => document.getElementById('savedmsg')?.textContent === 'Saved',
    null, { timeout: 5000 }).catch(() => {});
  await page.reload();
  await page.waitForSelector('#comment');
  ok('note persists across reload',
    (await page.locator('#comment').inputValue()) === 'test note · ½ lemon');

  // A green run proves behaviour, not layout: always read these back. Park the
  // mouse in a dead corner first — it sits wherever the last click left it, and
  // a `:hover` row photographs exactly like a selected one.
  await page.mouse.move(1270, 10);
  await page.screenshot({ path: `${SHOT}/1-recipe.png` });

  // ---------------------------------------------------------------- teardown
  // This writes into the shared auth.db, so put the recipe back as it was.
  await page.goto(`${BASE}/#/recipe/${recipeId}`);
  await page.waitForSelector('#favbtn');
  if (await page.locator('#favbtn.on').count()) {
    await page.click('#favbtn');
    await page.waitForSelector('#favbtn:not(.on)');
  }
  for (const sel of ['.tagrow .tag .x', '.cooklog .chip .x']) {
    while (await page.locator(sel).count()) {
      const n = await page.locator(sel).count();
      await page.locator(sel).first().click();
      await page.waitForFunction(
        ([s, k]) => document.querySelectorAll(s).length === k - 1, [sel, n]);
    }
  }
  await page.evaluate(() => { document.querySelector('.notes').open = true; });
  await page.fill('#comment', '');
  await page.locator('#comment').blur();
  await page.waitForTimeout(1200);
  await page.reload();
  await page.waitForSelector('#favbtn');
  const left = await page.evaluate(() => ({
    fav: !!document.querySelector('#favbtn.on'),
    tags: document.querySelectorAll('.tagrow .tag').length,
    cooks: document.querySelectorAll('.cooklog .chip').length,
    note: document.getElementById('comment').value,
  }));
  ok('teardown left no marks behind',
    !left.fav && left.tags === 0 && left.cooks === 0 && left.note === '',
    JSON.stringify(left));

  ok('no console errors', errors.length === 0, errors.join(' | '));

  console.log(`recipe used: ${title} (#${recipeId})`);
  console.log(log.join('\n'));
  await browser.close();
  process.exit(log.some(l => l.startsWith('FAIL')) ? 1 : 0);
})();
