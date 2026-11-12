// Job Command Center: native desktop app (Slint UI, local SQLite, agents on a tokio runtime).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ai;
mod ats;
mod chat;
#[cfg(debug_assertions)]
mod cli;
mod db;
mod embed;
mod fetch;
mod import;
mod options;
mod pdf;
mod pipeline;
mod prompts;
mod rag;
mod resume;
mod scanner;
mod ui;
mod update;
mod web;

use anyhow::Result;
use directories::ProjectDirs;
use slint::ComponentHandle;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

slint::include_modules!();

/// Where this install keeps its data (database, generated resumes, imports).
///
/// New installs use a visible folder, Documents/Job Command Center, so people can find their
/// resumes and back everything up by copying one folder. Installs from before keep their data in
/// the app-data folder until the user moves it (Settings > Your files), which records the choice.
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("JCC_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(chosen) = pointer_file().and_then(|p| std::fs::read_to_string(p).ok()).map(|s| PathBuf::from(s.trim())) {
        if chosen.is_dir() {
            return chosen;
        }
    }
    let legacy = ProjectDirs::from("com", "JobCommandCenter", "Job Command Center").map(|p| p.data_dir().to_path_buf());
    if let Some(l) = legacy.as_ref().filter(|l| l.join("job-command-center.db").exists()) {
        return l.clone();
    }
    directories::UserDirs::new()
        .and_then(|u| u.document_dir().map(|d| d.join("Job Command Center")))
        .or(legacy)
        .unwrap_or_else(|| PathBuf::from("Job Command Center"))
}

/// Where the user's choice of data folder is remembered (outside the data folder itself).
pub fn pointer_file() -> Option<PathBuf> {
    ProjectDirs::from("com", "JobCommandCenter", "Job Command Center").map(|p| p.config_dir().join("data-folder.txt"))
}

/// The packaged .app gets its icon from the bundle; a bare binary (developer builds) would show a
/// generic "exec" tile in the Dock, so set the icon at runtime.
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::AnyThread;
    let Some(mtm) = objc2::MainThreadMarker::new() else { return };
    let data = objc2_foundation::NSData::with_bytes(include_bytes!("../assets/icon-512.png"));
    if let Some(image) = objc2_app_kit::NSImage::initWithData(objc2_app_kit::NSImage::alloc(), &data) {
        #[allow(unused_unsafe)]
        unsafe { objc2_app_kit::NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
    }
}

fn main() -> Result<()> {
    let dir = data_dir();
    std::fs::create_dir_all(dir.join("Resumes"))?;
    let mut db = db::Db::open(&dir.join("job-command-center.db"))?;
    let imported = import::run(&mut db, &dir)?;
    db.adopt_legacy()?;
    if scanner::companies(&db)?.is_empty() {
        scanner::seed_catalog(&db)?;
    }
    let startup = ui::Startup {
        first_run: db.setting("setup_done")? != serde_json::json!(true),
        data_dir: dir.display().to_string(),
        applications: db.job_count()? as usize,
        imported,
        companies: scanner::companies(&db)?.len(),
    };

    #[cfg(debug_assertions)]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.first().map(String::as_str) == Some("--cli") {
            return cli::run(db.store(), &dir, &args[1..]);
        }
    }

    let store = db.store();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(4).build()?;
    let window = AppWindow::new()?;
    #[cfg(target_os = "macos")]
    slint::Timer::single_shot(std::time::Duration::ZERO, set_dock_icon);

    // Agents report each step; refresh what's on screen (coalesced on the UI thread).
    let notify: Arc<dyn Fn(i64) + Send + Sync> = {
        let weak = window.as_weak();
        let store = store.clone();
        let pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Arc::new(move |_job: i64| {
            if pending.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let (store, pending) = (store.clone(), pending.clone());
            let _ = weak.upgrade_in_event_loop(move |w| {
                pending.store(false, std::sync::atomic::Ordering::SeqCst);
                ui::refresh_lists(&w, &store);
                ui::refresh_job(&w, &store);
            });
        })
    };
    let ctx = pipeline::Ctx::new(store.clone(), dir.join("Resumes"), notify);
    let wiring = Rc::new(ui::Wiring { store: store.clone(), ctx: ctx.clone(), rt: rt.handle().clone(), data_dir: dir.clone() });
    ui::wire(&window, wiring.clone());
    ui::start_scheduler(&window, &wiring);
    ui::start_setup(&window, &wiring, startup);
    ui::check_ai(&window, &wiring);
    ui::start_updates(&window, &wiring);
    if imported > 0 {
        window.global::<App>().set_toast(format!("Imported {imported} applications").into());
    }

    // Pick up work interrupted when the app last closed.
    pipeline::resume_unfinished(&ctx, rt.handle());

    #[cfg(debug_assertions)]
    if let Some(id) = std::env::var("JCC_OPEN_JOB").ok().and_then(|v| v.parse::<i32>().ok()) {
        let app = window.global::<App>();
        app.invoke_open_job_page(id);
        if let Some(tab) = std::env::var("JCC_TAB").ok().and_then(|v| v.parse::<i32>().ok()) {
            app.set_job_tab(tab);
        }
    }
    // Debug builds: JCC_SIZE=1080x680 opens the window at that size (layout checks).
    #[cfg(debug_assertions)]
    if let Some((w, h)) = std::env::var("JCC_SIZE").ok().and_then(|v| v.split_once('x').map(|(a, b)| (a.parse::<f32>(), b.parse::<f32>()))) {
        if let (Ok(w), Ok(h)) = (w, h) {
            window.window().set_size(slint::LogicalSize::new(w, h));
        }
    }
    #[cfg(debug_assertions)]
    if let Some(page) = std::env::var("JCC_PAGE").ok().and_then(|v| v.parse::<i32>().ok()) {
        window.global::<App>().invoke_go(page);
    }

    // Debug builds: JCC_DO="ask 2 Why us?; tab 3" runs UI callbacks as if clicked (for end-to-end checks).
    #[cfg(debug_assertions)]
    if let Ok(script) = std::env::var("JCC_DO") {
        let weak = window.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(800), move || {
            let Some(w) = weak.upgrade() else { return };
            let app = w.global::<App>();
            // Run commands 400 ms apart so each (e.g. a click) finishes before the next.
            let cmds: Vec<String> = script.split(';').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
            let _ = &app;
            for (i, cmd) in cmds.into_iter().enumerate() {
                let weak = w.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_millis(400 * i as u64), move || {
                let Some(w) = weak.upgrade() else { return };
                let app = w.global::<App>();
                let cmd = cmd.as_str();
                let parts: Vec<&str> = cmd.splitn(4, ' ').collect();
                let n = |i: usize| parts.get(i).and_then(|x| x.parse::<i32>().ok()).unwrap_or(-1);
                let rest = |i: usize| parts[i.min(parts.len())..].join(" ");
                match parts[0] {
                    "ask" => app.invoke_ask(n(1), rest(2).into()),
                    "revise" => app.invoke_revise(n(1), rest(2).into()),
                    "restyle" => app.invoke_restyle(n(1), n(2), parts.get(3).unwrap_or(&"modern").to_string().into()),
                    "tailor" => app.invoke_tailor(n(1)),
                    "add" => { app.set_adding(true); app.invoke_add_application(rest(1).into(), "".into(), -1) }
                    "scan" => app.invoke_scan(n(1)),
                    "tab" => app.set_job_tab(n(1)),
                    "ptab" => app.set_profile_tab(n(1)),
                    "stab" => app.set_settings_tab(n(1)),
                    "web" => app.invoke_web_toggle(n(1) != 0),
                    // a real pointer press + release at window coordinates, like a user's click
                    "click" => {
                        let pos = slint::LogicalPosition::new(n(1) as f32, n(2) as f32);
                        let win = w.window();
                        win.dispatch_event(slint::platform::WindowEvent::PointerMoved { position: pos });
                        win.dispatch_event(slint::platform::WindowEvent::PointerPressed { position: pos, button: slint::platform::PointerEventButton::Left });
                        // release ~120 ms later, like a real click (scroll areas wait to tell taps from drags)
                        let weak = w.as_weak();
                        slint::Timer::single_shot(std::time::Duration::from_millis(120), move || {
                            if let Some(w) = weak.upgrade() {
                                w.window().dispatch_event(slint::platform::WindowEvent::PointerReleased { position: pos, button: slint::platform::PointerEventButton::Left });
                            }
                        });
                    }
                    "light" => app.set_appearance("Light".into()),
                    "type" => app.invoke_chip_typed(parts.get(1).unwrap_or(&"").to_string().into(), rest(2).into()),
                    "version" => app.invoke_select_version(n(1)),
                    "company" => app.invoke_select_company(n(1)),
                    "save-search" => app.invoke_save_search(),
                    "reindex" => app.invoke_reindex(),
                    other => eprintln!("JCC_DO: unknown command {other}"),
                }
                });
            }
        });
    }

    // Debug builds: JCC_SNAPSHOT=out.rgba captures the window after JCC_SNAPSHOT_AFTER ms and quits
    // (raw RGBA with an 8-byte width/height header), so screens can be checked without taking focus.
    #[cfg(debug_assertions)]
    if let Ok(path) = std::env::var("JCC_SNAPSHOT") {
        let after = std::env::var("JCC_SNAPSHOT_AFTER").ok().and_then(|v| v.parse().ok()).unwrap_or(4000);
        let weak = window.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(after), move || {
            if let Some(w) = weak.upgrade() {
                match w.window().take_snapshot() {
                    Ok(buf) => {
                        let mut out = Vec::with_capacity(8 + buf.as_bytes().len());
                        out.extend_from_slice(&buf.width().to_le_bytes());
                        out.extend_from_slice(&buf.height().to_le_bytes());
                        out.extend_from_slice(buf.as_bytes());
                        let _ = std::fs::write(&path, out);
                    }
                    Err(e) => eprintln!("snapshot failed: {e}"),
                }
            }
            let _ = slint::quit_event_loop();
        });
    }

    window.run()?;
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
    db::lock(&store).tidy();
    Ok(())
}
