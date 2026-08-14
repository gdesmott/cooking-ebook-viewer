'use strict';

const view = document.getElementById('view');
const qInput = document.getElementById('q');
const PAGE = 60;

const esc = s => String(s ?? '').replace(/[&<>"']/g, c =>
  ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

// Server snippets contain <mark> and nothing else; escape everything, then
// let the highlight tags back through.
const snip = s => esc(s).replaceAll('&lt;mark&gt;', '<mark>').replaceAll('&lt;/mark&gt;', '</mark>');

const get = async url => {
  const r = await fetch(url);
  // A session can expire while the page is open; send the user to sign in
  // again rather than rendering an error into the list.
  if (r.status === 401) { location.href = '/login?next=' + encodeURIComponent(location.pathname + location.hash); throw new Error('signed out'); }
  if (!r.ok) throw new Error(await r.text());
  return r.json();
};

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
  return `<a class="rcard" href="#/recipe/${r.id}">
    ${img}
    <div>
      <div class="rt">${esc(r.title)}</div>
      <div class="rb">${esc(r.book)}${r.chapter ? ' · ' + esc(r.chapter) : ''}</div>
      ${meta ? `<div class="rb">${meta}</div>` : ''}
      ${r.snippet ? `<div class="rs">${snip(r.snippet)}</div>` : ''}
    </div>
  </a>`;
}

// ------------------------------------------------------------------ views

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
  stopPaging();
  loading();
  let offset = 0, done = false, busy = false;
  const url = () => {
    const p = new URLSearchParams({ limit: PAGE, offset });
    if (q) p.set('q', q);
    if (bookId) p.set('book', bookId);
    return '/api/search?' + p;
  };

  const first = await get(url());
  const heading = q
    ? `<h1>Results for “${esc(q)}”</h1>`
    : `<h1>All recipes</h1>`;

  view.innerHTML = `${heading}
    <p class="sub" id="count"></p>
    <div class="recipes" id="list"></div>
    <div class="sentinel" id="sentinel"></div>`;

  const list = document.getElementById('list');
  const count = document.getElementById('count');
  const sentinel = document.getElementById('sentinel');

  const render = items => {
    list.insertAdjacentHTML('beforeend', items.map(recipeCard).join(''));
    offset += items.length;
    count.textContent = `${offset}${items.length === PAGE ? '+' : ''} recipes`;
    if (items.length < PAGE) {
      done = true;
      sentinel.textContent = offset ? 'End of results' : '';
    }
  };

  if (!first.items.length) {
    count.textContent = 'No matches.';
    done = true;
  } else render(first.items);

  const loadMore = async () => {
    if (busy || done) return;
    busy = true;
    sentinel.textContent = 'Loading…';
    try {
      render((await get(url())).items);
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
    <div class="reader">
      <aside class="toc">
        <h2>${esc(b.title)}</h2>
        ${groups.map(g => `
          ${g.key ? `<div class="chap">${esc(g.key)}</div>` : ''}
          ${g.items.map(r => `<a data-doc="${esc(r.doc)}" data-anchor="${esc(r.anchor || '')}"
              href="#/book/${id}?r=${r.id}">${esc(r.title)}</a>`).join('')}`).join('')}
      </aside>
      <iframe class="frame" id="frame"></iframe>
    </div>`;

  const frame = document.getElementById('frame');
  const links = [...view.querySelectorAll('.toc a')];
  const open = a => {
    links.forEach(l => l.classList.toggle('on', l === a));
    const anchor = a.dataset.anchor ? '#' + a.dataset.anchor : '';
    frame.src = `/page/${id}/${a.dataset.doc}${anchor}`;
  };
  links.forEach(a => a.addEventListener('click', e => { e.preventDefault(); open(a); }));

  // Deep link: #/book/3?r=42 opens that recipe directly.
  const want = new URLSearchParams(location.hash.split('?')[1] || '').get('r');
  const target = want && links.find(l => l.getAttribute('href').endsWith('r=' + want));
  if (links.length) open(target || links[0]);
}

async function viewRecipe(id) {
  loading();
  const r = await get(`/api/recipes/${id}`);
  const anchor = r.anchor ? '#' + r.anchor : '';
  view.innerHTML = `
    <div class="crumb">
      <a href="#/">Books</a> ›
      <a href="#/book/${r.book_id}">${esc(r.book)}</a>
      ${r.chapter ? ' › ' + esc(r.chapter) : ''}
    </div>
    <h1>${esc(r.title)}</h1>
    <p class="sub">${[r.yield, r.time].filter(Boolean).map(esc).join(' · ')}</p>
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
  void anchor;
}

// ----------------------------------------------------------------- router

async function route() {
  stopPaging();
  const hash = location.hash.replace(/^#/, '') || '/';
  const [path] = hash.split('?');
  const parts = path.split('/').filter(Boolean);

  document.querySelectorAll('.navlinks a').forEach(a =>
    a.classList.toggle('on', a.getAttribute('href') === '#/' + (parts[0] || '')));

  try {
    if (!parts.length) return await viewBooks();
    if (parts[0] === 'recipes') return await viewRecipes('');
    if (parts[0] === 'search') {
      const q = decodeURIComponent(parts[1] || '');
      qInput.value = q;
      return await viewRecipes(q);
    }
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
