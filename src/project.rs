use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

const STATE_FILE: &str = "agent_latch.json";
const AGENTS_FILE: &str = "AGENTS.md";
const GITIGNORE_FILE: &str = ".gitignore";
const AGENTS_START: &str = "<!-- agentlatch:start -->";
const AGENTS_END: &str = "<!-- agentlatch:end -->";
const AGENTS_TEMPLATE: &str = include_str!("../templates/AGENTS_SECTION_TEMPLATE.md");
const INITIAL_STATE: &str = "{\n  \"version\": 1,\n  \"agents\": {},\n  \"files\": {}\n}\n";

pub struct InitResult {
    pub root: PathBuf,
    pub changes: Vec<&'static str>,
}

pub fn init(root: &Path) -> Result<InitResult, String> {
    let root = absolute_existing_directory(root)?;
    let mut changes = Vec::new();

    if create_state_file(&root)? {
        changes.push("created agent_latch.json");
    }

    if ensure_gitignore(&root)? {
        changes.push("updated .gitignore");
    }

    match update_agents_file(&root)? {
        AgentsChange::Created => changes.push("created AGENTS.md with AgentLatch instructions"),
        AgentsChange::Added => changes.push("added AgentLatch instructions to AGENTS.md"),
        AgentsChange::Updated => changes.push("updated AgentLatch instructions in AGENTS.md"),
        AgentsChange::Unchanged => {}
    }

    if changes.is_empty() {
        changes.push("already up to date");
    }

    Ok(InitResult { root, changes })
}

fn absolute_existing_directory(root: &Path) -> Result<PathBuf, String> {
    let root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("failed to get current directory: {error}"))?
            .join(root)
    };

    let metadata = fs::metadata(&root)
        .map_err(|error| format!("cannot initialize {}: {error}", root.display()))?;

    if !metadata.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }

    root.canonicalize()
        .map_err(|error| format!("failed to resolve {}: {error}", root.display()))
}

fn create_state_file(root: &Path) -> Result<bool, String> {
    let path = root.join(STATE_FILE);
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(INITIAL_STATE.as_bytes())
                .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(format!("failed to create {}: {error}", path.display())),
    }
}

fn ensure_gitignore(root: &Path) -> Result<bool, String> {
    let path = root.join(GITIGNORE_FILE);
    let existing = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };

    let mut updated = existing;
    let mut changed = false;
    for entry in ["/agent_latch.json"] {
        if updated.lines().any(|line| line.trim() == entry) {
            continue;
        }
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(entry);
        updated.push('\n');
        changed = true;
    }
    if !changed {
        return Ok(false);
    }

    fs::write(&path, updated)
        .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
    Ok(true)
}

enum AgentsChange {
    Created,
    Added,
    Updated,
    Unchanged,
}

fn update_agents_file(root: &Path) -> Result<AgentsChange, String> {
    let path = root.join(AGENTS_FILE);
    let existing = match fs::read_to_string(&path) {
        Ok(content) => Some(content),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };

    let managed = managed_agents_section();

    let Some(existing) = existing else {
        fs::write(&path, format!("{managed}\n"))
            .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
        return Ok(AgentsChange::Created);
    };

    let start = existing.find(AGENTS_START);
    let end = existing.find(AGENTS_END);

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

            fs::write(&path, updated)
                .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
            Ok(AgentsChange::Added)
        }
        (Some(start), Some(end)) if end >= start => {
            let managed_end = end + AGENTS_END.len();
            let current = &existing[start..managed_end];

            if normalize_newlines(current) == normalize_newlines(&managed) {
                return Ok(AgentsChange::Unchanged);
            }

            let mut updated = String::with_capacity(existing.len() + managed.len());
            updated.push_str(&existing[..start]);
            updated.push_str(&managed);
            updated.push_str(&existing[managed_end..]);

            fs::write(&path, updated)
                .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
            Ok(AgentsChange::Updated)
        }
        _ => Err(format!(
            "{} contains an incomplete AgentLatch managed section; fix the markers before running init again",
            path.display()
        )),
    }
}

fn managed_agents_section() -> String {
    format!("{AGENTS_START}\n{}\n{AGENTS_END}", AGENTS_TEMPLATE.trim())
}

fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_project(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "agentlatch-test-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn init_is_idempotent() {
        let root = temp_project("idempotent");

        let first = init(&root).unwrap();
        assert!(!first.changes.is_empty());

        let agents_once = fs::read_to_string(root.join(AGENTS_FILE)).unwrap();
        let ignore_once = fs::read_to_string(root.join(GITIGNORE_FILE)).unwrap();
        let state_once = fs::read_to_string(root.join(STATE_FILE)).unwrap();

        let second = init(&root).unwrap();
        assert_eq!(second.changes, vec!["already up to date"]);

        assert_eq!(
            agents_once,
            fs::read_to_string(root.join(AGENTS_FILE)).unwrap()
        );
        assert_eq!(
            ignore_once,
            fs::read_to_string(root.join(GITIGNORE_FILE)).unwrap()
        );
        assert_eq!(
            state_once,
            fs::read_to_string(root.join(STATE_FILE)).unwrap()
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn init_updates_only_managed_agents_section() {
        let root = temp_project("agents");
        let path = root.join(AGENTS_FILE);
        fs::write(
            &path,
            "# Existing\n\nKeep me.\n\n<!-- agentlatch:start -->\nold\n<!-- agentlatch:end -->\n\nKeep me too.\n",
        )
        .unwrap();

        init(&root).unwrap();
        let updated = fs::read_to_string(&path).unwrap();

        assert!(updated.contains("# Existing"));
        assert!(updated.contains("Keep me."));
        assert!(updated.contains("Keep me too."));
        assert!(!updated.contains("\nold\n"));
        assert!(updated.contains(AGENTS_TEMPLATE.trim()));

        fs::remove_dir_all(root).unwrap();
    }
}
