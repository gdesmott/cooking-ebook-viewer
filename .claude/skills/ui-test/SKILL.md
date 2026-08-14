---
name: ui-test
description: >-
  How to drive this app's frontend in a real browser: stand up a throwaway
  server and account, run a Playwright script against it, read the screenshots,
  and tear it all down without touching the user's running server or their
  account data. Use whenever a change to static/app.js, static/app.css,
  static/index.html or any /api route needs checking in a browser rather than
  with curl — and whenever asked to "test the UI", "check it in a browser",
  "screenshot the app", or "use playwright" here.
---

# Testing the UI in a browser

There is no test suite in this repo; UI verification is a browser run you set up
yourself. The whole procedure is below, in order. Budget five minutes for the
first run (a browser download) and under one for later ones.

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
  CB_PASS=pw-temp-test-9f2a node uitest.js
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
- **Exercise infinite scroll on `#/recipes`, not on a search.** A query with one
  page of hits passes `after >= before` trivially. The full index really has a
  second page: 60 → 120.
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
recipe page at 1280×800.

Emoji render as tofu boxes in this headless Chromium (no emoji font). The 🥕 in
the header does too, and it is fine in a real browser — do not "fix" it.

Ways a screenshot lies, each of which has cost a round of false findings:

- **`page.goto()` on a hash URL restores that URL's old scroll position.** It is
  a same-document navigation, so the browser puts back where you were and the
  shot comes out mid-page, heading cut off — which reads as "the app doesn't
  scroll to top between views". It does; `route()` resets it on a real
  navigation. Reach views the way a user does — `page.click('.navlinks
  a[href="#/history"]')` — or `window.scrollTo(0, 0)` before the shot. Verify
  `window.scrollY` before reporting a scroll bug.
- **Thumbnails are still loading the instant the rows appear.** Screenshot right
  after `waitForSelector('.hrow')` and every cover is a blank placeholder, which
  looks like missing-image extraction. The boxes are correctly sized so nothing
  reflows; give it a beat, or re-shoot, before calling an image missing.
- **The mouse stays where the last click left it**, and a `:hover` row is tinted
  exactly like a selected one — a reader screenshot appeared to highlight the
  wrong TOC entry. `await page.mouse.move(1270, 10)` before every screenshot.
- **An iframe can photograph blank while holding content.** `/page` renders in a
  sandboxed iframe that paints a beat after it loads. Before believing an empty
  pane, check the DOM: `frame.contentDocument.body.textContent.length` and the
  `src`. Assert on those; treat the picture as secondary for iframe content.

## 7. Tear down

Stop your server (`TaskStop` on the background task), `user rm` the temp
account, verify the counts are back where they started, and say plainly what you
left behind. Scratchpad files can stay.
