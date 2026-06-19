//! `demo` — end-to-end smoke binary.
//!
//! Usage:
//!
//! ```text
//! just demo "<theme>" "<writing task>"
//! ```
//!
//! It reads `DEEPSEEK_API_KEY` from the environment (load `.env` first, which the
//! `just demo` recipe does), then has a [`Master`](ai_write::engine::Master)
//! create the theme and dispatch one [`spawn_slave`](ai_write::engine::spawn_slave)
//! thread to write a single article into `workspace/<theme>/`. When the slave
//! finishes it prints the resulting article, the slave's structured report, and
//! the cumulative token usage to stdout.
//!
//! The demo requires the default `blocking` feature (v0 is synchronous).

#[cfg(feature = "blocking")]
fn main() {
    use std::process::exit;

    use ai_write::engine::{Master, SlaveTask};
    use ai_write::req::blocking::Client;
    use ai_write::session::{Session, SessionOptions};
    use ai_write::tool::ToolRegistry;
    use ai_write::tool::workspace::{Workspace, WriterId};

    // --- CLI args: <theme> <writing-task> -----------------------------------
    let mut args = std::env::args().skip(1);
    let (theme, task) = match (args.next(), args.next()) {
        (Some(theme), Some(task)) => (theme, task),
        _ => {
            eprintln!("usage: demo <theme> <writing-task>");
            eprintln!("example: just demo \"rust\" \"Write a short introduction to ownership.\"");
            exit(2);
        }
    };

    // --- Client from environment (never hardcode the key) -------------------
    let client = match Client::from_env() {
        Ok(client) => client,
        Err(e) => {
            eprintln!("failed to build DeepSeek client from environment: {e}");
            eprintln!("set DEEPSEEK_API_KEY (the `just demo` recipe sources ./.env for you)");
            exit(1);
        }
    };

    // --- Workspace at ./workspace -------------------------------------------
    let workspace_root = "workspace";
    let ws = match Workspace::open(workspace_root) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("failed to open workspace at {workspace_root:?}: {e}");
            exit(1);
        }
    };

    // The article file the slave will write. v0 writes one article per run.
    let file_name = "article.md";

    // The slave writes under an agent identity tagged with the model id, so the
    // article's contributor provenance records which model produced it.
    let model = SessionOptions::default().model;
    let writer = WriterId::Agent {
        model: model.as_str().to_string(),
        label: "slave-1".to_string(),
    };

    // --- Master orchestration session ---------------------------------------
    // v0's `Master::run_one` is deterministic Rust (no master-side chat), so the
    // master session needs no tools; it only carries the shared `Client`.
    let master_session = Session::new(
        client,
        "You are the orchestrator. You create themes and dispatch writing agents.",
        ToolRegistry::new(),
        SessionOptions::default(),
    );
    let mut master = Master::new(master_session, ws);

    let slave_task = SlaveTask {
        theme: theme.clone(),
        file_name: file_name.to_string(),
        task: task.clone(),
        writer: writer.clone(),
        system_prompt: None,
        skill: None,
    };

    eprintln!("dispatching slave: theme={theme:?} file={file_name:?}");
    let report = match master.run_one(slave_task) {
        Ok(report) => report,
        Err(e) => {
            eprintln!("master failed to set up the workspace: {e}");
            exit(1);
        }
    };

    // --- Print the article ---------------------------------------------------
    println!("===== article: {theme}/{file_name} =====");
    let reopened = match Workspace::open(workspace_root) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("failed to reopen workspace to read the article: {e}");
            exit(1);
        }
    };
    match reopened.read_article(&theme, file_name) {
        Ok(text) => println!("{text}"),
        Err(e) => println!("(could not read article: {e})"),
    }

    // --- Print the slave's structured report --------------------------------
    println!("\n===== slave report =====");
    println!("status:  {:?}", report.status);
    println!("summary: {}", report.summary);
    if let Some(result) = &report.result {
        println!("result:  {result}");
    }
    if let Some(needs) = &report.needs {
        println!("needs:   {needs}");
    }

    // --- Print cumulative usage --------------------------------------------
    // In v0 the master's orchestration is deterministic Rust and performs no chat
    // completion, so the master session's own totals are zero. The slave's token
    // usage accrues inside the slave's session, which lives and dies on the slave
    // thread; v0 surfaces the slave's outcome through the structured report above
    // rather than threading its `UsageTotals` back across the join. We print the
    // master session totals for completeness and label them accordingly.
    let usage = master.usage();
    println!("\n===== usage (master orchestration session) =====");
    println!("rounds:                  {}", usage.rounds);
    println!("prompt_tokens:           {}", usage.prompt_tokens);
    println!("completion_tokens:       {}", usage.completion_tokens);
    println!("total_tokens:            {}", usage.total_tokens);
    println!("prompt_cache_hit_tokens: {}", usage.prompt_cache_hit_tokens);
    println!("reasoning_tokens:        {}", usage.reasoning_tokens);
    if usage.rounds == 0 {
        println!(
            "(note: v0 orchestration is deterministic; per-slave token usage is \
             reported by the slave thread, not folded into the master.)"
        );
    }
}

#[cfg(not(feature = "blocking"))]
fn main() {
    eprintln!("the `demo` binary requires the `blocking` feature (the default)");
    std::process::exit(1);
}
