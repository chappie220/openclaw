//! `openclaw-rs`: single-binary OpenClaw.

mod access;
mod agent;
mod attachments;
mod browser;
mod cli_text;
mod completions;
mod config;
mod context;
mod cron;
mod gateway;
mod guide;
mod i18n;
mod identity;
mod llm;
mod mail;
mod memory;
mod qq;
mod review;
mod search;
mod service;
mod setup;
mod store;
mod tools;
mod usage;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{FromArgMatches, Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::agent::{Agent, AgentEvent};
use crate::cli_text as text;
use crate::config::Config;
use crate::i18n::Lang;
use crate::store::Store;
use crate::tools::{BuiltinTools, TerminalApprover, with_approver};

#[derive(Parser)]
#[command(name = "openclaw-rs", version, about = "Single-binary OpenClaw")]
struct Cli {
    /// Config file (default: <state dir>/config.toml).
    #[arg(long, global = true, value_hint = clap::ValueHint::FilePath)]
    config: Option<PathBuf>,
    /// Language of output and help (default: `language` in config.toml, else the locale).
    #[arg(long, global = true, value_enum)]
    lang: Option<Lang>,
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
        /// A file to send with the message; repeat for more.
        #[arg(short, long, value_hint = clap::ValueHint::FilePath)]
        attach: Vec<PathBuf>,
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
    /// Mail channel helpers.
    Mail {
        #[command(subcommand)]
        action: MailAction,
    },
    /// Show, set or reset the agent's identity and soul.
    Identity {
        #[command(subcommand)]
        action: Option<IdentityAction>,
    },
    /// Manage long-term memory.
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
    /// Edit config.toml interactively: model, tools, gateway, QQ, email, search, access.
    Config,
    /// Print a shell completion script: bash, zsh, fish, elvish or powershell.
    #[command(args_conflicts_with_subcommands = true)]
    Completions {
        shell: Option<clap_complete::Shell>,
        #[command(subcommand)]
        action: Option<CompletionsAction>,
    },
    /// Tokens and cost of model calls, per session.
    Usage {
        /// How many days back to count.
        #[arg(long, default_value_t = 30)]
        days: u32,
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
enum CompletionsAction {
    /// Install completion for your shell, asking before editing its rc file.
    Install {
        /// Shell to install for (default: from $SHELL).
        #[arg(long, value_enum)]
        shell: Option<clap_complete::Shell>,
        /// Edit the rc file without asking.
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum MailAction {
    /// Log in to the configured IMAP and SMTP servers and report what works.
    Check,
    /// Show inbound mail by state, and every message not yet answered.
    Queue,
    /// Queue a failed or uncertain message again; a stored reply is resent
    /// under the same Message-ID, otherwise the turn runs again.
    Retry { id: i64 },
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
enum IdentityAction {
    Show,
    /// Set the identity directly instead of in a conversation.
    Set {
        #[arg(long)]
        name: String,
        /// What the agent is: an AI, a robot, a familiar.
        #[arg(long)]
        creature: String,
        /// One line on how it comes across.
        #[arg(long)]
        vibe: String,
        #[arg(long)]
        emoji: Option<String>,
        /// SOUL.md text: voice, stance, style, boundaries.
        #[arg(
            long,
            conflicts_with = "soul_file",
            required_unless_present = "soul_file"
        )]
        soul: Option<String>,
        /// Read the soul from a SOUL.md file.
        #[arg(long, value_hint = clap::ValueHint::FilePath)]
        soul_file: Option<PathBuf>,
    },
    /// Forget the identity; the next conversation sets it up again.
    Reset,
    /// Show the identity draft waiting for approval, from any channel.
    Drafts,
    /// Save identity draft <id> exactly as shown by `identity drafts`.
    Approve {
        id: i64,
        /// The draft's code, to be sure it is the version you read.
        code: Option<String>,
    },
    /// Discard identity draft <id>.
    Reject {
        id: i64,
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
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    // Chosen before parsing, so `--help` is in the same language.
    i18n::set(startup_lang(&args));
    let matches = text::command::<Cli>(i18n::current()).get_matches_from(args);
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|err| err.exit());
    if let Err(err) = run(cli).await {
        eprintln!("{}", text::ERROR.with(&[&format!("{err:#}")]));
        std::process::exit(1);
    }
}

/// `--lang`, else `language` in the config file, else the locale.
fn startup_lang(args: &[std::ffi::OsString]) -> Lang {
    use clap::ValueEnum;
    let mut lang = None;
    let mut config = None;
    let mut words = args
        .iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned());
    while let Some(word) = words.next() {
        if word == "--" {
            break;
        }
        if let Some(value) = word.strip_prefix("--lang=") {
            lang = Some(value.to_owned());
        } else if word == "--lang" {
            lang = words.next();
        } else if let Some(value) = word.strip_prefix("--config=") {
            config = Some(PathBuf::from(value));
        } else if word == "--config" {
            config = words.next().map(PathBuf::from);
        }
    }
    if let Some(lang) = lang.and_then(|l| Lang::from_str(&l, true).ok()) {
        return lang;
    }
    let path = config.or_else(|| config::state_dir().ok().map(|d| d.join("config.toml")));
    path.and_then(|p| Config::load(&p).ok())
        .and_then(|c| c.language)
        .unwrap_or_else(Lang::detect)
}

async fn run(cli: Cli) -> Result<()> {
    if let Some(lang) = cli.lang {
        i18n::set(lang);
    }
    if let Command::Completions { shell, action } = cli.command {
        return completions_command(shell, action);
    }
    if let Command::Service { action } = &cli.command {
        return match action {
            ServiceAction::Install { user } => service::install(user),
            ServiceAction::Uninstall => service::uninstall(),
        };
    }
    let state = config::state_dir()?;
    let config_path = cli.config.unwrap_or_else(|| state.join("config.toml"));
    // Before loading, so a file that does not load can still be repaired.
    if let Command::Config = cli.command {
        return setup::run(&config_path, i18n::current());
    }
    let config = Config::load(&config_path)?;
    let store = Store::open(&state)?;
    match cli.command {
        Command::Memory { action } => memory(&store, action),
        Command::Identity { action } => identity(&store, action.unwrap_or(IdentityAction::Show)),
        Command::Cron { action } => cron_command(&store, action),
        Command::Mail {
            action: MailAction::Queue,
        } => mail_queue(&store),
        Command::Mail {
            action: MailAction::Retry { id },
        } => {
            if !store.mail_requeue(id)? {
                bail!(text::MAIL_RETRY_NONE.with(&[&id.to_string()]));
            }
            println!("{}", text::MAIL_REQUEUED.with(&[&id.to_string()]));
            Ok(())
        }
        Command::Mail {
            action: MailAction::Check,
        } => {
            let bot = mail::MailBot::connect_only(config.mail.clone())?;
            let mut failed = false;
            for (target, result) in bot.check().await {
                match result {
                    Ok(detail) => println!("✓ {target}: {detail}"),
                    Err(err) => {
                        failed = true;
                        println!("✗ {target}: {err:#}");
                    }
                }
            }
            if failed {
                bail!(text::MAIL_CHECK_FAILED.now());
            }
            Ok(())
        }
        Command::Service { .. } => unreachable!("handled before state is opened"),
        Command::Config => unreachable!("handled before the config is loaded"),
        Command::Completions { .. } => unreachable!("handled before state is opened"),
        Command::Serve { bind } => {
            if store.identity()?.is_none() {
                eprintln!("{}", text::FIRST_START.now());
            }
            let agent = Arc::new(build_agent(&config, &state, store)?);
            let bind = bind.unwrap_or_else(|| config.gateway.bind.clone());
            let gateway =
                gateway::Gateway::new(agent, config.gateway.token(), config.access.clone());
            if let Some(guide) =
                guide::Guide::from_config(&config.guide, &config.model, &config.api_key()?)?
            {
                gateway.set_guide(guide);
            }
            if config.qq.enabled {
                // Auto-review runs commands nobody approved, so it counts as unattended.
                let reviewed_shell = config.tools.shell == config::Permission::Ask
                    && config.tools.review.provider != config::ReviewProvider::Off;
                let strangers = config.access.unnamed("qq");
                let stranger_runs = (strangers.contains(&access::Capability::Shell)
                    && (config.tools.shell == config::Permission::Allow || reviewed_shell))
                    || (strangers.contains(&access::Capability::FilesWrite)
                        && config.tools.write == config::Permission::Allow);
                if stranger_runs && config.qq.allow.is_empty() {
                    bail!(text::QQ_OPEN_TO_STRANGERS.now());
                }
                let bot = qq::QqBot::new(config.qq.clone())?;
                gateway.add_notifier(bot.clone());
                tokio::spawn(qq::run(bot, gateway.clone()));
            }
            if (config.qq.enabled || config.mail.enabled) && config.access.owners.is_empty() {
                eprintln!(
                    "{}",
                    text::NO_OWNERS.with(&[&format!("{:?}", config.access.guest)])
                );
            }
            if config.mail.enabled {
                let bot = mail::MailBot::new(config.mail.clone())?;
                gateway.add_notifier(bot.clone());
                tokio::spawn(mail::run(bot, gateway.clone()));
            }
            tokio::spawn(gateway.clone().run_scheduler());
            gateway::serve(gateway, &bind).await
        }
        Command::Sessions { action } => sessions(&store, action.unwrap_or(SessionsAction::List)),
        Command::Usage { days } => {
            let since = store::now() - i64::from(days) * 86_400;
            print!(
                "{}",
                usage::report(&store.usage_since(since)?, i18n::current())
            );
            Ok(())
        }
        Command::Ask {
            session,
            attach,
            message,
        } => {
            let message = message.join(" ");
            if message.trim().is_empty() && attach.is_empty() {
                bail!(text::NOTHING_TO_SEND.now());
            }
            let uploads = attach
                .iter()
                .map(|path| read_upload(path))
                .collect::<Result<Vec<_>>>()?;
            let agent = build_agent(&config, &state, store)?;
            turn(&agent, &session, &message, &uploads).await
        }
        Command::Chat { session } => {
            let name = store.identity()?.map(|i| i.name);
            let agent = build_agent(&config, &state, store)?;
            eprintln!(
                "{}",
                text::CHAT_BANNER.with(&[
                    name.as_deref().unwrap_or("OpenClaw"),
                    &session,
                    &config.model.model
                ])
            );
            if name.is_none() {
                eprintln!("{}", text::FIRST_START.now());
            }
            let mut lines = BufReader::new(tokio::io::stdin()).lines();
            // Files from `/attach`, sent with the next message.
            let mut pending = Vec::new();
            loop {
                eprint!("> ");
                // Once a turn has handled Ctrl-C, it no longer ends the
                // program by itself, so the prompt does.
                let line = tokio::select! {
                    line = lines.next_line() => line?,
                    _ = tokio::signal::ctrl_c() => None,
                };
                let Some(line) = line else {
                    eprintln!();
                    break;
                };
                if line.trim().is_empty() {
                    break;
                }
                if let Some(path) = line.trim().strip_prefix("/attach ") {
                    match read_upload(Path::new(path.trim())) {
                        Ok(upload) => {
                            eprintln!("{}", text::ATTACHED.with(&[&upload.name]));
                            pending.push(upload);
                        }
                        Err(err) => eprintln!("{}", text::ERROR.with(&[&format!("{err:#}")])),
                    }
                    continue;
                }
                let uploads = std::mem::take(&mut pending);
                if let Err(err) = turn(&agent, &session, &line, &uploads).await {
                    eprintln!("{}", text::ERROR.with(&[&format!("{err:#}")]));
                }
            }
            Ok(())
        }
    }
}

fn completions_command(
    shell: Option<clap_complete::Shell>,
    action: Option<CompletionsAction>,
) -> Result<()> {
    let lang = i18n::current();
    match (shell, action) {
        // Descriptions follow the current language (zsh, fish, elvish and
        // PowerShell show them); options and commands are the same in all.
        (Some(shell), _) => print!("{}", text::completions::<Cli>(shell, lang)),
        (None, Some(CompletionsAction::Install { shell, yes })) => completions::install(
            shell,
            std::env::var("SHELL").ok().as_deref(),
            &completions::Home::from_env()?,
            None,
            yes,
            lang,
            &|shell| text::completions::<Cli>(shell, lang),
            &mut std::io::stdin().lock(),
            &mut std::io::stdout(),
        )?,
        (None, None) => bail!(text::NAME_A_SHELL.now()),
    }
    Ok(())
}

type CliAgent = Agent<llm::Client, BuiltinTools>;

fn build_agent(config: &Config, state: &Path, store: Store) -> Result<CliAgent> {
    let workspace = config
        .tools
        .workspace
        .clone()
        .unwrap_or_else(|| state.join("workspace"));
    config.model_id()?;
    let api_key = config.api_key()?;
    let search = search::Searcher::new(&config.search, &config.model, &api_key)?;
    let review = review::Reviewer::from_config(&config.tools.review, &config.model, &api_key)?;
    let summarizer = match &config.agent.summary_model {
        Some(model) if !model.trim().is_empty() => Some(llm::Client::new(
            &config::ModelConfig {
                model: model.clone(),
                ..config.model.clone()
            },
            api_key.clone(),
        )?),
        _ => None,
    };
    let vision = match &config.agent.vision_model {
        Some(model) if !model.trim().is_empty() => Some(llm::Client::new(
            &config::ModelConfig {
                model: model.clone(),
                ..config.model.clone()
            },
            api_key.clone(),
        )?),
        _ => None,
    };
    Ok(Agent {
        model: llm::Client::new(&config.model, api_key)?,
        summarizer,
        vision,
        tools: BuiltinTools::new(workspace.clone(), config.tools.clone(), store.clone())?
            .with_search(search)
            .with_browser(browser::Browser::new(&config.browser, state, &workspace))
            .with_review(review),
        store,
        config: config::AgentConfig {
            workspace,
            ..config.agent.clone()
        },
    })
}

/// A file named on the command line, to send with a message.
fn read_upload(path: &Path) -> Result<attachments::Upload> {
    let data = std::fs::read(path)
        .with_context(|| text::CANNOT_READ.with(&[&path.display().to_string()]))?;
    let name = path
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    Ok(attachments::Upload {
        name,
        mime: None,
        data,
    })
}

async fn turn(
    agent: &CliAgent,
    session: &str,
    input: &str,
    uploads: &[attachments::Upload],
) -> Result<()> {
    if input.trim_start().starts_with("/identity") {
        owner_command(&agent.store, input);
        return Ok(());
    }
    let (files, problems) = agent.save_uploads(uploads);
    for problem in &problems {
        eprintln!("{}", text::ERROR.with(&[problem]));
    }
    let input = attachments::with_problems(input, &problems);
    let input = input.as_str();
    let mut stdout = std::io::stdout();
    let mut on_event = |event| match event {
        AgentEvent::Text(text) => {
            let _ = stdout.write_all(text.as_bytes());
            let _ = stdout.flush();
        }
        AgentEvent::ToolStart { name, arguments } => {
            eprintln!("\n{}", text::TOOL_START.with(&[&name, &arguments]))
        }
        AgentEvent::ToolEnd { name, output } => {
            eprintln!(
                "{}",
                text::TOOL_END.with(&[&name, &output.len().to_string()])
            );
        }
        // The terminal reads no input while a turn runs.
        AgentEvent::FollowUp(_) => {}
    };
    let cancel = agent::Cancel::default();
    let run = agent.run_turn_until(
        access::Actor::owner(access::CLI),
        session,
        input,
        &files,
        &cancel,
        &mut on_event,
    );
    let run = with_approver(Arc::new(TerminalApprover), run);
    tokio::pin!(run);
    // Ctrl-C stops this turn, not the program.
    let mut stopped = false;
    let reply = loop {
        tokio::select! {
            reply = &mut run => break reply?,
            _ = tokio::signal::ctrl_c(), if !stopped => {
                stopped = true;
                cancel.cancel();
            }
        }
    };
    println!();
    if stopped {
        eprintln!("{reply}");
    }
    Ok(())
}

fn mail_queue(store: &Store) -> Result<()> {
    let (counts, entries) = store.mail_inbox()?;
    if counts.is_empty() {
        println!("{}", text::NO_MAIL.now());
        return Ok(());
    }
    let counts: Vec<String> = counts.iter().map(|(s, n)| format!("{s} {n}")).collect();
    println!("{}", counts.join(", "));
    for e in entries {
        let when = {
            use chrono::TimeZone;
            chrono::Local
                .timestamp_opt(e.updated_at, 0)
                .single()
                .map_or_else(String::new, |t| t.format("%Y-%m-%d %H:%M").to_string())
        };
        println!(
            "{}",
            text::MAIL_ENTRY.with(&[
                &e.id.to_string(),
                &e.state,
                &e.key,
                e.sender.as_deref().unwrap_or("?"),
                &e.attempts.to_string(),
                &when
            ])
        );
        if let Some(error) = e.last_error {
            println!("    {error}");
        }
    }
    Ok(())
}

fn cron_command(store: &Store, action: CronAction) -> Result<()> {
    let time = |unix: i64| {
        use chrono::TimeZone;
        chrono::Local.timestamp_opt(unix, 0).single().map_or_else(
            || text::NEVER.now().into(),
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
            let job = store.job_add(&name, &schedule, &session, &prompt.join(" "), access::CLI)?;
            println!(
                "{}",
                text::CRON_ADDED.with(&[&job.name, &time(job.next_run)])
            );
        }
        CronAction::List => {
            for j in store.job_list()? {
                println!(
                    "{}",
                    text::CRON_ROW.with(&[
                        &j.name,
                        &j.schedule,
                        &j.session,
                        &time(j.next_run),
                        j.last_status.as_deref().unwrap_or("-"),
                        &j.prompt
                    ])
                );
            }
        }
        CronAction::Remove { name } => {
            if !store.job_remove(&name)? {
                bail!(text::NO_JOB.with(&[&name]));
            }
            println!("{}", text::REMOVED.with(&[&name]));
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
            let id = store.memory_save(&content.join(" "))?;
            println!("{}", text::MEMORY_SAVED.with(&[&id.to_string()]))
        }
        MemoryAction::Search { query, limit } => {
            print(store.memory_search(&query.join(" "), limit)?)
        }
        MemoryAction::List { limit } => print(store.memory_list(limit)?),
        MemoryAction::Delete { id } => {
            if !store.memory_delete(id)? {
                bail!(text::NO_MEMORY.with(&[&id.to_string()]));
            }
            println!("{}", text::DELETED_NUMBER.with(&[&id.to_string()]));
        }
    }
    Ok(())
}

/// Runs an `/identity` command as the terminal's owner.
fn owner_command(store: &Store, command: &str) {
    let owner = access::Actor::owner(access::CLI);
    if let Some(reply) = identity::command(store, &owner, command) {
        println!("{reply}");
    }
}

fn identity(store: &Store, action: IdentityAction) -> Result<()> {
    match action {
        IdentityAction::Show => match store.identity()? {
            Some(i) => println!(
                "# IDENTITY.md\n\n{}\n\n# SOUL.md\n\n{}",
                i.identity_md(),
                i.soul
            ),
            None => println!("{}", text::NO_IDENTITY.now()),
        },
        IdentityAction::Set {
            name,
            creature,
            vibe,
            emoji,
            soul,
            soul_file,
        } => {
            let soul = match (soul, soul_file) {
                (Some(soul), _) => soul,
                (None, Some(path)) => std::fs::read_to_string(&path)
                    .with_context(|| text::CANNOT_READ.with(&[&path.display().to_string()]))?,
                (None, None) => unreachable!("clap requires --soul or --soul-file"),
            };
            store.identity_set(&identity::Identity {
                name,
                creature,
                vibe,
                emoji,
                soul,
                ..identity::Identity::default()
            })?;
            println!("{}", text::IDENTITY_SAVED.now());
        }
        IdentityAction::Drafts => owner_command(store, "/identity show"),
        IdentityAction::Approve { id, code } => owner_command(
            store,
            &format!("/identity approve {id} {}", code.unwrap_or_default()),
        ),
        IdentityAction::Reject { id } => owner_command(store, &format!("/identity reject {id}")),
        IdentityAction::Reset => {
            if store.identity_clear()? {
                println!("{}", text::IDENTITY_REMOVED.now());
            } else {
                println!("{}", text::NO_IDENTITY_TO_REMOVE.now());
            }
        }
    }
    Ok(())
}

fn sessions(store: &Store, action: SessionsAction) -> Result<()> {
    match action {
        SessionsAction::List => {
            for s in store.sessions()? {
                println!(
                    "{}",
                    text::SESSION_ROW.with(&[
                        &s.name,
                        &s.messages.to_string(),
                        &s.updated_at.to_string()
                    ])
                );
            }
        }
        SessionsAction::Delete { name } => {
            if !store.delete_session(&name)? {
                bail!(text::NO_SESSION.with(&[&name]));
            }
            println!("{}", text::DELETED.with(&[&name]));
        }
    }
    Ok(())
}
