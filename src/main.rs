mod codex;
mod coordination;
mod project;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args_os();
    let _exe = args.next();

    let Some(command) = args.next() else {
        print_help();
        return Ok(());
    };

    match command.to_string_lossy().as_ref() {
        "init" => {
            let path = args.next().map(PathBuf::from);
            if args.next().is_some() {
                return Err("usage: agentlatch init [path]".into());
            }

            let root = match path {
                Some(path) => path,
                None => env::current_dir()
                    .map_err(|error| format!("failed to get current directory: {error}"))?,
            };

            let result = project::init(&root)?;
            println!("AgentLatch initialized at {}", result.root.display());

            for change in result.changes {
                println!("  {change}");
            }

            match codex::is_configured() {
                Ok(true) => println!("Codex integration: configured"),
                Ok(false) => {
                    println!("Codex integration: not configured");
                    println!("Run: agentlatch configure codex");
                }
                Err(error) => println!("Codex integration: could not check ({error})"),
            }

            Ok(())
        }
        "configure" => {
            let target = args
                .next()
                .ok_or_else(|| "usage: agentlatch configure <codex|all>".to_string())?;
            if args.next().is_some() {
                return Err("usage: agentlatch configure <codex|all>".into());
            }

            match target.to_string_lossy().as_ref() {
                "codex" | "all" => codex::configure(),
                _ => Err("usage: agentlatch configure <codex|all>".into()),
            }
        }
        "lock" => coordination::run_lock(args.collect()),
        "unlock" => coordination::run_unlock(args.collect()),
        "wait" => coordination::run_wait(args.collect()),
        "status" => coordination::run_status(args.collect()),
        "hook" => codex::run_hook(args.collect()),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        "version" | "--version" | "-V" => {
            println!("agentlatch {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => Err(format!(
            "unknown command: {other}\n\nRun 'agentlatch help' for usage."
        )),
    }
}

fn print_help() {
    println!(
        "AgentLatch {version}

Usage:
  agentlatch init [path]
  agentlatch configure <codex|all>
  agentlatch lock --owner <id> [--reason <text>] <paths...>
  agentlatch unlock --owner <id> <paths...>
  agentlatch unlock --owner <id> --mine
  agentlatch wait [--timeout <seconds>] <paths...>
  agentlatch status
  agentlatch help
  agentlatch version

Commands:
  init       Initialize or update AgentLatch project integration
  configure  Configure agent integrations
  lock       Acquire file claims
  unlock     Release file claims
  wait       Wait until file claims become available
  status     Show active claims
  help       Show this help
  version    Show the AgentLatch version
",
        version = env!("CARGO_PKG_VERSION")
    );
}
