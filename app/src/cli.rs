//! Developer commands for exercising the agents without the window:
//! `job-command-center --cli <command> ...` (debug builds only).
use crate::ai::AiSettings;
use crate::db::{self, Store};
use crate::pipeline::{self, Ctx, Start};
use anyhow::{bail, Result};
use std::path::Path;
use std::sync::Arc;

pub fn run(store: Store, data_dir: &Path, args: &[String]) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let seen = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<i64, i64>::new()));
    let printer = {
        let store = store.clone();
        let seen = seen.clone();
        Arc::new(move |job: i64| {
            let logs = db::lock(&store).logs(job).unwrap_or_default();
            let mut seen = seen.lock().unwrap();
            let last = seen.entry(job).or_insert(0);
            let from = *last;
            for l in logs.iter().filter(|l| l.id > from) {
                println!("[{}] {:<10} {}", l.level, l.agent, l.message);
                *last = l.id;
            }
        })
    };
    let ctx = Ctx::new(store.clone(), data_dir.join("Resumes"), printer);
    let arg = |i: usize| args.get(i).cloned().unwrap_or_default();
    match arg(0).as_str() {
        "set-ai" => {
            let s = AiSettings { provider: arg(1), base_url: arg(2), models: arg(3).split(',').map(String::from).filter(|m| !m.is_empty()).collect(), embed_model: arg(4), embed_provider: arg(5), ..Default::default() };
            db::lock(&store).set_setting("ai", &serde_json::to_value(&s)?)?;
            println!("saved {s:?}");
        }
        // The key comes from JCC_KEY so it never appears in the process list or shell history.
        "set-key" => {
            let key = std::env::var("JCC_KEY").unwrap_or_default();
            crate::ai::save_key(&arg(1), key.trim())?;
            println!("saved key for {} ({}); read back: {}", arg(1), crate::ai::mask(key.trim()), crate::ai::mask(&crate::ai::load_key(&arg(1))));
        }
        "test-ai" => println!("{}", rt.block_on(async { ctx.ai()?.test().await })?),
        "models" => println!("{:?}", rt.block_on(async { ctx.ai()?.list_models().await })?),
        "run" => {
            let id = {
                let db = db::lock(&store);
                let p = db.active_profile()?;
                db.insert_job(&arg(1), &arg(2), "", "", "", Some(p.id), "manual")?
            };
            println!("application {id}");
            rt.block_on(pipeline::run(ctx.clone(), id, Start::Full));
            let j = db::lock(&store).job(id)?;
            println!("stage {} status {} ats {:?} pdf {} error {}", j.stage, j.status, j.ats_score, j.pdf_path, j.error);
        }
        "tailor" => rt.block_on(pipeline::run(ctx.clone(), arg(1).parse()?, Start::Tailor)),
        "revise" => rt.block_on(pipeline::revise(ctx.clone(), arg(1).parse()?, arg(2))),
        "restyle" => {
            let vid = rt.block_on(pipeline::restyle(ctx.clone(), arg(1).parse()?, arg(2).parse()?, arg(3)))?;
            println!("version {vid}: {}", db::lock(&store).version(vid)?.pdf_path);
        }
        "ask" => {
            let id = rt.block_on(crate::chat::answer(&ctx, arg(1).parse()?, &arg(2)))?;
            println!("{}", db::lock(&store).chat_msg(id)?.1.content);
        }
        "save" => println!("saved: {}", crate::chat::toggle_saved(&ctx, arg(1).parse()?)?),
        "index" => {
            let n = rt.block_on(async {
                let db = db::lock(&store);
                for p in db.profiles()? { crate::rag::sync_profile(&db, &p)?; }
                for j in db.jobs()? { crate::rag::sync_job(&db, &j)?; }
                drop(db);
                crate::rag::embed_pending(&store, ctx.ai().ok().as_ref()).await
            })?;
            let model = ctx.ai()?.settings.embed_model;
            println!("embedded {n} chunks; status {:?}", crate::rag::index_status(&db::lock(&store), &model)?);
        }
        "search" => {
            let p = db::lock(&store).active_profile()?;
            let scope = crate::rag::Scope { profile_id: p.id, exclude_job: None, exclude_kinds: vec![] };
            let hits = rt.block_on(crate::rag::retrieve(&store, ctx.ai().ok().as_ref(), &arg(1), &scope, 5));
            for h in hits { println!("{:.4} [{}] {}\n  {}\n", h.score, h.kind, h.title, h.text.chars().take(160).collect::<String>()); }
        }
        other => bail!("unknown command {other:?}; try set-ai, test-ai, models, run, tailor, revise, restyle, ask, save, index, search"),
    }
    Ok(())
}
