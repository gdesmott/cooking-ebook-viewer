---
name: ui-test
description: >-
  How to drive this app's frontend in a real browser — phone viewport by
  default, desktop on request: stand up a throwaway server and account, run a
  Playwright script against it, read the screenshots, and tear it all down
  without touching the user's running server or their account data. Use
  whenever a change to static/app.js, static/app.css, static/index.html or any
  /api route needs checking in a browser rather than with curl — and whenever
  asked to "test the UI", "check it in a browser", "check it on mobile",
  "screenshot the app", or "use playwright" here.
---

# Testing the UI in a browser

There is no test suite in this repo; UI verification is a browser run you set up
yourself. The whole procedure is below, in order. Budget five minutes for the
first run (a browser download) and under one for later ones.

**Runs default to phone emulation** (iPhone 13, 390×844, touch). That is the
layout that breaks — `app.css` has one breakpoint, `@media (max-width: 820px)`,
and everything below it is a different app: no nav, single-column reader,
stacked detail. `UI_DEVICE=desktop` gives the old 1280×800 window. Run both
whenever a change touches `static/app.css` or any layout in `app.js`; mobile
alone is the right default for everything else. Section 8 has what differs.

## 1. Get a browser

There is **no Chrome or Chromium on this machine**, and the Playwright MCP is
configured as plain `npx @playwright/mcp@latest`, which defaults to
`channel: chrome` and fails with:

```
Chromium distribution 'chrome' is not found at /opt/google/chrome/chrome
```

Do not try to install Chrome (it needs root on this rpm-ostree system). Use
Playwright's own Chromium and drive it from a script instead:

```sh
npx --yes playwright@latest install chromium     # ~115 MB, cached in ~/.cache/ms-playwright
cd "$SCRATCH" && npm install playwright          # the module, not the browser
```

`$SCRATCH` is this session's scratchpad directory. Both steps are idempotent and
no-ops once done. The MCP browser tools stay unusable either way — everything
here goes through `node`.

If the user wants the MCP tools working long-term, the fix is
`"args": ["@playwright/mcp@latest", "--browser", "chromium"]` in the plugin's
`.mcp.json` plus a Claude Code restart. Mention it; don't do it mid-session.

## 2. Build first — `static/` is embedded

`app.js`, `app.css` and `index.html` are compiled in with `include_str!`.
Editing them changes nothing until `cargo build --release`, and restarting a
server built from older sources changes nothing either. Always:

```sh
cargo build --release
```

## 3. Stand up your own server — never disturb the user's

The user usually has a server running on `127.0.0.1:8080`. Leave it alone: don't
kill it, don't restart it, and don't assume it runs your build (check with
`ps -eo pid,lstart,args | grep [c]ooking-ebook-viewer` — compare its start time
to the binary's mtime).

Turso holds an **exclusive lock on `library.db`** for the life of a connection,
so a second server cannot open the same file. Copy it — both files:

```sh
cp library.db "$SCRATCH/lib.db"
cp library.db-wal "$SCRATCH/lib.db-wal"     # the -wal is not optional
```

`auth.db` is plain SQLite in WAL mode and *is* safe to share between processes —
that is the point of the split. Point your server at the real one so accounts
and marks behave exactly as in production:

```sh
cd "$SCRATCH" && "$REPO/target/release/cooking-ebook-viewer" \
  serve --db lib.db --auth-db "$REPO/auth.db" --addr 127.0.0.1:8099
```

Run it in the background and check its first line. `Serving 61 books on …` means
good; a `WARNING: N recipes share a stable key` line is a real finding worth
reporting.

## 4. Make a temp account, never use the user's

Ask before using someone's real account: the password would land in the
transcript and the test would write real cook-log entries and tags into their
data. Default to a throwaway, which works **while the server runs** (that is why
`auth.db` is SQLite):

```sh
echo 'pw-temp-test-9f2a' | ./target/release/cooking-ebook-viewer user add pwtest
```

Piping the password skips the interactive double prompt. Delete it at the end
with `user rm pwtest`, which also deletes that account's marks, cook log and
tags — verify with:

```sh
python3 -c "import sqlite3;c=sqlite3.connect('auth.db');print([(t,c.execute(f'select count(*) from {t}').fetchone()[0]) for t in ['users','recipe_marks','cook_log','recipe_tags']])"
```

(Stock `sqlite3` can read `auth.db` but **not** `library.db` — the FTS index
makes it unopenable. Use the API for library data.)

## 5. Write the test

Start from `harness.js` next to this file: it signs in, exercises one feature,
screenshots, cleans up after itself and exits non-zero on failure. Copy it into
`$SCRATCH`, replace the middle section, and run:

```sh
cd "$SCRATCH" && env BASE=http://127.0.0.1:8099 CB_USER=pwtest \
  CB_PASS=pw-temp-test-9f2a node uitest.js                     # phone
cd "$SCRATCH" && env UI_DEVICE=desktop BASE=http://127.0.0.1:8099 \
  CB_USER=pwtest CB_PASS=pw-temp-test-9f2a node uitest.js      # 1280×800
```

Points that matter here:

- **Credentials come from the environment**, never from the file.
- **Fail the run on console errors.** Collect `console` (type `error`) and
  `pageerror`; the frontend swallows fetch failures into the view, so a silent
  console is the only proof nothing broke.
- **Wait on state, not on time.** `page.waitForFunction` against the text that
  should change; the marks endpoints answer with the updated marks and the panel
  redraws, so there is always something concrete to await.
- **Never wait on a selector that also matches the loading placeholder.** Every
  paged view paints `<p class="loading">Loading…</p>` first, so
  `waitForSelector('.rcard, .loading')` resolves immediately and the count that
  follows is 0 — a green-looking test that reports "search returns 0 hits". Use
  the `settled()` helper in `harness.js`, which waits for that placeholder to
  clear. Empty results are *not* `.loading`; they set the count line to
  "Nothing here yet.", so `settled()` resolves for them too.
- **After a view change, wait for something only the *new* view has.** The
  frontend replaces `#view` in place, so the old view's nodes stay on screen
  until the fetch resolves. `waitForSelector('.rcard')` after searching from
  `#/recipes` matches the 60 cards already there and the assertions then
  describe the previous page — a two-word search "returned" 60 unhighlighted
  hits when the API had correctly ANDed it down to 3. Wait on the heading:

  ```js
  await page.waitForFunction(
    () => /Results for/.test(document.querySelector('#view h1')?.textContent || ''));
  ```

  `settled()` does not save you here: the placeholder has already cleared.
  Check the heading each view actually renders (`viewRecipes` → `Results for
  “…”`, `viewHistory` → `Cooking history`, …), and note that history rows are
  `.hrow`, not `.rcard`.
- **Navigate by hash on a phone, by nav click on desktop.** `.navlinks` is
  `display: none` under 820px, so `page.click('.navlinks a[href="#/history"]')`
  times out on mobile — use `page.goto('#/history')` and then
  `window.scrollTo(0, 0)`, because a hash goto restores the old scroll offset.
  The `go()` helper in `harness.js` picks the right one and resets the scroll.
- **Wait for exactly `Saved`, not for any text, after typing a note.** The
  savebox shows `Editing…` first, so a "has any text" wait resolves before the
  POST lands and the following `reload()` races it away. The failure surfaces
  later and looks unrelated: `#comment` is hidden after the reload (the panel
  only renders `open` when a note exists) and the run dies on a timeout waiting
  for a textarea that is present but collapsed.
- **Exercise infinite scroll on `#/recipes`, not on a search.** A query with one
  page of hits passes `after >= before` trivially. The full index really has a
  second page: 60 → 120 — the same on a phone, where the cards are one per row.
- **Assert there is no horizontal overflow** on every view you visit:
  `document.documentElement.scrollWidth - window.innerWidth <= 1`. It is the
  classic phone break and a screenshot will not show it — the page just looks
  cropped, exactly like a correct narrow layout. `noOverflow()` in `harness.js`.
- **The listing APIs return `{items: [...]}`, not a bare array.** `/api/history`,
  `/api/favorites`, `/api/tags` and `/api/tags/{tag}` all wrap; `.length` on the
  response is `undefined`, which compares false against 0 and fails a teardown
  assertion that actually succeeded.
- **Undo everything at the end** and assert the recipe is back to zero marks.
  The test writes into the shared `auth.db`.
- Watch out for `<details class="notes">`: clicking `summary` *toggles* it, so a
  cleanup pass that clicks it closes the panel and `fill('#comment')` then times
  out on an invisible element. Set `.open = true` via `evaluate` instead.

## 6. Look at the screenshots

**A green run does not mean the UI looks right.** Read the PNGs with the Read
tool. The first browser run of the marks feature passed every assertion while
the history rows were visibly mangled — squeezed into the card grid, titles
wrapping to three lines. Assertions catch behaviour; only looking catches
layout.

Seed a realistic amount of data before the screenshots — one row hides layout
problems. Log in with `curl -c` and POST a handful of entries with different
`at` timestamps, then screenshot `#/history`, `#/favorites`, `#/tags` and a
recipe page — on the phone viewport, and at 1280×800 if the change touches CSS.

**Phone screenshots are where the layout bugs actually are.** A mobile pass with
every assertion green still photographed `#/history` with each row's card
crushed to about a third of the width, titles breaking one word per line and
"Remove" printed on top of the book name. Nothing in the DOM said so: the row's
own width fitted the viewport, so a width assertion passed. Only the picture
showed it.

Pin `deviceScaleFactor: 2` on the phone context. `devices['iPhone 13']` sets 3,
which yields 1170×2532 PNGs with no more legible detail.

Emoji render as tofu boxes in this headless Chromium (no emoji font). The 🥕 in
the header does too, and it is fine in a real browser — do not "fix" it.

Ways a screenshot lies, each of which has cost a round of false findings:

- **`page.goto()` on a hash URL restores that URL's old scroll position.** It is
  a same-document navigation, so the browser puts back where you were and the
  shot comes out mid-page, heading cut off — which reads as "the app doesn't
  scroll to top between views". It does; `route()` resets it on a real
  navigation. Reach views the way a user does — `page.click('.navlinks
  a[href="#/history"]')` — or `window.scrollTo(0, 0)` before the shot. On a
  phone there is no nav to click, so the `scrollTo` is not optional. Verify
  `window.scrollY` before reporting a scroll bug.
- **Thumbnails are still loading the instant the rows appear.** Screenshot right
  after `waitForSelector('.hrow')` and every cover is a blank placeholder, which
  looks like missing-image extraction. The boxes are correctly sized so nothing
  reflows; give it a beat, or re-shoot, before calling an image missing.
- **The mouse stays where the last click left it**, and a `:hover` row is tinted
  exactly like a selected one — a reader screenshot appeared to highlight the
  wrong TOC entry. `await page.mouse.move(1270, 10)` before every screenshot.
  Not needed under phone emulation: `tap()` leaves no hover behind, and the
  park coordinate is off-screen at 390px anyway.
- **An iframe can photograph blank while holding content.** `/page` renders in a
  sandboxed iframe that paints a beat after it loads. Before believing an empty
  pane, check the DOM: `frame.contentDocument.body.textContent.length` and the
  `src`. Assert on those; treat the picture as secondary for iframe content.

## 7. What differs below 820px

`static/app.css` has exactly one layout breakpoint, `@media (max-width: 820px)`,
and it changes four things. Everything below was confirmed in a browser at
390×844, not read off the stylesheet.

- **`.navlinks { display: none }`; a fixed `.tabbar` replaces it.** The same
  five destinations sit in a bottom bar (`index.html`, hidden on desktop), so
  navigate with `.tabbar a[href="#/history"]` on a phone and `.navlinks a[…]`
  on desktop — the `go()` helper picks. `body` gets a matching `padding-bottom`
  so the bar never covers the last row; assert that geometrically
  (`lastCard.bottom <= tabbar.top`) if you touch the bar or the padding.
  The header wraps to two rows below the breakpoint: brand + sign-out, then the
  search box full width.
- **`.signout` is a sibling of `.navlinks`, not a child** — deliberately. It
  used to live inside it, which meant the breakpoint hid sign-out and `#whoami`
  along with the nav and left phone users unable to log out. If you move it
  back under `.navlinks`, that regresses; the check is
  `reachable('.signout button')` at 390px.
- **`.reader` collapses to one column**, TOC above the frame (`max-height:
  40vh`) and `.frame` at `75vh`. Assert the stacking geometrically
  (`frame.top >= toc.bottom`) rather than trusting the picture — see the iframe
  caveat above.
- **`.detail` collapses to one column**, so the hero image, ingredients and
  method run in sequence and the recipe page gets long. The marks bar, tag row,
  cook-log chips and notes panel all keep working and stay within the viewport.
- **`.rcard` grid becomes one card per row**, and `.book` two per row. Infinite
  scroll still pages 60 → 120.

Two things a mobile pass reliably finds that a desktop pass cannot:

- **Tap-target size.** The breakpoint raises `.mbtn`, `.tagadd input`,
  `.linkbtn` and the sign-out button to a 44px minimum; they are 36px, 25px and
  27px on desktop, which is fine for a mouse. `boundingBox().height` is the
  check — and note a padding change alone does not do it, `min-height` does.
- **`.hrow` still has no mobile rule.** It keeps the desktop
  date-gutter/card/Remove grid at 390px, which mangles `#/history`: the card is
  crushed to about a third of the width and "Remove" prints over the book name.
  Known and unfixed — do not report it as new.

Use `page.tap()` rather than `page.click()` once the context has touch — a
handler bound to `click` still fires, and the test then matches what a phone
user does.

## 8. Tear down

Stop your server (`TaskStop` on the background task), `user rm` the temp
account, verify the counts are back where they started, and say plainly what you
left behind. Scratchpad files can stay.
