# Web companion API

Served by the desktop app (`src/web.rs`) when "Use on your phone" is on. Same origin as the page.

## Auth
- `POST /api/login` `{ "pin": "123456" }` → `200 {}` and sets cookie `jcc_session` (HttpOnly, SameSite=Strict). `401 {"error": "..."}` on a wrong PIN; `429` after too many tries.
- `POST /api/logout` → clears the session.
- Every other `/api/*` call needs the cookie (`401` otherwise) and, for POST, the header `X-JCC: 1`.
- Errors are always `{ "error": "human sentence" }`.

## Read
- `GET /api/me` → `{ "name": "Alex", "working": 2, "ai_connected": true }`
- `GET /api/today` → `{ "date": "Thursday 15 October", "items": [JobRow] }`
- `GET /api/jobs?status=Active|Closed|All|<status>&q=text&page=0` → `{ "items": [JobRow], "page": 0, "pages": 2, "total": 38 }` (25 per page)
- `GET /api/jobs/{id}` → `JobDetail`
- `GET /api/jobs/{id}/pdf?version={vid}` → the PDF (current version when `version` is absent). `Content-Disposition: inline; filename="..."`.
- `GET /api/discover?country=&q=&page=0` → `{ "items": [DiscoverRow], "countries": ["All countries", ...], "page", "pages", "total" }`

## Write (POST, JSON body)
- `/api/jobs` `{ "url": "https://...", "text": "" }` → `{ "id": 50 }` or `409 { "error": "...", "duplicate_of": 22 }` (send `"force": true` to add anyway). Starts the agents.
- `/api/jobs/{id}/close` `{ "reason": "I'm not interested", "note": "" }`
- `/api/jobs/close` `{ "ids": [1,2], "reason": "...", "note": "" }` (several at once)
- `/api/jobs/{id}/reopen` `{}`
- `/api/jobs/{id}/status` `{ "status": "Applied" }`
- `/api/jobs/{id}/progress` `{ "next_step": "", "date": "YYYY-MM-DD", "notes": "" }`
- `/api/jobs/{id}/tailor` `{}` → starts a new tailored version
- `/api/jobs/{id}/revise` `{ "instruction": "shorter summary" }`
- `/api/jobs/{id}/ask` `{ "question": "..." }` → `ChatMsg` (the answer; may take 10–60 s)
- `/api/chat/{msg_id}/save` `{}` → `{ "saved": true|false }` (toggle)
- `/api/discover/{id}/tailor` `{}` → `{ "job_id": 51 }`
- `/api/discover/{id}/dismiss` `{}`

## Shapes
```
JobRow    { id, title, company, location, status, fit: "4.1"|"", ats: "58"|"", position: 0..5, ended: bool,
            working: bool, stage_label: "Tailoring the resume"|"", when: "Today"|"Overdue"|"12 Oct"|"Now"|"–",
            board: "Boarding"|"Delayed"|"Working"|"Stopped"|"", reason: "Resume is ready…", updated: "14 Oct", initial: "T", hue: 0..999 }
JobDetail { ...JobRow fields, url, recommendation, fit_summary, archetype, work_auth, strengths: [str], gaps: [str],
            red_flags: [str], job_summary, salary, remote, seniority, next_step, next_step_date, applied, notes,
            cover_letter, error, duplicate_note,
            versions: [ { id, label: "Version 3", kind, note, ats: 58|null, template_name, created, current: bool } ],
            current_version: id|null,
            ats_report: { score, coverage, structure, pages, matched: [str], missing: [str], gaps: [str], unsupported: [str] } | null,
            chat: [ChatMsg], events: [ { time, text } ], work: [ { time, agent, level, message } ],
            statuses: ["New","Evaluated","Ready","Applied","Interview","Offer","Rejected","Discarded","Closed"],
            close_reasons: [str] }
ChatMsg   { id, role: "user"|"assistant", content, meta, saved: bool }
DiscoverRow { id, title, company, location, country, url, seen, state, job_id: id|null, initial, hue }
```
Stage line: positions 0..5 = Found, Fit checked, Tailored, Applied, Interview, Offer. Stage colours
`#8592A0 #3B7DD8 #7A5AF0 #1F9D6E #E3A83B #E4572E`; monogram colour = HSV(hue*47 mod 360, 0.55, 0.62).

## Profile (added)
- `GET /api/profile` → `{ profiles: [{id,name,active}], profile: ProfileForm, options: { countries, currencies, seniority, work_modes, notice, tones } }` (the active profile; API keys are never included)
- `POST /api/profile` body `ProfileForm` → `{ ok: true }`
- `GET /api/suggest?field=<field>&q=<text>` → `{ items: [str] }` (fields: target-roles, countries, cities, avoid-places, authorized-in, languages, strengths)
- `ProfileForm = { id, name, full_name, headline, email, phone, city, country, linkedin, github, website, other_link, target_roles: [str], seniority, work_mode, countries: [str], cities: [str], relocate: bool, avoid_places: [str], exclude_words: [str], salary_target, salary_minimum, currency, authorized_in: [str], needs_sponsorship: bool, visa_notes, notice, languages: [str], story, strengths: [str], avoid, tone, notes_md, cv_md }`
- Sign-in field is a "passcode": 6 digits on the home network, or a longer code like `k7mq-x4tn-9vbr` when reached over the internet.

## Settings (added) — only what is safe from a browser
- `GET /api/settings` → `{ ai: {connected, provider, model}, search: {auto, limit, interval}, favourite_companies, scanning, desktop_only: [str] }`
- `POST /api/settings/search` `{ auto: bool, limit: int, interval: hours }` → `{ ok: true }`
- `POST /api/scan` `{}` → `{ started: true }` (409 if one is running). Scans the favourite companies.
- Not available from the browser (show `desktop_only` as a short note): AI service and keys, phone/web access and passcode, file location, deleting applications or profiles.

## Install as an app
- `/manifest.webmanifest`, `/sw.js` (register it), `/icon-512.png`. Add `<link rel="manifest">`, theme-color, apple-touch-icon.
