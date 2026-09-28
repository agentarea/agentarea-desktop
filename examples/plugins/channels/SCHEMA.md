# Channels — data format

Plain files, edited by agents, read by the **Channels** plugin (AgentArea
Desktop → sidebar "Channels"). The plugin only reads; it re-checks the files
every few seconds. Formulas live in `~/AgentArea/plugins/channels/grid.js`.

```
~/AgentArea/channels/
  SCHEMA.md          this file
  channels.json      config/state: one object per channel (persona × platform)
  metrics.csv        history: one row per channel per measurement (daily is best)
  posts.json         planned / published posts
  grid.html          the same grid as a plain page (see "Open in a browser")
  <handle>/          one folder per persona, named by the @handle to register (a-z 0-9 . _ , ≤ 24)
    SOUL.md          the persona's soul (open SOUL.md spec)
    config.json      structured fields the UI uses
```

Rewrite a file whole (write a temp file, then rename) so readers never see
half of it. Numbers are plain: `12400`, not `12 400` or `12,400`.

## Open in a browser

```
cd ~/AgentArea/channels && python3 -m http.server 8765
open http://127.0.0.1:8765/grid.html
```

`grid.html` is generated from the plugin page (`~/AgentArea/plugins/channels/`,
`python3 make-grid.py`); outside the app "Discuss in a thread" copies the prompt
instead of opening a thread.

## Persona: `<handle>/SOUL.md` + `<handle>/config.json`

Each persona is a fictional, AI-generated creator living in Germany, Poland or
Italy (`DE`, `PL`, `IT`) with a Ukrainian or Belarusian name. The folder name is the
handle we want to register on every platform (e.g. `oleks.sparks`); the
per-platform `handle` in `channels.json` stays empty until actually registered
(it may differ if taken).

### SOUL.md — open SOUL.md spec (github.com/AntonioTF5/soul-spec)

YAML front matter. Required: `name` (`"@handle"`), `version`, `description`,
`personality`. Also used: `tone`, `values[]`, `constraints[]` (must include
AI-generated disclosure and never impersonating real people), `knowledge_domains[]`,
`communication_style`, `memory_mode`, `goals[]`, `language` (BCP 47, `en`),
`platform_hints` (one note per platform, reflecting the channel's role).

Body sections: Identity & backstory · Personality (incl. the hypothesis) ·
Likes · Dislikes · Voice (5 example lines) · Audience · Content pillars &
series (with hooks) · References (2–3 real creators, format only) · Boundaries.

### config.json

```json
{
 "handle": "oleks.sparks",
 "name": "Oleksandr Kovalenko",
 "emoji": "⚡",
 "niche": "Electrician",
 "category": "Home & Construction",
 "country": "DE",
 "city": "Munich",
 "languages": ["English", "German"],
 "target_audience": "Homeowners and renters 22–45 …",
 "references": [{"name": "Electrician U (Dustin Stelzer)", "why": "short explainers"}],
 "platforms": ["tiktok", "instagram", "youtube", "facebook", "x", "threads", "telegram"],
 "personality": {
  "openness": 4, "conscientiousness": 5, "extraversion": 2, "agreeableness": 3, "neuroticism": 2,
  "valence": "neutral", "humor": "dry", "energy": "calm", "archetype": "Expert",
  "likes": ["…"], "dislikes": ["…"]
 },
 "hypothesis": "In electrician content, a neutral Expert voice … earns more reach per post than the niche's usual tone."
}
```

| field | values |
|---|---|
| `country` | `DE` · `PL` · `IT` (the persona's home; channel proxy `geo` defaults to its city) |
| `languages` | English first + the local language (German / Polish / Italian) |
| OCEAN (`openness` … `neuroticism`) | 1–5 |
| `valence` | `positive` · `neutral` · `edgy` (contrarian) |
| `humor` | `none` · `dry` · `goofy` · `sarcastic` |
| `energy` | `calm` · `medium` · `high` |
| `archetype` | `Mentor` · `Expert` · `Everyman` · `Jester` · `Rebel` · `Caregiver` · `Explorer` · `Creator` |
| `category` (cluster) | Home & Construction · Home Services & Garden · Real Estate · Finance, Legal & Business · Auto · Food & Drink · Beauty · Health & Fitness · Education & Family · Creative & Events · Pets · Travel & Outdoors |

Personalities are deliberately varied across the network (and within each
niche cluster) to test which personality types get the most reach per niche;
`hypothesis` records what each persona tests.

## Channels: `channels.json`

An array; one object per persona × platform.

```json
[
 {
  "persona": "oleks.sparks",
  "platform": "tiktok",
  "handle": "",
  "stage": "setup",
  "role": "creator",
  "tasks": [
   {"type": "post", "cadence": "3/week", "notes": ""},
   {"type": "reply", "cadence": "daily", "notes": "answer comments in the first hour"}
  ],
  "plan": "Posts 3 Shorts a week from the TikTok cut + answers comments daily",
  "budget": {"currency": "EUR", "proxy": 4, "account": 0, "content": 12, "ads": 0, "other": 1},
  "spent": {"currency": "EUR", "proxy": 4, "account": 0, "content": 3, "ads": 0, "other": 0},
  "kpi_metric": "video views, 30d",
  "kpi_target": 20000,
  "mcp": "",
  "network": {
   "egress": "Residential · DE-BY · provider X #12",
   "ip": "",
   "geo": "Munich, DE",
   "device_profile": "chrome-profile-oleks-sparks",
   "url": ""
  },
  "notes": "",
  "created": "2026-09-28",
  "updated": "2026-09-28"
 }
]
```

| field        | meaning                                                                   |
|--------------|---------------------------------------------------------------------------|
| `persona`    | persona folder name (= the @handle, without @)                            |
| `platform`   | platform id (below); every persona has all 7 platforms                    |
| `role`       | `creator` (publishes content) · `community` (in groups/chats, talks sometimes) · `engager` (browses, likes, occasionally comments) |
| `tasks`      | `[{type, cadence, notes}]`, type = `post` · `reply` · `comment` · `like` · `browse` · `chat`; cadence like `3/week`, `daily`, `weekly` |
| `plan`       | one line: what this persona does on this network (shown in the cell and panel) |
| `budget`     | planned **per month**: `{currency, proxy, account, content, ads, other}` (numbers; `currency` default EUR) |
| `spent`      | actual spend this month, same shape as `budget`                           |
| `handle`     | account handle or URL once registered; `""` until then                    |
| `stage`      | `idea` → `setup` → `setup_done` → `live` → `paused` / `stopped`           |
| `kpi_metric` | what counts as reach on this channel (defaults below)                     |
| `kpi_target` | reach target for 30 days (number) or `null`                               |
| `mcp`        | MCP server the agents use to run the account; `""` until connected        |
| `network`    | where the account connects from and to (below)                            |
| `notes`      | free text: next step, reason for a stage change                           |
| `created` / `updated` | `YYYY-MM-DD`; bump `updated` on every change                     |

### `network` — where the account connects from → to

| field            | meaning                                                                      |
|------------------|------------------------------------------------------------------------------|
| `egress`         | proxy / provider label, e.g. `Residential · DE-BY · provider X #12`. **Never credentials.** If a proxy needs a login, write the Secrets vault name, e.g. `secret: proxy-de-by-12` (the value lives in the Keychain vault). |
| `ip`             | last known exit IP (optional)                                                |
| `geo`            | country/city of the exit — defaults to the persona's own city, e.g. `Munich, DE` |
| `device_profile` | browser/device profile name used for the account                             |
| `url`            | the account's profile URL once registered                                    |

Empty strings until set. The table shows the persona's country and the proxy
(exit) country from `geo`, flagging a mismatch; the channel panel shows the full block.

## Metrics: `metrics.csv`

```csv
date,persona,platform,reach,cost,currency,followers,posts_published
2026-10-01,oleks.sparks,tiktok,18400,35,EUR,212,1
```

One row = what happened on that channel in the period ending on `date`
(one row per channel per day is best; never rewrite old rows, append).

| column            | meaning                                                         |
|-------------------|-----------------------------------------------------------------|
| `date`            | `YYYY-MM-DD`                                                    |
| `persona`, `platform` | the channel                                                 |
| `reach`           | reach in that period, by the channel's `kpi_metric`             |
| `cost`            | spend in that period (tools, ads, LLM…)                         |
| `currency`        | `EUR` (default) · `USD` · `GBP` · `PLN` · `CZK`                 |
| `followers`       | followers at `date` (a level, not a delta)                      |
| `posts_published` | posts published in that period                                  |

Derived per channel: **reach** and **cost** = sums over rows dated in the last
30 days; **followers** = the latest row's value; **posts (30d)** = sum of
`posts_published` over 30 days.

Stages in the UI: **Idea** (`idea`) · **Setting up** (`setup`) · **Set up**
(`setup_done`) · **Live** (`live`) · **Paused** (`paused`) · **Stopped**
(`stopped`). Unknown values show as `idea`.

### KPI: reach

The north-star KPI is **reach over the last 30 days**, measured per channel by
its own `kpi_metric` and compared with its own `kpi_target`.

Default `kpi_metric` per platform:

| platform         | id          | reach = (30 days)                     |
|------------------|-------------|---------------------------------------|
| TikTok           | `tiktok`    | video views                           |
| Instagram Reels  | `instagram` | Reels plays / accounts reached        |
| YouTube Shorts   | `youtube`   | Shorts views                          |
| Facebook Reels   | `facebook`  | Reels plays                           |
| X                | `x`         | impressions                           |
| Threads          | `threads`   | views                                 |
| Telegram         | `telegram`  | post views                            |

Per channel:

- **% of KPI** = `reach / kpi_target` (shown when both are set)
- **cost per 1k reach** = `cost / (reach / 1000)` (shown when both are set)

Header totals (over the channels currently shown):

- **budget / month** = Σ `budget` per currency, split by category; **spent** = Σ `spent`

- **reach** = Σ channel reach (30 days); **target** = Σ `kpi_target` of channels that have one
- **% of KPI** = Σ `reach` of channels with a target / Σ `kpi_target`
- **cost** = Σ `cost` (per currency); **cost per 1k** = Σ cost / (Σ reach / 1000)
- **on target** = channels with a target and `reach ≥ kpi_target`; **below** = the rest with a target

Audiences may overlap between channels; totals are not deduplicated.

## Posts: `posts.json`

```json
[
 {
  "persona": "oleks.sparks",
  "platform": "tiktok",
  "datetime": "2026-10-03 18:00",
  "status": "scheduled",
  "title": "Would you plug this in?",
  "text_or_file": "oleks.sparks/posts/01.md"
 }
]
```

| field          | meaning                                                            |
|----------------|--------------------------------------------------------------------|
| `persona`, `platform` | the channel (must exist in `channels.json`)                 |
| `datetime`     | local time, `YYYY-MM-DD HH:MM` (or `YYYY-MM-DD`)                   |
| `status`       | `draft` · `scheduled` · `published`                                |
| `title`        | short title                                                        |
| `text_or_file` | the text, or a path relative to this folder                        |

Each channel shows its number of `scheduled` posts and the next scheduled date.
