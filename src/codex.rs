use crate::coordination::{self, AcquireError, Owner};
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

const CONFIG_START: &str = "# >>> AgentLatch managed Codex hooks >>>";
const CONFIG_END: &str = "# <<< AgentLatch managed Codex hooks <<<";
const HOOKS_TEMPLATE: &str = include_str!("../templates/CODEX_HOOKS_TEMPLATE.toml");

pub fn configure() -> Result<(), String> {
    let config = codex_home()?.join("config.toml");
    let change = configure_at(&config)?;
    match change {
        ConfigChange::Created => println!("created Codex config with AgentLatch hooks"),
        ConfigChange::Added => println!("added AgentLatch hooks to {}", config.display()),
        ConfigChange::Updated => println!("updated AgentLatch hooks in {}", config.display()),
        ConfigChange::Unchanged => println!("Codex AgentLatch hooks are already up to date"),
    }
    if change != ConfigChange::Unchanged {
        println!(
            "Codex may ask you to review/trust the new or changed hooks. If prompted, approve them in /hooks."
        );
    }
    Ok(())
}

pub fn is_configured() -> Result<bool, String> {
    let config = codex_home()?.join("config.toml");
    let Ok(existing) = fs::read_to_string(&config) else {
        return Ok(false);
    };
    Ok(current_managed_block(&existing)
        .map(|block| normalize_newlines(block) == normalize_newlines(&managed_block()))
        .unwrap_or(false))
}

fn codex_home() -> Result<PathBuf, String> {
    if let Some(home) = env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(home));
    }
    let home = env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .ok_or_else(|| "could not determine home directory".to_string())?;
    Ok(PathBuf::from(home).join(".codex"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigChange {
    Created,
    Added,
    Updated,
    Unchanged,
}

fn configure_at(path: &Path) -> Result<ConfigChange, String> {
    let existing = match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };
    let managed = managed_block();

    let Some(existing) = existing else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
        }
        fs::write(path, format!("{managed}\n"))
            .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
        return Ok(ConfigChange::Created);
    };

    let start = existing.find(CONFIG_START);
    let end = existing.find(CONFIG_END);
    match (start, end) {
        (None, None) => {
            let mut updated = existing;
            if !updated.is_empty() {
                if !updated.ends_with('\n') {
                    updated.push('\n');
                }
                updated.push('\n');
            }
            updated.push_str(&managed);
            updated.push('\n');
            fs::write(path, updated)
                .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
            Ok(ConfigChange::Added)
        }
        (Some(start), Some(end)) if end >= start => {
            let managed_end = end + CONFIG_END.len();
            let current = &existing[start..managed_end];
            if normalize_newlines(current) == normalize_newlines(&managed) {
                return Ok(ConfigChange::Unchanged);
            }
            let mut updated = String::with_capacity(existing.len() + managed.len());
            updated.push_str(&existing[..start]);
            updated.push_str(&managed);
            updated.push_str(&existing[managed_end..]);
            fs::write(path, updated)
                .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
            Ok(ConfigChange::Updated)
        }
        _ => Err(format!(
            "{} contains an incomplete AgentLatch managed hook section; fix the markers before configuring again",
            path.display()
        )),
    }
}

fn current_managed_block(existing: &str) -> Option<&str> {
    let start = existing.find(CONFIG_START)?;
    let end = existing[start..].find(CONFIG_END)? + start + CONFIG_END.len();
    Some(&existing[start..end])
}

fn managed_block() -> String {
    format!("{CONFIG_START}\n{}\n{CONFIG_END}", HOOKS_TEMPLATE.trim())
}

fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n")
}

pub fn run_hook(args: Vec<std::ffi::OsString>) -> Result<(), String> {
    let args: Vec<String> = args
        .into_iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect();
    if args.len() != 2 || args[0] != "codex" {
        return Err(
            "usage: agentlatch hook codex <pre-tool-use|post-tool-use|stop|subagent-stop|interrupt|session-end>"
                .into(),
        );
    }

    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| format!("failed to read hook input: {error}"))?;
    let event: Value = serde_json::from_str(&input)
        .map_err(|error| format!("invalid Codex hook input: {error}"))?;

    match args[1].as_str() {
        "pre-tool-use" => {
            if let Err(error) = pre_tool_use(&event) {
                print_pre_tool_denial(format!(
                    "AgentLatch internal coordination error: {error}. The mutation was blocked to avoid an unsafe race."
                ));
            }
            Ok(())
        }
        "post-tool-use" => {
            if let Err(error) = post_tool_use(&event) {
                println!(
                    "{}",
                    json!({"systemMessage": format!("AgentLatch could not release a tool claim immediately: {error}. It will expire automatically.")})
                );
            }
            Ok(())
        }
        "stop" => {
            let _ = stop(&event);
            println!("{{}}");
            Ok(())
        }
        "subagent-stop" => {
            let _ = subagent_stop(&event);
            println!("{{}}");
            Ok(())
        }
        "interrupt" => {
            let _ = interrupt(&event);
            Ok(())
        }
        "session-end" => {
            let _ = session_end(&event);
            Ok(())
        }
        _ => Err("unknown Codex hook".into()),
    }
}

fn pre_tool_use(event: &Value) -> Result<(), String> {
    let cwd = PathBuf::from(field(event, "cwd")?);
    if coordination::find_project_root(&cwd).is_none() {
        return Ok(());
    }

    let tool = field(event, "tool_name")?;
    let input = event.get("tool_input").cloned().unwrap_or(Value::Null);
    let resources = resources_for_tool(tool, &input);
    if resources.deny_state_file {
        print_pre_tool_denial(
            "agent_latch.json is private AgentLatch state. Do not read, edit, replace, delete, or generate it directly."
                .into(),
        );
        return Ok(());
    }
    if resources.unresolved_mutation {
        print_pre_tool_denial(
            "AgentLatch blocked this potentially mutating tool call because it could not determine a concrete file or directory target. Whole-project and wildcard claims are not allowed. Use a path-aware edit/write tool or otherwise target concrete files or directories."
                .into(),
        );
        return Ok(());
    }
    if !resources.claim || resources.paths.is_empty() {
        return Ok(());
    }

    let owner = tool_owner(event, tool)?;
    match coordination::acquire(&cwd, &resources.paths, &owner, Duration::ZERO) {
        Ok(()) => Ok(()),
        Err(AcquireError::Conflict(conflict)) => {
            let path_hint = resources
                .paths
                .first()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| ".".into());
            print_pre_tool_denial(format!(
                "AgentLatch conflict: {conflict}. This tool call did not run. Do not edit around the claim, and do not wait yet while any independent work from the user's request remains. Continue that independent work now. Only after no independent work remains, re-read the affected file because the other task may have changed it, run agentlatch wait --timeout 600 \"{path_hint}\", re-read the file again after the wait, and retry the blocked work."
            ));
            Ok(())
        }
        Err(AcquireError::Internal(error)) => Err(error),
    }
}

fn post_tool_use(event: &Value) -> Result<(), String> {
    let cwd = PathBuf::from(field(event, "cwd")?);
    if coordination::find_project_root(&cwd).is_none() {
        return Ok(());
    }
    let tool = field(event, "tool_name")?;
    let input = event.get("tool_input").cloned().unwrap_or(Value::Null);
    let resources = resources_for_tool(tool, &input);
    if !resources.claim || resources.paths.is_empty() {
        return Ok(());
    }
    let lease = field(event, "tool_use_id")?;
    coordination::release_lease(&cwd, &resources.paths, lease)?;
    Ok(())
}

fn stop(event: &Value) -> Result<(), String> {
    let cwd = PathBuf::from(field(event, "cwd")?);
    let key = agent_key(event)?;
    coordination::release_agent(&cwd, &key)?;
    Ok(())
}

fn subagent_stop(event: &Value) -> Result<(), String> {
    let cwd = PathBuf::from(field(event, "cwd")?);
    let key = agent_key(event)?;
    coordination::release_agent(&cwd, &key)?;
    Ok(())
}

fn interrupt(event: &Value) -> Result<(), String> {
    let cwd = PathBuf::from(field(event, "cwd")?);
    let key = agent_key(event)?;
    coordination::release_agent(&cwd, &key)?;
    Ok(())
}

fn session_end(event: &Value) -> Result<(), String> {
    let cwd = PathBuf::from(field(event, "cwd")?);
    let session = field(event, "session_id")?;
    coordination::release_session(&cwd, session)?;
    Ok(())
}

fn tool_owner(event: &Value, tool: &str) -> Result<Owner, String> {
    let session = field(event, "session_id")?;
    let turn = field(event, "turn_id")?;
    let tool_use_id = field(event, "tool_use_id")?;
    let agent_id = optional_field(event, "agent_id");
    let agent_type = optional_field(event, "agent_type");
    let reason = if let Some(agent_type) = agent_type {
        format!("{agent_type} using {tool}")
    } else {
        format!("Codex using {tool}")
    };
    Ok(Owner::codex_tool(
        session,
        turn,
        agent_id,
        agent_type,
        tool_use_id,
        reason,
        tool_lease_ttl(tool),
    ))
}

fn agent_key(event: &Value) -> Result<String, String> {
    let session = field(event, "session_id")?;
    let turn = field(event, "turn_id")?;
    let owner = Owner::codex_tool(
        session,
        turn,
        optional_field(event, "agent_id"),
        optional_field(event, "agent_type"),
        "cleanup",
        "cleanup".into(),
        Duration::ZERO,
    );
    Ok(owner.key)
}

fn field<'a>(event: &'a Value, name: &str) -> Result<&'a str, String> {
    event
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Codex hook input is missing {name}"))
}

fn optional_field<'a>(event: &'a Value, name: &str) -> Option<&'a str> {
    event.get(name).and_then(Value::as_str)
}

fn print_pre_tool_denial(reason: String) {
    println!(
        "{}",
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason
            }
        })
    );
}

#[derive(Default)]
struct ToolResources {
    paths: Vec<PathBuf>,
    deny_state_file: bool,
    claim: bool,
    unresolved_mutation: bool,
}

fn resources_for_tool(tool: &str, input: &Value) -> ToolResources {
    let lower = tool.to_ascii_lowercase();
    let execution_text = collect_execution_text(input);

    let mut resources = if is_patch_tool(&lower) {
        let patch = first_string_field(input, &["input", "patch", "command"]).unwrap_or_default();
        let paths = patch_paths(patch);
        ToolResources {
            claim: !paths.is_empty(),
            unresolved_mutation: !patch.is_empty() && paths.is_empty(),
            paths,
            deny_state_file: false,
        }
    } else if is_direct_write_tool(&lower) {
        let paths = collect_path_fields(input);
        ToolResources {
            claim: !paths.is_empty(),
            unresolved_mutation: paths.is_empty(),
            paths,
            deny_state_file: false,
        }
    } else if is_shell_tool(&lower) {
        shell_resources(&execution_text, collect_path_fields(input))
    } else if is_read_tool(&lower) {
        ToolResources {
            paths: collect_path_fields(input),
            deny_state_file: false,
            claim: false,
            unresolved_mutation: false,
        }
    } else if lower.starts_with("mcp__") && mcp_may_mutate(&lower) {
        let paths = collect_path_fields(input);
        ToolResources {
            claim: !paths.is_empty(),
            unresolved_mutation: paths.is_empty(),
            paths,
            deny_state_file: false,
        }
    } else if lower.starts_with("mcp__") {
        ToolResources {
            paths: collect_path_fields(input),
            deny_state_file: false,
            claim: false,
            unresolved_mutation: false,
        }
    } else {
        ToolResources::default()
    };

    resources.paths.sort();
    resources.paths.dedup();
    resources.deny_state_file = resources.paths.iter().any(|path| is_state_path(path))
        || execution_text
            .to_ascii_lowercase()
            .contains("agent_latch.json")
        || first_string_field(input, &["input", "patch", "command"])
            .is_some_and(|text| text.to_ascii_lowercase().contains("agent_latch.json"));
    resources
}

fn first_string_field<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))
}

fn is_patch_tool(lower: &str) -> bool {
    lower == "apply_patch"
        || lower.ends_with(".apply_patch")
        || lower.ends_with("__apply_patch")
        || lower.ends_with("__codex_apply_patch")
}

fn is_direct_write_tool(lower: &str) -> bool {
    ["edit", "write", "edit_file", "write_file"]
        .iter()
        .any(|name| {
            lower == *name
                || lower.ends_with(&format!(".{name}"))
                || lower.ends_with(&format!("__{name}"))
        })
}

fn is_read_tool(lower: &str) -> bool {
    ["read", "read_file", "grep_files"].iter().any(|name| {
        lower == *name
            || lower.ends_with(&format!(".{name}"))
            || lower.ends_with(&format!("__{name}"))
    })
}

fn is_shell_tool(lower: &str) -> bool {
    matches!(
        lower,
        "bash"
            | "shell"
            | "shell_command"
            | "local_shell"
            | "exec_command"
            | "container.exec"
            | "functions.exec_command"
            | "powershell"
            | "pwsh"
    ) || lower.ends_with(".exec_command")
        || lower.ends_with("__exec_command")
        || lower.ends_with("__codex_exec")
}

fn collect_execution_text(input: &Value) -> String {
    let mut parts = Vec::new();
    collect_execution_text_inner(input, None, &mut parts);
    parts.join("\n")
}

fn collect_execution_text_inner(value: &Value, key: Option<&str>, output: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                collect_execution_text_inner(value, Some(key), output);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_execution_text_inner(value, key, output);
            }
        }
        Value::String(text) => {
            let Some(key) = key else {
                return;
            };
            if matches!(
                key.to_ascii_lowercase().as_str(),
                "command" | "cmd" | "code" | "script"
            ) {
                output.push(text.clone());
            }
        }
        _ => {}
    }
}

fn is_state_path(path: &Path) -> bool {
    path.file_name()
        .map(|name| {
            name.to_string_lossy()
                .eq_ignore_ascii_case("agent_latch.json")
        })
        .unwrap_or(false)
}

fn patch_paths(patch: &str) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for line in patch.lines() {
        for prefix in [
            "*** Update File: ",
            "*** Add File: ",
            "*** Delete File: ",
            "*** Move to: ",
        ] {
            if let Some(path) = line.strip_prefix(prefix) {
                result.push(PathBuf::from(path.trim()));
                break;
            }
        }
    }
    result
}

#[cfg(test)]
fn shell_needs_claim(command: &str) -> bool {
    let resources = shell_resources(command, vec![]);
    resources.claim || resources.unresolved_mutation
}

#[derive(Debug, PartialEq, Eq)]
enum ShellToken {
    Word(String),
    Output,
}

fn flush_shell_word(word: &mut String, started: &mut bool, segment: &mut Vec<ShellToken>) {
    if *started {
        segment.push(ShellToken::Word(std::mem::take(word)));
        *started = false;
    }
}

// Analyze command tokens; search patterns quoted as arguments are not commands.
// Dynamic command substitutions fail closed because their targets are unknown.
fn shell_segments(command: &str) -> Option<Vec<Vec<ShellToken>>> {
    let chars: Vec<char> = command.chars().collect();
    let mut segments = Vec::new();
    let mut segment = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '$' && chars.get(i + 1) == Some(&'(') && quote != Some('\'') {
            return None;
        }
        if c == '\u{60}' && quote != Some('\'') {
            return None;
        }
        if let Some(active) = quote {
            if c == active {
                quote = None;
            } else {
                word.push(c);
            }
            started = true;
        } else {
            match c {
                '\'' | '"' => {
                    quote = Some(c);
                    started = true;
                }
                '\\' if chars
                    .get(i + 1)
                    .is_some_and(|c| c.is_whitespace() || *c == '"' || *c == '\'') =>
                {
                    i += 1;
                    word.push(chars[i]);
                    started = true;
                }
                '#' if !started => {
                    while i < chars.len() && chars[i] != '\n' {
                        i += 1;
                    }
                    continue;
                }
                c if c.is_whitespace() && c != '\n' && c != '\r' => {
                    flush_shell_word(&mut word, &mut started, &mut segment);
                }
                ';' | '\n' | '\r' | '|' => {
                    flush_shell_word(&mut word, &mut started, &mut segment);
                    if !segment.is_empty() {
                        segments.push(std::mem::take(&mut segment));
                    }
                    if c == '|' && chars.get(i + 1) == Some(&'|') {
                        i += 1;
                    }
                }
                '&' if chars.get(i + 1) == Some(&'&') => {
                    flush_shell_word(&mut word, &mut started, &mut segment);
                    if !segment.is_empty() {
                        segments.push(std::mem::take(&mut segment));
                    }
                    i += 1;
                }
                '>' => {
                    flush_shell_word(&mut word, &mut started, &mut segment);
                    segment.push(ShellToken::Output);
                    if chars.get(i + 1) == Some(&'>') {
                        i += 1;
                    }
                }
                '(' | ')' | '{' | '}' => return None,
                _ => {
                    word.push(c);
                    started = true;
                }
            }
        }
        i += 1;
    }
    if quote.is_some() {
        return None;
    }
    flush_shell_word(&mut word, &mut started, &mut segment);
    if !segment.is_empty() {
        segments.push(segment);
    }
    Some(segments)
}

fn concrete_shell_path(value: &str) -> Option<PathBuf> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || value.contains(['*', '?', '[', ']', '$', '%', '{', '}'])
        || value.starts_with('-')
        || value.contains("://")
    {
        return None;
    }
    Some(PathBuf::from(value))
}

fn shell_resources(command: &str, mut paths: Vec<PathBuf>) -> ToolResources {
    let Some(segments) = shell_segments(command) else {
        return ToolResources {
            unresolved_mutation: true,
            ..ToolResources::default()
        };
    };
    let mut needs_claim = command.trim().is_empty();
    let mut unresolved = false;
    for segment in segments {
        let mut words = Vec::new();
        let mut redirect = false;
        for token in segment {
            match token {
                ShellToken::Output => {
                    if redirect {
                        unresolved = true;
                    }
                    redirect = true;
                    needs_claim = true;
                }
                ShellToken::Word(word) if redirect => {
                    if let Some(path) = concrete_shell_path(&word) {
                        paths.push(path);
                    } else {
                        unresolved = true;
                    }
                    redirect = false;
                }
                ShellToken::Word(word) => words.push(word),
            }
        }
        if redirect {
            unresolved = true;
        }
        if words.first().is_some_and(|s| s == "&") {
            words.remove(0);
        }
        if words.is_empty() {
            continue;
        }
        let exe = words[0]
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let args: Vec<&str> = words[1..].iter().map(String::as_str).collect();
        if matches!(
            exe.as_str(),
            "powershell"
                | "powershell.exe"
                | "pwsh"
                | "pwsh.exe"
                | "bash"
                | "bash.exe"
                | "sh"
                | "sh.exe"
                | "cmd"
                | "cmd.exe"
        ) {
            let nested_flag = if matches!(exe.as_str(), "cmd" | "cmd.exe") {
                "/c"
            } else if matches!(
                exe.as_str(),
                "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"
            ) {
                "-command"
            } else {
                "-c"
            };
            if let Some(i) = args
                .iter()
                .position(|arg| arg.eq_ignore_ascii_case(nested_flag))
            {
                let nested = shell_resources(&args[i + 1..].join(" "), vec![]);
                paths.extend(nested.paths);
                needs_claim |= nested.claim || nested.unresolved_mutation;
                unresolved |= nested.unresolved_mutation;
            } else {
                unresolved = true;
            }
            continue;
        }
        let (mutates, targets) = shell_command_targets(&exe, &args);
        if mutates {
            needs_claim = true;
            if let Some(targets) = targets {
                for target in targets {
                    if let Some(path) = concrete_shell_path(target) {
                        paths.push(path);
                    } else {
                        unresolved = true;
                    }
                }
            } else {
                unresolved = true;
            }
        }
    }
    ToolResources {
        claim: needs_claim && !paths.is_empty() && !unresolved,
        unresolved_mutation: unresolved || (needs_claim && paths.is_empty()),
        paths,
        deny_state_file: false,
    }
}

// None denotes a mutation with an unknown or project-wide target.
fn shell_command_targets<'a>(exe: &str, args: &'a [&'a str]) -> (bool, Option<Vec<&'a str>>) {
    if matches!(exe, "agentlatch" | "agentlatch.exe") {
        return (false, Some(vec![]));
    }
    if matches!(exe, "git" | "git.exe") {
        let mut i = 0;
        let mut alternate_tree = false;
        while i < args.len() && args[i].starts_with('-') {
            if matches!(args[i], "-C" | "--git-dir" | "--work-tree") {
                alternate_tree = true;
                i += 1;
            } else if args[i] == "-c" {
                i += 1;
            }
            i += 1;
        }
        let sub = args
            .get(i)
            .copied()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let rest = &args[(i + 1).min(args.len())..];
        let (mutates, targets) = match sub.as_str() {
            // Git index and ref operations do not normally write working-tree files.
            "add" | "commit" | "status" | "diff" | "log" | "show" | "fetch" | "push"
            | "ls-files" => (false, Some(vec![])),
            "restore" | "rm" | "mv" | "clean" => (true, git_pathspecs(rest)),
            "checkout" => (
                true,
                rest.iter()
                    .position(|s| *s == "--")
                    .map(|i| rest[i + 1..].to_vec())
                    .filter(|v| !v.is_empty()),
            ),
            "reset"
                if rest
                    .iter()
                    .any(|a| matches!(*a, "--hard" | "--merge" | "--keep")) =>
            {
                (true, None)
            }
            "reset" => (false, Some(vec![])),
            "merge" | "rebase" | "switch" | "stash" | "apply" | "cherry-pick" | "revert"
            | "pull" => (true, None),
            _ => (false, Some(vec![])),
        };
        return (
            mutates,
            if mutates && alternate_tree {
                None
            } else {
                targets
            },
        );
    }
    if matches!(
        exe,
        "set-content"
            | "add-content"
            | "out-file"
            | "remove-item"
            | "move-item"
            | "copy-item"
            | "new-item"
            | "rename-item"
            | "clear-content"
    ) {
        return (true, powershell_path_args(args));
    }
    if matches!(
        exe,
        "rm" | "del" | "erase" | "mv" | "cp" | "touch" | "mkdir" | "rmdir"
    ) {
        let targets: Vec<_> = args
            .iter()
            .copied()
            .filter(|a| !a.starts_with('-'))
            .collect();
        return (true, (!targets.is_empty()).then_some(targets));
    }
    if exe == "sed" && args.iter().any(|a| a.starts_with("-i")) {
        return (true, None);
    }
    if exe == "cargo" && args.first() == Some(&"fmt") {
        return (!args.contains(&"--check"), Some(vec![]));
    }
    if matches!(exe, "npm" | "pnpm") && args.first() == Some(&"version") {
        return (true, None);
    }
    if exe == "bunx" && args.first() == Some(&"prettier") {
        return (true, None);
    }
    if matches!(exe, "python" | "python3" | "node" | "bun") {
        return (true, None);
    }
    (false, Some(vec![]))
}

fn git_pathspecs<'a>(args: &'a [&'a str]) -> Option<Vec<&'a str>> {
    let after_separator = args.iter().position(|a| *a == "--").map(|i| &args[i + 1..]);
    let paths: Vec<_> = if let Some(paths) = after_separator {
        paths.to_vec()
    } else {
        args.iter()
            .copied()
            .filter(|a| !a.starts_with('-'))
            .collect()
    };
    (!paths.is_empty()).then_some(paths)
}

fn powershell_path_args<'a>(args: &'a [&'a str]) -> Option<Vec<&'a str>> {
    let mut paths = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let lower = args[i].to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "-path" | "-literalpath" | "-filepath" | "-destination" | "-target" | "-newname"
        ) {
            i += 1;
            paths.push(*args.get(i)?);
        } else if matches!(
            lower.as_str(),
            "-value" | "-itemtype" | "-name" | "-encoding" | "-filter" | "-include" | "-exclude"
        ) {
            i += 1;
            args.get(i)?;
        } else if !lower.starts_with('-') && paths.is_empty() {
            paths.push(args[i]);
        }
        i += 1;
    }
    (!paths.is_empty()).then_some(paths)
}

fn tool_lease_ttl(tool: &str) -> Duration {
    let lower = tool.to_ascii_lowercase();
    if is_patch_tool(&lower) || is_direct_write_tool(&lower) {
        Duration::from_secs(2 * 60)
    } else if is_shell_tool(&lower) {
        Duration::from_secs(15 * 60)
    } else {
        Duration::from_secs(5 * 60)
    }
}

fn mcp_may_mutate(tool: &str) -> bool {
    let lower = tool.to_ascii_lowercase();
    [
        "write", "edit", "patch", "apply", "create", "delete", "remove", "move", "rename",
        "replace", "save",
    ]
    .iter()
    .any(|part| lower.contains(part))
}

fn collect_path_fields(input: &Value) -> Vec<PathBuf> {
    let mut result = Vec::new();
    collect_path_fields_inner(input, None, &mut result);
    result
}

fn collect_path_fields_inner(value: &Value, key: Option<&str>, output: &mut Vec<PathBuf>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                collect_path_fields_inner(value, Some(key), output);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_path_fields_inner(value, key, output);
            }
        }
        Value::String(text) => {
            let Some(key) = key else {
                return;
            };
            let lower = key.to_ascii_lowercase();
            let path_key = lower.contains("path")
                || matches!(
                    lower.as_str(),
                    "file" | "filename" | "source" | "destination" | "target" | "src" | "dst"
                );
            if path_key && !text.contains("://") && !text.is_empty() {
                output.push(PathBuf::from(text));
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn extracts_apply_patch_paths() {
        let paths = patch_paths(
            "*** Begin Patch\n*** Update File: src/a.rs\n*** Move to: src/b.rs\n*** Add File: c.txt\n*** End Patch",
        );
        assert_eq!(
            paths,
            vec![
                PathBuf::from("src/a.rs"),
                PathBuf::from("src/b.rs"),
                PathBuf::from("c.txt")
            ]
        );
    }

    #[test]
    fn classifies_common_shell_commands() {
        assert!(!shell_needs_claim("git status --short"));
        assert!(!shell_needs_claim("cargo test"));
        assert!(!shell_needs_claim(
            "& 'C:\\Tools\\check_project.ps1' 'D:\\Projects\\Example'"
        ));
        assert!(!shell_needs_claim("custom-readonly-tool inspect project"));
        assert!(!shell_needs_claim("agentlatch wait shared.txt"));
        assert!(!shell_needs_claim(
            "D:\\Documents\\agent-latch\\target\\debug\\agentlatch.exe status"
        ));
        assert!(!shell_needs_claim(
            "& 'D:\\Documents\\agent-latch\\target\\debug\\agentlatch.exe' claim 'index.html'"
        ));
        assert!(!shell_needs_claim(
            "& 'D:\\Documents\\agent-latch\\target\\debug\\agentlatch.exe' claim 'index.html'; & 'D:\\Documents\\agent-latch\\target\\debug\\agentlatch.exe' claim 'styles.css'"
        ));
        assert!(shell_needs_claim(
            "& 'D:\\Documents\\agent-latch\\target\\debug\\agentlatch.exe' status; Remove-Item index.html"
        ));
        assert!(shell_needs_claim(
            "agentlatch status && Set-Content index.html 'changed'"
        ));
        assert!(shell_needs_claim("Set-Content src/a.rs 'x'"));
        assert!(shell_needs_claim("python tools/rewrite.py"));
    }

    #[test]
    fn extracts_mcp_path_fields() {
        let input = json!({
            "file_path": "src/main.rs",
            "nested": {"destination_path": "src/out.rs"},
            "url": "https://example.com/file"
        });
        let mut paths = collect_path_fields(&input);
        paths.sort();
        assert_eq!(
            paths,
            vec![PathBuf::from("src/main.rs"), PathBuf::from("src/out.rs")]
        );
    }

    #[test]
    fn classifies_real_apply_patch_input_shape() {
        let resources = resources_for_tool(
            "apply_patch",
            &json!({
                "input": "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-old\n+new\n*** End Patch"
            }),
        );
        assert!(resources.claim);
        assert!(!resources.unresolved_mutation);
        assert_eq!(resources.paths, vec![PathBuf::from("src/main.rs")]);
    }

    #[test]
    fn malformed_patch_never_creates_project_wide_claim() {
        let resources = resources_for_tool(
            "functions.apply_patch",
            &json!({"input": "patch text without a recognized path"}),
        );
        assert!(!resources.claim);
        assert!(resources.unresolved_mutation);
        assert!(resources.paths.is_empty());
    }

    #[test]
    fn classifies_real_exec_command_input_shape() {
        let mutating = resources_for_tool(
            "exec_command",
            &json!({"cmd": "Set-Content src/main.rs 'changed'"}),
        );
        assert!(mutating.claim);
        assert!(!mutating.unresolved_mutation);
        assert_eq!(mutating.paths, vec![PathBuf::from("src/main.rs")]);

        let read_only = resources_for_tool("exec_command", &json!({"cmd": "git status --short"}));
        assert!(!read_only.claim);
        assert!(!read_only.unresolved_mutation);
    }

    #[test]
    fn git_staging_and_commit_do_not_require_worktree_claims() {
        for command in [
            "git add -- README.md",
            "git add -A",
            "git add -- 'src/file with spaces.rs'",
            r#"git add -- "D:\Projects\Game\README.md""#,
            "git commit -m 'git restore -- src/a.rs'",
            r"git -C D:\somewhere add -- README.md",
        ] {
            let resources = resources_for_tool("exec_command", &json!({"cmd": command}));
            assert!(!resources.claim, "{command}");
            assert!(!resources.unresolved_mutation, "{command}");
        }
    }

    #[test]
    fn quoted_patterns_are_not_shell_mutations() {
        for command in [
            "rg 'git add' src/codex.rs",
            "rg \"git restore -- src/main.rs\" src/codex.rs",
            "echo 'Set-Content src/a.rs'",
            "Select-String -Pattern 'git reset --hard' -Path src/codex.rs",
            "echo '>'",
            "git status --short; rg 'git clean' README.md",
        ] {
            let resources = resources_for_tool("exec_command", &json!({"cmd": command}));
            assert!(!resources.claim, "{command}");
            assert!(!resources.unresolved_mutation, "{command}");
        }
    }

    #[test]
    fn shell_mutations_claim_concrete_paths_and_reject_unknown_targets() {
        for (command, expected) in [
            ("git restore -- src/main.rs", "src/main.rs"),
            ("git checkout -- src/main.rs", "src/main.rs"),
            ("git rm -- src/main.rs", "src/main.rs"),
            (
                "Set-Content -LiteralPath 'src/file with spaces.rs' -Value 'hello'",
                "src/file with spaces.rs",
            ),
            ("echo test > output.txt", "output.txt"),
            ("git add -- README.md && Remove-Item src/a.rs", "src/a.rs"),
        ] {
            let resources = resources_for_tool("exec_command", &json!({"cmd": command}));
            assert!(resources.claim, "{command}");
            assert!(!resources.unresolved_mutation, "{command}");
            assert_eq!(resources.paths, vec![PathBuf::from(expected)], "{command}");
        }
        for command in [
            "git reset --hard",
            "git clean -fd",
            "git checkout other-branch",
            "git restore -- .",
            "rm *.rs",
            "echo test >",
            "echo $(rm file.txt)",
        ] {
            let resources = resources_for_tool("exec_command", &json!({"cmd": command}));
            assert!(!resources.claim, "{command}");
            assert!(resources.unresolved_mutation, "{command}");
        }
    }

    #[test]
    fn nested_shell_invocations_are_inspected() {
        let examples = [
            (
                "pwsh -NoProfile -Command \"Remove-Item src/old.txt\"",
                Some("src/old.txt"),
            ),
            ("bash -c 'rm src/old.txt'", Some("src/old.txt")),
            ("powershell -Command \"rg 'git add' src/codex.rs\"", None),
        ];
        for (command, expected_path) in examples {
            let resources = resources_for_tool("exec_command", &json!({"cmd": command}));
            assert!(!resources.unresolved_mutation, "{command}");
            assert_eq!(resources.claim, expected_path.is_some(), "{command}");
            assert_eq!(
                resources.paths,
                expected_path
                    .into_iter()
                    .map(PathBuf::from)
                    .collect::<Vec<_>>(),
                "{command}"
            );
        }
        for command in ["pwsh -File tools/rewrite.ps1", "echo $(rm file.txt)"] {
            let resources = resources_for_tool("exec_command", &json!({"cmd": command}));
            assert!(resources.unresolved_mutation, "{command}");
        }
    }

    #[test]
    fn generic_execute_mcp_tools_are_not_assumed_to_mutate() {
        let execute_file = resources_for_tool(
            "mcp__analysis_sandbox__execute_file",
            &json!({
                "path": "src/a.rs",
                "language": "javascript",
                "code": "console.log('analysis')"
            }),
        );
        assert!(!execute_file.claim);
        assert!(!execute_file.unresolved_mutation);
        assert_eq!(execute_file.paths, vec![PathBuf::from("src/a.rs")]);

        let execute_without_path = resources_for_tool(
            "mcp__analysis_sandbox__execute",
            &json!({
                "language": "javascript",
                "code": "const fs = require('fs'); fs.writeFileSync('src/a.rs', 'x');"
            }),
        );
        assert!(!execute_without_path.claim);
        assert!(!execute_without_path.unresolved_mutation);
        assert!(execute_without_path.paths.is_empty());
    }

    #[test]
    fn read_tools_do_not_claim_but_private_state_is_denied() {
        let normal_read = resources_for_tool("read_file", &json!({"path": "src/main.rs"}));
        assert!(!normal_read.claim);
        assert!(!normal_read.deny_state_file);

        let state_read = resources_for_tool("read_file", &json!({"path": "agent_latch.json"}));
        assert!(!state_read.claim);
        assert!(state_read.deny_state_file);

        let state_shell = resources_for_tool(
            "exec_command",
            &json!({"cmd": "Get-Content agent_latch.json"}),
        );
        assert!(state_shell.deny_state_file);
    }

    #[test]
    fn direct_write_tools_extract_real_path_fields() {
        let resources = resources_for_tool(
            "Write",
            &json!({"file_path": "src/generated.rs", "content": "hello"}),
        );
        assert!(resources.claim);
        assert!(!resources.unresolved_mutation);
        assert_eq!(resources.paths, vec![PathBuf::from("src/generated.rs")]);
    }

    #[test]
    fn generic_mcp_read_of_private_state_is_denied_without_claim() {
        let resources = resources_for_tool(
            "mcp__filesystem__read_text_file",
            &json!({"path": "agent_latch.json"}),
        );
        assert!(!resources.claim);
        assert!(resources.deny_state_file);
    }

    #[test]
    fn configure_reconciles_without_duplicates() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "agentlatch-codex-config-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("config.toml");
        fs::write(
            &path,
            "[features]\nhooks = true\n\n[[hooks.Interrupt]]\n[[hooks.Interrupt.hooks]]\ntype = \"command\"\ncommand = \"other hook\"\n",
        )
        .unwrap();

        assert_eq!(configure_at(&path).unwrap(), ConfigChange::Added);
        let once = fs::read_to_string(&path).unwrap();
        assert_eq!(configure_at(&path).unwrap(), ConfigChange::Unchanged);
        let twice = fs::read_to_string(&path).unwrap();
        assert_eq!(once, twice);
        assert!(twice.contains("other hook"));
        assert_eq!(
            twice.matches("agentlatch hook codex pre-tool-use").count(),
            1
        );

        fs::remove_dir_all(root).unwrap();
    }
}
