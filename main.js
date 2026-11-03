// Liveback LP. The language is already decided by the inline script in <head>; this file
// adds the JA / EN switch, the timeline rail, the showreel buttons and the download link.
(function () {
  'use strict';
  var root = document.documentElement;
  var reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;

  // ---------- Language ----------
  // Strings that live outside the body: everything else sits in the DOM as data-l pairs.
  var HEAD = {
    ja: {
      title: 'Liveback | いまを録りながら、さっきを見返す',
      description: 'ゲームやアプリを登録しておけば、起動と同時に録画が始まる。録画を止めずに見返して、気に入った範囲だけを MP4 に書き出せる。Windows 11 向けの無料・オープンソースの録画アプリ。'
    },
    en: {
      title: 'Liveback | Record now. Rewind to a moment ago.',
      description: 'Register a game or an app, and recording starts the moment it launches. Look back without stopping, and export just the part you like as an MP4. A free, open-source recorder for Windows 11.'
    }
  };
  var video = document.querySelector('.lp-reel video');

  function reelFor(l) {
    if (!video) return;
    var want = window.lpReel(l); // defined next to the <video> in index.html
    if (video.getAttribute('src') === want) return;
    video.src = want;
    if (!video.closest('.lp-reel').hasAttribute('data-motion')) video.play().catch(function () {});
  }

  function setLang(l, remember) {
    root.lang = l;
    document.title = HEAD[l].title;
    var d = document.querySelector('meta[name="description"]');
    if (d) d.setAttribute('content', HEAD[l].description);
    document.querySelectorAll('[data-lang-btn]').forEach(function (b) {
      b.setAttribute('aria-pressed', String(b.getAttribute('data-lang-btn') === l));
    });
    if (remember) { try { localStorage.setItem('lp-lang', l); } catch (e) {} }
    reelFor(l);
  }
  setLang(root.lang === 'en' ? 'en' : 'ja', false);

  // The English reel is optional: if it is missing or unreadable, show the Japanese one.
  if (video) {
    video.addEventListener('error', function () {
      var ja = window.lpReel('ja');
      if (video.getAttribute('src') !== ja) { video.src = ja; if (!video.closest('.lp-reel').hasAttribute('data-motion')) video.play().catch(function () {}); }
    });
  }

  // ---------- Timeline rail ----------
  var tl = document.querySelector('.lp-tl');
  var labels = function () {}, place = function () {};
  if (tl) {
    var rail = tl.querySelector('.lp-rail'), head = rail.querySelector('.lp-rail__head');
    var rew = rail.querySelector('.lp-rail__rew'), readout = rail.querySelector('.lp-rail__tc');
    // 160 px of page = 30 s (the pitch of the labelled ticks), so 1 px = 0.1875 s.
    var fmt = function (px, tenths) {
      var d = Math.round(px * 1.875), m = Math.floor(d / 600), r = (d - m * 600) / 10;
      return '−' + m + ':' + (r < 10 ? '0' : '') + (tenths ? r.toFixed(1) : Math.round(r));
    };
    var markYs = [];
    // With scroll timelines, style.css moves the knob and the rewound line; only the readout stays here.
    var cssDriven = window.CSS && CSS.supports('animation-timeline: scroll()');
    labels = function () {
      rail.style.setProperty('--rail-h', rail.offsetHeight + 'px');
      rail.querySelectorAll('.lp-rail__lab').forEach(function (e) { e.remove(); });
      var top = rail.getBoundingClientRect().top, marks = [];
      markYs = [];
      tl.querySelectorAll('.lp-mark').forEach(function (m) {
        if (!m.getClientRects().length) return; // hidden at this width (an item that does not start a row)
        var y = m.getBoundingClientRect().top - top, s = fmt(y, true);
        marks.push(y); markYs.push([m, y]);
        m.parentElement.querySelectorAll('[data-tc]').forEach(function (e) { e.textContent = s; });
      });
      for (var y = 160; y < rail.offsetHeight - 16; y += 160) {
        if (marks.some(function (v) { return Math.abs(v - y) < 18; })) continue;
        var l = document.createElement('span');
        l.className = 'lp-rail__lab'; l.style.top = y + 'px'; l.textContent = fmt(y, false);
        rail.appendChild(l);
      }
    };
    place = function () {
      var max = root.scrollHeight - innerHeight, h = rail.offsetHeight;
      var p = max > 8 ? Math.min(1, Math.max(0, scrollY / max)) : 0;
      var y = Math.round(p * h);
      if (!cssDriven) {
        head.style.transform = 'translateY(' + y + 'px)';
        rew.style.height = y + 'px';
      }
      readout.textContent = fmt(y, true);
      rail.classList.toggle('is-live', y < 12);
      // While the playhead passes a marker, its time gives way to the playhead's.
      markYs.forEach(function (q) { q[0].classList.toggle('is-near', Math.abs(q[1] - y) < 20); });
    };
    var queued = false;
    addEventListener('scroll', function () {
      if (queued) return;
      queued = true;
      requestAnimationFrame(function () { queued = false; place(); });
    }, { passive: true });
    addEventListener('resize', function () { labels(); place(); });
    addEventListener('load', function () { labels(); place(); });
    if (document.fonts) document.fonts.ready.then(function () { labels(); place(); });
    labels(); place();
  }

  // ---------- Clicks ----------
  var ICON_ON = '<svg class="lp-icon" viewBox="0 0 20 20" aria-hidden="true"><path d="M3.25 8v4c0 .41.34.75.75.75h2.5L10 16V4L6.5 7.25H4c-.41 0-.75.34-.75.75z"/><path d="M13 7.5a3.5 3.5 0 0 1 0 5"/><path d="M15.25 5.25a6.75 6.75 0 0 1 0 9.5"/></svg><span class="lp-reel__label"><span data-l="ja" lang="ja">音を消す</span><span data-l="en" lang="en">Mute</span></span>';
  var ICON_OFF = '<svg class="lp-icon" viewBox="0 0 20 20" aria-hidden="true"><path d="M3.25 8v4c0 .41.34.75.75.75h2.5L10 16V4L6.5 7.25H4c-.41 0-.75.34-.75.75z"/><path d="m13.25 8 3.75 4M17 8l-3.75 4"/></svg><span class="lp-reel__label"><span data-l="ja" lang="ja">音を出す</span><span data-l="en" lang="en">Sound on</span></span>';

  document.addEventListener('click', function (e) {
    var b = e.target.closest('[data-lang-btn]');
    if (b) { setLang(b.getAttribute('data-lang-btn'), true); labels(); place(); return; }
    var s = e.target.closest('[data-reel-sound]');
    if (s) {
      var r = s.closest('.lp-reel'), on = r.getAttribute('data-sound') !== 'on';
      r.setAttribute('data-sound', on ? 'on' : 'off');
      s.setAttribute('aria-pressed', String(on));
      s.innerHTML = on ? ICON_ON : ICON_OFF;
      if (video) video.muted = !on;
      return;
    }
    var p = e.target.closest('[data-reel-play]');
    if (p) {
      p.closest('.lp-reel').removeAttribute('data-motion');
      if (video) { video.loop = true; video.play().catch(function () {}); }
      return;
    }
    var g = e.target.closest('[data-golive]');
    if (g) {
      e.preventDefault();
      scrollTo({ top: 0, behavior: reduced ? 'auto' : 'smooth' });
      var d = document.querySelector('.lp-hero .lp-dl');
      if (d) d.focus({ preventScroll: true });
    }
  });

  // ---------- Download ----------
  // The HTML already points at the Releases page; only a successful read replaces it.
  var dl = document.querySelector('.lp-hero .lp-dl');
  if (dl && window.fetch) {
    fetch('https://api.github.com/repos/koorinonaka/liveback/releases/latest', { headers: { Accept: 'application/vnd.github+json' } })
      .then(function (res) { if (!res.ok) throw new Error(String(res.status)); return res.json(); })
      .then(function (rel) {
        var exe = (rel.assets || []).filter(function (a) { return /_x64-setup\.exe$/.test(a.name); })[0];
        if (!exe || !exe.browser_download_url || !rel.tag_name) return;
        dl.href = exe.browser_download_url;
        dl.setAttribute('data-dl-state', 'ready');
        var info = dl.parentElement.querySelector('.lp-dl-info'), first = info.firstChild;
        [rel.tag_name, (exe.size / 1e6).toFixed(1) + ' MB'].forEach(function (t) {
          var v = document.createElement('span'), sep = document.createElement('span');
          v.textContent = t;
          sep.className = 'lp-sep'; sep.textContent = '·';
          info.insertBefore(v, first); info.insertBefore(sep, first);
        });
      })
      .catch(function () {});
  }
})();
