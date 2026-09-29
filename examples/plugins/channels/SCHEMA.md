# Channels — data format

The network of AI personas, their channels and the inventory that runs them
(proxies, emails, phones, device profiles, platform accounts…) lives in **one
SQLite database: `~/AgentArea/state.db`**. AgentArea Desktop owns it. Long texts
stay as files.

```
~/AgentArea/
  state.db                 the database (WAL; never edit it while the app runs, use the tools)
  secrets.json             vault metadata (names only); values are in the Keychain
  channels/
    SCHEMA.md              this file
    <handle>/SOUL.md       each persona's soul (open SOUL.md spec) — edit directly
    <handle>/posts/…       long post drafts (referenced from posts.body_or_path)
    export/*.json          read-only snapshots, rewritten ~1 s after every change
    grid.html              standalone grid (reads export/)
    _legacy/               the old channels.json / config.json / plan.json / posts.json / metrics.csv (backup)
```

## How to read and change it

**Agents** (local threads) use the `agentarea_data` MCP server:

| tool | what it does |
|---|---|
| `state_query` | read-only SQL: one `SELECT` / `WITH`, ≤ 1000 rows (default 200), 3 s limit. Writes, `PRAGMA`, `ATTACH` are rejected. JSON columns: use `json_extract()`. |
| `channel_update` | `{persona, platform \| channel_id, set: {…}}` — stage, role, plan, handle_registered, url, kpi_metric, kpi_target, mcp, notes, `tasks` (full list), `budget` (planned per month), `spent` (actuals, `period` `YYYY-MM`, default this month) |
| `persona_update` | `{handle, set: {…}}` — name, emoji, niche, category, country, city, languages, target_audience, hypothesis, personality, references, soul_path |
| `inventory_add` | `{type, label, provider, geo, cost_month, currency, status, capacity, secret_ref, meta, notes}` → the item with its `id` |
| `inventory_update` | `{id, set: {…}}` — any item field |
| `inventory_bind` | `{item_id, subject_type: persona\|channel\|mcp, subject_id, purpose}` |
| `inventory_unbind` | `{binding_id}` or `{item_id, subject_type, subject_id, purpose?}` |
| `post_upsert` | create (give the channel) or update (give `id`) a post |
| `metrics_add` | one row per channel per date (same date again = replace); batch with `rows: [...]` |
| `inventory_gaps` | what's missing per requirement, plus warnings |

Every write runs in a transaction and lands in `audit_log` with its actor
(`agent:<thread id>`, `user`, `plugin:channels`, `import`, `migration`).

**The Channels plugin** (sidebar → Channels) reads the same data through the
`state` bridge and writes through the same typed calls. Tabs: Channels (table /
graph), Inventory, Gaps.

**Scripts / diffs / backups**: read `channels/export/*.json` (read-only, same
shapes as the old files plus inventory). Don't write them — they are replaced.

## Conventions

- **Secrets never go into the database.** Passwords, proxy logins, API keys,
  2FA seeds, cookies live in the user's vault (AgentArea → Secrets, values in the
  Keychain). An item stores only the secret's **name** in `secret_ref`
  (`UPPER_SNAKE_CASE`, e.g. `PROXY_PL_20001`). `meta` keys like `password`,
  `token`, `secret`, `cookie`, `otp`, `cvv`, `pin` and `user:pass@` URLs are rejected.
- A persona's items serve all its channels. Bind to a channel only what is
  channel-specific (a platform account, a per-channel API key).
- Mark a dead item `broken` (or `retired`) instead of deleting it; bindings to
  broken/retired items stop counting as coverage and show as warnings.
- Numbers are numbers (`12400`), dates `YYYY-MM-DD`, timestamps UTC ISO.

## Tables

### personas
| column | |
|---|---|
| `handle` PK | the @handle to register (a-z 0-9 . _), also the folder name |
| `name`, `emoji`, `niche`, `category`, `city` | |
| `country` | `DE` · `PL` · `IT` (the persona's home; proxies should exit there) |
| `languages` | JSON array — English first + the local language |
| `target_audience`, `hypothesis` | text; `hypothesis` = what this persona tests |
| `personality` | JSON: `{ocean: {openness, conscientiousness, extraversion, agreeableness, neuroticism (1–5)}, valence positive\|neutral\|edgy, humor none\|dry\|goofy\|sarcastic, energy calm\|medium\|high, archetype, likes[], dislikes[]}` |
| `refs` | JSON array of `{name, platform, why}` (tools accept `references`) |
| `soul_path` | `<handle>/SOUL.md`, relative to `~/AgentArea/channels` |

Categories: Home & Construction · Home Services & Garden · Real Estate ·
Finance, Legal & Business · Auto · Food & Drink · Beauty · Health & Fitness ·
Education & Family · Creative & Events · Pets · Travel & Outdoors.

### channels — one per persona × platform, `UNIQUE(persona, platform)`
| column | |
|---|---|
| `id` | integer, used by bindings / posts / metrics |
| `persona` → personas | |
| `platform` | `tiktok` · `instagram` · `youtube` · `facebook` · `x` · `threads` · `telegram` |
| `handle_registered` | the real handle once registered (`""` until then; Telegram: `_` instead of `.`) |
| `url` | profile URL once registered |
| `stage` | `idea` → `setup` → `setup_done` → `live` → `paused` / `stopped` |
| `role` | `creator` (publishes) · `community` (groups/chats) · `engager` (browses, likes, comments) |
| `plan` | one line: what the persona does on this network |
| `kpi_metric`, `kpi_target` | what counts as reach (below) and the 30-day target (or null) |
| `mcp` | MCP server that runs the account; `""` until connected |
| `notes` | next step, reason for a stage change |

`channel_tasks(channel_id, position, type post|reply|comment|like|browse|chat, cadence '3/week'|'daily'…, notes)`.

### budget — `PRIMARY KEY(channel_id, kind, period)`
`kind` `planned` (period `monthly`) or `spent` (period `YYYY-MM`, one row per
month of actuals); `currency` (EUR default) and amounts `proxy`, `account`,
`content`, `ads`, `other`.

Planned defaults (EUR / month): proxy €8 per persona (one residential IP in its
country, shared by its 7 accounts → €1.14 per channel); content €12 for creators
on TikTok / Instagram / YouTube / Facebook, €4 on X / Threads / Telegram, €3
community, €1.50 engager.

### metrics — `UNIQUE(channel_id, date)`
`date`, `reach` (by the channel's `kpi_metric`, in the period ending on `date`),
`followers` (a level at `date`), `posts_published`, `cost`, `currency`. One row
per channel per day is best.

Derived: reach and cost = sums over the last 30 days; followers = latest row;
% of KPI = reach / kpi_target; cost per 1k = cost / (reach / 1000).

Default `kpi_metric`: TikTok video views · Instagram Reels plays / accounts
reached · YouTube Shorts views · Facebook Reels plays · X impressions · Threads
views · Telegram post views (all over 30 days).

### posts
`channel_id`, `scheduled_at` (`YYYY-MM-DD HH:MM` local), `status`
`draft` · `scheduled` · `published`, `title`, `body_or_path` (text, or a path
relative to `~/AgentArea/channels`).

### inventory_items
| column | |
|---|---|
| `type` | `proxy` · `email` · `phone` · `device_profile` · `account` · `api_key` · `domain` · `card` · `other` |
| `label` | short name, e.g. `PL · 146.59.46.82:20001`, `@username` |
| `provider`, `geo` | `geo` starts with the country code: `DE`, `DE-BY`, `PL / Warsaw`, `Munich, DE` |
| `cost_month`, `currency` | |
| `status` | `active` · `reserved` (set aside, still bindable) · `broken` · `retired` |
| `capacity` | how many subjects may share it; `null` = unlimited; a persona's proxy: 1 |
| `secret_ref` | vault secret NAME, or null |
| `meta` | JSON of plain facts — proxy `{host, port, protocol, ip, verified}`, email `{address}`, phone `{number}`, device_profile `{tool, profile_id}`, api_key `{service}` |
| `notes` | |

An **account** is one registered or purchased platform login:
`meta {platform, username, url, email_item, recovery_email_item, has_2fa}`
(`email_item` = the id of its email item). It binds only to a channel of the
same platform, purpose `login`.

### bindings
`item_id` → inventory_items, `subject_type` `persona` · `channel` · `mcp`,
`subject_id` (persona handle · channel id · MCP name), `purpose` `egress` ·
`login` · `2fa` · `api` · `device` · `other`, `since`, `until` (null = active;
unbinding sets it, history stays). Default purpose by type: proxy → egress,
email → login, phone → 2fa, device_profile → device, api_key → api,
account → login. Binding is refused when the item is broken/retired or its
capacity is full.

### inventory_requirements
`platform` (`*` or one platform), `type`, `scope` `persona` · `channel`,
`purpose`, `count`, `geo_match` (item country must equal the persona's).
Seeded: every persona needs 1 proxy (egress, same country), 1 email (login),
1 phone (2fa), 1 device_profile (device). Add e.g. an account per TikTok
channel with `requirement_add` (plugin / app call).

**Gaps** = subjects with fewer usable bindings (matching type and purpose, item
active or reserved) than required. **Warnings**: `geo_mismatch`, `capacity`
(shared beyond capacity), `bad_status` (broken/retired item still bound).

### audit_log
`at`, `actor`, `action` (create · update · delete · bind · unbind · import),
`entity`, `entity_id`, `diff` (JSON `{field: {from, to}}`).

## Examples

```sql
-- channels per stage and platform
SELECT platform, stage, count(*) FROM channels GROUP BY 1, 2;
-- personas without a proxy, with their country
SELECT p.handle, p.country FROM personas p
WHERE NOT EXISTS (SELECT 1 FROM bindings b JOIN inventory_items i ON i.id = b.item_id
                  WHERE b.until IS NULL AND b.subject_type = 'persona' AND b.subject_id = p.handle
                    AND i.type = 'proxy' AND i.status IN ('active', 'reserved'));
-- free TikTok accounts
SELECT id, json_extract(meta, '$.username') FROM inventory_items i
WHERE type = 'account' AND json_extract(meta, '$.platform') = 'tiktok'
  AND NOT EXISTS (SELECT 1 FROM bindings b WHERE b.item_id = i.id AND b.until IS NULL);
```
