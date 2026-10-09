//! `openclaw-rs`: single-binary OpenClaw.

mod agent;
mod config;
mod cron;
mod gateway;
mod llm;
mod memory;
mod qq;
mod service;
mod store;
mod tools;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::agent::{Agent, AgentEvent};
use crate::config::Config;
use crate::store::Store;
use crate::tools::{BuiltinTools, TerminalApprover, with_approver};

#[derive(Parser)]
#[command(name = "openclaw-rs", version, about = "Single-binary OpenClaw")]
struct Cli {
    /// Config file (default: <state dir>/config.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Interactive chat in the terminal.
    Chat {
        #[arg(short, long, default_value = "main")]
        session: String,
    },
    /// Send one message and print the reply.
    Ask {
        #[arg(short, long, default_value = "main")]
        session: String,
        message: Vec<String>,
    },
    /// Run the Gateway: Web UI and WebSocket API.
    Serve {
        /// Listen address (default: gateway.bind, 127.0.0.1:18789).
        #[arg(long)]
        bind: Option<String>,
    },
    /// Install or remove the OpenRC system service (run as root).
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Manage scheduled prompts (run by `serve`).
    Cron {
        #[command(subcommand)]
        action: CronAction,
    },
    /// Manage long-term memory.
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
    /// List or delete sessions.
    Sessions {
        #[command(subcommand)]
        action: Option<SessionsAction>,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Write /etc/init.d/openclaw-rs, enable it, and start it.
    Install {
        /// Account the Gateway runs as; its home holds the state.
        #[arg(long, default_value = "root")]
        user: String,
    },
    /// Stop and remove the service; state is kept.
    Uninstall,
}

#[derive(Subcommand)]
enum CronAction {
    /// Add a job: 5-field cron schedule in local time.
    Add {
        name: String,
        /// e.g. "*/30 * * * *" or "0 9 * * 1-5"
        schedule: String,
        prompt: Vec<String>,
        #[arg(short, long, default_value = "main")]
        session: String,
    },
    List,
    Remove {
        name: String,
    },
}

#[derive(Subcommand)]
enum MemoryAction {
    /// Save a fact.
    Add { content: Vec<String> },
    /// Search saved facts.
    Search {
        query: Vec<String>,
        #[arg(short, long, default_value_t = 10)]
        limit: usize,
    },
    /// Show the newest facts.
    List {
        #[arg(short, long, default_value_t = 50)]
        limit: usize,
    },
    /// Delete a fact by id.
    Delete { id: i64 },
}

#[derive(Subcommand)]
enum SessionsAction {
    List,
    Delete { name: String },
}

#[tokio::main]
async fn main() {
    if let Err(err) = run(Cli::parse()).await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    if let Command::Service { action } = &cli.command {
        return match action {
            ServiceAction::Install { user } => service::install(user),
            ServiceAction::Uninstall => service::uninstall(),
        };
    }
    let state = config::state_dir()?;
    let config_path = cli.config.unwrap_or_else(|| state.join("config.toml"));
    let config = Config::load(&config_path)?;
    let store = Store::open(&state.join("state.sqlite"))?;
    match cli.command {
        Command::Memory { action } => memory(&store, action),
        Command::Cron { action } => cron_command(&store, action),
        Command::Service { .. } => unreachable!("handled before state is opened"),
        Command::Serve { bind } => {
            let agent = Arc::new(build_agent(&config, &state, store)?);
            let bind = bind.unwrap_or_else(|| config.gateway.bind.clone());
            let gateway = gateway::Gateway::new(agent, config.gateway.token());
            if config.qq.enabled {
                let unattended_allow = config.tools.shell == config::Permission::Allow
                    || config.tools.write == config::Permission::Allow;
                if unattended_allow && config.qq.allow.is_empty() {
                    bail!(
                        "tools.shell or tools.write is \"allow\", so any QQ user could run them; \
                         list trusted openids in qq.allow (the log shows each sender's openid) or use \"ask\""
                    );
                }
                let bot = qq::QqBot::new(config.qq.clone())?;
                gateway.add_notifier(bot.clone());
                tokio::spawn(qq::run(bot, gateway.clone()));
            }
            tokio::spawn(gateway.clone().run_scheduler());
            gateway::serve(gateway, &bind).await
        }
        Command::Sessions { action } => sessions(&store, action.unwrap_or(SessionsAction::List)),
        Command::Ask { session, message } => {
            let message = message.join(" ");
            if message.trim().is_empty() {
                bail!("nothing to send");
            }
            let agent = build_agent(&config, &state, store)?;
            turn(&agent, &session, &message).await
        }
        Command::Chat { session } => {
            let agent = build_agent(&config, &state, store)?;
            eprintln!(
                "session {session} · model {} · empty line or Ctrl-D to quit",
                config.model.model
            );
            let mut lines = BufReader::new(tokio::io::stdin()).lines();
            loop {
                eprint!("> ");
                let Some(line) = lines.next_line().await? else {
                    break;
                };
                if line.trim().is_empty() {
                    break;
                }
                if let Err(err) = turn(&agent, &session, &line).await {
                    eprintln!("error: {err:#}");
                }
            }
            Ok(())
        }
    }
}

type CliAgent = Agent<llm::Client, BuiltinTools>;

fn build_agent(config: &Config, state: &Path, store: Store) -> Result<CliAgent> {
    let workspace = config
        .tools
        .workspace
        .clone()
        .unwrap_or_else(|| state.join("workspace"));
    Ok(Agent {
        model: llm::Client::new(&config.model, config.api_key()?)?,
        tools: BuiltinTools::new(workspace, config.tools.clone(), store.clone())?,
        store,
        config: config.agent.clone(),
    })
}

async fn turn(agent: &CliAgent, session: &str, input: &str) -> Result<()> {
    let mut stdout = std::io::stdout();
    let mut on_event = |event| match event {
        AgentEvent::Text(text) => {
            let _ = stdout.write_all(text.as_bytes());
            let _ = stdout.flush();
        }
        AgentEvent::ToolStart { name, arguments } => eprintln!("\n[tool {name} {arguments}]"),
        AgentEvent::ToolEnd { name, output } => {
            eprintln!("[tool {name} → {} bytes]", output.len());
        }
    };
    let run = agent.run_turn(session, input, &mut on_event);
    with_approver(Arc::new(TerminalApprover), run).await?;
    println!();
    Ok(())
}

fn cron_command(store: &Store, action: CronAction) -> Result<()> {
    let time = |unix: i64| {
        use chrono::TimeZone;
        chrono::Local.timestamp_opt(unix, 0).single().map_or_else(
            || "never".into(),
            |t| t.format("%Y-%m-%d %H:%M").to_string(),
        )
    };
    match action {
        CronAction::Add {
            name,
            schedule,
            prompt,
            session,
        } => {
            let job = store.job_add(&name, &schedule, &session, &prompt.join(" "))?;
            println!("added {} · next run {}", job.name, time(job.next_run));
        }
        CronAction::List => {
            for j in store.job_list()? {
                println!(
                    "{}\t{}\tsession={}\tnext={}\tlast={}\t{}",
                    j.name,
                    j.schedule,
                    j.session,
                    time(j.next_run),
                    j.last_status.as_deref().unwrap_or("-"),
                    j.prompt
                );
            }
        }
        CronAction::Remove { name } => {
            if !store.job_remove(&name)? {
                bail!("no job named {name}");
            }
            println!("removed {name}");
        }
    }
    Ok(())
}

fn memory(store: &Store, action: MemoryAction) -> Result<()> {
    let print = |memories: Vec<memory::Memory>| {
        for m in memories {
            println!("#{}\t{}", m.id, m.content);
        }
    };
    match action {
        MemoryAction::Add { content } => {
            println!("saved #{}", store.memory_save(&content.join(" "))?)
        }
        MemoryAction::Search { query, limit } => {
            print(store.memory_search(&query.join(" "), limit)?)
        }
        MemoryAction::List { limit } => print(store.memory_list(limit)?),
        MemoryAction::Delete { id } => {
            if !store.memory_delete(id)? {
                bail!("no memory #{id}");
            }
            println!("deleted #{id}");
        }
    }
    Ok(())
}

fn sessions(store: &Store, action: SessionsAction) -> Result<()> {
    match action {
        SessionsAction::List => {
            for s in store.sessions()? {
                println!(
                    "{}\t{} messages\tupdated {}",
                    s.name, s.messages, s.updated_at
                );
            }
        }
        SessionsAction::Delete { name } => {
            if !store.delete_session(&name)? {
                bail!("no session named {name}");
            }
            println!("deleted {name}");
        }
    }
    Ok(())
}
