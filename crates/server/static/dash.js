(() => {
  function dashboard(box, snapEl) {
    let snapshot;
    try {
      snapshot = JSON.parse(snapEl.textContent);
    } catch {
      return;
    }

    const COLORS = {
      typing: '#ffd166',
      reading: '#6ee7c8',
      editing: '#ff8fab',
      testing: '#8fd3ff',
      terminal: '#c3c8f5',
    };
    const LABELS = {
      typing: 'typing a prompt',
      reading: 'reading a reply',
      editing: 'in your editor',
      testing: 'testing',
      terminal: 'in the terminal',
    };
    const SVG_NS = 'http://www.w3.org/2000/svg';
    const svg = document.getElementById('dial');
    const timeEl = document.getElementById('dial-b');
    const captionEl = document.getElementById('dial-s');
    const defaultCaption = captionEl.textContent;
    let minuteKinds;
    let clockOffset;
    let hovering = false;

    function addSvg(name, attrs) {
      const node = document.createElementNS(SVG_NS, name);
      for (const [key, value] of Object.entries(attrs)) node.setAttribute(key, value);
      svg.appendChild(node);
      return node;
    }

    // Hours into the user's day, by the server's clock rather than this browser's.
    function nowHours() {
      return ((Date.now() + clockOffset) / 1000 - snapshot.day_start) / 3600;
    }

    function dominantKind(hour, radius) {
      const counts = {};
      let best = null;
      let bestCount = 0;
      const last = Math.round((hour + radius) * 60);
      for (let i = Math.round((hour - radius) * 60); i <= last; i++) {
        const kind = minuteKinds[(i + 1440) % 1440];
        if (kind) {
          counts[kind] = (counts[kind] || 0) + 1;
          if (counts[kind] > bestCount) {
            bestCount = counts[kind];
            best = kind;
          }
        }
      }
      return best;
    }

    function load(next) {
      snapshot = next;
      clockOffset = snapshot.now * 1000 - Date.now();
      minuteKinds = new Array(1440).fill(null);
      snapshot.spans.forEach(([start, end, kind]) => {
        for (let i = Math.max(0, Math.floor(start * 60)); i < Math.min(1440, Math.ceil(end * 60)); i++) {
          minuteKinds[i] = kind;
        }
      });
    }

    function draw() {
      const now = nowHours();
      svg.textContent = '';

      for (let y = 0; y < 25; y++) {
        let first = -1;
        let last = -1;
        for (let x = 0; x < 25; x++) {
          if (Math.hypot(x - 12, y - 12) <= 12.45) {
            if (first < 0) first = x;
            last = x;
          }
        }
        if (first >= 0) addSvg('rect', { x: first, y, width: last - first + 1, height: 1, fill: '#1d1633' });
      }

      [['00', 12, 4.3], ['06', 19.8, 12.3], ['12', 12, 20.3], ['18', 4.2, 12.3]].forEach(([text, x, y]) => {
        addSvg('text', {
          x: x + 0.5,
          y: y + 0.5,
          'text-anchor': 'middle',
          'dominant-baseline': 'middle',
          'font-size': '.95',
          fill: '#6f66a0',
        }).textContent = text;
      });

      for (let y = 0; y < 25; y++) {
        for (let x = 0; x < 25; x++) {
          const distance = Math.hypot(x - 12, y - 12);
          if (distance < 8.3 || distance > 11.5) continue;
          let angle = Math.atan2(x - 12, -(y - 12));
          if (angle < 0) angle += Math.PI * 2;
          const hour = (angle / (Math.PI * 2)) * 24;
          const kind = dominantKind(hour, 0.17);
          if (kind) {
            addSvg('rect', { x: x + 0.1, y: y + 0.1, width: 0.8, height: 0.8, fill: COLORS[kind] || '#e4e1f0' });
          } else {
            addSvg('rect', {
              x: x + 0.39,
              y: y + 0.39,
              width: 0.22,
              height: 0.22,
              fill: hour > now ? '#2e2758' : '#4a4180',
            });
          }
        }
      }

      const nowAngle = (Math.min(now, 23.99) / 24) * Math.PI * 2;
      addSvg('rect', {
        x: Math.round(12 + Math.sin(nowAngle) * 12) + 0.15,
        y: Math.round(12 - Math.cos(nowAngle) * 12) + 0.15,
        width: 0.7,
        height: 0.7,
        fill: '#fff',
      });
    }

    function showHour(hour) {
      const kind = dominantKind(hour, 0.05);
      const hours = Math.floor(hour);
      const minutes = Math.floor((hour - hours) * 60);
      timeEl.textContent = `${String(hours).padStart(2, '0')}:${String(minutes).padStart(2, '0')}`;
      captionEl.textContent = hour > nowHours()
        ? 'later today'
        : kind ? `focus time, ${LABELS[kind]}` : 'no focus time';
    }

    function renderWeek(bars) {
      const days = document.querySelectorAll('#wk .wk-day');
      bars.forEach((bar, i) => {
        const day = days[i];
        if (!day) return;
        day.title = bar.title;
        day.className = `wk-day${bar.today ? ' now' : ''}`;
        day.querySelector('.l').textContent = bar.label;
        day.querySelector('.f').style.height = `${bar.focus}%`;
        const agent = day.querySelector('.a');
        if (agent) agent.style.height = `${bar.agent}%`;
      });
    }

    function apply(next) {
      load(next);
      document.querySelectorAll('[data-live]').forEach((el) => {
        const value = snapshot[el.dataset.live];
        if (typeof value === 'string') el.textContent = value;
      });
      if (!hovering) timeEl.textContent = snapshot.today;
      renderWeek(snapshot.bars);
      draw();
    }

    load(snapshot);
    draw();

    box.addEventListener('pointermove', (e) => {
      const rect = box.getBoundingClientRect();
      let angle = Math.atan2(e.clientX - rect.left - rect.width / 2, -(e.clientY - rect.top - rect.height / 2));
      if (angle < 0) angle += Math.PI * 2;
      hovering = true;
      showHour((angle / (Math.PI * 2)) * 24);
    });
    box.addEventListener('pointerleave', () => {
      hovering = false;
      timeEl.textContent = snapshot.today;
      captionEl.textContent = defaultCaption;
    });

    setInterval(() => {
      if (nowHours() >= 24) location.reload();
      else draw();
    }, 30000);

    if (window.EventSource) {
      const source = new EventSource('/me/live');
      source.addEventListener('snapshot', (e) => {
        try {
          apply(JSON.parse(e.data));
        } catch {
          // A bad event leaves the page as it was.
        }
      });
      source.onerror = () => {
        if (source.readyState === EventSource.CLOSED) source.close();
      };
      addEventListener('pagehide', () => source.close());
    }
  }

  function datePicker() {
    const $ = (id) => document.getElementById(id);
    const parse = (s) => {
      const [y, m, d] = s.split('-').map(Number);
      return new Date(y, m - 1, d);
    };
    const pad = (n) => String(n).padStart(2, '0');
    const iso = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
    let mode = 'day';
    let anchor = null;
    let hover = null;
    let month;
    let secs = {};
    let max = 0;
    let seq = 0;

    function human(s) {
      const m = Math.round(s / 60);
      const h = Math.floor(m / 60);
      if (m === 0) return s > 0 ? 'under 1 min' : '0 min';
      if (h === 0) return `${m} min`;
      return m % 60 ? `${h} hr ${m % 60} min` : `${h} hr`;
    }

    function info() {
      const data = $('dates').dataset;
      return { from: parse(data.from), to: parse(data.to), today: parse(data.today), start: parse(data.start) };
    }

    // Swaps in the same page for the new dates, so the dial and scroll position stay put.
    async function go(url) {
      const n = ++seq;
      $('boxes').classList.add('loading');
      try {
        const res = await fetch(url);
        if (!res.ok) throw new Error(String(res.status));
        const doc = new DOMParser().parseFromString(await res.text(), 'text/html');
        if (n !== seq) return;
        const parts = ['days', 'dates', 'boxes'].map((id) => [$(id), doc.getElementById(id)]);
        if (parts.some(([, fresh]) => !fresh)) throw new Error('not the dashboard');
        const refocus = $('dates').contains(document.activeElement);
        parts.forEach(([old, fresh]) => old.replaceWith(fresh));
        if (refocus) $('pick').focus();
        history.replaceState(null, '', url);
      } catch {
        location.href = url;
      }
    }

    function pick(a, b) {
      const [lo, hi] = a <= b ? [a, b] : [b, a];
      closeCal();
      go(`/me?from=${iso(lo)}&to=${iso(hi)}`);
    }

    function paint() {
      let { from: lo, to: hi } = info();
      if (mode === 'range') {
        lo = hi = anchor;
        if (anchor && hover) [lo, hi] = hover < anchor ? [hover, anchor] : [anchor, hover];
      }
      const today = +info().today;
      $('cal-days').querySelectorAll('button').forEach((button) => {
        const d = parse(button.dataset.day);
        const inside = !!lo && d >= lo && d <= hi;
        const end = inside && (+d === +lo || +d === +hi);
        button.classList.toggle('today', +d === today);
        button.classList.toggle('end', end);
        button.classList.toggle('in', inside && !end);
      });
      $('cal-hint').textContent = mode === 'range' ? (anchor ? 'Pick the last day' : 'Pick the first day') : '';
      $('cal-mode').textContent = mode === 'range' ? 'Pick one day' : 'Pick a range';
    }

    function draw() {
      const { today, start } = info();
      $('cal-month').textContent = month.toLocaleDateString('en-US', { month: 'long', year: 'numeric' });
      $('cal-prev').disabled = month <= new Date(start.getFullYear(), start.getMonth(), 1);
      $('cal-next').disabled = month >= new Date(today.getFullYear(), today.getMonth(), 1);
      let html = ['Mo', 'Tu', 'We', 'Th', 'Fr', 'Sa', 'Su'].map((d) => `<span class="dow">${d}</span>`).join('');
      html += '<span></span>'.repeat((month.getDay() + 6) % 7);
      const last = new Date(month.getFullYear(), month.getMonth() + 1, 0).getDate();
      for (let n = 1; n <= last; n++) {
        const d = new Date(month.getFullYear(), month.getMonth(), n);
        const off = d > today || d < start;
        const v = secs[iso(d)] || 0;
        const level = v > 0 && max > 0 ? Math.min(4, Math.max(1, Math.ceil((v / max) * 4))) : 0;
        html += `<button type="button" data-day="${iso(d)}"${off ? ' disabled' : ` title="${human(v)}"`}>${n}${level ? `<i class="h${level}"></i>` : ''}</button>`;
      }
      $('cal-days').innerHTML = html;
      paint();
    }

    function openCal() {
      try {
        secs = JSON.parse($('cal-data').textContent);
      } catch {
        secs = {};
      }
      max = Math.max(0, ...Object.values(secs));
      mode = 'day';
      anchor = hover = null;
      const { to } = info();
      month = new Date(to.getFullYear(), to.getMonth(), 1);
      $('cal').hidden = false;
      $('pick').setAttribute('aria-expanded', 'true');
      draw();
      const first = $('cal-days').querySelector('.end') || $('cal-days').querySelector('button:not(:disabled)');
      if (first) first.focus();
    }

    function closeCal() {
      if ($('cal').hidden) return;
      $('cal').hidden = true;
      $('pick').setAttribute('aria-expanded', 'false');
    }

    document.addEventListener('click', (e) => {
      const t = e.target;
      const path = e.composedPath();
      if (!path.includes($('cal')) && !path.includes($('pick'))) closeCal();
      if (t.closest('#pick')) {
        if ($('cal').hidden) openCal();
        else closeCal();
        return;
      }
      const chip = t.closest('#dates .seg a');
      if (chip) {
        if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
        e.preventDefault();
        go(chip.getAttribute('href'));
        return;
      }
      const bar = t.closest('#days div[data-day]');
      if (bar) {
        const d = parse(bar.dataset.day);
        pick(d, d);
        return;
      }
      if (t.closest('#cal-prev') || t.closest('#cal-next')) {
        month = new Date(month.getFullYear(), month.getMonth() + (t.closest('#cal-prev') ? -1 : 1), 1);
        draw();
        return;
      }
      if (t.closest('#cal-mode')) {
        mode = mode === 'day' ? 'range' : 'day';
        anchor = hover = null;
        paint();
        return;
      }
      const day = t.closest('#cal-days button');
      if (!day || day.disabled) return;
      const d = parse(day.dataset.day);
      if (mode === 'day') {
        pick(d, d);
      } else if (!anchor) {
        anchor = d;
        paint();
      } else {
        pick(anchor, d);
      }
    });

    document.addEventListener('mouseover', (e) => {
      if (mode !== 'range' || !anchor || $('cal').hidden) return;
      const day = e.target.closest('#cal-days button');
      const next = day && !day.disabled ? parse(day.dataset.day) : null;
      if (+next !== +hover) {
        hover = next;
        paint();
      }
    });

    document.addEventListener('keydown', (e) => {
      if (e.key === 'Escape' && !$('cal').hidden) {
        closeCal();
        $('pick').focus();
      }
    });
  }

  const box = document.getElementById('dialbox');
  const snapEl = document.getElementById('snap');
  if (box && snapEl) dashboard(box, snapEl);
  if (document.getElementById('dates')) datePicker();

  document.querySelectorAll('.heat').forEach((heat) => {
    heat.scrollLeft = heat.scrollWidth;
  });

  document.querySelectorAll('.cmd .copy').forEach((button) => {
    const code = button.parentNode.querySelector('code');
    button.hidden = false;
    button.addEventListener('click', () => {
      const done = () => {
        button.textContent = 'Copied';
        setTimeout(() => {
          button.textContent = 'Copy';
        }, 1400);
      };
      const selectCode = () => {
        const range = document.createRange();
        range.selectNodeContents(code);
        const selection = getSelection();
        selection.removeAllRanges();
        selection.addRange(range);
      };
      try {
        navigator.clipboard.writeText(code.textContent).then(done, selectCode);
      } catch {
        selectCode();
      }
    });
  });

  const hint = document.getElementById('tzhint');
  if (hint) {
    let zone;
    try {
      zone = Intl.DateTimeFormat().resolvedOptions().timeZone;
    } catch {
      zone = null;
    }
    if (zone && hint.dataset.auto) {
      fetch('/me/timezone', {
        method: 'POST',
        body: new URLSearchParams({ csrf: hint.dataset.csrf, timezone: zone }),
      })
        .then((res) => (res.ok ? res.json() : null))
        .then((json) => {
          if (json && json.changed) location.reload();
        })
        .catch(() => {});
    } else if (zone && zone !== hint.dataset.tz) {
      document.getElementById('tzname').textContent = zone;
      hint.hidden = false;
    }
  }
})();
