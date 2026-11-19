// The page of chappe-bus. The service sends an event for every change:
//   msg:<project>/<topic>  a message was stored
//   board                  the registry changed (who is working)
//   nav                    a project or topic appeared
//   (empty)                events were lost, fetch everything again.
// The page fetches the part it shows again. Without JavaScript every form still
// works as a plain form: reload the page to see news.
(() => {
  const main = document.querySelector('.main');
  if (!main) return;
  const project = main.dataset.project;
  const topic = main.dataset.topic;
  const feed = document.getElementById('messages');
  const people = document.getElementById('people');
  const board = document.getElementById('board');
  const navBox = document.getElementById('nav');
  const composer = document.querySelector('.composer');
  const box = composer && composer.querySelector('textarea');

  const localise = (root) => {
    root.querySelectorAll('time[datetime]').forEach((t) => {
      const d = new Date(t.getAttribute('datetime'));
      if (!isNaN(d)) t.textContent = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', hour12: false });
    });
  };
  const nearBottom = () => feed && feed.scrollHeight - feed.scrollTop - feed.clientHeight < 80;
  const toBottom = () => { if (feed) feed.scrollTop = feed.scrollHeight; };
  const get = async (url) => {
    try {
      const r = await fetch(url);
      return r.ok ? await r.text() : null;
    } catch (_) { return null; /* the next event or reconnect retries */ }
  };

  const refreshFeed = async () => {
    if (!feed) return;
    const html = await get('/p/' + encodeURIComponent(project) + '/' + encodeURIComponent(topic) + '/messages');
    if (html === null) return;
    const stick = nearBottom();
    feed.innerHTML = html;
    localise(feed);
    if (stick) toBottom();
  };
  const refreshPeople = async () => {
    if (!people || !project) return;
    const html = await get('/p/' + encodeURIComponent(project) + '/people');
    if (html !== null) people.innerHTML = html;
  };
  const refreshBoard = async () => {
    if (!board) return;
    const html = await get('/board');
    if (html !== null) board.innerHTML = html;
  };
  const refreshNav = async () => {
    if (!navBox) return;
    const q = project ? '?project=' + encodeURIComponent(project) + '&topic=' + encodeURIComponent(topic) : '';
    const html = await get('/nav' + q);
    if (html !== null) navBox.innerHTML = html;
  };
  const refreshAll = () => { refreshFeed(); refreshPeople(); refreshBoard(); refreshNav(); };

  if (window.EventSource) {
    const events = new EventSource('/events');
    events.onmessage = (e) => {
      const data = e.data;
      if (data === '') refreshAll();
      else if (data === 'board') { refreshBoard(); refreshPeople(); }
      else if (data === 'nav') refreshNav();
      else if (data.startsWith('msg:')) {
        if (data === 'msg:' + project + '/' + topic) refreshFeed();
        refreshNav(); // the counters of the topics
      }
    };
    // Also covers what happened while the connection was down.
    events.onopen = refreshAll;
  }
  // A session turns idle without any event, so the panels age by the clock.
  // The state of an ask (waiting, out of time) changes the same way.
  setInterval(() => { refreshBoard(); refreshPeople(); refreshFeed(); }, 60000);

  // ---- the composer: Enter sends, Shift+Enter makes a new line ----
  if (composer && box) {
    const grow = () => { box.style.height = 'auto'; box.style.height = Math.min(box.scrollHeight, 160) + 'px'; };
    box.addEventListener('input', grow);
    box.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
        e.preventDefault();
        if (box.value.trim()) composer.requestSubmit();
      }
    });
    composer.addEventListener('submit', async (e) => {
      if (!window.fetch) return;
      e.preventDefault();
      const button = composer.querySelector('button');
      button.disabled = true;
      try {
        const r = await fetch(composer.action, {
          method: 'POST',
          headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
          body: new URLSearchParams(new FormData(composer)),
        });
        if (r.ok) {
          box.value = '';
          grow();
          await refreshFeed();
          toBottom();
        } else {
          composer.submit(); // let the server explain on its own page
        }
      } catch (_) {
        composer.submit();
      } finally {
        button.disabled = false;
      }
    });
  }

  // ---- narrow screens ----
  const close = () => document.body.classList.remove('nav-open', 'people-open');
  const toggle = (cls) => () => {
    const open = document.body.classList.contains(cls);
    close();
    if (!open) document.body.classList.add(cls);
  };
  const menu = document.getElementById('menu');
  const toggler = document.getElementById('people-toggle');
  const scrim = document.getElementById('scrim');
  if (menu) menu.addEventListener('click', toggle('nav-open'));
  if (toggler) toggler.addEventListener('click', toggle('people-open'));
  if (scrim) scrim.addEventListener('click', close);
  document.addEventListener('keydown', (e) => { if (e.key === 'Escape') close(); });

  localise(document);
  // A link to a message opens the topic at that message; otherwise the feed starts at its end.
  const target = /^#m\d+$/.test(location.hash) && document.getElementById(location.hash.slice(1));
  if (target) target.scrollIntoView({ block: 'center' });
  else toBottom();
})();
