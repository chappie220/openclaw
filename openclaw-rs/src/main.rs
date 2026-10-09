//! `openclaw-rs`: single-binary OpenClaw.

mod agent;
mod config;
mod llm;
mod store;

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::agent::{Agent, AgentEvent, NoTools};
use crate::config::Config;
use crate::store::Store;

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
    /// List or delete sessions.
    Sessions {
        #[command(subcommand)]
        action: Option<SessionsAction>,
    },
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
    let state = config::state_dir()?;
    let config_path = cli.config.unwrap_or_else(|| state.join("config.toml"));
    let config = Config::load(&config_path)?;
    let store = Store::open(&state.join("state.sqlite"))?;
    match cli.command {
        Command::Sessions { action } => sessions(&store, action.unwrap_or(SessionsAction::List)),
        Command::Ask { session, message } => {
            let message = message.join(" ");
            if message.trim().is_empty() {
                bail!("nothing to send");
            }
            let agent = build_agent(&config, store)?;
            turn(&agent, &session, &message).await
        }
        Command::Chat { session } => {
            let agent = build_agent(&config, store)?;
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

fn build_agent(config: &Config, store: Store) -> Result<Agent<llm::Client, NoTools>> {
    Ok(Agent {
        model: llm::Client::new(&config.model, config.api_key()?)?,
        tools: NoTools,
        store,
        config: config.agent.clone(),
    })
}

async fn turn(agent: &Agent<llm::Client, NoTools>, session: &str, input: &str) -> Result<()> {
    let mut stdout = std::io::stdout();
    agent
        .run_turn(session, input, &mut |event| match event {
            AgentEvent::Text(text) => {
                let _ = stdout.write_all(text.as_bytes());
                let _ = stdout.flush();
            }
            AgentEvent::ToolStart { name, arguments } => eprintln!("\n[tool {name} {arguments}]"),
            AgentEvent::ToolEnd { name, output } => {
                eprintln!("[tool {name} → {} bytes]", output.len());
            }
        })
        .await?;
    println!();
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
