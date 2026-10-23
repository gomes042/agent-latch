use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const STATE_FILE: &str = "agent_latch.json";
const MANUAL_LEASE_MS: u64 = 30 * 60 * 1000;
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct State {
    #[serde(default = "state_version")]
    version: u32,
    #[serde(default)]
    agents: BTreeMap<String, AgentRecord>,
    #[serde(default)]
    files: BTreeMap<String, Claim>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: state_version(),
            agents: BTreeMap::new(),
            files: BTreeMap::new(),
        }
    }
}

fn state_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentRecord {
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_type: Option<String>,
    reason: String,
    first_seen_ms: u64,
    last_seen_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ClaimMode {
    Tool,
    Reservation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Claim {
    owner: String,
    lease: String,
    mode: ClaimMode,
    acquired_ms: u64,
    expires_ms: u64,
}

#[derive(Debug, Clone)]
pub struct Owner {
    pub key: String,
    pub lease: String,
    pub kind: String,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub reason: String,
    mode: ClaimMode,
    ttl_ms: u64,
}

impl Owner {
    pub fn codex_tool(
        session_id: &str,
        turn_id: &str,
        agent_id: Option<&str>,
        agent_type: Option<&str>,
        tool_use_id: &str,
        reason: String,
        ttl: Duration,
    ) -> Self {
        let identity = format!(
            "codex\0{session_id}\0{turn_id}\0{}",
            agent_id.unwrap_or("main")
        );
        Self {
            key: format!("codex-{:016x}", stable_hash(identity.as_bytes())),
            lease: tool_use_id.to_string(),
            kind: "codex".into(),
            session_id: Some(session_id.into()),
            turn_id: Some(turn_id.into()),
            agent_id: agent_id.map(str::to_string),
            agent_type: agent_type.map(str::to_string),
            reason,
            mode: ClaimMode::Tool,
            ttl_ms: ttl.as_millis().min(u64::MAX as u128) as u64,
        }
    }

    fn manual(id: String, reason: String, ttl: Duration) -> Self {
        let ttl_ms = ttl.as_millis().min(u64::MAX as u128) as u64;
        Self {
            key: format!("manual-{:016x}", stable_hash(id.as_bytes())),
            lease: id.clone(),
            kind: "manual".into(),
            session_id: None,
            turn_id: None,
            agent_id: None,
            agent_type: None,
            reason,
            mode: ClaimMode::Reservation,
            ttl_ms,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Conflict {
    pub resource: String,
    pub owner: String,
    pub reason: String,
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.reason.is_empty() {
            write!(f, "{} is owned by {}", self.resource, self.owner)
        } else {
            write!(
                f,
                "{} is owned by {} ({})",
                self.resource, self.owner, self.reason
            )
        }
    }
}

#[derive(Debug, Clone)]
pub enum AcquireError {
    Conflict(Conflict),
    Internal(String),
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(conflict) => conflict.fmt(f),
            Self::Internal(error) => f.write_str(error),
        }
    }
}

struct LockedState {
    root: PathBuf,
    mutex: File,
    state: State,
    dirty: bool,
}

impl LockedState {
    fn open(root: &Path) -> Result<Self, String> {
        let root = root
            .canonicalize()
            .map_err(|e| format!("failed to resolve project root {}: {e}", root.display()))?;
        let mutex_path = mutex_path(&root);
        let mutex = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&mutex_path)
            .map_err(|e| format!("failed to open {}: {e}", mutex_path.display()))?;
        mutex
            .lock_exclusive()
            .map_err(|e| format!("failed to lock {}: {e}", mutex_path.display()))?;
        let state = read_state(&root)?;
        Ok(Self {
            root,
            mutex,
            state,
            dirty: false,
        })
    }

    fn flush(&mut self) -> Result<(), String> {
        if !self.dirty {
            return Ok(());
        }
        write_state_atomic(&self.root, &self.state)?;
        self.dirty = false;
        Ok(())
    }
}

impl Drop for LockedState {
    fn drop(&mut self) {
        let _ = self.flush();
        let _ = self.mutex.unlock();
    }
}

fn mutex_path(root: &Path) -> PathBuf {
    let normalized = normalize_path(root);
    std::env::temp_dir().join(format!(
        "agentlatch-{:016x}.mutex",
        stable_hash(normalized.as_bytes())
    ))
}

fn read_state(root: &Path) -> Result<State, String> {
    let path = root.join(STATE_FILE);
    let mut file = File::open(&path).map_err(|e| {
        format!(
            "failed to open {}: {e}. Run 'agentlatch init' in this project first",
            path.display()
        )
    })?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|e| format!("invalid AgentLatch state {}: {e}", path.display()))
}

fn write_state_atomic(root: &Path, state: &State) -> Result<(), String> {
    let path = root.join(STATE_FILE);
    let tmp = root.join(format!(".agent_latch.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|e| format!("failed to serialize AgentLatch state: {e}"))?;

    let result = (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
            .map_err(|e| format!("failed to open {}: {e}", tmp.display()))?;
        file.write_all(&bytes)
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.flush())
            .map_err(|e| format!("failed to write {}: {e}", tmp.display()))?;
        file.sync_all()
            .map_err(|e| format!("failed to sync {}: {e}", tmp.display()))?;
        drop(file);
        fs::rename(&tmp, &path).map_err(|e| {
            format!(
                "failed to atomically replace {} with {}: {e}",
                path.display(),
                tmp.display()
            )
        })
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn cleanup_stale(state: &mut State, now: u64) -> bool {
    let before_files = state.files.len();
    state
        .files
        .retain(|resource, claim| claim.expires_ms > now && valid_resource_key(resource));
    let referenced: BTreeSet<String> = state.files.values().map(|c| c.owner.clone()).collect();
    let before_agents = state.agents.len();
    state.agents.retain(|key, _| referenced.contains(key));
    before_files != state.files.len() || before_agents != state.agents.len()
}

fn contains_pattern_syntax(path: &Path) -> bool {
    path.to_string_lossy()
        .chars()
        .any(|ch| matches!(ch, '*' | '?'))
}

fn valid_resource_key(resource: &str) -> bool {
    !resource.is_empty() && !resource.chars().any(|ch| matches!(ch, '*' | '?'))
}

fn normalize_path(path: &Path) -> String {
    let s = path.to_string_lossy().replace('/', "\\");
    #[cfg(windows)]
    {
        s.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        s
    }
}

fn resource_key(root: &Path, physical: &Path) -> String {
    physical
        .strip_prefix(root)
        .map(normalize_path)
        .unwrap_or_else(|_| normalize_path(physical))
}

fn physical_path(path: &Path) -> Result<PathBuf, String> {
    if path.exists() {
        return path
            .canonicalize()
            .map_err(|e| format!("failed to resolve {}: {e}", path.display()));
    }

    let mut missing = Vec::new();
    let mut cursor = path.to_path_buf();
    while !cursor.exists() {
        let name = cursor
            .file_name()
            .ok_or_else(|| format!("cannot resolve {}", path.display()))?
            .to_os_string();
        missing.push(name);
        cursor = cursor
            .parent()
            .ok_or_else(|| format!("cannot resolve {}", path.display()))?
            .to_path_buf();
    }

    let mut resolved = cursor
        .canonicalize()
        .map_err(|e| format!("failed to resolve {}: {e}", cursor.display()))?;
    for part in missing.into_iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    let absolute = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };
    let mut cursor = if absolute.is_dir() {
        absolute
    } else {
        absolute.parent()?.to_path_buf()
    };

    loop {
        if cursor.join(STATE_FILE).is_file() {
            return cursor.canonicalize().ok().or(Some(cursor));
        }
        if !cursor.pop() {
            return None;
        }
    }
}

fn resolve_resources(
    cwd: &Path,
    paths: &[PathBuf],
) -> Result<BTreeMap<PathBuf, BTreeSet<String>>, String> {
    let cwd_root = find_project_root(cwd).ok_or_else(|| {
        format!(
            "{} is not inside an initialized AgentLatch project",
            cwd.display()
        )
    })?;
    let mut grouped: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();

    for path in paths {
        if contains_pattern_syntax(path) {
            return Err(format!(
                "AgentLatch only accepts concrete file or directory paths; patterns are not allowed: {}",
                path.display()
            ));
        }
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        let physical = physical_path(&absolute)?;
        let root = find_project_root(&physical).unwrap_or_else(|| cwd_root.clone());
        let key = resource_key(&root, &physical);
        if key.is_empty() {
            return Err(format!(
                "AgentLatch does not allow locking an entire project root: {}",
                physical.display()
            ));
        }
        grouped.entry(root).or_default().insert(key);
    }

    Ok(grouped)
}

fn resources_overlap(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    fn is_descendant(child: &str, parent: &str) -> bool {
        child
            .strip_prefix(parent)
            .is_some_and(|rest| rest.starts_with('\\'))
    }
    is_descendant(a, b) || is_descendant(b, a)
}

fn owner_label(state: &State, key: &str) -> (String, String) {
    match state.agents.get(key) {
        Some(agent) => {
            let label = if let Some(agent_id) = &agent.agent_id {
                format!("{}:{agent_id}", agent.kind)
            } else {
                agent.kind.clone()
            };
            (label, agent.reason.clone())
        }
        None => (key.to_string(), String::new()),
    }
}

fn find_conflict(state: &State, requested: &BTreeSet<String>, owner: &Owner) -> Option<Conflict> {
    for resource in requested {
        if let Some((existing, claim)) = state.files.iter().find(|(existing, claim)| {
            claim.lease != owner.lease && resources_overlap(resource, existing)
        }) {
            let (label, reason) = owner_label(state, &claim.owner);
            return Some(Conflict {
                resource: existing.clone(),
                owner: label,
                reason,
            });
        }
    }
    None
}

fn register_agent(state: &mut State, owner: &Owner, now: u64) {
    state
        .agents
        .entry(owner.key.clone())
        .and_modify(|agent| {
            agent.last_seen_ms = now;
            agent.reason = owner.reason.clone();
        })
        .or_insert_with(|| AgentRecord {
            kind: owner.kind.clone(),
            session_id: owner.session_id.clone(),
            turn_id: owner.turn_id.clone(),
            agent_id: owner.agent_id.clone(),
            agent_type: owner.agent_type.clone(),
            reason: owner.reason.clone(),
            first_seen_ms: now,
            last_seen_ms: now,
        });
}

pub fn acquire(
    cwd: &Path,
    paths: &[PathBuf],
    owner: &Owner,
    wait: Duration,
) -> Result<(), AcquireError> {
    let grouped = resolve_resources(cwd, paths).map_err(AcquireError::Internal)?;
    if grouped.is_empty() {
        return Ok(());
    }

    let started = Instant::now();
    loop {
        let now = now_ms();
        let mut states = Vec::with_capacity(grouped.len());
        for root in grouped.keys() {
            states.push(LockedState::open(root).map_err(AcquireError::Internal)?);
        }

        let mut conflict = None;
        for state in &mut states {
            if cleanup_stale(&mut state.state, now) {
                state.dirty = true;
            }
            let requested = grouped.get(&state.root).expect("grouped project root");
            if let Some(found) = find_conflict(&state.state, requested, owner) {
                conflict = Some(found);
                break;
            }
        }

        if let Some(found) = conflict {
            drop(states);
            if started.elapsed() >= wait {
                return Err(AcquireError::Conflict(found));
            }
            thread::sleep(POLL_INTERVAL);
            continue;
        }

        let expires_ms = now.saturating_add(owner.ttl_ms);
        for state in &mut states {
            register_agent(&mut state.state, owner, now);
            let requested = grouped.get(&state.root).expect("grouped project root");
            for resource in requested {
                state.state.files.insert(
                    resource.clone(),
                    Claim {
                        owner: owner.key.clone(),
                        lease: owner.lease.clone(),
                        mode: owner.mode,
                        acquired_ms: now,
                        expires_ms,
                    },
                );
            }
            state.dirty = true;
            state.flush().map_err(AcquireError::Internal)?;
        }
        return Ok(());
    }
}

pub fn release_lease(cwd: &Path, paths: &[PathBuf], lease: &str) -> Result<usize, String> {
    let grouped = resolve_resources(cwd, paths)?;
    let mut removed = 0;

    for (root, resources) in grouped {
        let mut state = LockedState::open(&root)?;
        let before = state.state.files.len();
        state
            .state
            .files
            .retain(|resource, claim| !(resources.contains(resource) && claim.lease == lease));
        removed += before - state.state.files.len();
        if before != state.state.files.len() {
            cleanup_stale(&mut state.state, now_ms());
            state.dirty = true;
            state.flush()?;
        }
    }
    Ok(removed)
}

pub fn release_agent(cwd: &Path, owner_key: &str) -> Result<usize, String> {
    let Some(root) = find_project_root(cwd) else {
        return Ok(0);
    };
    release_in_root(&root, |claim, _| claim.owner == owner_key)
}

pub fn release_session(cwd: &Path, session_id: &str) -> Result<usize, String> {
    let Some(root) = find_project_root(cwd) else {
        return Ok(0);
    };
    release_in_root(&root, |claim, state| {
        state
            .agents
            .get(&claim.owner)
            .and_then(|agent| agent.session_id.as_deref())
            == Some(session_id)
    })
}

fn release_in_root<F>(root: &Path, predicate: F) -> Result<usize, String>
where
    F: Fn(&Claim, &State) -> bool,
{
    let mut locked = LockedState::open(root)?;
    let keys: Vec<String> = locked
        .state
        .files
        .iter()
        .filter(|(_, claim)| predicate(claim, &locked.state))
        .map(|(resource, _)| resource.clone())
        .collect();
    for key in &keys {
        locked.state.files.remove(key);
    }
    if !keys.is_empty() {
        cleanup_stale(&mut locked.state, now_ms());
        locked.dirty = true;
        locked.flush()?;
    }
    Ok(keys.len())
}

pub fn wait_until_free(cwd: &Path, paths: &[PathBuf], timeout: Duration) -> Result<(), String> {
    let grouped = resolve_resources(cwd, paths)?;
    let started = Instant::now();
    loop {
        let now = now_ms();
        let mut busy = None;
        for (root, resources) in &grouped {
            let mut state = LockedState::open(root)?;
            if cleanup_stale(&mut state.state, now) {
                state.dirty = true;
                state.flush()?;
            }
            for resource in resources {
                if let Some((existing, claim)) = state
                    .state
                    .files
                    .iter()
                    .find(|(existing, _)| resources_overlap(resource, existing))
                {
                    let (label, reason) = owner_label(&state.state, &claim.owner);
                    busy = Some(Conflict {
                        resource: existing.clone(),
                        owner: label,
                        reason,
                    });
                    break;
                }
            }
            if busy.is_some() {
                break;
            }
        }
        if busy.is_none() {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(format!("timed out waiting for {}", busy.unwrap()));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

pub fn status_lines(cwd: &Path) -> Result<Vec<String>, String> {
    let root = find_project_root(cwd).ok_or_else(|| {
        format!(
            "{} is not inside an initialized AgentLatch project",
            cwd.display()
        )
    })?;
    let mut state = LockedState::open(&root)?;
    if cleanup_stale(&mut state.state, now_ms()) {
        state.dirty = true;
        state.flush()?;
    }
    let mut result = Vec::with_capacity(state.state.files.len());
    for (resource, claim) in &state.state.files {
        let (owner, reason) = owner_label(&state.state, &claim.owner);
        result.push(format!(
            "{resource}\t{owner}\t{}\t{reason}",
            match claim.mode {
                ClaimMode::Tool => "tool",
                ClaimMode::Reservation => "reservation",
            }
        ));
    }
    Ok(result)
}

fn parse_option(args: &mut Vec<String>, name: &str) -> Result<Option<String>, String> {
    let Some(index) = args.iter().position(|arg| arg == name) else {
        return Ok(None);
    };
    if index + 1 >= args.len() {
        return Err(format!("{name} requires a value"));
    }
    let value = args.remove(index + 1);
    args.remove(index);
    Ok(Some(value))
}

fn os_args(args: Vec<OsString>) -> Vec<String> {
    args.into_iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect()
}

fn manual_owner(args: &mut Vec<String>) -> Result<Owner, String> {
    let id = parse_option(args, "--owner")?
        .or_else(|| std::env::var("AGENTLATCH_OWNER").ok())
        .ok_or_else(|| "manual locking requires --owner <id> or AGENTLATCH_OWNER".to_string())?;
    let reason = parse_option(args, "--reason")?.unwrap_or_else(|| "manual reservation".into());
    let ttl_seconds = parse_option(args, "--ttl")?
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| "--ttl must be an integer number of seconds".to_string())
        })
        .transpose()?
        .unwrap_or(MANUAL_LEASE_MS / 1000);
    Ok(Owner::manual(id, reason, Duration::from_secs(ttl_seconds)))
}

pub fn run_lock(args: Vec<OsString>) -> Result<(), String> {
    let mut args = os_args(args);
    let owner = manual_owner(&mut args)?;
    let wait_seconds = parse_option(&mut args, "--wait")?
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| "--wait must be an integer number of seconds".to_string())
        })
        .transpose()?
        .unwrap_or(0);
    if args.is_empty() {
        return Err(
            "usage: agentlatch lock --owner <id> [--reason <text>] [--wait <seconds>] <paths...>"
                .into(),
        );
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let paths = args.into_iter().map(PathBuf::from).collect::<Vec<_>>();
    acquire(&cwd, &paths, &owner, Duration::from_secs(wait_seconds))
        .map_err(|error| error.to_string())?;
    println!("locked {} path(s)", paths.len());
    Ok(())
}

pub fn run_unlock(args: Vec<OsString>) -> Result<(), String> {
    let mut args = os_args(args);
    let owner_id = parse_option(&mut args, "--owner")?
        .or_else(|| std::env::var("AGENTLATCH_OWNER").ok())
        .ok_or_else(|| "manual unlocking requires --owner <id> or AGENTLATCH_OWNER".to_string())?;
    let owner_key = format!("manual-{:016x}", stable_hash(owner_id.as_bytes()));
    let mine = args.iter().any(|arg| arg == "--mine");
    if mine {
        args.retain(|arg| arg != "--mine");
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;

    if mine {
        if !args.is_empty() {
            return Err("usage: agentlatch unlock --owner <id> --mine".into());
        }
        let removed = release_agent(&cwd, &owner_key)?;
        println!("released {removed} claim(s)");
        return Ok(());
    }

    if args.is_empty() {
        return Err("usage: agentlatch unlock --owner <id> <paths...>".into());
    }

    let paths = args.into_iter().map(PathBuf::from).collect::<Vec<_>>();
    let grouped = resolve_resources(&cwd, &paths)?;
    let mut removed = 0;
    for (root, resources) in grouped {
        let mut state = LockedState::open(&root)?;
        let before = state.state.files.len();
        state
            .state
            .files
            .retain(|resource, claim| !(resources.contains(resource) && claim.owner == owner_key));
        removed += before - state.state.files.len();
        if before != state.state.files.len() {
            cleanup_stale(&mut state.state, now_ms());
            state.dirty = true;
            state.flush()?;
        }
    }
    println!("released {removed} claim(s)");
    Ok(())
}

pub fn run_wait(args: Vec<OsString>) -> Result<(), String> {
    let mut args = os_args(args);
    let timeout_seconds = parse_option(&mut args, "--timeout")?
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| "--timeout must be an integer number of seconds".to_string())
        })
        .transpose()?
        .unwrap_or(600);
    if args.is_empty() {
        return Err("usage: agentlatch wait [--timeout <seconds>] <paths...>".into());
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let paths = args.into_iter().map(PathBuf::from).collect::<Vec<_>>();
    wait_until_free(&cwd, &paths, Duration::from_secs(timeout_seconds))?;
    println!("requested path(s) are available");
    Ok(())
}

pub fn run_status(args: Vec<OsString>) -> Result<(), String> {
    if !args.is_empty() {
        return Err("usage: agentlatch status".into());
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let lines = status_lines(&cwd)?;
    if lines.is_empty() {
        println!("no active claims");
    } else {
        for line in lines {
            println!("{line}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn fixture(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agentlatch-coordination-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join(STATE_FILE),
            "{\"version\":1,\"agents\":{},\"files\":{}}\n",
        )
        .unwrap();
        fs::write(root.join("a.txt"), "a").unwrap();
        root
    }

    fn owner(id: &str, lease: &str) -> Owner {
        Owner {
            key: id.into(),
            lease: lease.into(),
            kind: "test".into(),
            session_id: None,
            turn_id: None,
            agent_id: None,
            agent_type: None,
            reason: "test".into(),
            mode: ClaimMode::Tool,
            ttl_ms: 10 * 60 * 1000,
        }
    }

    #[test]
    fn conflicting_lease_waits_then_acquires() {
        let root = fixture("wait");
        let path = PathBuf::from("a.txt");
        let first = owner("one", "lease-one");
        acquire(&root, std::slice::from_ref(&path), &first, Duration::ZERO).unwrap();

        let root2 = root.clone();
        let path2 = path.clone();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let result = acquire(
                &root2,
                &[path2],
                &owner("two", "lease-two"),
                Duration::from_secs(2),
            );
            tx.send(result).unwrap();
        });

        thread::sleep(Duration::from_millis(150));
        release_lease(&root, &[path], "lease-one").unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap().is_ok());
        handle.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn same_lease_is_idempotent() {
        let root = fixture("same");
        let path = PathBuf::from("a.txt");
        let who = owner("one", "lease-one");
        acquire(&root, std::slice::from_ref(&path), &who, Duration::ZERO).unwrap();
        acquire(&root, &[path], &who, Duration::ZERO).unwrap();
        assert_eq!(status_lines(&root).unwrap().len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wildcard_and_project_root_claims_are_rejected() {
        let root = fixture("invalid-wide");
        let wildcard = resolve_resources(&root, &[PathBuf::from("*")]).unwrap_err();
        assert!(wildcard.contains("patterns are not allowed"));

        let project_root = resolve_resources(&root, &[root.clone()]).unwrap_err();
        assert!(project_root.contains("does not allow locking an entire project root"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directory_claim_conflicts_with_descendant_file() {
        let root = fixture("directory-recursive");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src").join("nested.rs"), "nested").unwrap();
        acquire(
            &root,
            &[PathBuf::from("src")],
            &owner("one", "lease-one"),
            Duration::ZERO,
        )
        .unwrap();
        let error = acquire(
            &root,
            &[PathBuf::from("src/nested.rs")],
            &owner("two", "lease-two"),
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(matches!(error, AcquireError::Conflict(_)));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn atomic_state_write_replaces_existing_file() {
        let root = fixture("atomic");
        let mut state = read_state(&root).unwrap();
        state.agents.insert(
            "x".into(),
            AgentRecord {
                kind: "test".into(),
                session_id: None,
                turn_id: None,
                agent_id: None,
                agent_type: None,
                reason: "atomic".into(),
                first_seen_ms: 1,
                last_seen_ms: 1,
            },
        );
        write_state_atomic(&root, &state).unwrap();
        let parsed = read_state(&root).unwrap();
        assert!(parsed.agents.contains_key("x"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_claim_is_reclaimed_automatically() {
        let root = fixture("stale");
        let path = PathBuf::from("a.txt");
        let mut who = owner("one", "lease-one");
        who.ttl_ms = 1;
        acquire(&root, &[path], &who, Duration::ZERO).unwrap();
        thread::sleep(Duration::from_millis(5));
        assert!(status_lines(&root).unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }
}
