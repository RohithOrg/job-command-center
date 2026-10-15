# Job Command Center (desktop)

A native app for Mac, Windows and Linux that turns job postings into honest, tailored applications. Everything runs on your computer; the only thing that goes online is the AI service you connect.

## Install

Download the installer for your computer from the project's Releases page:

| Computer | File |
|---|---|
| Mac (Apple silicon or Intel, macOS 11+) | `Job-Command-Center-<version>-macos.dmg`: open it and drag the app to Applications |
| Windows 10/11 | `Job Command Center_<version>_x64-setup.exe` |
| Linux | `.deb` (Ubuntu, Debian) or `.AppImage` (any distribution) |

Nothing else needs installing. The search model, PDF designs, fonts and company list are inside the app.

On first launch the app sets up its private folder, then walks you through four steps you can skip: connect your AI, import your resume, say what you're looking for, and star your favourite companies.

## What it does

- **Applications**: paste a job link. Agents read the posting, score the fit against your profile, tailor a resume and check it the way an applicant tracking system (ATS) would, by rendering the real PDF and reading it back. Every claim is checked against your profile; skills you don't have are never added.
- **Resume versions**: every draft, revision and design change is kept. Preview the pages in the app, switch between four designs, or tell the agent what to change.
- **Application Q&A**: paste a question from an application form and get an answer from your profile and that job's resume. Save good answers; later applications reuse them.
- **Discover and Companies**: the app checks your favourite companies' job pages for roles that match your titles and places.
- **History**: close applications instead of deleting them. If the same job comes back, by link or by a near-identical description, the app tells you.

## AI providers

Anthropic (Claude), OpenAI, Google Gemini, OpenRouter, Groq, Ollama or LM Studio on your computer, or any OpenAI-compatible URL. API keys are stored in the operating system's password store (Keychain, Windows Credential Manager, Secret Service), never in the app's files.

## Use it on your phone

Settings → Phone and web turns on a small website served by the app itself. Scan the QR code on a phone on the same Wi-Fi and enter the passcode shown in the desktop app (it changes every 24 hours). It can be added to the home screen like an app.

To reach it from anywhere, paste an ngrok authtoken and turn on "From anywhere"; the passcode then becomes a longer phrase, and you can optionally limit access to one Google account. AI keys, phone access, the data folder and deleting things can only be changed in the desktop app.

## Where your data lives

| OS | Folder |
|---|---|
| macOS | `~/Library/Application Support/com.JobCommandCenter.Job-Command-Center/` |
| Windows | `%APPDATA%\JobCommandCenter\Job Command Center\data\` |
| Linux | `~/.local/share/jobcommandcenter/` |

It holds one SQLite database and the generated PDFs. Back it up by copying the folder.

## For developers

```bash
cargo run                      # debug build
cargo test                     # unit tests (add -- --include-ignored for live network tests)
./scripts/macos-bundle.sh      # local .app + .dmg
cargo packager --release       # installers for the current OS (cargo install cargo-packager)
```

Pushing a tag like `v0.1.0` runs `.github/workflows/desktop.yml`, which builds all installers and attaches them to a GitHub release. Linux builds need `libxkbcommon-dev libfontconfig1-dev libdbus-1-dev`.

| Part | Where |
|---|---|
| Window and screens (Slint) | `ui/` |
| AI providers | `src/ai.rs` |
| Agent pipeline and ATS loop | `src/pipeline.rs`, `src/ats.rs`, `src/prompts.rs`, `rules/` |
| Document library and search | `src/rag.rs`, `src/embed.rs` (all-MiniLM-L6-v2, bundled) |
| Q&A | `src/chat.rs` |
| Resume PDFs and previews (Typst) | `src/pdf.rs`, `templates/` |
| Reading postings, scanning career sites | `src/fetch.rs`, `src/scanner.rs`, `catalog.json` |
| Storage | `src/db.rs` |

Debug builds accept `JCC_DATA_DIR=<folder>` to use a separate data folder, and `--cli <command>` to run the agents without the window (`set-ai`, `test-ai`, `run <url>`, `ask <id> <question>`, `revise`, `restyle`, `index`, `search`).

## Licenses

App: MIT. Bundled: all-MiniLM-L6-v2 (Apache-2.0), Source Sans 3, IBM Plex Sans, Libertinus Serif, Bricolage Grotesque, Atkinson Hyperlegible Next (SIL OFL 1.1). Job titles and skills suggestions come from ESCO (European Commission); city suggestions from Open-Meteo.
