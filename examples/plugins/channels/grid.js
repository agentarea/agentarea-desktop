// Channels plugin core, shared by index.html and sidebar.html (and Node, for checks).
// Data = plain files in the granted folder (format: SCHEMA.md there):
//   <persona-slug>/README.md   one folder per persona: front matter + story
//   channels.json              array: one object per channel (persona × platform): stage, handle, KPI target…
//   metrics.csv                history rows: date, persona, platform, reach, cost, currency, followers, posts_published
//   posts.json                 array: planned / published posts
// Used by the plugin pages (bridge I/O) and by the standalone grid.html (fetch I/O).
(function (root) {
  'use strict';

  const STAGES = ['idea', 'setup', 'setup_done', 'live', 'paused', 'stopped'];
  const STAGE_LABEL = { idea: 'Idea', setup: 'Setting up', setup_done: 'Set up', live: 'Live', paused: 'Paused', stopped: 'Stopped' };
  const POST_STATUSES = ['draft', 'scheduled', 'published'];
  const PLATFORM_ORDER = ['tiktok', 'instagram', 'youtube', 'facebook', 'x', 'threads', 'telegram'];
  const ROLES = ['creator', 'community', 'engager'];
  const DEFAULT_CURRENCY = '€';
  // What counts as "reach" per platform when kpi_metric is empty (same list in SCHEMA.md).
  const DEFAULT_KPI = {
    tiktok: 'video views, 30d', instagram: 'Reels plays / accounts reached, 30d', youtube: 'Shorts views, 30d',
    facebook: 'Reels plays, 30d', x: 'impressions, 30d', threads: 'views, 30d', telegram: 'post views, 30d',
  };

  const low = (s) => String(s || '').trim().toLowerCase();
  const empty = (v) => { const s = String(v ?? '').trim(); return !s || s === '-' || s === '—'; };
  const text = (v) => (empty(v) ? '' : String(v).trim());

  /** README.md → { meta, body }. Front matter: `key: value` lines between `---` fences. */
  function frontMatter(src) {
    src = String(src || '').replace(/\r/g, '');
    const m = /^---\n([\s\S]*?)\n---\n?/.exec(src);
    if (!m) return { meta: {}, body: src };
    const meta = {};
    for (const line of m[1].split('\n')) {
      const kv = /^([A-Za-z_][\w-]*)\s*:\s*(.*)$/.exec(line);
      if (kv) meta[kv[1].toLowerCase()] = kv[2].trim().replace(/^["']|["']$/g, '');
    }
    return { meta, body: src.slice(m[0].length) };
  }
  const list = (v) => String(v || '').replace(/^\[|\]$/g, '').split(/[,\s]+/).map(low).filter(Boolean);

  /** "12 400", "€12400", "$15.5", "1,2k" → number; empty → null. */
  function amount(v) {
    if (empty(v)) return null;
    let s = String(v).toLowerCase().replace(/\s/g, '').replace(',', '.');
    let mul = 1;
    if (/[kк]$/.test(s)) { mul = 1e3; s = s.slice(0, -1); } else if (/[mм]$/.test(s)) { mul = 1e6; s = s.slice(0, -1); }
    const n = parseFloat(s.replace(/[^0-9.\-]/g, ''));
    return Number.isFinite(n) ? n * mul : null;
  }
  function currencyOf(row) {
    const c = text(row.currency);
    if (c) return /usd|\$/i.test(c) ? '$' : /eur|€/i.test(c) ? '€' : /gbp|£/i.test(c) ? '£' : /pln|zł/i.test(c) ? 'zł' : /czk/i.test(c) ? 'Kč' : c;
    return DEFAULT_CURRENCY;
  }
  /** "2026-10-03 18:00" / "2026-10-03" → ms (local time); bad → NaN. */
  function when(v) {
    const s = text(v);
    if (!s) return NaN;
    const m = /^(\d{4})-(\d{2})-(\d{2})(?:[ T](\d{1,2}):(\d{2}))?/.exec(s);
    return m ? new Date(+m[1], +m[2] - 1, +m[3], +(m[4] || 0), +(m[5] || 0)).getTime() : NaN;
  }

  /** CSV → rows keyed by lower-cased header. Handles "quoted, fields" and "" escapes. */
  function parseCsv(src) {
    const rows = []; let row = [], cur = '', q = false;
    src = String(src || '').replace(/\r/g, '');
    for (let i = 0; i < src.length; i++) {
      const ch = src[i];
      if (q) { if (ch === '"') { if (src[i + 1] === '"') { cur += '"'; i++; } else q = false; } else cur += ch; }
      else if (ch === '"') q = true;
      else if (ch === ',') { row.push(cur); cur = ''; }
      else if (ch === '\n') { row.push(cur); rows.push(row); row = []; cur = ''; }
      else cur += ch;
    }
    if (cur || row.length) { row.push(cur); rows.push(row); }
    const nonEmpty = rows.filter((r) => r.some((c) => c.trim()));
    if (!nonEmpty.length) return [];
    const head = nonEmpty[0].map(low);
    return nonEmpty.slice(1).map((r) => { const o = {}; head.forEach((h, k) => { o[h] = (r[k] ?? '').trim(); }); return o; });
  }
  const json = (src, fallback) => { if (!String(src || '').trim()) return fallback; const v = JSON.parse(src); return v; };

  /**
   * Read the data folder. `io` = { list(): Promise<string[] | null> (sub-folder names, or null
   * when the host can't list — then persona slugs come from channels.json), read(path): Promise<string> }.
   * Files: channels.json, metrics.csv, posts.json, <slug>/README.md. `sig` changes when any file does.
   */
  async function load(io) {
    const opt = (path) => io.read(path).catch(() => '');
    const [channels, metrics, posts] = await Promise.all([opt('channels.json'), opt('metrics.csv'), opt('posts.json')]);
    let errors = [];
    let chList = [];
    try { chList = json(channels, []); if (!Array.isArray(chList)) throw new Error('expected an array'); } catch (e) { errors.push(`channels.json: ${e.message}`); chList = []; }
    let slugs = await io.list();
    if (!slugs) slugs = [...new Set(chList.map((c) => low(c && c.persona)).filter(Boolean))];
    // SOUL.md (current layout) or README.md (older folders). The first persona decides which name to try
    // first for the rest, so a static server logs at most one miss.
    let order = ['SOUL.md', 'README.md'];
    const readPersona = async (slug) => {
      let text = null;
      for (const name of order) { try { text = await io.read(`${slug}/${name}`); if (name !== order[0]) order = [name, ...order.filter((x) => x !== name)]; break; } catch { /* try next */ } }
      if (text === null) return null;
      const config = await io.read(`${slug}/config.json`).catch(() => '');
      return { slug, folder: slug, text, config };
    };
    const first = slugs.length ? [await readPersona(slugs[0])] : [];
    const personas = first.concat(await Promise.all(slugs.slice(1).map(readPersona))).filter(Boolean);
    let postList = [];
    try { postList = json(posts, []); if (!Array.isArray(postList)) throw new Error('expected an array'); } catch (e) { errors.push(`posts.json: ${e.message}`); postList = []; }
    return { personas, channels: chList, metrics: parseCsv(metrics), posts: postList, errors,
      sig: [channels, metrics, posts, ...personas.map((p) => p.slug + '\u0000' + p.text + '\u0000' + p.config)].join('\u0001') };
  }

  const DAY = 86400000;
  const BUDGET_KEYS = ['proxy', 'account', 'content', 'ads', 'other'];
  /** channels.json `budget` / `spent` → { currency, items: {proxy, account, content, ads, other}, total } or null when empty. */
  function moneyOf(b) {
    if (!b || typeof b !== 'object') return null;
    const items = {}; let total = 0, any = false;
    for (const k of BUDGET_KEYS) { const v = amount(b[k]); items[k] = v; if (v !== null) { total += v; any = true; } }
    return any ? { currency: currencyOf({ currency: b.currency }), items, total } : null;
  }
  /** channels.json `network` → { egress, ip, geo, device, url } (strings; '' when unset). Never holds credentials. */
  function netOf(n) {
    n = n && typeof n === 'object' ? n : {};
    return { egress: text(n.egress), ip: text(n.ip), geo: text(n.geo), device: text(n.device_profile), url: text(n.url) };
  }

  /**
   * Join into { personas, platforms, platformCount, errors }.
   * persona.cells[platform] = channel:
   *   { stage, handle, mcp, kpiMetric, target, notes, created, updated,
   *     reach (Σ metrics.reach, last 30 days), cost (Σ metrics.cost, last 30 days), currency,
   *     followers (latest row), published30 (Σ posts_published, 30 days), lastMetric (date),
   *     pct = reach / target, cpk = cost / (reach / 1000), posts, scheduled, next }
   * North-star KPI = reach over the last 30 days vs the channel's own kpi_target.
   */
  function join(data, now) {
    now = now || Date.now();
    const since = now - 30 * DAY;
    const byId = new Map();
    const add = (id, init) => { if (!byId.has(id)) byId.set(id, { id, folder: null, name: id, niche: '', emoji: '👤', category: 'No persona folder', country: '', city: '', languages: [], audience: '', references: [], character: '', platforms: [], story: '', summary: '', cells: {}, ...init }); return byId.get(id); };
    for (const p of data.personas) {
      const { meta, body } = frontMatter(p.text);
      let cfg = {};
      try { cfg = p.config ? JSON.parse(p.config) : {}; } catch { cfg = {}; }
      const title = (/^#\s+(.+)$/m.exec(body) || [])[1] || '';
      add(p.slug, {
        folder: p.folder, name: cfg.name || title || meta.name || p.slug, niche: cfg.niche || meta.name || '', emoji: cfg.emoji || meta.emoji || '👤',
        category: cfg.category || meta.category || 'Other', country: cfg.country || '', city: cfg.city || '',
        languages: cfg.languages || [], audience: cfg.target_audience || '', references: cfg.references || [],
        handle: cfg.handle || (/^@?([a-z0-9._]+)$/.test(p.slug) ? p.slug : ''), personality: cfg.personality || null, hypothesis: cfg.hypothesis || '',
        character: title, platforms: (cfg.platforms || list(meta.platforms)).map(low), story: body,
        summary: meta.summary || (body.replace(/^#.*$/gm, '').trim().split(/\n\s*\n/)[0] || '').replace(/[*_`]/g, '').replace(/\s+/g, ' ').trim(),
      });
    }
    for (const r of data.channels) {
      if (!r) continue;
      const pid = low(r.persona), plat = low(r.platform);
      if (!pid || !PLATFORM_ORDER.includes(plat)) continue;
      const stage = STAGES.includes(low(r.stage)) ? low(r.stage) : 'idea';
      add(pid).cells[plat] = {
        persona: pid, platform: plat, stage, handle: text(r.handle), mcp: text(r.mcp), notes: text(r.notes),
        kpiMetric: text(r.kpi_metric) || DEFAULT_KPI[plat] || 'reach, 30d', target: amount(r.kpi_target),
        created: text(r.created), updated: text(r.updated), network: netOf(r.network),
        role: ROLES.includes(low(r.role)) ? low(r.role) : 'creator',
        plan: text(r.plan), budget: moneyOf(r.budget), spentPlan: moneyOf(r.spent),
        tasks: (Array.isArray(r.tasks) ? r.tasks : []).filter((t) => t && t.type).map((t) => ({ type: low(t.type), cadence: text(t.cadence), notes: text(t.notes) })),
        reach: null, cost: null, currency: DEFAULT_CURRENCY, followers: null, published30: null, lastMetric: '',
        posts: [], scheduled: 0, next: null,
      };
    }
    // metrics.csv: each row = what happened in the period ending on `date` (daily rows recommended).
    for (const m of data.metrics) {
      const cell = byId.get(low(m.persona)) && byId.get(low(m.persona)).cells[low(m.platform)];
      const at = when(m.date);
      if (!cell || !Number.isFinite(at)) continue;
      if (!cell.lastMetric || m.date >= cell.lastMetric) {
        cell.lastMetric = m.date;
        if (amount(m.followers) !== null) cell.followers = amount(m.followers);
        if (text(m.currency)) cell.currency = currencyOf({ currency: m.currency });
      }
      if (at > since - DAY && at <= now) {
        const r = amount(m.reach), c = amount(m.cost), pp = amount(m.posts_published);
        if (r !== null) cell.reach = (cell.reach || 0) + r;
        if (c !== null) cell.cost = (cell.cost || 0) + c;
        if (pp !== null) cell.published30 = (cell.published30 || 0) + pp;
      }
    }
    for (const r of data.posts) {
      if (!r) continue;
      const cell = byId.get(low(r.persona)) && byId.get(low(r.persona)).cells[low(r.platform)];
      if (!cell) continue;
      const status = POST_STATUSES.includes(low(r.status)) ? low(r.status) : 'draft';
      const dt = text(r.datetime || r.when || r.date);
      cell.posts.push({ when: dt, at: when(dt), status, title: text(r.title), content: text(r.text_or_file || r.text || r.file) });
    }
    for (const p of byId.values()) {
      for (const c of Object.values(p.cells)) {
        c.pct = c.target > 0 && c.reach !== null ? c.reach / c.target : null;
        c.cpk = c.cost !== null && c.reach > 0 ? c.cost / (c.reach / 1000) : null;
        c.posts.sort((a, b) => (a.at || 0) - (b.at || 0));
        const sch = c.posts.filter((x) => x.status === 'scheduled');
        c.scheduled = sch.length;
        c.next = sch.find((x) => x.at >= now) || null;
      }
      p.channels = Object.values(p.cells);
      p.budget = sumMoney(p.channels.map((c) => c.budget));
      // Exit (proxy) countries from network.geo "City, CC"; flag when any differs from the persona's country.
      p.proxyCountries = [...new Set(p.channels.map((c) => (/,\s*([A-Za-z]{2})\s*$/.exec(c.network.geo) || [])[1]).filter(Boolean).map((x) => x.toUpperCase()))];
      p.proxyMismatch = !!p.country && p.proxyCountries.some((x) => x !== String(p.country).toUpperCase());
    }
    const count = new Map();
    for (const p of byId.values()) for (const c of p.channels) count.set(c.platform, (count.get(c.platform) || 0) + 1);
    const platforms = [...count.keys()].sort((a, b) => {
      const ia = PLATFORM_ORDER.indexOf(a), ib = PLATFORM_ORDER.indexOf(b);
      return (ia < 0 ? 99 : ia) - (ib < 0 ? 99 : ib) || count.get(b) - count.get(a);
    });
    // Keep the order of channels.json (the catalog order), then personas without channels by name.
    const order = new Map();
    data.channels.forEach((r, i) => { const id = r && low(r.persona); if (id && !order.has(id)) order.set(id, i); });
    const personas = [...byId.values()].sort((a, b) => (order.has(a.id) ? order.get(a.id) : 1e9) - (order.has(b.id) ? order.get(b.id) : 1e9) || a.name.localeCompare(b.name, 'ru'));
    return { personas, platforms, platformCount: count, errors: data.errors || [] };
  }

  /** Sum budget-shaped objects per currency: { [cur]: { total, items } }. */
  function sumMoney(list) {
    const out = {};
    for (const m of list) {
      if (!m) continue;
      const o = out[m.currency] || (out[m.currency] = { total: 0, items: Object.fromEntries(BUDGET_KEYS.map((k) => [k, 0])) });
      o.total += m.total;
      for (const k of BUDGET_KEYS) o.items[k] += m.items[k] || 0;
    }
    return out;
  }
  /**
   * Header totals over the given channels (after filters).
   *   reach   = Σ reach;  target = Σ kpi_target of channels that have one;  pct = Σ reach (of those) / target
   *   cost    = Σ cost per currency;  cpk = cost / (reach / 1000) when one currency
   *   onTarget / below = channels with a target whose reach ≥ / < it
   */
  function totals(channels, now) {
    now = now || Date.now();
    const byStage = {}; let reach = 0, reachKnown = 0, target = 0, reachOfTargeted = 0, onTarget = 0, below = 0, scheduled = 0, next = null;
    const cost = {};
    for (const c of channels) {
      byStage[c.stage] = (byStage[c.stage] || 0) + 1;
      if (c.reach !== null) { reach += c.reach; reachKnown++; }
      if (c.target > 0) { target += c.target; reachOfTargeted += c.reach || 0; if (c.pct !== null && c.pct >= 1) onTarget++; else below++; }
      if (c.cost !== null) cost[c.currency] = (cost[c.currency] || 0) + c.cost;
      scheduled += c.scheduled;
      for (const x of c.posts) if (x.status === 'scheduled' && x.at >= now && (!next || x.at < next.at)) next = x;
    }
    const curs = Object.keys(cost);
    return {
      channels: channels.length, byStage, reach: reachKnown ? reach : null, target: target || null,
      pct: target ? reachOfTargeted / target : null, cost,
      cpk: curs.length === 1 && reach > 0 ? cost[curs[0]] / (reach / 1000) : null, cpkCurrency: curs[0] || DEFAULT_CURRENCY,
      onTarget, below, scheduled, next,
      budget: sumMoney(channels.map((c) => c.budget)), spentPlan: sumMoney(channels.map((c) => c.spentPlan)),
    };
  }

  // ── formatting
  function fmtNum(n) {
    if (n === null || n === undefined || !Number.isFinite(n)) return '—';
    const a = Math.abs(n);
    if (a >= 1e6) return (n / 1e6).toFixed(a >= 1e7 ? 0 : 1).replace(/\.0$/, '') + 'M';
    if (a >= 1e3) return (n / 1e3).toFixed(a >= 1e4 ? 0 : 1).replace(/\.0$/, '') + 'k';
    return String(Math.round(n));
  }
  const fmtPct = (x) => (x === null || x === undefined || !Number.isFinite(x) ? '—' : `${Math.round(x * 100)}%`);
  function fmtMoney(n, cur) {
    if (n === null || n === undefined || !Number.isFinite(n)) return '—';
    const v = Math.round(n).toLocaleString('en-US');
    return ['zł', 'Kč'].includes(cur) ? `${v} ${cur}` : `${cur}${v}`;
  }
  /** { '€': 1200, '$': 5 } → "€1,200 + $5"; empty → "—". */
  const fmtMoneyMap = (m) => (Object.keys(m).length ? Object.entries(m).map(([c, v]) => fmtMoney(v, c)).join(' + ') : '—');
  function fmtWhen(post) {
    if (!post) return '—';
    if (!Number.isFinite(post.at)) return post.when || '—';
    const d = new Date(post.at);
    const hasTime = /\d{1,2}:\d{2}/.test(post.when);
    return d.toLocaleDateString('en-GB', { day: 'numeric', month: 'short' }) + (hasTime ? ' ' + d.toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit' }) : '');
  }

  const PLATFORM_NAMES = { tiktok: 'TikTok', instagram: 'Instagram', youtube: 'YouTube', facebook: 'Facebook', x: 'X', threads: 'Threads', telegram: 'Telegram' };
  // Monochrome glyphs (currentColor), 16×16.
  const PLATFORM_ICONS = {
    tiktok: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M9.2 1.8v8.6a2.6 2.6 0 1 1-2.6-2.6"/><path d="M9.2 1.8c.3 1.8 1.5 3 3.4 3.2"/></svg>',
    instagram: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><rect x="2.25" y="2.25" width="11.5" height="11.5" rx="3.4"/><circle cx="8" cy="8" r="2.7"/><circle cx="11.6" cy="4.4" r=".55" fill="currentColor" stroke="none"/></svg>',
    youtube: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round"><rect x="1.5" y="3.3" width="13" height="9.4" rx="2.6"/><path d="M6.7 5.9v4.2L10.3 8z" fill="currentColor"/></svg>',
    facebook: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><circle cx="8" cy="8" r="6.3"/><path d="M9.8 5.2H9a1.4 1.4 0 0 0-1.4 1.4v7.6M6 8.6h3.6"/></svg>',
    x: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"><path d="M3 2.5 13 13.5M13 2.5 3 13.5"/></svg>',
    threads: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"><path d="M11.3 7.3C11 5 9.7 4 8 4 5.6 4 4.2 5.8 4.2 8.1s1.4 4.1 3.9 4.1c2.3 0 3.7-1.4 3.7-3.1 0-1.9-1.8-2.5-3.3-2.3-1.2.2-1.9.9-1.9 1.7 0 .9.9 1.5 1.9 1.4 1.6-.1 2.2-1.6 2.1-3.5"/></svg>',
    telegram: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round"><path d="M14 2.6 1.9 7.3c-.5.2-.5.9 0 1.1l3 1 1.2 3.7c.1.4.7.5 1 .2l1.8-1.7 3 2.2c.4.3 1 .1 1.1-.4L14.9 3.4c.1-.6-.4-1-.9-.8z"/><path d="m4.9 9.4 6.4-4.1-4.5 5"/></svg>',
  };
  const GENERIC_ICON = '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><circle cx="8" cy="8" r="6"/><path d="M2 8h12M8 2c2 2 2 10 0 12M8 2c-2 2-2 10 0 12"/></svg>';
  const FLAGS = { DE: '🇩🇪', PL: '🇵🇱', FR: '🇫🇷', IT: '🇮🇹', CZ: '🇨🇿', ES: '🇪🇸', NL: '🇳🇱', AT: '🇦🇹', BE: '🇧🇪', PT: '🇵🇹' };
  const flag = (c) => FLAGS[String(c || '').toUpperCase()] || '';
  const platformIcon = (p) => PLATFORM_ICONS[low(p)] || GENERIC_ICON;
  const platformName = (p) => PLATFORM_NAMES[low(p)] || String(p || 'Other');

  const api = {
    BUDGET_KEYS, sumMoney, STAGES, STAGE_LABEL, POST_STATUSES, DEFAULT_CURRENCY, DEFAULT_KPI, PLATFORM_ORDER, ROLES,
    frontMatter, parseCsv, amount, load, join, totals,
    fmtNum, fmtPct, fmtMoney, fmtMoneyMap, fmtWhen, platformIcon, platformName, flag,
  };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  else root.CH = api;
})(typeof window !== 'undefined' ? window : this);
