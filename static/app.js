'use strict';

const view = document.getElementById('view');
const qInput = document.getElementById('q');
const PAGE = 60;

const esc = s => String(s ?? '').replace(/[&<>"']/g, c =>
  ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

// Server snippets contain <mark> and nothing else; escape everything, then
// let the highlight tags back through.
const snip = s => esc(s).replaceAll('&lt;mark&gt;', '<mark>').replaceAll('&lt;/mark&gt;', '</mark>');

const fmtDate = ts => new Date(ts * 1000).toLocaleDateString(undefined,
  { day: 'numeric', month: 'short', year: 'numeric' });

const get = async (url, opts) => {
  const r = await fetch(url, opts);
  // A session can expire while the page is open; send the user to sign in
  // again rather than rendering an error into the list.
  if (r.status === 401) { location.href = '/login?next=' + encodeURIComponent(location.pathname + location.hash); throw new Error('signed out'); }
  if (!r.ok) throw new Error(await r.text());
  return r.json();
};

// Marks endpoints all answer with the recipe's updated marks, so every write
// doubles as a refresh and the UI never has to guess the new state.
const send = (url, method, body) => get(url, {
  method,
  headers: body === undefined ? {} : { 'content-type': 'application/json' },
  body: body === undefined ? undefined : JSON.stringify(body),
});

const loading = () => { view.innerHTML = '<p class="loading">Loading…</p>'; };

// Infinite scroll for the recipe list; torn down whenever the route changes so
// a stale observer can't append into the next view.
let pageObserver = null;
const stopPaging = () => {
  if (pageObserver) { pageObserver.disconnect(); pageObserver = null; }
};

function recipeCard(r) {
  const img = r.image
    ? `<img src="/asset/${r.book_id}/${r.image}" loading="lazy" alt="">`
    : `<img src="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg'/%3E" alt="">`;
  const meta = [r.yield, r.time].filter(Boolean).map(x => `<span class="pill">${esc(x)}</span>`).join('');
  const badges = [
    r.fav ? '<span class="badge" title="Favourite">♥</span>' : '',
    r.cooked ? '<span class="badge" title="You have cooked this">🍳</span>' : '',
  ].join('');
  return `<a class="rcard" href="#/recipe/${r.id}">
    ${img}
    <div>
      <div class="rt">${esc(r.title)}${badges}</div>
      <div class="rb">${esc(r.book)}${r.chapter ? ' · ' + esc(r.chapter) : ''}</div>
      ${meta ? `<div class="rb">${meta}</div>` : ''}
      ${r.snippet ? `<div class="rs">${snip(r.snippet)}</div>` : ''}
    </div>
  </a>`;
}

// A cook-log entry is a card plus when it was cooked, and a way to undo a
// mis-click.
function historyCard(r) {
  return `<div class="hrow">
    <div class="hdate">${fmtDate(r.cooked_at)}</div>
    ${recipeCard(r)}
    <button class="linkbtn" data-entry="${r.entry_id}" data-recipe="${r.id}">Remove</button>
  </div>`;
}

// ------------------------------------------------------------------ views

/// Shared infinite-scroll list. `url(offset)` returns the page to fetch.
async function pagedList({ heading, url, card = recipeCard, unit = 'recipes', empty = 'Nothing here yet.', listClass = 'recipes' }) {
  stopPaging();
  loading();
  let offset = 0, done = false, busy = false;

  const first = await get(url(0));

  view.innerHTML = `${heading}
    <p class="sub" id="count"></p>
    <div class="${listClass}" id="list"></div>
    <div class="sentinel" id="sentinel"></div>`;

  const list = document.getElementById('list');
  const count = document.getElementById('count');
  const sentinel = document.getElementById('sentinel');

  const render = items => {
    list.insertAdjacentHTML('beforeend', items.map(card).join(''));
    offset += items.length;
    count.textContent = `${offset}${items.length === PAGE ? '+' : ''} ${unit}`;
    if (items.length < PAGE) {
      done = true;
      sentinel.textContent = offset ? 'End of results' : '';
    }
  };

  if (!first.items.length) {
    count.textContent = empty;
    done = true;
  } else render(first.items);

  const loadMore = async () => {
    if (busy || done) return;
    busy = true;
    sentinel.textContent = 'Loading…';
    try {
      render((await get(url(offset))).items);
      if (!done) sentinel.textContent = '';
    } catch (e) {
      done = true;
      sentinel.textContent = `Error: ${e.message}`;
    }
    busy = false;
  };

  // Fetch ahead of the viewport so scrolling stays smooth. Re-fires while the
  // sentinel is still visible, which also fills a short first page.
  pageObserver = new IntersectionObserver(
    entries => { if (entries.some(e => e.isIntersecting)) loadMore(); },
    { rootMargin: '600px' });
  pageObserver.observe(sentinel);

  return list;
}

async function viewBooks() {
  loading();
  const books = await get('/api/books');
  const total = books.reduce((n, b) => n + b.n_recipes, 0);
  view.innerHTML = `
    <h1>Your cookbooks</h1>
    <p class="sub">${books.length} books · ${total.toLocaleString()} recipes indexed</p>
    <div class="books">
      ${books.map(b => `
        <a class="book" href="#/book/${b.id}">
          <img class="cover" loading="lazy" src="/cover/${b.id}" alt="">
          <div class="bt">${esc(b.title)}</div>
          <div class="ba">${esc(b.author || '')} · ${b.n_recipes} recipes</div>
        </a>`).join('')}
    </div>`;
}

async function viewRecipes(q = '', bookId = null) {
  await pagedList({
    heading: q ? `<h1>Results for “${esc(q)}”</h1>` : `<h1>All recipes</h1>`,
    empty: 'No matches.',
    url: offset => {
      const p = new URLSearchParams({ limit: PAGE, offset });
      if (q) p.set('q', q);
      if (bookId) p.set('book', bookId);
      return '/api/search?' + p;
    },
  });
}

async function viewFavorites() {
  await pagedList({
    heading: '<h1>Favourites</h1>',
    empty: 'Nothing loved yet — open a recipe and hit ♥.',
    url: offset => `/api/favorites?limit=${PAGE}&offset=${offset}`,
  });
}

async function viewHistory() {
  const list = await pagedList({
    heading: '<h1>Cooking history</h1>',
    empty: 'No cooking recorded yet — open a recipe and hit “Cooked it”.',
    unit: 'sessions',
    card: historyCard,
    // One session per row, not a card grid: the date belongs beside the recipe.
    listClass: 'hlist',
    url: offset => `/api/history?limit=${PAGE}&offset=${offset}`,
  });

  list.addEventListener('click', async e => {
    const btn = e.target.closest('.linkbtn');
    if (!btn) return;
    e.preventDefault();
    btn.disabled = true;
    await send(`/api/recipes/${btn.dataset.recipe}/cook/${btn.dataset.entry}`, 'DELETE');
    btn.closest('.hrow').remove();
    const count = document.getElementById('count');
    const left = list.querySelectorAll('.hrow').length;
    count.textContent = left ? `${left} sessions` : 'No cooking recorded yet.';
  });
}

async function viewTags() {
  loading();
  const { items } = await get('/api/tags');
  view.innerHTML = `
    <h1>Your tags</h1>
    <p class="sub">${items.length ? `${items.length} tags` : 'No tags yet — add one from any recipe.'}</p>
    <div class="tagcloud">
      ${items.map(t => `<a class="tag" href="#/tag/${encodeURIComponent(t.tag)}">
        ${esc(t.tag)}<span class="n">${t.count}</span></a>`).join('')}
    </div>`;
}

async function viewTag(tag) {
  await pagedList({
    heading: `<div class="crumb"><a href="#/tags">Tags</a></div><h1>#${esc(tag)}</h1>`,
    empty: 'Nothing tagged with this.',
    url: offset => `/api/tags/${encodeURIComponent(tag)}?limit=${PAGE}&offset=${offset}`,
  });
}

async function viewBook(id) {
  loading();
  const b = await get(`/api/books/${id}/toc`);

  // Group recipes under their chapter heading, preserving book order.
  const groups = [];
  for (const r of b.recipes) {
    const key = r.chapter || '';
    if (!groups.length || groups.at(-1).key !== key) groups.push({ key, items: [] });
    groups.at(-1).items.push(r);
  }

  view.innerHTML = `
    <div class="crumb"><a href="#/">Books</a> › ${esc(b.title)}</div>
    <div class="readerbar">
      <h2 class="rtitle" id="rtitle"></h2>
      <a class="btn" id="fullrecipe" href="#/">Full recipe →</a>
    </div>
    <div id="marks" class="marks"></div>
    <div class="reader">
      <aside class="toc">
        <h2>${esc(b.title)}</h2>
        ${groups.map(g => `
          ${g.key ? `<div class="chap">${esc(g.key)}</div>` : ''}
          ${g.items.map(r => `<a data-doc="${esc(r.doc)}" data-anchor="${esc(r.anchor || '')}"
              data-id="${r.id}"
              href="#/book/${id}?r=${r.id}">${esc(r.title)}</a>`).join('')}`).join('')}
      </aside>
      <iframe class="frame" id="frame"></iframe>
    </div>`;

  const frame = document.getElementById('frame');
  const links = [...view.querySelectorAll('.toc a')];

  // The marks panel follows the TOC selection. Clicking an entry does not
  // change the hash, so nothing else would redraw it. A sequence number keeps
  // a slow fetch for an earlier entry from painting over a later one.
  let seq = 0;
  const showMarks = async rid => {
    const mine = ++seq;
    const r = await get(`/api/recipes/${rid}`);
    if (mine !== seq) return;
    document.getElementById('rtitle').textContent = r.title;
    document.getElementById('fullrecipe').href = `#/recipe/${rid}`;
    renderMarks(rid, r.marks, { compact: true });
  };

  const open = (a, reveal = false) => {
    links.forEach(l => l.classList.toggle('on', l === a));
    const anchor = a.dataset.anchor ? '#' + a.dataset.anchor : '';
    frame.src = `/page/${id}/${a.dataset.doc}${anchor}`;
    showMarks(a.dataset.id);
    // Deep-linking to a recipe late in the book otherwise leaves the TOC at the
    // top, so the reader looks like it opened the wrong entry. Only on arrival:
    // scrolling under a click the reader just made would yank the list away.
    if (reveal) a.scrollIntoView({ block: 'center' });
  };
  links.forEach(a => a.addEventListener('click', e => { e.preventDefault(); open(a); }));

  // Deep link: #/book/3?r=42 opens that recipe directly.
  const want = new URLSearchParams(location.hash.split('?')[1] || '').get('r');
  const target = want && links.find(l => l.getAttribute('href').endsWith('r=' + want));
  if (links.length) open(target || links[0], !!target);
}

async function viewRecipe(id) {
  loading();
  const r = await get(`/api/recipes/${id}`);
  view.innerHTML = `
    <div class="rhead">
      <div class="crumb">
        <a href="#/">Books</a> ›
        <a href="#/book/${r.book_id}">${esc(r.book)}</a>
        ${r.chapter ? ' › ' + esc(r.chapter) : ''}
      </div>
      <h1>${esc(r.title)}</h1>
      <p class="sub">${[r.yield, r.time].filter(Boolean).map(esc).join(' · ')}</p>
    </div>
    <div id="marks" class="marks"></div>
    <div class="detail">
      <div>
        ${r.image ? `<img class="hero" src="/asset/${r.book_id}/${r.image}" alt="">` : ''}
        <h2>Ingredients</h2>
        <ul>${r.ingredients.map(i => `<li>${esc(i)}</li>`).join('')}</ul>
        <a class="btn" href="#/book/${r.book_id}?r=${r.id}">Open in the book →</a>
      </div>
      <div class="steps">
        <h2>Method</h2>
        ${r.body.split('\n\n').filter(Boolean).map(p => `<p>${esc(p)}</p>`).join('')}
      </div>
    </div>`;

  renderMarks(id, r.marks);
}

// ------------------------------------------------------------------ marks

/// The whole marks panel is redrawn from whatever the server last returned,
/// so there is one source of truth and no local state to drift.
///
/// `compact` trims it for the reader, where every row it adds pushes the page
/// itself down: no cook-log chips (the full recipe page lists them) and notes
/// stay shut rather than springing open whenever one exists.
function renderMarks(id, m, { compact = false } = {}) {
  const box = document.getElementById('marks');
  if (!box) return;

  const times = m.cooked.length;
  const last = times ? `last on ${fmtDate(m.cooked[0].at)}` : 'never cooked yet';

  box.innerHTML = `
    <div class="markbar">
      <button class="mbtn cook" id="cookbtn">🍳 Cooked it</button>
      <button class="mbtn fav${m.favorite ? ' on' : ''}" id="favbtn"
              aria-pressed="${m.favorite}">${m.favorite ? '♥' : '♡'} Favourite</button>
      <span class="cooknote">${times ? `Cooked ${times}×` : 'Not cooked'} · ${last}</span>
    </div>
    <div class="tagrow">
      ${m.tags.map(t => `<span class="tag">
        <a href="#/tag/${encodeURIComponent(t)}">${esc(t)}</a>
        <button class="x" data-tag="${esc(t)}" title="Remove tag">×</button></span>`).join('')}
      <form class="tagadd" id="tagform">
        <input id="taginput" placeholder="+ tag" maxlength="40" autocomplete="off">
      </form>
    </div>
    ${compact ? '' : `<div class="cooklog">${m.cooked.map(c =>
      `<span class="chip">${fmtDate(c.at)}<button class="x" data-entry="${c.id}"
        title="Remove this session">×</button></span>`).join('')}</div>`}
    <details class="notes"${m.comment && !compact ? ' open' : ''}>
      <summary>Notes${m.comment ? '' : ' — none yet'}</summary>
      <textarea id="comment" rows="4"
        placeholder="Tweaks, timings, who liked it…">${esc(m.comment)}</textarea>
      <span class="saved" id="savedmsg"></span>
    </details>`;

  const refresh = updated => renderMarks(id, updated, { compact });

  document.getElementById('cookbtn').addEventListener('click', async () => {
    refresh(await send(`/api/recipes/${id}/cook`, 'POST', {}));
  });

  document.getElementById('favbtn').addEventListener('click', async () => {
    refresh(await send(`/api/recipes/${id}/favorite`, 'POST', { on: !m.favorite }));
  });

  box.querySelectorAll('.tag .x').forEach(b => b.addEventListener('click', async () => {
    refresh(await send(`/api/recipes/${id}/tags/${encodeURIComponent(b.dataset.tag)}`, 'DELETE'));
  }));

  document.getElementById('tagform').addEventListener('submit', async e => {
    e.preventDefault();
    const input = document.getElementById('taginput');
    const tag = input.value.trim();
    if (!tag) return;
    input.value = '';
    refresh(await send(`/api/recipes/${id}/tags`, 'POST', { tag }));
  });

  // Notes save on a pause in typing, and again on blur so nothing is lost by
  // navigating away mid-sentence.
  const area = document.getElementById('comment');
  const msg = document.getElementById('savedmsg');
  let saveTimer, lastSaved = m.comment;
  const save = async () => {
    if (area.value === lastSaved) return;
    lastSaved = area.value;
    await send(`/api/recipes/${id}/comment`, 'POST', { text: area.value });
    msg.textContent = 'Saved';
    setTimeout(() => { msg.textContent = ''; }, 1500);
  };
  area.addEventListener('input', () => {
    clearTimeout(saveTimer);
    msg.textContent = 'Editing…';
    saveTimer = setTimeout(save, 800);
  });
  area.addEventListener('blur', () => { clearTimeout(saveTimer); save(); });

  // The cook log doubles as an undo list for accidental taps.
  box.querySelectorAll('.cooklog .x').forEach(b => b.addEventListener('click', async () => {
    refresh(await send(`/api/recipes/${id}/cook/${b.dataset.entry}`, 'DELETE'));
  }));
}

// ----------------------------------------------------------------- router

async function route() {
  stopPaging();
  const hash = location.hash.replace(/^#/, '') || '/';
  const [path] = hash.split('?');
  const parts = path.split('/').filter(Boolean);

  // Both navs: .navlinks on desktop, .tabbar on a phone. They carry the same
  // five hrefs, so one pass marks whichever is on screen.
  document.querySelectorAll('.navlinks a, .tabbar a').forEach(a =>
    a.classList.toggle('on', a.getAttribute('href') === '#/' + (parts[0] || '')));

  // Set here rather than in each view: the views assign `view.innerHTML`
  // directly, so a class one of them set would leak into the next one.
  view.className = 'view-' + (parts[0] || 'books');

  try {
    if (!parts.length) return await viewBooks();
    if (parts[0] === 'recipes') return await viewRecipes('');
    if (parts[0] === 'search') {
      const q = decodeURIComponent(parts[1] || '');
      qInput.value = q;
      return await viewRecipes(q);
    }
    if (parts[0] === 'favorites') return await viewFavorites();
    if (parts[0] === 'history') return await viewHistory();
    if (parts[0] === 'tags') return await viewTags();
    if (parts[0] === 'tag') return await viewTag(decodeURIComponent(parts[1] || ''));
    if (parts[0] === 'book') return await viewBook(parts[1]);
    if (parts[0] === 'recipe') return await viewRecipe(parts[1]);
    view.innerHTML = '<p class="loading">Not found.</p>';
  } catch (e) {
    view.innerHTML = `<p class="loading">Error: ${esc(e.message)}</p>`;
  }
}

let timer;
qInput.addEventListener('input', () => {
  clearTimeout(timer);
  timer = setTimeout(() => {
    const q = qInput.value.trim();
    location.hash = q ? '#/search/' + encodeURIComponent(q) : '#/recipes';
  }, 250);
});
document.getElementById('searchform').addEventListener('submit', e => e.preventDefault());

window.addEventListener('hashchange', route);
route();

get('/api/me')
  .then(me => { document.getElementById('whoami').textContent = me.username; })
  .catch(() => {});
