use crate::{
    platform,
    projection::{
        self, CatalogState, EligibleSession, ProjectionScope, ProjectionStore, ProviderPlanPreview,
        SourceProvider,
    },
    rollout,
};
use chrono::{DateTime, Local, Utc};
#[cfg(test)]
use rusqlite::TransactionBehavior;
use rusqlite::{
    backup::Backup,
    params,
    types::{Value as SqlValue, ValueRef as SqlValueRef},
    Connection, ErrorCode, OpenFlags, OptionalExtension,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, Once,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use walkdir::WalkDir;

#[cfg(test)]
const ALLOWED_PROVIDERS: &[&str] = &["openai", "custom", "codexpilot"];

const AUTOMATIC_BACKUP_LIMIT: usize = 5;
const MINIMUM_AUTOMATIC_BACKUPS: usize = 2;
const BACKUP_CAPACITY_LIMIT_BYTES: u64 = 250 * 1024 * 1024;
const BACKUP_FREE_SPACE_RESERVE_BYTES: u64 = 64 * 1024 * 1024;
const INCOMPLETE_BACKUP_GRACE: Duration = Duration::from_secs(24 * 60 * 60);
/// Process-local reuse window so recovery preview can skip a second full scan
/// immediately after desktop refresh. Apply paths always force a fresh scan.
const SNAPSHOT_CACHE_TTL: Duration = Duration::from_secs(3);

fn canonical_provider(id: &str) -> String {
    source_provider(id)
        .map(|provider| provider.as_str().to_owned())
        .unwrap_or_default()
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSummary {
    pub id: String,
    pub name: String,
    pub color: String,
    pub source_sessions: usize,
    pub currently_visible: usize,
    pub status: String,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SourceSummary {
    pub name: String,
    pub path: String,
    pub records: usize,
    pub readable: bool,
    pub note: String,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LockSummary {
    pub state: String,
    pub path: String,
    pub owner_pid: Option<u32>,
    pub age_seconds: Option<u64>,
    pub active_processes: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ScanResult {
    pub codex_home: String,
    pub current_provider: String,
    pub providers: Vec<ProviderSummary>,
    pub sessions: usize,
    pub discovered_sessions: usize,
    pub orphaned_sessions: usize,
    pub archived_sessions: usize,
    pub ordinary_sessions: usize,
    pub recoverable_sessions: usize,
    pub recoverable_indexed: usize,
    pub session_index_covered: usize,
    pub remote_sessions: usize,
    pub remote_excluded_sessions: usize,
    pub automated_sessions: usize,
    pub rollout_sessions: usize,
    pub valid_rollout_sessions: usize,
    pub indexed: usize,
    pub session_indexed: usize,
    pub drift: usize,
    pub provider_drift: usize,
    pub rollout_provider_drift: usize,
    pub missing_catalog: usize,
    pub missing_rollout: usize,
    pub skipped: usize,
    pub sqlite: usize,
    pub jsonl: usize,
    pub lock: String,
    pub lock_detail: LockSummary,
    pub needs_admin: bool,
    pub last_backup: Option<String>,
    pub pending_operation: Option<PendingOperation>,
    pub sources: Vec<SourceSummary>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BackupKind {
    #[default]
    Automatic,
    Manual,
    RestoreSafety,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BackupEntry {
    pub name: String,
    pub path: String,
    pub created_at: String,
    pub size_bytes: u64,
    pub provider: String,
    pub kind: BackupKind,
    pub pinned: bool,
    pub protected: bool,
    pub protection_reason: Option<String>,
    pub restorable: bool,
    pub status: String,
    pub manifest_version: Option<u32>,
}

#[derive(Debug, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct BackupSummary {
    pub entries: Vec<BackupEntry>,
    pub restorable_count: usize,
    pub automatic_count: usize,
    pub pinned_count: usize,
    pub legacy_count: usize,
    pub incomplete_count: usize,
    pub total_bytes: u64,
    pub legacy_bytes: u64,
    pub automatic_limit: usize,
    pub minimum_automatic: usize,
    pub capacity_limit_bytes: u64,
    pub over_limit: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct BackupCleanupResult {
    pub removed_count: usize,
    pub removed_legacy_count: usize,
    pub reclaimed_bytes: u64,
    pub remaining_count: usize,
    pub remaining_bytes: u64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BackupResult {
    pub path: String,
    pub files: Vec<String>,
    pub manifest: String,
    pub cleanup: Option<BackupCleanupResult>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PendingOperation {
    pub command: String,
    pub backup_path: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<RepairPhase>,
    #[serde(default, skip_serializing)]
    repair_journal: Option<RepairJournal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RepairPhase {
    Prepared,
    Compensating,
    Committed,
    VerificationFailed,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SkipReason {
    pub thread_id: Option<String>,
    pub reason: String,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RepairResult {
    pub changed_threads: usize,
    pub restored_threads: usize,
    pub state_updates: usize,
    pub rollout_updates: usize,
    pub catalog_updates: usize,
    pub catalog_inserts: usize,
    pub catalog_deletes: usize,
    pub workspace_hint_updates: usize,
    pub skipped: usize,
    pub skipped_reasons: Vec<SkipReason>,
    pub dry_run: bool,
    pub verified: bool,
    pub backup_path: Option<String>,
    pub backup_cleanup: Option<BackupCleanupResult>,
    pub plan_token: Option<String>,
    pub lock: String,
    pub needs_admin: bool,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RepairProgressStage {
    Planning,
    AcquiringOperationLock,
    PlanValidated,
    AcquiringWriteFence,
    Backup,
    SqliteStaging,
    MetadataSync,
    Commit,
    Verification,
    Completed,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RepairProgress {
    pub stage: RepairProgressStage,
    pub percent: u8,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
}

struct RepairProgressReporter<'a> {
    callback: &'a mut dyn FnMut(RepairProgress),
    last_percent: u8,
}

impl<'a> RepairProgressReporter<'a> {
    fn new(callback: &'a mut dyn FnMut(RepairProgress)) -> Self {
        Self {
            callback,
            last_percent: 0,
        }
    }

    fn report(&mut self, stage: RepairProgressStage, percent: u8, message: impl Into<String>) {
        self.report_with_counts(stage, percent, message, None);
    }

    fn report_counted(
        &mut self,
        stage: RepairProgressStage,
        percent: u8,
        message: impl Into<String>,
        completed: usize,
        total: usize,
    ) {
        self.report_with_counts(stage, percent, message, Some((completed, total)));
    }

    fn report_optional_count(
        &mut self,
        stage: RepairProgressStage,
        percent: u8,
        message: impl Into<String>,
        completed: usize,
        total: usize,
    ) {
        let counts = (total > 0).then_some((completed, total));
        self.report_with_counts(stage, percent, message, counts);
    }

    fn report_with_counts(
        &mut self,
        stage: RepairProgressStage,
        percent: u8,
        message: impl Into<String>,
        counts: Option<(usize, usize)>,
    ) {
        debug_assert!(percent <= 100);
        debug_assert!(percent >= self.last_percent);
        if let Some((completed, total)) = counts {
            debug_assert!(completed <= total);
        }
        self.last_percent = percent;
        let (completed, total) = counts
            .map(|(completed, total)| (Some(completed), Some(total)))
            .unwrap_or((None, None));
        (self.callback)(RepairProgress {
            stage,
            percent,
            message: message.into(),
            completed,
            total,
        });
    }
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct VerifyResult {
    pub ok: bool,
    pub checked: usize,
    pub remaining: usize,
    pub skipped: usize,
    pub reasons: Vec<SkipReason>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionPreviewResult {
    pub plan: ProviderPlanPreview,
    pub plan_token: String,
    pub changed_threads: usize,
    pub rollout_updates: usize,
    pub source_counts: BTreeMap<String, usize>,
    pub reconcile_pending: usize,
    pub reconcile_conflicts: usize,
    pub reconcile_reasons: Vec<SkipReason>,
    pub workspace_hint_updates: usize,
    pub workspace_conflicts: usize,
    pub workspace_conflict_reasons: Vec<SkipReason>,
    pub skipped: usize,
    pub skipped_reasons: Vec<SkipReason>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LocalSessionSummary {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub rollout_path: String,
    pub project_name: String,
    pub provider: String,
    pub origin_provider: String,
    pub updated_at: i64,
    pub archived: bool,
    pub internal: bool,
    pub visibility: String,
    pub status: String,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DesktopRefreshResult {
    pub scan: ScanResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<ProjectionPreviewResult>,
    pub local_sessions: Vec<LocalSessionSummary>,
    pub blocking_processes: Vec<platform::BlockingProcess>,
    pub selected_sources: Vec<String>,
    pub target_provider: String,
    pub backup_cleanup: Option<BackupCleanupResult>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct ThreadRow {
    id: String,
    rollout_path: String,
    provider: String,
    archived: bool,
    source: String,
    thread_source: String,
    agent_role: Option<String>,
    title: String,
    cwd: String,
    created_at: f64,
    updated_at: f64,
    first_user_message: String,
    has_user_event: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct CatalogRow {
    host_id: String,
    thread_id: String,
    provider: String,
    missing_candidate: bool,
    source_kind: String,
    source_detail: String,
    cwd: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
struct GlobalStateSnapshot {
    readable: bool,
    sha256: Option<String>,
    saved_workspace_roots: Vec<String>,
    thread_workspace_root_hints: HashMap<String, Value>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct Snapshot {
    threads: Vec<ThreadRow>,
    catalog: Vec<CatalogRow>,
    rollouts: HashSet<String>,
    valid_active_rollouts: HashSet<String>,
    valid_archived_rollouts: HashSet<String>,
    rollout_providers: HashMap<String, HashSet<String>>,
    rollout_provider_values: HashMap<String, String>,
    rollout_sources: HashMap<String, Value>,
    rollout_thread_sources: HashMap<String, Value>,
    rollout_cwds: HashMap<String, String>,
    rollout_locality: HashMap<String, Value>,
    rollout_paths: HashMap<String, PathBuf>,
    primary_rollouts: BTreeMap<String, rollout::PrimaryRollout>,
    global_state: GlobalStateSnapshot,
    session_index: HashSet<String>,
    jsonl_files: Vec<PathBuf>,
    sqlite_readable: usize,
    threads_readable: bool,
    catalog_readable: bool,
    sources: Vec<SourceSummary>,
}

#[derive(Debug, Clone, Serialize)]
struct StateUpdate {
    thread_id: String,
    expected_rollout_path: String,
    expected_provider: String,
    expected_archived: bool,
    expected_thread_source: String,
    expected_has_user_event: bool,
    rollout_path: String,
    provider: String,
    archived: bool,
    thread_source: String,
    has_user_event: bool,
}

#[derive(Debug, Clone, Serialize)]
struct StateInsert {
    thread_id: String,
    rollout_path: String,
    archived: bool,
    created_at: i64,
    updated_at: i64,
    source: String,
    thread_source: String,
    provider: String,
    cwd: String,
    title: String,
    sandbox_policy: String,
    approval_mode: String,
    cli_version: String,
    first_user_message: String,
    preview: String,
    has_user_event: bool,
    git_sha: Option<String>,
    git_branch: Option<String>,
    git_origin_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct StateDelete {
    thread_id: String,
    expected_provider: String,
    expected_rollout_path: String,
}

#[derive(Debug, Clone, Serialize)]
struct RolloutUpdate {
    thread_id: String,
    path: PathBuf,
    archived: bool,
    expected_provider: Option<String>,
    provider: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CatalogUpdate {
    host_id: String,
    thread_id: String,
    expected_provider: String,
    expected_missing_candidate: bool,
    provider: String,
    missing_candidate: bool,
}

#[derive(Debug, Clone, Serialize)]
struct CatalogDelete {
    host_id: String,
    thread_id: String,
    expected_provider: String,
    expected_missing_candidate: bool,
}

#[derive(Debug, Clone, Serialize)]
struct CatalogInsert {
    host_id: String,
    thread_id: String,
    title: String,
    created_at: f64,
    updated_at: f64,
    cwd: String,
    source_detail: String,
    source_kind: String,
    provider: String,
    git_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceHintUpdate {
    thread_id: String,
    expected_hint: Option<Value>,
    workspace_root: String,
}

#[derive(Debug, Clone)]
struct RepairPlan {
    state_updates: Vec<StateUpdate>,
    state_restores: Vec<StateUpdate>,
    state_inserts: Vec<StateInsert>,
    state_deletes: Vec<StateDelete>,
    rollout_updates: Vec<RolloutUpdate>,
    catalog_updates: Vec<CatalogUpdate>,
    catalog_inserts: Vec<CatalogInsert>,
    catalog_deletes: Vec<CatalogDelete>,
    workspace_hint_updates: Vec<WorkspaceHintUpdate>,
    expected_global_state_sha256: Option<String>,
    changed_ids: HashSet<String>,
    skipped: Vec<SkipReason>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepairStateJournal {
    thread_id: String,
    before_provider: String,
    after_provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    row_images: Option<RepairStateRowImages>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepairStateRowImages {
    before: Option<RepairStateImage>,
    after: Option<RepairStateImage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepairStateImage {
    values: BTreeMap<String, RepairSqlValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "camelCase")]
enum RepairSqlValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepairRolloutJournal {
    thread_id: String,
    path: String,
    archived: bool,
    #[serde(default)]
    before_provider: Option<String>,
    #[serde(default)]
    after_provider: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepairCatalogImage {
    host_id: String,
    thread_id: String,
    display_title: String,
    source_created_at: f64,
    source_updated_at: f64,
    cwd: String,
    source_kind: String,
    source_detail: Option<String>,
    model_provider: String,
    git_branch: Option<String>,
    observation_sequence: i64,
    missing_candidate: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepairCatalogJournal {
    host_id: String,
    thread_id: String,
    before: Option<RepairCatalogImage>,
    after: Option<RepairCatalogImage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "camelCase")]
enum RepairJsonSlot {
    Missing,
    Present(Value),
}

impl RepairJsonSlot {
    fn from_option(value: Option<Value>) -> Self {
        match value {
            Some(value) => Self::Present(value),
            None => Self::Missing,
        }
    }

    fn to_option(&self) -> Option<Value> {
        match self {
            Self::Missing => None,
            Self::Present(value) => Some(value.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepairWorkspaceJournal {
    thread_id: String,
    before: RepairJsonSlot,
    after: RepairJsonSlot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepairCatalogWatermarkJournal {
    before_observation_sequence: i64,
    after_observation_sequence: i64,
    before_catalog_revision: i64,
    after_catalog_revision: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepairJournal {
    version: u32,
    target_provider: String,
    state_rows: Vec<RepairStateJournal>,
    #[serde(default)]
    rollout_rows: Vec<RepairRolloutJournal>,
    catalog_rows: Vec<RepairCatalogJournal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    catalog_watermarks: Option<RepairCatalogWatermarkJournal>,
    workspace_hints: Vec<RepairWorkspaceJournal>,
    projection_before: Option<ProjectionStore>,
    projection_after: ProjectionStore,
}

#[derive(Debug, Default)]
struct SessionCohorts {
    local_catalog_ids: HashSet<String>,
    remote_catalog_ids: HashSet<String>,
    remote_session_ids: HashSet<String>,
    remote_excluded_thread_ids: HashSet<String>,
    ordinary_active_ids: HashSet<String>,
    recoverable_ids: HashSet<String>,
    recoverable_indexed_ids: HashSet<String>,
    session_index_covered_ids: HashSet<String>,
    missing_rollout_ids: HashSet<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ManifestFile {
    path: String,
    size: u64,
    modified: Option<String>,
    sha256: Option<String>,
    backed_up: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct BackupManifest {
    version: u32,
    created_at: String,
    source: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    sqlite_user_versions: BTreeMap<String, i64>,
    #[serde(default)]
    projection_state_present: bool,
    #[serde(default)]
    projection_state_sha256: Option<String>,
    #[serde(default)]
    rollout_provider_preimages: Vec<RepairRolloutJournal>,
    #[serde(default)]
    kind: BackupKind,
    #[serde(default)]
    pinned: bool,
    files: Vec<ManifestFile>,
}

fn normalize_provider(value: &str) -> String {
    match value {
        "OpenAI" => "openai".into(),
        value => value.into(),
    }
}

fn provider_id_is_safe(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn source_provider(value: &str) -> Option<SourceProvider> {
    if !provider_id_is_safe(value) {
        return None;
    }
    Some(SourceProvider::from_id(value.to_owned()))
}

fn provider_name(id: &str) -> String {
    match id {
        "openai" => "OpenAI".into(),
        "codexpilot" => "CodexPilot".into(),
        "custom" => "Custom".into(),
        _ => id.into(),
    }
}

fn provider_color(id: &str) -> &'static str {
    match id {
        "openai" => "#4779a7",
        "codexpilot" => "#b17842",
        "custom" => "#2d7b6f",
        _ => {
            const COLORS: [&str; 6] = [
                "#3f7c85", "#6d6b9a", "#8a6748", "#587a52", "#8a5c72", "#52708c",
            ];
            let hash = id.bytes().fold(0usize, |hash, byte| {
                hash.wrapping_mul(31).wrapping_add(byte as usize)
            });
            COLORS[hash % COLORS.len()]
        }
    }
}

pub fn default_codex_home() -> PathBuf {
    if let Ok(value) = std::env::var("CODEX_HOME") {
        return PathBuf::from(value);
    }
    #[cfg(windows)]
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
    #[cfg(not(windows))]
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".codex")
}

pub fn validate_provider(provider: &str) -> Result<String, String> {
    source_provider(provider)
        .map(|provider| provider.as_str().to_owned())
        .ok_or_else(|| format!("invalid provider id: {provider}"))
}

fn configured_current_provider(home: &Path) -> Option<String> {
    fs::read_to_string(home.join("config.toml"))
        .ok()
        .and_then(|content| content.parse::<toml::Value>().ok())
        .and_then(|value| {
            value
                .get("model_provider")
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
        })
        .and_then(|value| validate_provider(&value).ok())
}

fn current_provider(home: &Path) -> String {
    configured_current_provider(home).unwrap_or_else(|| "unknown".into())
}

fn configured_providers(home: &Path) -> BTreeSet<String> {
    let Some(config) = fs::read_to_string(home.join("config.toml"))
        .ok()
        .and_then(|content| content.parse::<toml::Value>().ok())
    else {
        return BTreeSet::new();
    };
    config
        .get("model_providers")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|table| table.keys())
        .filter_map(|provider| validate_provider(provider).ok())
        .collect()
}

fn validate_current_target_provider(home: &Path, provider: &str) -> Result<String, String> {
    let provider = validate_provider(provider)?;
    let current = configured_current_provider(home)
        .ok_or_else(|| "config.toml has no valid current model_provider".to_string())?;
    if provider != current {
        return Err(format!(
            "target provider must match config.toml model_provider (current: {current})"
        ));
    }
    Ok(provider)
}

fn acquire_target_provider_guard(home: &Path, expected: &str) -> Result<File, String> {
    let path = home.join("config.toml");
    let mut file = platform::open_provider_config_guard(&path)?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let provider = content
        .parse::<toml::Value>()
        .ok()
        .and_then(|value| {
            value
                .get("model_provider")
                .and_then(toml::Value::as_str)
                .and_then(|provider| validate_provider(provider).ok())
        })
        .ok_or_else(|| format!("{} has no valid model_provider", path.display()))?;
    if provider != expected {
        return Err(format!(
            "target provider changed before repair (expected: {expected}, current: {provider})"
        ));
    }
    Ok(file)
}

fn table_columns(connection: &Connection, table: &str) -> Result<HashSet<String>, String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| error.to_string())?;
    let mut columns = HashSet::new();
    for row in rows {
        columns.insert(row.map_err(|error| error.to_string())?);
    }
    Ok(columns)
}

#[derive(Debug, Clone)]
struct TableColumnInfo {
    name: String,
    not_null: bool,
    default_value: Option<String>,
}

fn table_column_info(connection: &Connection, table: &str) -> Result<Vec<TableColumnInfo>, String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(TableColumnInfo {
                name: row.get(1)?,
                not_null: row.get::<_, i64>(3)? != 0,
                default_value: row.get(4)?,
            })
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

fn state_insert_supports_column(name: &str) -> bool {
    matches!(
        name,
        "id" | "rollout_path"
            | "created_at"
            | "created_at_ms"
            | "updated_at"
            | "updated_at_ms"
            | "recency_at"
            | "recency_at_ms"
            | "source"
            | "thread_source"
            | "model_provider"
            | "cwd"
            | "title"
            | "preview"
            | "sandbox_policy"
            | "approval_mode"
            | "tokens_used"
            | "has_user_event"
            | "archived"
            | "git_sha"
            | "git_branch"
            | "git_origin_url"
            | "cli_version"
            | "first_user_message"
            | "agent_nickname"
            | "agent_role"
            | "memory_mode"
    )
}

fn validate_state_insert_schema(connection: &Connection) -> Result<(), String> {
    let columns = table_column_info(connection, "threads")?;
    let names = columns
        .iter()
        .map(|column| column.name.as_str())
        .collect::<HashSet<_>>();
    for required in [
        "id",
        "rollout_path",
        "source",
        "model_provider",
        "cwd",
        "title",
        "archived",
    ] {
        if !names.contains(required) {
            return Err(format!(
                "unsupported threads insert schema: missing {required}"
            ));
        }
    }
    for column in columns {
        if column.not_null
            && column.default_value.is_none()
            && !state_insert_supports_column(&column.name)
        {
            return Err(format!(
                "unsupported threads insert schema: required column {} has no safe value",
                column.name
            ));
        }
    }
    Ok(())
}

fn select_expr(columns: &HashSet<String>, name: &str, fallback: &str) -> String {
    if columns.contains(name) {
        format!("\"{name}\"")
    } else {
        fallback.to_string()
    }
}

fn select_time_expr(columns: &HashSet<String>, millis: &str, seconds: &str) -> String {
    match (columns.contains(millis), columns.contains(seconds)) {
        (true, true) => format!("COALESCE(\"{millis}\", \"{seconds}\", 0)"),
        (true, false) => format!("COALESCE(\"{millis}\", 0)"),
        (false, true) => format!("COALESCE(\"{seconds}\", 0)"),
        (false, false) => "0".into(),
    }
}

static SNAPSHOT_COUNTER: AtomicU64 = AtomicU64::new(0);
static SNAPSHOT_CLEANUP: Once = Once::new();

const LIGHT_SQLITE_BUSY_RETRIES: usize = 8;
const LIGHT_SQLITE_BUSY_SLEEP: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SqliteReadMode {
    /// Fast list/scan path: open live DB read-only, no copy, no quick_check.
    Light,
    /// Validation/repair path: copy DB (+sidecars) to temp and optionally quick_check.
    Safe,
}

#[derive(Clone, PartialEq, Eq)]
struct FileFingerprint {
    length: u64,
    modified: SystemTime,
}

struct SnapshotConnection {
    connection: Option<Connection>,
    cleanup_directory: Option<PathBuf>,
}

impl std::ops::Deref for SnapshotConnection {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.connection
            .as_ref()
            .expect("snapshot connection already closed")
    }
}

impl Drop for SnapshotConnection {
    fn drop(&mut self) {
        self.connection.take();
        if let Some(directory) = self.cleanup_directory.take() {
            let _ = fs::remove_dir_all(directory);
        }
    }
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn file_fingerprint(path: &Path) -> Result<Option<FileFingerprint>, String> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(FileFingerprint {
            length: metadata.len(),
            modified: metadata.modified().map_err(|error| error.to_string())?,
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn is_sqlite_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn format_sqlite_open_error(path: &Path, error: rusqlite::Error) -> String {
    if is_sqlite_busy(&error) {
        format!(
            "database is busy while opening {} ({error})",
            path.display()
        )
    } else {
        format!("{}: {error}", path.display())
    }
}

fn open_readonly(path: &Path) -> Result<SnapshotConnection, String> {
    open_sqlite_readonly(path, SqliteReadMode::Safe)
}

fn open_sqlite_readonly(path: &Path, mode: SqliteReadMode) -> Result<SnapshotConnection, String> {
    match mode {
        SqliteReadMode::Light => open_sqlite_light(path),
        SqliteReadMode::Safe => open_sqlite_safe_snapshot(path),
    }
}

fn sqlite_path_uri(path: &Path, query: &str) -> String {
    // SQLite URI paths use forward slashes; Windows drive letters become file:///C:/...
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let raw = absolute.to_string_lossy().replace('\\', "/");
    let with_slash = if raw.starts_with('/') {
        raw
    } else {
        format!("/{raw}")
    };
    let mut encoded = String::with_capacity(with_slash.len() + 16);
    for ch in with_slash.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '/' | ':' | '-' | '_' | '.' | '~' => {
                encoded.push(ch)
            }
            _ => {
                for byte in ch.to_string().into_bytes() {
                    encoded.push_str(&format!("%{byte:02X}"));
                }
            }
        }
    }
    format!("file:{encoded}?{query}")
}

fn open_sqlite_light(path: &Path) -> Result<SnapshotConnection, String> {
    let metadata = fs::metadata(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => format!("SQLite file not found: {}", path.display()),
        _ => format!("{}: {error}", path.display()),
    })?;
    if !metadata.is_file() {
        return Err(format!("SQLite file not found: {}", path.display()));
    }

    // When no -wal exists, immutable=1 avoids creating -wal/-shm on a pure
    // list/scan open. When a WAL is already present (Codex or our own writer),
    // open with mode=ro only so we still observe recent commits for verify.
    let has_wal = sidecar_path(path, "-wal").is_file();
    let uri = if has_wal {
        sqlite_path_uri(path, "mode=ro")
    } else {
        sqlite_path_uri(path, "mode=ro&immutable=1")
    };
    let mut last_error = format!("database is busy while opening {}", path.display());
    for attempt in 0..LIGHT_SQLITE_BUSY_RETRIES {
        match Connection::open_with_flags(
            &uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(connection) => {
                let _ = connection.pragma_update(None, "query_only", true);
                return Ok(SnapshotConnection {
                    connection: Some(connection),
                    cleanup_directory: None,
                });
            }
            Err(error) if is_sqlite_busy(&error) => {
                last_error = format_sqlite_open_error(path, error);
                if attempt + 1 < LIGHT_SQLITE_BUSY_RETRIES {
                    std::thread::sleep(LIGHT_SQLITE_BUSY_SLEEP);
                }
            }
            Err(error) => return Err(format_sqlite_open_error(path, error)),
        }
    }
    Err(last_error)
}

fn open_sqlite_safe_snapshot(path: &Path) -> Result<SnapshotConnection, String> {
    let snapshot_root = std::env::temp_dir().join("codex-provider-hub-readonly");
    fs::create_dir_all(&snapshot_root).map_err(|error| error.to_string())?;
    SNAPSHOT_CLEANUP.call_once(|| {
        let Ok(entries) = fs::read_dir(&snapshot_root) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let stale = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_some_and(|age| age > Duration::from_secs(3600));
            if stale {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    });
    let mut last_error = "SQLite changed while creating a read-only snapshot".to_string();
    for _ in 0..4 {
        let nonce = SNAPSHOT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let directory = snapshot_root.join(format!(
            "{}-{}-{}",
            std::process::id(),
            Local::now().timestamp_millis(),
            nonce
        ));
        fs::create_dir(&directory).map_err(|error| error.to_string())?;
        let destination = directory.join("snapshot.sqlite");
        let sources = [
            (path.to_path_buf(), destination.clone()),
            (
                sidecar_path(path, "-wal"),
                sidecar_path(&destination, "-wal"),
            ),
            (
                sidecar_path(path, "-journal"),
                sidecar_path(&destination, "-journal"),
            ),
        ];
        let copy_result = (|| {
            let before = sources
                .iter()
                .map(|(source, _)| file_fingerprint(source))
                .collect::<Result<Vec<_>, _>>()?;
            if before.first().is_none_or(Option::is_none) {
                return Err(format!("SQLite file not found: {}", path.display()));
            }
            for ((source, target), fingerprint) in sources.iter().zip(&before) {
                if fingerprint.is_some() {
                    fs::copy(source, target)
                        .map_err(|error| format!("{}: {error}", source.display()))?;
                }
            }
            let after = sources
                .iter()
                .map(|(source, _)| file_fingerprint(source))
                .collect::<Result<Vec<_>, _>>()?;
            if before != after {
                return Err(format!(
                    "SQLite changed while snapshotting: {}",
                    path.display()
                ));
            }
            Connection::open(&destination).map_err(|error| error.to_string())
        })();
        match copy_result {
            Ok(connection) => {
                return Ok(SnapshotConnection {
                    connection: Some(connection),
                    cleanup_directory: Some(directory),
                });
            }
            Err(error) => {
                last_error = error;
                let _ = fs::remove_dir_all(&directory);
            }
        }
    }
    Err(last_error)
}

fn sqlite_quick_check(connection: &Connection) -> Result<(), String> {
    let result = connection
        .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    if result == "ok" {
        Ok(())
    } else {
        Err(format!("SQLite quick_check failed: {result}"))
    }
}

fn sqlite_user_version(path: &Path) -> Option<i64> {
    let connection = open_sqlite_readonly(path, SqliteReadMode::Light).ok()?;
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .ok()
}

fn validate_repair_schema(home: &Path) -> Result<(), String> {
    let path = home.join("state_5.sqlite");
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        format!(
            "required SQLite is unavailable ({}): {error}",
            path.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("unsafe state SQLite path: {}", path.display()));
    }
    let connection = open_readonly(&path)?;
    validate_state_repair_schema(&connection)
}

fn validate_state_repair_schema(state: &Connection) -> Result<(), String> {
    sqlite_quick_check(state)?;
    let columns = table_columns(state, "threads")?;
    for required in [
        "id",
        "rollout_path",
        "model_provider",
        "archived",
        "source",
        "thread_source",
        "has_user_event",
    ] {
        if !columns.contains(required) {
            return Err(format!("unsupported threads schema: missing {required}"));
        }
    }
    Ok(())
}

fn ensure_home_sqlite_paths(home: &Path) -> Result<(), String> {
    let canonical_home = fs::canonicalize(home)
        .map_err(|error| format!("CODEX_HOME is unavailable ({}): {error}", home.display()))?;
    for relative in ["state_5.sqlite", "sqlite/codex-dev.db"] {
        let path = home.join(relative);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "required SQLite is unavailable ({}): {error}",
                path.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!("SQLite path is a symlink: {}", path.display()));
        }
        if !metadata.is_file() {
            return Err(format!(
                "SQLite path is not a regular file: {}",
                path.display()
            ));
        }
        let canonical = fs::canonicalize(&path).map_err(|error| error.to_string())?;
        if !canonical.starts_with(&canonical_home) {
            return Err(format!(
                "SQLite path escapes CODEX_HOME: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn validate_repair_schema_files(state_path: &Path, catalog_path: &Path) -> Result<(), String> {
    let state = open_readonly(state_path)?;
    let catalog = open_readonly(catalog_path)?;
    validate_repair_schema_connections(&state, &catalog)
}

fn validate_repair_schema_connections(
    state: &Connection,
    catalog: &Connection,
) -> Result<(), String> {
    validate_state_repair_schema(state)?;
    sqlite_quick_check(catalog)?;
    let catalog_columns = table_columns(catalog, "local_thread_catalog")?;
    for required in [
        "host_id",
        "thread_id",
        "display_title",
        "source_created_at",
        "source_updated_at",
        "cwd",
        "source_kind",
        "source_detail",
        "model_provider",
        "git_branch",
        "observation_sequence",
        "missing_candidate",
    ] {
        if !catalog_columns.contains(required) {
            return Err(format!(
                "unsupported local_thread_catalog schema: missing {required}"
            ));
        }
    }
    for (table, required) in [
        (
            "local_thread_catalog_sync_state",
            vec!["host_id", "observation_sequence"],
        ),
        (
            "local_thread_catalog_metadata",
            vec!["id", "catalog_revision"],
        ),
    ] {
        let columns = table_columns(catalog, table)?;
        for column in required {
            if !columns.contains(column) {
                return Err(format!(
                    "unsupported catalog schema: {table}.{column} missing"
                ));
            }
        }
    }
    Ok(())
}

fn read_threads(home: &Path) -> (Vec<ThreadRow>, SourceSummary) {
    let path = home.join("state_5.sqlite");
    let base = SourceSummary {
        name: "threads".into(),
        path: path.to_string_lossy().to_string(),
        records: 0,
        readable: false,
        note: String::new(),
    };
    if let Err(note) = match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err("not a regular file".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err("file not found".to_string())
        }
        Err(error) => Err(error.to_string()),
    } {
        return (Vec::new(), SourceSummary { note, ..base });
    }
    let connection = match open_sqlite_readonly(&path, SqliteReadMode::Light) {
        Ok(connection) => connection,
        Err(error) => {
            return (
                Vec::new(),
                SourceSummary {
                    note: error,
                    ..base
                },
            )
        }
    };
    let Ok(columns) = table_columns(&connection, "threads") else {
        return (
            Vec::new(),
            SourceSummary {
                note: "table schema unreadable".into(),
                ..base
            },
        );
    };
    if !columns.contains("id") || !columns.contains("model_provider") {
        return (
            Vec::new(),
            SourceSummary {
                note: "required columns missing".into(),
                ..base
            },
        );
    }
    let sql = format!(
        "SELECT id, model_provider, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {} FROM threads",
        select_expr(&columns, "rollout_path", "''"),
        select_expr(&columns, "archived", "0"),
        select_expr(&columns, "source", "''"),
        select_expr(&columns, "thread_source", "''"),
        select_expr(&columns, "agent_role", "NULL"),
        select_expr(&columns, "title", "''"),
        select_expr(&columns, "cwd", "''"),
        select_time_expr(&columns, "created_at_ms", "created_at"),
        select_time_expr(&columns, "updated_at_ms", "updated_at"),
        select_expr(&columns, "first_user_message", "''"),
        select_expr(&columns, "has_user_event", "0"),
    );
    let Ok(mut statement) = connection.prepare(&sql) else {
        return (
            Vec::new(),
            SourceSummary {
                note: "query failed".into(),
                ..base
            },
        );
    };
    let rows = statement.query_map([], |row| {
        Ok(ThreadRow {
            id: row.get(0)?,
            provider: row.get(1)?,
            rollout_path: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            archived: row.get::<_, Option<i64>>(3)?.unwrap_or_default() != 0,
            source: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            thread_source: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
            agent_role: row.get::<_, Option<String>>(6)?,
            title: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
            cwd: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
            created_at: row.get::<_, Option<f64>>(9)?.unwrap_or_default(),
            updated_at: row.get::<_, Option<f64>>(10)?.unwrap_or_default(),
            first_user_message: row.get::<_, Option<String>>(11)?.unwrap_or_default(),
            has_user_event: row.get::<_, Option<i64>>(12)?.unwrap_or_default() != 0,
        })
    });
    let Ok(rows) = rows else {
        return (
            Vec::new(),
            SourceSummary {
                note: "row read failed".into(),
                ..base
            },
        );
    };
    let mut threads = Vec::new();
    for row in rows {
        match row {
            Ok(value) => threads.push(value),
            Err(error) => {
                return (
                    Vec::new(),
                    SourceSummary {
                        note: format!("row read failed: {error}"),
                        ..base
                    },
                );
            }
        }
    }
    (
        threads.clone(),
        SourceSummary {
            records: threads.len(),
            readable: true,
            note: "read-only".into(),
            ..base
        },
    )
}

fn read_catalog(home: &Path) -> (Vec<CatalogRow>, SourceSummary) {
    let path = home.join("sqlite/codex-dev.db");
    let base = SourceSummary {
        name: "local_thread_catalog".into(),
        path: path.to_string_lossy().to_string(),
        records: 0,
        readable: false,
        note: String::new(),
    };
    if let Err(note) = match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err("not a regular file".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err("file not found".to_string())
        }
        Err(error) => Err(error.to_string()),
    } {
        return (Vec::new(), SourceSummary { note, ..base });
    }
    let connection = match open_sqlite_readonly(&path, SqliteReadMode::Light) {
        Ok(connection) => connection,
        Err(error) => {
            return (
                Vec::new(),
                SourceSummary {
                    note: error,
                    ..base
                },
            )
        }
    };
    let Ok(columns) = table_columns(&connection, "local_thread_catalog") else {
        return (
            Vec::new(),
            SourceSummary {
                note: "table schema unreadable".into(),
                ..base
            },
        );
    };
    let required = ["host_id", "thread_id", "model_provider"];
    if required.iter().any(|column| !columns.contains(*column)) {
        return (
            Vec::new(),
            SourceSummary {
                note: "required columns missing".into(),
                ..base
            },
        );
    }
    let sql = format!(
        "SELECT host_id, thread_id, model_provider, {}, {}, {}, {} FROM local_thread_catalog",
        select_expr(&columns, "missing_candidate", "0"),
        select_expr(&columns, "source_kind", "''"),
        select_expr(&columns, "source_detail", "''"),
        select_expr(&columns, "cwd", "''"),
    );
    let Ok(mut statement) = connection.prepare(&sql) else {
        return (
            Vec::new(),
            SourceSummary {
                note: "query failed".into(),
                ..base
            },
        );
    };
    let rows = statement.query_map([], |row| {
        Ok(CatalogRow {
            host_id: row.get(0)?,
            thread_id: row.get(1)?,
            provider: row.get(2)?,
            missing_candidate: row.get::<_, Option<i64>>(3)?.unwrap_or_default() != 0,
            source_kind: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            source_detail: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
            cwd: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
        })
    });
    let Ok(rows) = rows else {
        return (
            Vec::new(),
            SourceSummary {
                note: "row read failed".into(),
                ..base
            },
        );
    };
    let mut catalog = Vec::new();
    for row in rows {
        match row {
            Ok(value) => catalog.push(value),
            Err(error) => {
                return (
                    Vec::new(),
                    SourceSummary {
                        note: format!("row read failed: {error}"),
                        ..base
                    },
                );
            }
        }
    }
    let local_rows = catalog
        .iter()
        .filter(|row| catalog_row_is_local(row))
        .count();
    let remote_rows = catalog
        .iter()
        .filter(|row| catalog_row_is_remote(row))
        .count();
    (
        catalog.clone(),
        SourceSummary {
            records: catalog.len(),
            readable: true,
            note: format!("read-only; {local_rows} local rows; {remote_rows} non-local rows"),
            ..base
        },
    )
}

struct RolloutRead {
    ids: HashSet<String>,
    valid_active_ids: HashSet<String>,
    valid_archived_ids: HashSet<String>,
    providers: HashMap<String, HashSet<String>>,
    provider_values: HashMap<String, String>,
    sources: HashMap<String, Value>,
    thread_sources: HashMap<String, Value>,
    cwds: HashMap<String, String>,
    locality: HashMap<String, Value>,
    paths: HashMap<String, PathBuf>,
    primary_rollouts: BTreeMap<String, rollout::PrimaryRollout>,
    files: Vec<PathBuf>,
    file_count: usize,
    issues: HashMap<&'static str, usize>,
}

fn read_rollouts(home: &Path) -> RolloutRead {
    let inventory = rollout::scan_rollouts(home);
    let mut rollouts = HashSet::new();
    let mut rollout_providers: HashMap<String, HashSet<String>> = HashMap::new();
    let mut rollout_provider_values = HashMap::new();
    let mut rollout_sources = HashMap::new();
    let mut rollout_thread_sources = HashMap::new();
    let mut rollout_cwds = HashMap::new();
    let mut rollout_locality = HashMap::new();
    let mut rollout_paths = HashMap::new();
    let mut primary_rollouts = BTreeMap::new();
    let mut valid_active_ids = HashSet::new();
    let mut valid_archived_ids = HashSet::new();
    let mut files = Vec::with_capacity(inventory.rollouts.len());
    // Move PrimaryRollout into primary_rollouts; only clone fields needed by
    // parallel lookup maps (avoid full-struct clone of message/preview/git).
    for (id, primary) in inventory.rollouts {
        rollouts.insert(id.clone());
        rollout_paths.insert(id.clone(), primary.path.clone());
        files.push(primary.path.clone());
        if primary.archived {
            valid_archived_ids.insert(id.clone());
        } else {
            valid_active_ids.insert(id.clone());
        }
        if let Some(source) = primary.source.as_ref() {
            rollout_sources.insert(id.clone(), source.clone());
        }
        if let Some(thread_source) = primary.thread_source.as_ref() {
            rollout_thread_sources.insert(id.clone(), thread_source.clone());
        }
        if let Some(cwd) = primary.cwd.as_ref() {
            rollout_cwds.insert(id.clone(), cwd.clone());
        }
        rollout_locality.insert(id.clone(), primary.locality.clone());
        if let Some(raw_provider) = primary.model_provider.as_ref() {
            if let Ok(provider) = validate_provider(raw_provider) {
                rollout_provider_values.insert(id.clone(), raw_provider.clone());
                rollout_providers
                    .entry(id.clone())
                    .or_default()
                    .insert(provider);
            }
        }
        primary_rollouts.insert(id, primary);
    }
    let mut issues = HashMap::new();
    for issue in inventory.issues {
        *issues.entry(issue.code).or_default() += 1;
    }
    RolloutRead {
        ids: rollouts,
        valid_active_ids,
        valid_archived_ids,
        providers: rollout_providers,
        provider_values: rollout_provider_values,
        sources: rollout_sources,
        thread_sources: rollout_thread_sources,
        cwds: rollout_cwds,
        locality: rollout_locality,
        paths: rollout_paths,
        primary_rollouts,
        files,
        file_count: inventory.file_count,
        issues,
    }
}

fn read_session_index(home: &Path) -> (HashSet<String>, SourceSummary) {
    let path = home.join("session_index.jsonl");
    let base = SourceSummary {
        name: "session_index".into(),
        path: path.to_string_lossy().to_string(),
        records: 0,
        readable: false,
        note: String::new(),
    };
    if let Err(note) = match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err("not a regular file".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err("file not found".to_string())
        }
        Err(error) => Err(error.to_string()),
    } {
        return (HashSet::new(), SourceSummary { note, ..base });
    }
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) => {
            return (
                HashSet::new(),
                SourceSummary {
                    note: error.to_string(),
                    ..base
                },
            )
        }
    };
    let mut ids = HashSet::new();
    let mut malformed = 0;
    for line in BufReader::new(file).lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                return (
                    HashSet::new(),
                    SourceSummary {
                        note: format!("line read failed: {error}"),
                        ..base
                    },
                )
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let id = serde_json::from_str::<Value>(line.trim_start_matches('\u{feff}'))
            .ok()
            .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_owned));
        if let Some(id) = id {
            ids.insert(id);
        } else {
            malformed += 1;
        }
    }
    (
        ids.clone(),
        SourceSummary {
            records: ids.len(),
            readable: true,
            note: format!("index read-only; {malformed} malformed lines"),
            ..base
        },
    )
}

#[allow(dead_code)]
fn source_metadata(thread: &ThreadRow) -> Option<(String, String)> {
    let raw = thread.source.trim();
    let parsed = serde_json::from_str::<Value>(raw).ok();
    let (kind, detail) = match parsed {
        Some(Value::String(value)) => (value, String::new()),
        Some(Value::Object(object)) => {
            if let Some(custom) = object.get("custom").and_then(Value::as_str) {
                ("custom".into(), custom.into())
            } else if let Some(kind) = object
                .get("kind")
                .or_else(|| object.get("type"))
                .and_then(Value::as_str)
            {
                (
                    kind.into(),
                    object
                        .get("detail")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                )
            } else {
                return None;
            }
        }
        _ if !raw.is_empty() => (raw.into(), String::new()),
        _ => (thread.thread_source.clone(), String::new()),
    };
    let normalized = kind.trim().to_ascii_lowercase();
    let canonical = match normalized.as_str() {
        "cli" => "cli",
        "vscode" => "vscode",
        "appserver" | "app_server" => "appServer",
        "custom" => "custom",
        "user" => "user",
        _ => return None,
    };
    Some((canonical.into(), detail))
}

fn catalog_time(value: f64) -> f64 {
    if value.abs() > 10_000_000_000.0 {
        value / 1000.0
    } else {
        value
    }
}

fn catalog_title(thread: &ThreadRow) -> String {
    let raw = if !thread.title.trim().is_empty() {
        thread.title.as_str()
    } else if !thread.cwd.trim().is_empty() {
        thread.cwd.as_str()
    } else {
        thread.id.as_str()
    };
    let compact = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= 80 {
        compact
    } else {
        format!("{}...", compact.chars().take(77).collect::<String>())
    }
}

fn sqlite_text_value(value: Option<&Value>, fallback: &str) -> String {
    match value {
        Some(Value::String(value)) if !value.trim().is_empty() => value.clone(),
        Some(value) => serde_json::to_string(value).unwrap_or_else(|_| fallback.to_string()),
        None => fallback.to_string(),
    }
}

fn rollout_timestamp_seconds(primary: &rollout::PrimaryRollout) -> i64 {
    primary
        .session_timestamp
        .as_deref()
        .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
        .map(|timestamp| timestamp.timestamp())
        .or_else(|| {
            primary
                .updated_at_fallback
                .and_then(|updated| updated.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        })
        .unwrap_or_default()
}

fn rollout_updated_at_seconds(primary: &rollout::PrimaryRollout) -> i64 {
    primary
        .updated_at_fallback
        .and_then(|updated| updated.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or_else(|| rollout_timestamp_seconds(primary))
}

fn rollout_title(primary: &rollout::PrimaryRollout) -> String {
    let title = primary
        .title
        .as_deref()
        .or(primary.first_user_message.as_deref())
        .or(primary.preview.as_deref())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(primary.id.as_str());
    title.chars().take(120).collect()
}

fn rollout_thread_row(primary: &rollout::PrimaryRollout) -> ThreadRow {
    ThreadRow {
        id: primary.id.clone(),
        rollout_path: primary.path.to_string_lossy().to_string(),
        provider: primary.model_provider.clone().unwrap_or_default(),
        archived: primary.archived,
        source: sqlite_text_value(primary.source.as_ref(), "cli"),
        thread_source: sqlite_text_value(primary.thread_source.as_ref(), "user"),
        agent_role: None,
        title: rollout_title(primary),
        cwd: primary.cwd.clone().unwrap_or_default(),
        created_at: rollout_timestamp_seconds(primary) as f64,
        updated_at: rollout_updated_at_seconds(primary) as f64,
        first_user_message: primary.first_user_message.clone().unwrap_or_default(),
        has_user_event: primary.first_user_message.is_some(),
    }
}

fn rollout_backed_threads(snapshot: &Snapshot) -> Vec<(ThreadRow, bool)> {
    let state_by_id = snapshot
        .threads
        .iter()
        .map(|thread| (thread.id.as_str(), thread))
        .collect::<HashMap<_, _>>();
    let mut ids = snapshot
        .valid_active_rollouts
        .iter()
        .chain(snapshot.valid_archived_rollouts.iter())
        .map(String::as_str)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.into_iter()
        .filter_map(|id| {
            state_by_id
                .get(id)
                .map(|thread| ((*thread).clone(), true))
                .or_else(|| {
                    snapshot
                        .primary_rollouts
                        .get(id)
                        .map(|primary| (rollout_thread_row(primary), false))
                })
        })
        .collect()
}

fn state_insert_from_rollout(
    snapshot: &Snapshot,
    thread: &ThreadRow,
    target_provider: &str,
) -> Option<StateInsert> {
    let primary = snapshot.primary_rollouts.get(&thread.id)?;
    let git = primary.git.as_ref();
    Some(StateInsert {
        thread_id: thread.id.clone(),
        rollout_path: primary.path.to_string_lossy().to_string(),
        archived: primary.archived,
        created_at: rollout_timestamp_seconds(primary),
        updated_at: rollout_updated_at_seconds(primary),
        source: sqlite_text_value(primary.source.as_ref(), "cli"),
        thread_source: sqlite_text_value(primary.thread_source.as_ref(), "user"),
        provider: canonical_provider(target_provider),
        cwd: primary.cwd.clone().unwrap_or_default(),
        title: rollout_title(primary),
        sandbox_policy: sqlite_text_value(
            primary.sandbox_policy.as_ref(),
            r#"{"type":"read-only"}"#,
        ),
        approval_mode: primary
            .approval_mode
            .clone()
            .unwrap_or_else(|| "on-request".into()),
        cli_version: primary.cli_version.clone().unwrap_or_default(),
        first_user_message: primary.first_user_message.clone().unwrap_or_default(),
        preview: primary
            .preview
            .clone()
            .or_else(|| primary.first_user_message.clone())
            .or_else(|| primary.title.clone())
            .unwrap_or_else(|| primary.id.clone())
            .chars()
            .take(240)
            .collect(),
        has_user_event: primary.first_user_message.is_some(),
        git_sha: git.and_then(|git| git.commit_hash.clone()),
        git_branch: git.and_then(|git| git.branch.clone()),
        git_origin_url: git.and_then(|git| git.repository_url.clone()),
    })
}

fn project_name(cwd: &str) -> String {
    let cwd = cwd.trim().trim_end_matches(['\\', '/']);
    cwd.rsplit(['\\', '/'])
        .find(|part| !part.trim().is_empty())
        .unwrap_or("Unknown project")
        .to_string()
}

#[cfg_attr(not(test), allow(dead_code))]
fn local_session_summaries(
    snapshot: &Snapshot,
    store: Option<&ProjectionStore>,
    current_provider: &str,
) -> Vec<LocalSessionSummary> {
    local_session_summaries_with_cohorts(
        snapshot,
        store,
        current_provider,
        &session_cohorts(snapshot),
    )
}

fn local_session_summaries_with_cohorts(
    snapshot: &Snapshot,
    store: Option<&ProjectionStore>,
    current_provider: &str,
    cohorts: &SessionCohorts,
) -> Vec<LocalSessionSummary> {
    let target_provider = canonical_provider(current_provider);
    let mut sessions = Vec::new();

    for (thread, state_present) in rollout_backed_threads(snapshot) {
        if cohorts.remote_excluded_thread_ids.contains(&thread.id) {
            continue;
        }

        let provider = normalize_provider(&thread.provider);
        let rollout_provider = snapshot
            .rollout_provider_values
            .get(&thread.id)
            .map(String::as_str);
        let archived = snapshot
            .primary_rollouts
            .get(&thread.id)
            .is_some_and(|primary| primary.archived);
        let provider_aligned = state_present
            && thread.provider == target_provider
            && rollout_provider == Some(target_provider.as_str())
            && thread.archived == archived;
        let visible = provider_aligned && !archived;
        let status = if provider_aligned && archived {
            "archived"
        } else if visible {
            "visible"
        } else {
            "recoverable"
        };
        let origin_provider = store
            .and_then(|store| store.threads.get(&thread.id))
            .map(|record| record.origin_provider.as_str().to_string())
            .or_else(|| {
                snapshot
                    .rollout_provider_values
                    .get(&thread.id)
                    .and_then(|provider| source_provider(provider))
                    .map(|provider| provider.as_str().to_string())
            })
            .unwrap_or_else(|| provider.clone());

        sessions.push(LocalSessionSummary {
            id: thread.id.clone(),
            title: catalog_title(&thread),
            cwd: thread.cwd.clone(),
            rollout_path: snapshot
                .rollout_paths
                .get(&thread.id)
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_default(),
            project_name: project_name(&thread.cwd),
            provider,
            origin_provider,
            updated_at: catalog_time(thread.updated_at).round() as i64,
            archived,
            internal: is_skipped_thread(&thread).is_some()
                || rollout_source_not_ordinary(snapshot, &thread.id),
            visibility: if visible { "visible" } else { "hidden" }.into(),
            status: status.into(),
        });
    }

    sessions.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.id.cmp(&right.id))
    });
    sessions
}

#[derive(Clone, PartialEq, Eq)]
struct HomeScanFingerprint {
    signals: Vec<(String, u64, u64)>,
}

struct CachedSnapshot {
    home: PathBuf,
    fingerprint: HomeScanFingerprint,
    captured_at: Instant,
    snapshot: Snapshot,
}

static SNAPSHOT_CACHE: Mutex<Option<CachedSnapshot>> = Mutex::new(None);

fn system_time_as_millis(value: SystemTime) -> u64 {
    value
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn push_path_fingerprint(signals: &mut Vec<(String, u64, u64)>, label: &str, path: &Path) {
    match fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .map(system_time_as_millis)
                .unwrap_or(0);
            signals.push((label.to_owned(), metadata.len(), modified));
        }
        Err(_) => {
            signals.push((label.to_owned(), 0, 0));
        }
    }
}

fn push_jsonl_tree_fingerprint(signals: &mut Vec<(String, u64, u64)>, home: &Path, relative_root: &str) {
    let root = home.join(relative_root);
    if !root.is_dir() {
        signals.push((format!("{relative_root}/"), 0, 0));
        return;
    }
    let mut files = Vec::new();
    for entry in WalkDir::new(&root).follow_links(false) {
        let Ok(entry) = entry else {
            signals.push((format!("{relative_root}/!walk"), 1, 0));
            continue;
        };
        if !entry.file_type().is_file() {
            continue;
        }
        if entry
            .path()
            .extension()
            .and_then(|value| value.to_str())
            .is_none_or(|value| !value.eq_ignore_ascii_case("jsonl"))
        {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(home)
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| entry.path().to_string_lossy().replace('\\', "/"));
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                files.push((relative, 0u64, 0u64));
                continue;
            }
        };
        let modified = metadata
            .modified()
            .map(system_time_as_millis)
            .unwrap_or(0);
        files.push((relative, metadata.len(), modified));
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    signals.push((format!("{relative_root}/#count"), files.len() as u64, 0));
    for (relative, len, modified) in files {
        signals.push((relative, len, modified));
    }
}

fn home_scan_fingerprint(home: &Path) -> HomeScanFingerprint {
    let mut signals = Vec::new();
    for relative in [
        "state_5.sqlite",
        "state_5.sqlite-wal",
        "state_5.sqlite-shm",
        "sqlite/codex-dev.db",
        "sqlite/codex-dev.db-wal",
        "sqlite/codex-dev.db-shm",
        "session_index.jsonl",
        ".codex-global-state.json",
        "config.toml",
    ] {
        push_path_fingerprint(&mut signals, relative, &home.join(relative));
    }
    // Projection store can change eligibility without touching SQLite/jsonl.
    push_path_fingerprint(
        &mut signals,
        "projection-state.json",
        &projection_state_path(home),
    );
    push_jsonl_tree_fingerprint(&mut signals, home, "sessions");
    push_jsonl_tree_fingerprint(&mut signals, home, "archived_sessions");
    HomeScanFingerprint { signals }
}

/// Common-path scan with a short process-local cache (refresh → preview reuse).
fn scan_snapshot(home: &Path) -> Snapshot {
    scan_snapshot_with_cache(home, true)
}

/// Force a disk rescan and refresh the short cache. Used by apply / write paths.
fn scan_snapshot_fresh(home: &Path) -> Snapshot {
    scan_snapshot_with_cache(home, false)
}

fn scan_snapshot_with_cache(home: &Path, allow_cache: bool) -> Snapshot {
    let fingerprint = home_scan_fingerprint(home);
    if allow_cache {
        if let Ok(guard) = SNAPSHOT_CACHE.lock() {
            if let Some(cached) = guard.as_ref() {
                if cached.home == home
                    && cached.fingerprint == fingerprint
                    && cached.captured_at.elapsed() < SNAPSHOT_CACHE_TTL
                {
                    return cached.snapshot.clone();
                }
            }
        }
    }
    let snapshot = scan_snapshot_uncached(home);
    if let Ok(mut guard) = SNAPSHOT_CACHE.lock() {
        *guard = Some(CachedSnapshot {
            home: home.to_path_buf(),
            fingerprint,
            captured_at: Instant::now(),
            snapshot: snapshot.clone(),
        });
    }
    snapshot
}

fn invalidate_snapshot_cache() {
    if let Ok(mut guard) = SNAPSHOT_CACHE.lock() {
        *guard = None;
    }
}

fn scan_snapshot_uncached(home: &Path) -> Snapshot {
    let (
        (threads, thread_source),
        (catalog, catalog_source),
        rollout_read,
        (session_index, session_index_source),
        global_state,
    ) = std::thread::scope(|scope| {
        let threads = scope.spawn(|| read_threads(home));
        let catalog = scope.spawn(|| read_catalog(home));
        let rollouts = scope.spawn(|| read_rollouts(home));
        let session_index = scope.spawn(|| read_session_index(home));
        let global_state = scope.spawn(|| read_global_state_snapshot(home));
        (
            threads.join().expect("thread SQLite scan panicked"),
            catalog.join().expect("catalog SQLite scan panicked"),
            rollouts.join().expect("rollout scan panicked"),
            session_index.join().expect("session index scan panicked"),
            global_state.join().expect("global state scan panicked"),
        )
    });
    let sqlite_readable =
        usize::from(thread_source.readable) + usize::from(catalog_source.readable);
    let rollout_count = rollout_read.ids.len();
    let valid_rollout_count =
        rollout_read.valid_active_ids.len() + rollout_read.valid_archived_ids.len();
    let rollout_provider_count = rollout_read.providers.len();
    let jsonl_count = rollout_read.file_count;
    let rollout_issue_count: usize = rollout_read.issues.values().sum();
    let traversal_errors = rollout_read.issues.get("walk_error").copied().unwrap_or(0);
    let issue_summary = if rollout_read.issues.is_empty() {
        "no metadata issues".to_string()
    } else {
        let mut entries = rollout_read
            .issues
            .iter()
            .map(|(code, count)| format!("{code}={count}"))
            .collect::<Vec<_>>();
        entries.sort();
        entries.join(", ")
    };
    Snapshot {
        threads,
        catalog,
        rollouts: rollout_read.ids,
        valid_active_rollouts: rollout_read.valid_active_ids,
        valid_archived_rollouts: rollout_read.valid_archived_ids,
        rollout_providers: rollout_read.providers,
        rollout_provider_values: rollout_read.provider_values,
        rollout_sources: rollout_read.sources,
        rollout_thread_sources: rollout_read.thread_sources,
        rollout_cwds: rollout_read.cwds,
        rollout_locality: rollout_read.locality,
        rollout_paths: rollout_read.paths,
        primary_rollouts: rollout_read.primary_rollouts,
        global_state,
        session_index,
        jsonl_files: rollout_read.files,
        sqlite_readable,
        threads_readable: thread_source.readable,
        catalog_readable: catalog_source.readable,
        sources: vec![
            thread_source,
            catalog_source,
            SourceSummary {
                name: "rollouts".into(),
                path: home.join("sessions").to_string_lossy().to_string(),
                records: rollout_count,
                readable: (home.join("sessions").is_dir()
                    || home.join("archived_sessions").is_dir())
                    && traversal_errors == 0,
                note: format!(
                    "{jsonl_count} valid primary records; {valid_rollout_count} unique valid rollouts; provider metadata for {rollout_provider_count} IDs; {rollout_issue_count} issues ({issue_summary}); bounded first-record scan; read-only",
                ),
            },
            session_index_source,
        ],
    }
}

fn global_state_path(home: &Path) -> PathBuf {
    home.join(".codex-global-state.json")
}

fn read_global_state_snapshot(home: &Path) -> GlobalStateSnapshot {
    let path = global_state_path(home);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
        _ => return GlobalStateSnapshot::default(),
    };
    if metadata.len() == 0 {
        return GlobalStateSnapshot::default();
    }
    let Ok(bytes) = fs::read(&path) else {
        return GlobalStateSnapshot::default();
    };
    global_state_snapshot_from_bytes(&bytes)
}

fn global_state_snapshot_from_bytes(bytes: &[u8]) -> GlobalStateSnapshot {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return GlobalStateSnapshot::default();
    };
    let Some(object) = value.as_object() else {
        return GlobalStateSnapshot::default();
    };
    let saved_workspace_roots = object
        .get("electron-saved-workspace-roots")
        .and_then(Value::as_array)
        .map(|roots| {
            roots
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let thread_workspace_root_hints = object
        .get("thread-workspace-root-hints")
        .and_then(Value::as_object)
        .map(|hints| {
            hints
                .iter()
                .map(|(thread_id, hint)| (thread_id.clone(), hint.clone()))
                .collect()
        })
        .unwrap_or_default();
    GlobalStateSnapshot {
        readable: true,
        sha256: Some(hash_bytes(bytes)),
        saved_workspace_roots,
        thread_workspace_root_hints,
    }
}

fn global_state_source(home: &Path) -> SourceSummary {
    let path = global_state_path(home);
    let base = SourceSummary {
        name: "global_state".into(),
        path: path.to_string_lossy().to_string(),
        records: 0,
        readable: false,
        note: String::new(),
    };
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return SourceSummary {
                note: error.to_string(),
                ..base
            }
        }
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return SourceSummary {
            note: "invalid JSON; read-only check failed".into(),
            ..base
        };
    };
    let records = value.as_object().map_or(1, |object| object.len());
    SourceSummary {
        records,
        readable: true,
        note: "workspace roots are read for identity checks; only deterministic thread hints may be updated"
            .into(),
        ..base
    }
}

fn is_skipped_thread(thread: &ThreadRow) -> Option<String> {
    if thread
        .agent_role
        .as_deref()
        .is_some_and(|role| !role.trim().is_empty())
        || source_value_is_nonordinary(&parse_source_value(&thread.source))
        || source_value_is_nonordinary(&parse_source_value(&thread.thread_source))
    {
        return Some("subagent_or_automation".into());
    }
    None
}

fn parse_source_value(value: &str) -> Value {
    serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.trim().into()))
}

fn source_value_is_nonordinary(value: &Value) -> bool {
    fn token(value: &str) -> bool {
        matches!(
            value
                .trim()
                .to_ascii_lowercase()
                .replace(['-', '_'], "")
                .as_str(),
            "subagent"
                | "automation"
                | "automated"
                | "scheduled"
                | "exec"
                | "threadspawn"
                | "guardian"
                | "pullrequestfixautomation"
        )
    }
    match value {
        Value::String(value) => token(value),
        Value::Object(object) => object.iter().any(|(key, value)| {
            let key = key.to_ascii_lowercase().replace(['-', '_'], "");
            (matches!(
                key.as_str(),
                "subagent" | "automation" | "threadspawn" | "parentthreadid" | "ephemeral"
            ) && !matches!(value, Value::Null | Value::Bool(false)))
                || (matches!(key.as_str(), "kind" | "type" | "sourcekind")
                    && value.as_str().is_some_and(token))
                || matches!(value, Value::Object(_) | Value::Array(_))
                    && source_value_is_nonordinary(value)
        }),
        Value::Array(values) => values.iter().any(source_value_is_nonordinary),
        _ => false,
    }
}

fn has_explicit_remote_marker(value: &str) -> bool {
    fn remote_token(value: &str) -> bool {
        let value = value.trim().to_ascii_lowercase().replace(' ', "");
        matches!(
            value.as_str(),
            "remote"
                | "ssh"
                | "ssh-remote"
                | "wsl"
                | "devcontainer"
                | "dev-container"
                | "codespaces"
        ) || [
            "ssh-remote+",
            "wsl+",
            "dev-container+",
            "devcontainer+",
            "codespaces+",
            "vscode-remote://",
            "ssh://",
            r"\\wsl$\",
            r"\\wsl.localhost\",
        ]
        .iter()
        .any(|prefix| value.starts_with(prefix))
    }
    fn json_marker(value: &Value) -> bool {
        match value {
            Value::Object(object) => object.iter().any(|(key, value)| {
                let key = key.to_ascii_lowercase().replace(['-', '_'], "");
                if key == "remote" && value.as_bool() == Some(true) {
                    return true;
                }
                if matches!(key.as_str(), "remoteauthority" | "hostid")
                    && value.as_str().is_some_and(|value| {
                        !value.trim().is_empty() && !value.eq_ignore_ascii_case("local")
                    })
                {
                    return true;
                }
                if matches!(key.as_str(), "kind" | "type" | "sourcekind")
                    && value.as_str().is_some_and(remote_token)
                {
                    return true;
                }
                if matches!(key.as_str(), "source" | "workspaceuri")
                    && value.as_str().is_some_and(remote_token)
                {
                    return true;
                }
                matches!(value, Value::Object(_) | Value::Array(_)) && json_marker(value)
            }),
            Value::Array(values) => values.iter().any(json_marker),
            Value::String(value) => remote_token(value),
            _ => false,
        }
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(value) {
        return json_marker(&parsed);
    }
    remote_token(value)
}

fn thread_is_explicit_remote(thread: &ThreadRow) -> bool {
    has_explicit_remote_marker(&thread.source) || has_explicit_remote_marker(&thread.thread_source)
}

fn catalog_row_is_remote(row: &CatalogRow) -> bool {
    let kind = row.source_kind.trim().to_ascii_lowercase();
    !row.host_id.eq_ignore_ascii_case("local")
        || matches!(
            kind.as_str(),
            "remote"
                | "ssh"
                | "ssh-remote"
                | "wsl"
                | "devcontainer"
                | "dev-container"
                | "codespaces"
        )
        || has_explicit_remote_marker(&row.source_detail)
}

fn catalog_row_is_local(row: &CatalogRow) -> bool {
    row.host_id.eq_ignore_ascii_case("local") && !catalog_row_is_remote(row)
}

#[allow(dead_code)]
fn normalized_local_windows_path(value: &str) -> Option<String> {
    let mut path = value.trim().replace('/', "\\");
    if let Some(stripped) = path.strip_prefix(r"\\?\") {
        path = stripped.to_string();
    }
    let bytes = path.as_bytes();
    (bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\')
        .then(|| path.to_ascii_lowercase())
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum WorkspaceResolution {
    Stable,
    HintUpdate(WorkspaceHintUpdate),
    Conflict,
}

#[allow(dead_code)]
fn unique_saved_workspace_root(
    global_state: &GlobalStateSnapshot,
    state_cwd: &str,
) -> Option<String> {
    if !global_state.readable {
        return None;
    }
    let state_cwd = normalized_local_windows_path(state_cwd)?;
    let mut matches = BTreeMap::new();
    for root in &global_state.saved_workspace_roots {
        if !Path::new(root).is_dir() {
            continue;
        }
        let Some(normalized_root) = normalized_local_windows_path(root) else {
            continue;
        };
        if state_cwd == normalized_root
            || state_cwd
                .strip_prefix(&normalized_root)
                .is_some_and(|suffix| suffix.starts_with('\\'))
        {
            matches
                .entry(normalized_root)
                .or_insert_with(|| root.clone());
        }
    }
    (matches.len() == 1).then(|| matches.into_values().next().expect("one workspace root"))
}

#[allow(dead_code)]
fn workspace_resolution(snapshot: &Snapshot, thread: &ThreadRow) -> WorkspaceResolution {
    let Some(state_cwd) = normalized_local_windows_path(&thread.cwd) else {
        return WorkspaceResolution::Conflict;
    };
    let Some(rollout_cwd) = snapshot
        .rollout_cwds
        .get(&thread.id)
        .and_then(|cwd| normalized_local_windows_path(cwd))
    else {
        return WorkspaceResolution::Conflict;
    };
    let local_rows = snapshot
        .catalog
        .iter()
        .filter(|row| row.thread_id == thread.id && catalog_row_is_local(row))
        .collect::<Vec<_>>();
    if local_rows.len() > 1 {
        return WorkspaceResolution::Conflict;
    }
    if let Some(row) = local_rows.first() {
        if normalized_local_windows_path(&row.cwd).as_deref() != Some(state_cwd.as_str()) {
            return WorkspaceResolution::Conflict;
        }
    }
    let expected_hint = snapshot
        .global_state
        .thread_workspace_root_hints
        .get(&thread.id)
        .cloned();
    if state_cwd == rollout_cwd {
        return match expected_hint.as_ref() {
            None | Some(Value::Null) => WorkspaceResolution::Stable,
            Some(Value::String(hint)) if hint.trim().is_empty() => WorkspaceResolution::Stable,
            Some(Value::String(hint)) => {
                let compatible = normalized_local_windows_path(hint).is_some_and(|root| {
                    state_cwd == root
                        || state_cwd
                            .strip_prefix(&root)
                            .is_some_and(|suffix| suffix.starts_with('\\'))
                });
                if compatible {
                    WorkspaceResolution::Stable
                } else {
                    WorkspaceResolution::Conflict
                }
            }
            Some(_) => WorkspaceResolution::Conflict,
        };
    }
    if local_rows.is_empty() {
        return WorkspaceResolution::Conflict;
    }
    let Some(workspace_root) = unique_saved_workspace_root(&snapshot.global_state, &thread.cwd)
    else {
        return WorkspaceResolution::Conflict;
    };
    match expected_hint.as_ref() {
        Some(Value::String(hint)) if !hint.trim().is_empty() => {
            if normalized_local_windows_path(hint) == normalized_local_windows_path(&workspace_root)
            {
                WorkspaceResolution::Stable
            } else {
                WorkspaceResolution::Conflict
            }
        }
        None | Some(Value::Null) | Some(Value::String(_)) => {
            WorkspaceResolution::HintUpdate(WorkspaceHintUpdate {
                thread_id: thread.id.clone(),
                expected_hint,
                workspace_root,
            })
        }
        Some(_) => WorkspaceResolution::Conflict,
    }
}

fn rollout_source_not_ordinary(snapshot: &Snapshot, thread_id: &str) -> bool {
    snapshot
        .rollout_sources
        .get(thread_id)
        .is_some_and(source_value_is_nonordinary)
        || snapshot
            .rollout_thread_sources
            .get(thread_id)
            .is_some_and(source_value_is_nonordinary)
}

fn session_cohorts(snapshot: &Snapshot) -> SessionCohorts {
    let local_catalog_ids: HashSet<_> = snapshot
        .catalog
        .iter()
        .filter(|row| catalog_row_is_local(row) && !row.missing_candidate)
        .map(|row| row.thread_id.clone())
        .collect();
    let remote_catalog_ids: HashSet<_> = snapshot
        .catalog
        .iter()
        .filter(|row| catalog_row_is_remote(row))
        .map(|row| row.thread_id.clone())
        .collect();
    let mut explicit_remote_ids = snapshot
        .threads
        .iter()
        .filter(|thread| thread_is_explicit_remote(thread))
        .map(|thread| thread.id.clone())
        .collect::<HashSet<_>>();
    explicit_remote_ids.extend(
        snapshot
            .valid_active_rollouts
            .iter()
            .chain(snapshot.valid_archived_rollouts.iter())
            .filter(|thread_id| {
                snapshot
                    .rollout_locality
                    .get(*thread_id)
                    .is_some_and(|source| has_explicit_remote_marker(&source.to_string()))
            })
            .cloned(),
    );
    let mut remote_session_ids = remote_catalog_ids.clone();
    remote_session_ids.extend(explicit_remote_ids.iter().cloned());
    // The MVP is deliberately local-only. A remote marker wins even when the same
    // thread also has a local catalog row, because origin is otherwise ambiguous.
    let remote_excluded_thread_ids = remote_session_ids.clone();
    let ordinary_active_ids = rollout_backed_threads(snapshot)
        .into_iter()
        .filter(|(thread, _)| !remote_excluded_thread_ids.contains(&thread.id))
        .map(|(thread, _)| thread.id)
        .collect::<HashSet<_>>();
    let missing_rollout_ids = snapshot
        .threads
        .iter()
        .filter(|thread| {
            !remote_excluded_thread_ids.contains(&thread.id)
                && !snapshot.valid_active_rollouts.contains(&thread.id)
                && !snapshot.valid_archived_rollouts.contains(&thread.id)
        })
        .map(|thread| thread.id.clone())
        .collect::<HashSet<_>>();
    let recoverable_ids = ordinary_active_ids
        .iter()
        .filter(|id| {
            snapshot.rollout_paths.contains_key(*id)
                && (snapshot.rollout_provider_values.contains_key(*id)
                    || snapshot
                        .primary_rollouts
                        .get(*id)
                        .is_some_and(|primary| primary.provider_field_missing))
        })
        .cloned()
        .collect::<HashSet<_>>();
    let recoverable_indexed_ids = recoverable_ids
        .intersection(&local_catalog_ids)
        .cloned()
        .collect();
    let session_index_covered_ids = recoverable_ids
        .intersection(&snapshot.session_index)
        .cloned()
        .collect();
    SessionCohorts {
        local_catalog_ids,
        remote_catalog_ids,
        remote_session_ids,
        remote_excluded_thread_ids,
        ordinary_active_ids,
        recoverable_ids,
        recoverable_indexed_ids,
        session_index_covered_ids,
        missing_rollout_ids,
    }
}

fn projection_state_path(home: &Path) -> PathBuf {
    home.join("backups/provider-hub/projection-state.json")
}

fn pending_operation_path(home: &Path) -> PathBuf {
    home.join("backups/provider-hub/pending-operation.json")
}

fn load_pending_operation(home: &Path) -> Result<Option<PendingOperation>, String> {
    let path = pending_operation_path(home);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| format!("invalid pending operation ({}): {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "cannot read pending operation ({}): {error}",
            path.display()
        )),
    }
}

fn save_pending_operation(
    home: &Path,
    command: &str,
    backup_path: &Path,
) -> Result<PendingOperation, String> {
    let pending = PendingOperation {
        command: command.into(),
        backup_path: backup_path.to_string_lossy().to_string(),
        created_at: Local::now().to_rfc3339(),
        phase: None,
        repair_journal: None,
    };
    write_pending_operation(home, &pending)?;
    Ok(pending)
}

fn write_pending_operation(home: &Path, pending: &PendingOperation) -> Result<(), String> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct PersistedPendingOperation<'a> {
        command: &'a str,
        backup_path: &'a str,
        created_at: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        phase: Option<RepairPhase>,
        #[serde(skip_serializing_if = "Option::is_none")]
        repair_journal: &'a Option<RepairJournal>,
    }

    let root = ensure_backup_root(home, true)?;
    let path = root.join("pending-operation.json");
    let temporary = root.join(format!(
        ".pending-operation-{}-{}.tmp",
        std::process::id(),
        Local::now().format("%Y%m%d%H%M%S%3f")
    ));
    let persisted = PersistedPendingOperation {
        command: &pending.command,
        backup_path: &pending.backup_path,
        created_at: &pending.created_at,
        phase: pending.phase,
        repair_journal: &pending.repair_journal,
    };
    let result = (|| {
        let mut file = File::create(&temporary).map_err(|error| error.to_string())?;
        file.write_all(&serde_json::to_vec_pretty(&persisted).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        platform::atomic_replace_file(&temporary, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn clear_pending_operation(home: &Path) -> Result<(), String> {
    let path = pending_operation_path(home);
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn load_projection_store(home: &Path) -> Result<Option<ProjectionStore>, String> {
    let path = projection_state_path(home);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| format!("invalid projection state ({}): {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "cannot read projection state ({}): {error}",
            path.display()
        )),
    }
}

fn save_projection_store(home: &Path, store: &ProjectionStore) -> Result<(), String> {
    let root = ensure_backup_root(home, true)?;
    let path = root.join("projection-state.json");
    let temporary = root.join(format!(
        ".projection-state-{}-{}.tmp",
        std::process::id(),
        Local::now().format("%Y%m%d%H%M%S%3f")
    ));
    let bytes = serde_json::to_vec_pretty(store).map_err(|error| error.to_string())?;
    let result = (|| {
        let mut file = File::create(&temporary).map_err(|error| error.to_string())?;
        file.write_all(&bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        platform::atomic_replace_file(&temporary, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn read_regular_global_state(home: &Path) -> Result<Vec<u8>, String> {
    let path = global_state_path(home);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("global state is unavailable ({}): {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "global state is not a regular file: {}",
            path.display()
        ));
    }
    let canonical_home = fs::canonicalize(home).map_err(|error| error.to_string())?;
    let canonical_path = fs::canonicalize(&path).map_err(|error| error.to_string())?;
    if !canonical_path.starts_with(canonical_home) {
        return Err(format!(
            "global state escapes CODEX_HOME: {}",
            path.display()
        ));
    }
    let bytes = fs::read(&path).map_err(|error| error.to_string())?;
    serde_json::from_slice::<Value>(&bytes)
        .map_err(|error| format!("invalid global state JSON: {error}"))?;
    Ok(bytes)
}

struct ExclusiveGlobalState {
    file: File,
    original_bytes: Vec<u8>,
}

impl ExclusiveGlobalState {
    fn acquire(home: &Path, expected_sha256: Option<&str>) -> Result<Self, String> {
        let path = global_state_path(home);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!("global state is unavailable ({}): {error}", path.display())
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "global state is not a regular file: {}",
                path.display()
            ));
        }
        let canonical_home = fs::canonicalize(home).map_err(|error| error.to_string())?;
        let canonical_path = fs::canonicalize(&path).map_err(|error| error.to_string())?;
        if !canonical_path.starts_with(canonical_home) {
            return Err(format!(
                "global state escapes CODEX_HOME: {}",
                path.display()
            ));
        }
        let mut file = platform::open_exclusive_file(&path)?;
        let mut original_bytes = Vec::new();
        file.read_to_end(&mut original_bytes)
            .map_err(|error| error.to_string())?;
        serde_json::from_slice::<Value>(&original_bytes)
            .map_err(|error| format!("invalid global state JSON: {error}"))?;
        if expected_sha256.is_some_and(|expected| hash_bytes(&original_bytes) != expected) {
            return Err("global state changed after planning; refresh the preview".into());
        }
        Ok(Self {
            file,
            original_bytes,
        })
    }

    fn bytes(&self) -> &[u8] {
        &self.original_bytes
    }

    fn overwrite(&mut self, bytes: &[u8]) -> Result<(), String> {
        serde_json::from_slice::<Value>(bytes)
            .map_err(|error| format!("refusing to write invalid global state JSON: {error}"))?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|error| error.to_string())?;
        self.file.set_len(0).map_err(|error| error.to_string())?;
        self.file
            .write_all(bytes)
            .map_err(|error| error.to_string())?;
        self.file.sync_all().map_err(|error| error.to_string())?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|error| error.to_string())?;
        let mut verified = Vec::new();
        self.file
            .read_to_end(&mut verified)
            .map_err(|error| error.to_string())?;
        if hash_bytes(&verified) != hash_bytes(bytes) {
            return Err("global state write verification failed".into());
        }
        Ok(())
    }

    fn apply_workspace_hint_updates(&mut self, plan: &RepairPlan) -> Result<usize, String> {
        if plan.workspace_hint_updates.is_empty() {
            return Ok(0);
        }
        let expected_sha256 = plan
            .expected_global_state_sha256
            .as_deref()
            .ok_or("workspace hint plan is missing the global-state fingerprint")?;
        if hash_bytes(&self.original_bytes) != expected_sha256 {
            return Err("global state changed after planning; refresh the preview".into());
        }
        let mut value: Value =
            serde_json::from_slice(&self.original_bytes).map_err(|error| error.to_string())?;
        let object = value
            .as_object_mut()
            .ok_or("global state root is not a JSON object")?;
        let hints = object
            .entry("thread-workspace-root-hints")
            .or_insert_with(|| Value::Object(Default::default()))
            .as_object_mut()
            .ok_or("thread-workspace-root-hints is not a JSON object")?;
        for update in &plan.workspace_hint_updates {
            let current = hints.get(&update.thread_id).cloned();
            if current != update.expected_hint {
                return Err(format!(
                    "workspace hint changed after planning: {}",
                    update.thread_id
                ));
            }
            hints.insert(
                update.thread_id.clone(),
                Value::String(update.workspace_root.clone()),
            );
        }
        let updated = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
        self.overwrite(&updated)?;
        Ok(plan.workspace_hint_updates.len())
    }
}

fn write_global_state_exclusive(home: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut guard = ExclusiveGlobalState::acquire(home, None)?;
    guard.overwrite(bytes)
}

#[cfg(test)]
fn apply_workspace_hint_updates(home: &Path, plan: &RepairPlan) -> Result<usize, String> {
    if plan.workspace_hint_updates.is_empty() {
        return Ok(0);
    }
    let mut guard =
        ExclusiveGlobalState::acquire(home, plan.expected_global_state_sha256.as_deref())?;
    guard.apply_workspace_hint_updates(plan)
}

fn next_projection_store(
    existing: Option<&ProjectionStore>,
    preview: &ProviderPlanPreview,
    sessions: &[EligibleSession],
    snapshot: &Snapshot,
    reconciled: &HashSet<String>,
) -> Result<ProjectionStore, String> {
    let version = existing
        .map(|store| store.projection_version)
        .unwrap_or(0)
        .checked_add(1)
        .ok_or("projection version overflow")?;
    let timestamp = Utc::now().timestamp_millis();
    let captured = ProjectionStore::capture(preview, sessions, version, timestamp)
        .map_err(|error| error.to_string())?;
    let sessions_by_id = sessions
        .iter()
        .map(|session| (session.id.as_str(), session))
        .collect::<HashMap<_, _>>();
    let mut records = existing
        .map(|store| store.threads.clone())
        .unwrap_or_default();
    let mut changed = !reconciled.is_empty();
    records.retain(|thread_id, _| !reconciled.contains(thread_id));
    for (thread_id, mut record) in captured.threads {
        if let Some(previous) = records.get(&thread_id) {
            record.origin_provider = previous.origin_provider.clone();
            record.original_state_provider = previous.original_state_provider.clone();
            record.original_state_present = previous.original_state_present;
            record.original_catalog = previous.original_catalog.clone();
            record.original_rollout_provider = previous.original_rollout_provider.clone();
        } else {
            record.original_rollout_provider = snapshot
                .rollout_provider_values
                .get(&thread_id)
                .and_then(|provider| source_provider(provider));
        }
        record.version = version;
        record.timestamp = timestamp;
        records.insert(thread_id, record);
        changed = true;
    }
    for planned in &preview.sessions {
        let Some(current_rollout) = snapshot
            .rollout_provider_values
            .get(&planned.thread_id)
            .and_then(|provider| source_provider(provider))
        else {
            continue;
        };
        if let Some(record) = records.get_mut(&planned.thread_id) {
            let target_changed = record.projected_target != preview.target_provider;
            let origin_captured = record.original_rollout_provider.is_none()
                && current_rollout != preview.target_provider;
            if target_changed {
                record.projected_target = preview.target_provider.clone();
            }
            if origin_captured {
                record.original_rollout_provider = Some(current_rollout);
            }
            if target_changed || origin_captured {
                record.version = version;
                record.timestamp = timestamp;
                changed = true;
            }
            continue;
        }
        if current_rollout == preview.target_provider {
            continue;
        }
        let session = sessions_by_id
            .get(planned.thread_id.as_str())
            .ok_or_else(|| {
                format!(
                    "preview session is absent from projection input: {}",
                    planned.thread_id
                )
            })?;
        records.insert(
            planned.thread_id.clone(),
            projection::ProjectionRecord {
                thread_id: planned.thread_id.clone(),
                origin_provider: session.origin_provider.clone(),
                original_state_provider: session.state_provider.clone(),
                original_state_present: session.state_present,
                original_catalog: session.catalog.clone(),
                original_rollout_provider: Some(current_rollout),
                projected_target: preview.target_provider.clone(),
                version,
                timestamp,
            },
        );
        changed = true;
    }
    if !changed {
        if let Some(existing) = existing {
            return Ok(existing.clone());
        }
    }
    Ok(ProjectionStore {
        schema_version: 3,
        projection_version: version,
        target_provider: preview.target_provider.clone(),
        timestamp,
        threads: records,
    })
}

#[cfg_attr(not(test), allow(dead_code))]
fn eligible_projection_sessions(
    snapshot: &Snapshot,
    store: Option<&ProjectionStore>,
    target_provider: &SourceProvider,
) -> (Vec<EligibleSession>, Vec<SkipReason>) {
    let cohorts = session_cohorts(snapshot);
    eligible_projection_sessions_with_cohorts(snapshot, store, target_provider, &cohorts)
}

fn eligible_projection_sessions_with_cohorts(
    snapshot: &Snapshot,
    store: Option<&ProjectionStore>,
    target_provider: &SourceProvider,
    cohorts: &SessionCohorts,
) -> (Vec<EligibleSession>, Vec<SkipReason>) {
    let mut sessions = Vec::new();
    let mut skipped = Vec::new();
    for (thread, state_present) in rollout_backed_threads(snapshot) {
        if let Some(reason) = repair_exclusion_reason(snapshot, cohorts, &thread) {
            skipped.push(SkipReason {
                thread_id: Some(thread.id.clone()),
                reason,
            });
            continue;
        }
        let state_provider = state_present
            .then(|| source_provider(&thread.provider))
            .flatten()
            .or_else(|| {
                snapshot
                    .rollout_provider_values
                    .get(&thread.id)
                    .and_then(|provider| source_provider(provider))
            })
            .unwrap_or_else(|| target_provider.clone());
        // local_thread_catalog is a derived cache. Keep it out of eligibility and
        // planning so missing or stale catalog rows cannot hide valid rollouts.
        let catalog = CatalogState::Present {
            provider: state_provider.clone(),
        };
        let origin_provider = store
            .and_then(|store| store.threads.get(&thread.id))
            .map(|record| record.origin_provider.clone())
            .or_else(|| {
                snapshot
                    .rollout_provider_values
                    .get(&thread.id)
                    .and_then(|provider| source_provider(provider))
            })
            .unwrap_or_else(|| state_provider.clone());
        sessions.push(EligibleSession {
            id: thread.id.clone(),
            origin_provider,
            state_provider,
            state_present,
            catalog,
            updated_at: catalog_time(thread.updated_at).round() as i64,
        });
    }
    sort_reasons(&mut skipped);
    (sessions, skipped)
}

#[derive(Debug, Clone)]
struct RankedProjectionCandidate {
    id: String,
    updated_at: i64,
    session: Option<EligibleSession>,
    workspace_conflict: Option<SkipReason>,
}

fn scoped_projection_sessions(
    snapshot: &Snapshot,
    store: Option<&ProjectionStore>,
    sessions: &[EligibleSession],
    skipped_reasons: &[SkipReason],
    selected: &BTreeSet<SourceProvider>,
    target: SourceProvider,
    _scope: ProjectionScope,
) -> (Vec<EligibleSession>, Vec<SkipReason>) {
    let mut effective_sources = selected.clone();
    effective_sources.insert(target);
    let mut ranked = sessions
        .iter()
        .filter(|session| effective_sources.contains(&session.origin_provider))
        .cloned()
        .map(|session| RankedProjectionCandidate {
            id: session.id.clone(),
            updated_at: session.updated_at,
            session: Some(session),
            workspace_conflict: None,
        })
        .collect::<Vec<_>>();
    let conflict_ids = skipped_reasons
        .iter()
        .filter(|reason| reason.reason == "workspace_conflict")
        .filter_map(|reason| reason.thread_id.as_deref().map(|id| (id, reason)))
        .collect::<HashMap<_, _>>();
    for thread in &snapshot.threads {
        let Some(reason) = conflict_ids.get(thread.id.as_str()) else {
            continue;
        };
        let Some(state_provider) = source_provider(&thread.provider) else {
            continue;
        };
        let origin_provider = store
            .and_then(|store| store.threads.get(&thread.id))
            .map(|record| record.origin_provider.clone())
            .unwrap_or(state_provider);
        if !effective_sources.contains(&origin_provider) {
            continue;
        }
        ranked.push(RankedProjectionCandidate {
            id: thread.id.clone(),
            updated_at: catalog_time(thread.updated_at).round() as i64,
            session: None,
            workspace_conflict: Some((*reason).clone()),
        });
    }
    ranked.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut scoped_sessions = Vec::new();
    let mut scoped_conflicts = Vec::new();
    for candidate in ranked {
        if let Some(session) = candidate.session {
            scoped_sessions.push(session);
        }
        if let Some(reason) = candidate.workspace_conflict {
            scoped_conflicts.push(reason);
        }
    }
    sort_reasons(&mut scoped_conflicts);
    (scoped_sessions, scoped_conflicts)
}

fn fingerprint_projection_store(store: &ProjectionStore) -> String {
    let mut digest = Sha256::new();
    digest.update(b"projection-store-v1\0");
    digest.update(store.schema_version.to_le_bytes());
    digest.update(store.projection_version.to_le_bytes());
    digest.update(store.target_provider.as_str().as_bytes());
    digest.update(b"\0");
    digest.update(store.timestamp.to_le_bytes());
    digest.update((store.threads.len() as u64).to_le_bytes());
    for (id, record) in &store.threads {
        digest.update(id.as_bytes());
        digest.update(b"\0");
        digest.update(record.version.to_le_bytes());
        digest.update(record.timestamp.to_le_bytes());
        digest.update(record.origin_provider.as_str().as_bytes());
        digest.update(b"\0");
        digest.update(record.projected_target.as_str().as_bytes());
        digest.update(b"\0");
        digest.update([u8::from(record.original_state_present)]);
        digest.update(record.original_state_provider.as_str().as_bytes());
        digest.update(b"\0");
        if let Ok(catalog) = serde_json::to_vec(&record.original_catalog) {
            digest.update(&(catalog.len() as u64).to_le_bytes());
            digest.update(&catalog);
        }
        if let Some(provider) = record.original_rollout_provider.as_ref() {
            digest.update(b"rp\0");
            digest.update(provider.as_str().as_bytes());
            digest.update(b"\0");
        } else {
            digest.update(b"rp-none\0");
        }
    }
    format!("{:x}", digest.finalize())
}

fn projection_plan_token(
    preview: &ProviderPlanPreview,
    plan: &RepairPlan,
    reconciled: &HashSet<String>,
    store: Option<&ProjectionStore>,
    workspace_conflicts: &[SkipReason],
) -> Result<String, String> {
    fn sorted_json<T: Serialize>(values: &[T]) -> Result<Vec<String>, String> {
        let mut encoded = values
            .iter()
            .map(|value| serde_json::to_string(value).map_err(|error| error.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        encoded.sort();
        Ok(encoded)
    }

    // Compact session identity for the token (ordered ops already cover write set;
    // session rows catch category/origin drift without hashing the full preview UI blob).
    let mut session_rows = preview
        .sessions
        .iter()
        .map(|session| {
            json!({
                "threadId": session.thread_id,
                "originProvider": session.origin_provider,
                "updatedAt": session.updated_at,
                "category": session.category,
            })
        })
        .collect::<Vec<_>>();
    session_rows.sort_by(|left, right| {
        left["threadId"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["threadId"].as_str().unwrap_or_default())
    });

    let mut reconciled = reconciled.iter().cloned().collect::<Vec<_>>();
    reconciled.sort();
    let mut skipped = plan.skipped.clone();
    sort_reasons(&mut skipped);
    let mut workspace_conflicts = workspace_conflicts.to_vec();
    sort_reasons(&mut workspace_conflicts);

    // schema 5: stop hashing full ProjectionStore + full ProviderPlanPreview JSON.
    // Token still binds ordered ops, store fingerprint, counters, and session categories.
    let payload = json!({
        "schema": 5,
        "projectionVersion": store.map(|store| store.projection_version),
        "projectionStoreFingerprint": store.map(fingerprint_projection_store),
        "previewSummary": {
            "targetProvider": preview.target_provider,
            "scope": preview.scope,
            "selectedSources": preview.selected_sources,
            "totalCandidates": preview.total_candidates,
            "considered": preview.considered,
            "pending": preview.pending,
            "matrix": preview.matrix,
            "operations": preview.operations,
            "sessions": session_rows,
        },
        "stateUpdates": sorted_json(&plan.state_updates)?,
        "stateRestores": sorted_json(&plan.state_restores)?,
        "stateInserts": sorted_json(&plan.state_inserts)?,
        "stateDeletes": sorted_json(&plan.state_deletes)?,
        "rolloutUpdates": sorted_json(&plan.rollout_updates)?,
        "catalogUpdates": sorted_json(&plan.catalog_updates)?,
        "catalogInserts": sorted_json(&plan.catalog_inserts)?,
        "catalogDeletes": sorted_json(&plan.catalog_deletes)?,
        "workspaceHintUpdates": sorted_json(&plan.workspace_hint_updates)?,
        "expectedGlobalStateSha256": plan.expected_global_state_sha256,
        "reconciled": reconciled,
        "workspaceConflicts": workspace_conflicts,
        "skipped": skipped,
    });
    let bytes = serde_json::to_vec(&payload).map_err(|error| error.to_string())?;
    let mut digest = Sha256::new();
    digest.update(bytes);
    Ok(format!("{:x}", digest.finalize()))
}

fn projection_preview_for_snapshot(
    snapshot: &Snapshot,
    store: Option<&ProjectionStore>,
    selected_sources: &[String],
    target_provider: &str,
    scope: ProjectionScope,
    selected_thread_ids: Option<&HashSet<String>>,
) -> Result<(ProjectionPreviewResult, Vec<EligibleSession>), String> {
    let cohorts = session_cohorts(snapshot);
    projection_preview_for_snapshot_with_cohorts(
        snapshot,
        store,
        selected_sources,
        target_provider,
        scope,
        selected_thread_ids,
        &cohorts,
    )
}

fn projection_preview_for_snapshot_with_cohorts(
    snapshot: &Snapshot,
    store: Option<&ProjectionStore>,
    _selected_sources: &[String],
    target_provider: &str,
    scope: ProjectionScope,
    selected_thread_ids: Option<&HashSet<String>>,
    cohorts: &SessionCohorts,
) -> Result<(ProjectionPreviewResult, Vec<EligibleSession>), String> {
    let target_id = validate_provider(target_provider)?;
    let target =
        source_provider(&target_id).ok_or_else(|| "invalid target provider id".to_string())?;
    let (mut sessions, mut skipped_reasons) =
        eligible_projection_sessions_with_cohorts(snapshot, store, &target, cohorts);
    if let Some(selected_thread_ids) = selected_thread_ids {
        sessions.retain(|session| selected_thread_ids.contains(&session.id));
        skipped_reasons.retain(|reason| {
            reason
                .thread_id
                .as_ref()
                .is_some_and(|thread_id| selected_thread_ids.contains(thread_id))
        });
    }
    let mut selected = sessions
        .iter()
        .map(|session| session.origin_provider.clone())
        .collect::<BTreeSet<_>>();
    selected.insert(target.clone());
    let mut source_counts = BTreeMap::new();
    for session in &sessions {
        *source_counts
            .entry(session.origin_provider.as_str().to_string())
            .or_default() += 1;
    }
    let (scoped_sessions, workspace_conflict_reasons) = scoped_projection_sessions(
        snapshot,
        store,
        &sessions,
        &skipped_reasons,
        &selected,
        target.clone(),
        scope,
    );
    let workspace_conflicts = workspace_conflict_reasons.len();
    let plan = projection::build_provider_plan_preview(&scoped_sessions, &selected, target, scope)
        .map_err(|error| error.to_string())?;
    let repair_plan = build_plan_for_preview(snapshot, &target_id, &plan);
    let reconciled: HashSet<String> = HashSet::new();
    let reconcile_conflicts = repair_plan
        .skipped
        .iter()
        .filter(|reason| reason.reason.starts_with("projection_reconcile_"))
        .count();
    let reconcile_reasons = repair_plan
        .skipped
        .iter()
        .filter(|reason| reason.reason.starts_with("projection_reconcile_"))
        .cloned()
        .collect();
    let workspace_hint_updates = repair_plan.workspace_hint_updates.len();
    let changed_threads = repair_plan.changed_ids.len();
    let rollout_updates = repair_plan.rollout_updates.len();
    let plan_token = projection_plan_token(
        &plan,
        &repair_plan,
        &reconciled,
        store,
        &workspace_conflict_reasons,
    )?;
    Ok((
        ProjectionPreviewResult {
            plan,
            plan_token,
            changed_threads,
            rollout_updates,
            source_counts,
            reconcile_pending: reconciled.len(),
            reconcile_conflicts,
            reconcile_reasons,
            workspace_hint_updates,
            workspace_conflicts,
            workspace_conflict_reasons,
            skipped: skipped_reasons.len(),
            skipped_reasons,
        },
        sessions,
    ))
}

pub fn preview_projection_at(
    home: &Path,
    selected_sources: &[String],
    target_provider: &str,
    scope: ProjectionScope,
) -> Result<ProjectionPreviewResult, String> {
    let target_provider = validate_current_target_provider(home, target_provider)?;
    let snapshot = scan_snapshot(home);
    let store = load_projection_store(home)?;
    projection_preview_for_snapshot(
        &snapshot,
        store.as_ref(),
        selected_sources,
        &target_provider,
        scope,
        None,
    )
    .map(|(preview, _)| preview)
}

fn selected_thread_filter(selected_thread_ids: Option<&[String]>) -> Option<HashSet<String>> {
    selected_thread_ids.map(|thread_ids| {
        thread_ids
            .iter()
            .map(|thread_id| thread_id.trim())
            .filter(|thread_id| !thread_id.is_empty())
            .map(str::to_owned)
            .collect::<HashSet<_>>()
    })
}

pub fn preview_projection_selected_at(
    home: &Path,
    selected_sources: &[String],
    target_provider: &str,
    scope: ProjectionScope,
    selected_thread_ids: Option<&[String]>,
) -> Result<ProjectionPreviewResult, String> {
    let target_provider = validate_current_target_provider(home, target_provider)?;
    let snapshot = scan_snapshot(home);
    let store = load_projection_store(home)?;
    let selected_thread_ids = selected_thread_filter(selected_thread_ids);
    projection_preview_for_snapshot(
        &snapshot,
        store.as_ref(),
        selected_sources,
        &target_provider,
        scope,
        selected_thread_ids.as_ref(),
    )
    .map(|(preview, _)| preview)
}

fn repair_exclusion_reason(
    snapshot: &Snapshot,
    cohorts: &SessionCohorts,
    thread: &ThreadRow,
) -> Option<String> {
    if cohorts.remote_excluded_thread_ids.contains(&thread.id) {
        return Some("remote_mapped".into());
    }
    if !snapshot.valid_active_rollouts.contains(&thread.id)
        && !snapshot.valid_archived_rollouts.contains(&thread.id)
    {
        return Some("rollout_missing_or_ambiguous".into());
    }
    if !snapshot.rollout_provider_values.contains_key(&thread.id)
        && !snapshot
            .primary_rollouts
            .get(&thread.id)
            .is_some_and(|primary| primary.provider_field_missing)
    {
        return Some("rollout_provider_missing_or_invalid".into());
    }
    None
}

fn lock_summary(home: &Path, active_processes: Vec<String>) -> LockSummary {
    let operation = platform::inspect_operation_lock(home).unwrap_or_else(|error| {
        platform::OperationLockStatus {
            state: "error".into(),
            path: error,
            owner_pid: None,
            owner_started_at: None,
            command: None,
            age_seconds: None,
        }
    });
    LockSummary {
        state: if operation.state == "clear" && active_processes.is_empty() {
            "clear".into()
        } else if operation.state != "clear" {
            operation.state.clone()
        } else {
            "process-active".into()
        },
        path: operation.path,
        owner_pid: operation.owner_pid,
        age_seconds: operation.age_seconds,
        active_processes,
    }
}

pub fn inspect_lock(home: &Path) -> LockSummary {
    let active_processes = platform::blocking_processes(home)
        .map(|processes| {
            processes
                .into_iter()
                .map(|process| process.identity.name)
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|_| vec!["process-enumeration-failed".into()]);
    lock_summary(home, active_processes)
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

fn hash_file(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 64];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Some(format!("{:x}", digest.finalize()))
}

fn iso_modified(path: &Path) -> Option<String> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let timestamp: DateTime<Utc> = modified.into();
    Some(timestamp.to_rfc3339())
}

fn ensure_backup_root(home: &Path, create: bool) -> Result<PathBuf, String> {
    let canonical_home = fs::canonicalize(home)
        .map_err(|error| format!("CODEX_HOME is unavailable ({}): {error}", home.display()))?;
    let mut current = canonical_home.clone();
    for component in ["backups", "provider-hub"] {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "backup directory is a symlink: {}",
                    current.display()
                ));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(format!(
                    "backup path is not a directory: {}",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                fs::create_dir(&current).map_err(|error| {
                    format!(
                        "cannot create backup directory ({}): {error}",
                        current.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "backup directory unavailable ({}): {error}",
                    current.display()
                ));
            }
        }
        let canonical = fs::canonicalize(&current).map_err(|error| error.to_string())?;
        if !canonical.starts_with(&canonical_home) {
            return Err(format!(
                "backup directory escapes CODEX_HOME: {}",
                current.display()
            ));
        }
    }
    Ok(current)
}

fn backup_directory_size(path: &Path) -> Result<u64, String> {
    let mut bytes = 0u64;
    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry.map_err(|error| format!("backup traversal failed: {error}"))?;
        if entry.file_type().is_symlink() {
            return Err(format!(
                "backup contains a symlink: {}",
                entry.path().display()
            ));
        }
        if entry.file_type().is_file() {
            bytes =
                bytes.saturating_add(entry.metadata().map_err(|error| error.to_string())?.len());
        }
    }
    Ok(bytes)
}

fn paths_refer_to_same_file(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn backup_modified_at(path: &Path) -> Option<SystemTime> {
    fs::symlink_metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
}

fn is_ascii_digits(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_repair_backup_name(name: &str) -> bool {
    let Some(stamp) = name.strip_prefix("repair-") else {
        return false;
    };
    let mut parts = stamp.split('-');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(date), Some(time), Some(millis), None)
            if is_ascii_digits(date, 8)
                && is_ascii_digits(time, 6)
                && is_ascii_digits(millis, 3)
    )
}

fn is_restore_temporary_name(name: &str) -> bool {
    name.strip_prefix(".restore-before-")
        .is_some_and(|stamp| is_ascii_digits(stamp, 17))
}

fn is_cleanup_quarantine_name(name: &str) -> bool {
    let Some(remainder) = name.strip_prefix(".deleting-") else {
        return false;
    };
    let mut parts = remainder.rsplitn(3, '-');
    let (Some(cleanup_stamp), Some(pid), Some(original)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    is_ascii_digits(cleanup_stamp, 17)
        && !pid.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && (is_repair_backup_name(original) || is_restore_temporary_name(original))
}

fn is_known_incomplete_backup_name(name: &str) -> bool {
    is_restore_temporary_name(name) || is_cleanup_quarantine_name(name)
}

fn backup_created_at(path: &Path, manifest: Option<&BackupManifest>) -> String {
    if let Some(created_at) = manifest
        .map(|manifest| manifest.created_at.trim())
        .filter(|created_at| !created_at.is_empty())
    {
        return created_at.to_owned();
    }
    backup_modified_at(path)
        .map(DateTime::<Utc>::from)
        .map(|created_at| created_at.to_rfc3339())
        .unwrap_or_default()
}

fn backup_summary_at(home: &Path) -> Result<BackupSummary, String> {
    let requested_root = home.join("backups/provider-hub");
    if !requested_root.exists() {
        return Ok(BackupSummary {
            automatic_limit: AUTOMATIC_BACKUP_LIMIT,
            minimum_automatic: MINIMUM_AUTOMATIC_BACKUPS,
            capacity_limit_bytes: BACKUP_CAPACITY_LIMIT_BYTES,
            ..BackupSummary::default()
        });
    }
    let root = ensure_backup_root(home, false)?;
    let pending = load_pending_operation(home)?;
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    for item in fs::read_dir(&root).map_err(|error| error.to_string())? {
        let item = match item {
            Ok(item) => item,
            Err(error) => {
                warnings.push(format!("cannot inspect a backup entry: {error}"));
                continue;
            }
        };
        let path = item.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                warnings.push(format!(
                    "cannot inspect backup metadata ({}): {error}",
                    path.display()
                ));
                continue;
            }
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let name = item.file_name().to_string_lossy().to_string();
        if !name.starts_with("repair-")
            && !name.starts_with(".deleting-")
            && !name.starts_with(".restore-before-")
        {
            continue;
        }
        let manifest_path = path.join("manifest.json");
        let manifest_present = match fs::symlink_metadata(&manifest_path) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                warnings.push(format!(
                    "cannot inspect backup manifest metadata ({}): {error}",
                    manifest_path.display()
                ));
                true
            }
        };
        let manifest = if manifest_present {
            match fs::read(&manifest_path)
                .map_err(|error| error.to_string())
                .and_then(|bytes| {
                    serde_json::from_slice::<BackupManifest>(&bytes)
                        .map_err(|error| error.to_string())
                }) {
                Ok(manifest) => Some(manifest),
                Err(error) => {
                    warnings.push(format!(
                        "cannot parse backup manifest ({}): {error}",
                        manifest_path.display()
                    ));
                    None
                }
            }
        } else {
            None
        };
        let manifest_version = manifest.as_ref().map(|manifest| manifest.version);
        let mut restorable = is_restorable_backup(&path);
        let mut status = if restorable {
            "restorable".to_string()
        } else if manifest_version.is_some_and(|version| !(4..=6).contains(&version)) {
            "legacy".to_string()
        } else if manifest.is_some() || manifest_present || !is_known_incomplete_backup_name(&name)
        {
            "corrupt".to_string()
        } else {
            "incomplete".to_string()
        };
        let protected = pending.as_ref().is_some_and(|pending| {
            paths_refer_to_same_file(&path, Path::new(&pending.backup_path))
        });
        let protection_reason = protected.then(|| "pendingOperation".to_string());
        let size_bytes = match backup_directory_size(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                warnings.push(error);
                restorable = false;
                status = "corrupt".into();
                0
            }
        };
        entries.push(BackupEntry {
            name,
            path: path.to_string_lossy().to_string(),
            created_at: backup_created_at(&path, manifest.as_ref()),
            size_bytes,
            provider: manifest
                .as_ref()
                .map(|manifest| manifest.provider.clone())
                .unwrap_or_default(),
            kind: manifest
                .as_ref()
                .map(|manifest| manifest.kind)
                .unwrap_or_default(),
            pinned: manifest.as_ref().is_some_and(|manifest| manifest.pinned),
            protected,
            protection_reason,
            restorable,
            status,
            manifest_version,
        });
    }
    entries.sort_by(|left, right| {
        let left_time = DateTime::parse_from_rfc3339(&left.created_at).ok();
        let right_time = DateTime::parse_from_rfc3339(&right.created_at).ok();
        right_time
            .cmp(&left_time)
            .then_with(|| right.name.cmp(&left.name))
    });
    let total_bytes = entries.iter().map(|entry| entry.size_bytes).sum();
    let restorable_count = entries.iter().filter(|entry| entry.restorable).count();
    let automatic_count = entries
        .iter()
        .filter(|entry| entry.restorable && !entry.pinned)
        .count();
    let pinned_count = entries.iter().filter(|entry| entry.pinned).count();
    let legacy_count = entries
        .iter()
        .filter(|entry| entry.status == "legacy")
        .count();
    let incomplete_count = entries
        .iter()
        .filter(|entry| matches!(entry.status.as_str(), "incomplete" | "corrupt"))
        .count();
    let legacy_bytes = entries
        .iter()
        .filter(|entry| entry.status == "legacy")
        .map(|entry| entry.size_bytes)
        .sum();
    let managed_bytes: u64 = entries
        .iter()
        .filter(|entry| entry.restorable && !entry.pinned)
        .map(|entry| entry.size_bytes)
        .sum();
    let over_limit = automatic_count > AUTOMATIC_BACKUP_LIMIT
        || managed_bytes > BACKUP_CAPACITY_LIMIT_BYTES
        || total_bytes > BACKUP_CAPACITY_LIMIT_BYTES;
    Ok(BackupSummary {
        entries,
        restorable_count,
        automatic_count,
        pinned_count,
        legacy_count,
        incomplete_count,
        total_bytes,
        legacy_bytes,
        automatic_limit: AUTOMATIC_BACKUP_LIMIT,
        minimum_automatic: MINIMUM_AUTOMATIC_BACKUPS,
        capacity_limit_bytes: BACKUP_CAPACITY_LIMIT_BYTES,
        over_limit,
        warnings,
    })
}

/// Fast list path: manifest + presence/size surface checks only.
/// Full hash / `quick_check` stays on restore, latest-backup pick, and cleanup prune.
pub fn list_backups_at(home: &Path) -> Result<BackupSummary, String> {
    let mut summary = backup_summary_at(home)?;
    for entry in &mut summary.entries {
        if entry.restorable {
            if let Err(error) = validate_backup_surface(Path::new(&entry.path)) {
                entry.restorable = false;
                entry.status = "corrupt".into();
                summary.warnings.push(format!(
                    "backup failed surface validation ({}): {error}",
                    entry.path
                ));
            }
        }
    }
    summary.restorable_count = summary
        .entries
        .iter()
        .filter(|entry| entry.restorable)
        .count();
    summary.automatic_count = summary
        .entries
        .iter()
        .filter(|entry| entry.restorable && !entry.pinned)
        .count();
    summary.incomplete_count = summary
        .entries
        .iter()
        .filter(|entry| matches!(entry.status.as_str(), "incomplete" | "corrupt"))
        .count();
    let managed_bytes: u64 = summary
        .entries
        .iter()
        .filter(|entry| entry.restorable && !entry.pinned)
        .map(|entry| entry.size_bytes)
        .sum();
    summary.over_limit = summary.automatic_count > AUTOMATIC_BACKUP_LIMIT
        || managed_bytes > BACKUP_CAPACITY_LIMIT_BYTES
        || summary.total_bytes > BACKUP_CAPACITY_LIMIT_BYTES;
    Ok(summary)
}

fn validate_backup_surface(path: &Path) -> Result<(), String> {
    let manifest: BackupManifest = serde_json::from_slice(
        &fs::read(path.join("manifest.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("invalid backup manifest: {error}"))?;
    validate_backup_manifest_version(&manifest)?;
    if !is_restorable_backup(path) {
        return Err("backup manifest or required files are incomplete".into());
    }
    for file in manifest.files.iter().filter(|file| file.backed_up) {
        let source = path.join(file.path.replace('/', "_"));
        let metadata = fs::metadata(&source).map_err(|error| {
            format!("backup file missing ({}): {error}", file.path)
        })?;
        if !metadata.is_file() {
            return Err(format!("backup path is not a file: {}", file.path));
        }
        if metadata.len() != file.size {
            return Err(format!(
                "backup size mismatch: {} (expected {}, got {})",
                file.path,
                file.size,
                metadata.len()
            ));
        }
        // Light usability checks only — no full-file hash / PRAGMA quick_check.
        if file.path.ends_with(".sqlite") {
            let connection = open_readonly(&source)?;
            if file.path == "state_5.sqlite" || file.path.ends_with("state_5.sqlite") {
                validate_state_repair_schema(&connection)?;
            }
        } else if file.path == ".codex-global-state.json" {
            serde_json::from_slice::<Value>(
                &fs::read(&source).map_err(|error| error.to_string())?,
            )
            .map_err(|error| format!("invalid global state backup: {error}"))?;
        }
    }
    if manifest.projection_state_present {
        let source = path.join("projection-state.json");
        let metadata = fs::metadata(&source)
            .map_err(|error| format!("projection state backup missing: {error}"))?;
        if !metadata.is_file() {
            return Err("projection state backup is not a file".into());
        }
        serde_json::from_slice::<ProjectionStore>(
            &fs::read(&source).map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("invalid projection state backup: {error}"))?;
    }
    // Versioned dual-DB schema gate without hashing file bodies.
    if manifest.version == 6 {
        let state = open_readonly(&path.join("state_5.sqlite"))?;
        validate_state_repair_schema(&state)?;
    } else {
        validate_repair_schema_files(
            &path.join("state_5.sqlite"),
            &path.join("sqlite_codex-dev.db"),
        )?;
    }
    Ok(())
}

pub fn backup_directory_at(home: &Path) -> Result<PathBuf, String> {
    ensure_backup_root(home, true)
}

fn validate_backup_child(root: &Path, path: &Path) -> Result<PathBuf, String> {
    let canonical_root = fs::canonicalize(root).map_err(|error| error.to_string())?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("backup is unavailable ({}): {error}", path.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!(
            "backup cleanup refused a non-directory or symlink: {}",
            path.display()
        ));
    }
    let canonical = fs::canonicalize(path).map_err(|error| error.to_string())?;
    if canonical.parent() != Some(canonical_root.as_path()) {
        return Err(format!(
            "backup cleanup refused a path outside the backup root: {}",
            path.display()
        ));
    }
    let name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("backup name is invalid: {}", path.display()))?;
    if !name.starts_with("repair-")
        && !name.starts_with(".deleting-")
        && !name.starts_with(".restore-before-")
    {
        return Err(format!("backup name is not managed by this app: {name}"));
    }
    Ok(canonical)
}

fn remove_backup_directory(root: &Path, path: &Path) -> Result<(), String> {
    let canonical = validate_backup_child(root, path)?;
    let name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("backup name is invalid")?;
    let quarantine = if name.starts_with(".deleting-") {
        canonical.clone()
    } else {
        let quarantine = root.join(format!(
            ".deleting-{}-{}-{}",
            name,
            std::process::id(),
            Local::now().format("%Y%m%d%H%M%S%3f")
        ));
        fs::rename(&canonical, &quarantine).map_err(|error| {
            format!(
                "cannot quarantine old backup ({}): {error}",
                canonical.display()
            )
        })?;
        validate_backup_child(root, &quarantine)?
    };
    fs::remove_dir_all(&quarantine).map_err(|error| {
        format!(
            "cannot remove old backup ({}): {error}",
            quarantine.display()
        )
    })
}

fn validate_backup_integrity(home: &Path, path: &Path) -> Result<(), String> {
    let manifest: BackupManifest = serde_json::from_slice(
        &fs::read(path.join("manifest.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("invalid backup manifest: {error}"))?;
    validate_backup_manifest_version(&manifest)?;
    if !is_restorable_backup(path) {
        return Err("backup manifest or required files are incomplete".into());
    }
    for file in manifest.files.iter().filter(|file| file.backed_up) {
        let source = path.join(file.path.replace('/', "_"));
        let expected = file
            .sha256
            .as_deref()
            .ok_or_else(|| format!("backup checksum is missing: {}", file.path))?;
        if hash_file(&source).as_deref() != Some(expected) {
            return Err(format!("backup checksum mismatch: {}", file.path));
        }
        if file.path.ends_with(".sqlite") {
            let connection = open_readonly(&source)?;
            sqlite_quick_check(&connection)?;
        } else if file.path == ".codex-global-state.json" {
            serde_json::from_slice::<Value>(&fs::read(&source).map_err(|error| error.to_string())?)
                .map_err(|error| format!("invalid global state backup: {error}"))?;
        }
    }
    if manifest.projection_state_present {
        let source = path.join("projection-state.json");
        let expected = manifest
            .projection_state_sha256
            .as_deref()
            .ok_or("projection state checksum is missing")?;
        if hash_file(&source).as_deref() != Some(expected) {
            return Err("projection state checksum mismatch".into());
        }
        serde_json::from_slice::<ProjectionStore>(
            &fs::read(&source).map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("invalid projection state backup: {error}"))?;
    }
    if manifest.version == 6 {
        let state = open_readonly(&path.join("state_5.sqlite"))?;
        validate_state_repair_schema(&state)?;
    } else {
        validate_repair_schema_files(
            &path.join("state_5.sqlite"),
            &path.join("sqlite_codex-dev.db"),
        )?;
    }
    capture_restore_rollout_images(home, &manifest)?;
    Ok(())
}

fn incomplete_backup_is_expired(entry: &BackupEntry) -> bool {
    matches!(entry.status.as_str(), "incomplete")
        && backup_modified_at(Path::new(&entry.path))
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= INCOMPLETE_BACKUP_GRACE)
}

fn cleanup_backups_unlocked_with_policy(
    home: &Path,
    include_legacy: bool,
    additionally_protected: &[PathBuf],
    automatic_limit: usize,
    minimum_automatic: usize,
    capacity_limit_bytes: u64,
    deep_integrity: bool,
) -> Result<BackupCleanupResult, String> {
    let root = ensure_backup_root(home, true)?;
    // Prefer surface-validated list so corrupt/size-mismatch entries are visible.
    let mut summary = list_backups_at(home)?;
    let mut result = BackupCleanupResult::default();
    let is_protected = |entry: &BackupEntry| {
        entry.protected
            || additionally_protected
                .iter()
                .any(|path| paths_refer_to_same_file(Path::new(&entry.path), path))
    };

    // Broken backups are useless for restore: delete them automatically.
    // Pending/protected snapshots are never removed. Legacy format still
    // requires include_legacy (may look complete but is version-incompatible).
    let broken_candidates = summary
        .entries
        .iter()
        .filter(|entry| {
            !is_protected(entry)
                && (incomplete_backup_is_expired(entry)
                    || entry.status == "corrupt"
                    || (include_legacy && entry.status == "legacy"))
        })
        .cloned()
        .collect::<Vec<_>>();
    for entry in broken_candidates {
        match remove_backup_directory(&root, Path::new(&entry.path)) {
            Ok(()) => {
                result.removed_count += 1;
                result.reclaimed_bytes = result.reclaimed_bytes.saturating_add(entry.size_bytes);
                if entry.status == "legacy" {
                    result.removed_legacy_count += 1;
                } else if entry.status == "corrupt" {
                    result.warnings.push(format!(
                        "removed damaged backup ({}): failed surface or integrity checks",
                        entry.path
                    ));
                }
            }
            Err(error) => result.warnings.push(error),
        }
    }

    summary = list_backups_at(home)?;
    // Non-pinned restorable snapshots count toward the automatic window.
    // Pending/protected entries remain in the set so free older slots prune
    // first; removal still skips protected paths via is_protected().
    let managed = summary
        .entries
        .iter()
        .filter(|entry| entry.restorable && !entry.pinned)
        .cloned()
        .collect::<Vec<_>>();
    let mut healthy = Vec::new();
    for entry in managed {
        if !deep_integrity {
            // Startup/maintain prune path: surface validation already ran via
            // list_backups_at. Full hash / quick_check stays on restore, manual
            // cleanup, and repair preflight.
            healthy.push(entry);
            continue;
        }
        match validate_backup_integrity(home, Path::new(&entry.path)) {
            Ok(()) => healthy.push(entry),
            Err(error) => {
                // Looked restorable on the surface but full restore checks failed
                // (e.g. missing live rollout preimage) — delete rather than keep junk.
                match remove_backup_directory(&root, Path::new(&entry.path)) {
                    Ok(()) => {
                        result.removed_count += 1;
                        result.reclaimed_bytes =
                            result.reclaimed_bytes.saturating_add(entry.size_bytes);
                        result.warnings.push(format!(
                            "removed damaged backup ({}): {error}",
                            entry.path
                        ));
                    }
                    Err(remove_error) => result.warnings.push(format!(
                        "could not remove damaged backup ({}): {error}; remove failed: {remove_error}",
                        entry.path
                    )),
                }
            }
        }
    }
    let healthy_bytes: u64 = healthy.iter().map(|entry| entry.size_bytes).sum();
    let needs_prune = healthy.len() > automatic_limit || healthy_bytes > capacity_limit_bytes;
    let minimum_keep = healthy
        .iter()
        .take(minimum_automatic)
        .map(|entry| entry.path.clone())
        .collect::<HashSet<_>>();
    let mut remaining_count = healthy.len();
    let mut remaining_bytes = healthy_bytes;
    if needs_prune {
        for entry in healthy.iter().rev() {
            let over_policy =
                remaining_count > automatic_limit || remaining_bytes > capacity_limit_bytes;
            if !over_policy || remaining_count <= minimum_automatic {
                break;
            }
            if minimum_keep.contains(&entry.path) || is_protected(entry) {
                continue;
            }
            match remove_backup_directory(&root, Path::new(&entry.path)) {
                Ok(()) => {
                    remaining_count -= 1;
                    remaining_bytes = remaining_bytes.saturating_sub(entry.size_bytes);
                    result.removed_count += 1;
                    result.reclaimed_bytes =
                        result.reclaimed_bytes.saturating_add(entry.size_bytes);
                }
                Err(error) => result.warnings.push(error),
            }
        }
    }
    if remaining_count > automatic_limit || remaining_bytes > capacity_limit_bytes {
        result.warnings.push(
            "backup retention remains above its limit because protected restore points could not be removed"
                .into(),
        );
    }

    let remaining = list_backups_at(home)?;
    for warning in remaining.warnings.iter().cloned() {
        if !result.warnings.contains(&warning) {
            result.warnings.push(warning);
        }
    }
    result.remaining_count = remaining.entries.len();
    result.remaining_bytes = remaining.total_bytes;
    if remaining.total_bytes > BACKUP_CAPACITY_LIMIT_BYTES
        && remaining
            .entries
            .iter()
            .any(|entry| entry.pinned || entry.status != "restorable")
    {
        result.warnings.push(
            "backup storage remains above 250 MiB because retained or incompatible backups are excluded from automatic cleanup"
                .into(),
        );
    }
    Ok(result)
}

fn cleanup_backups_unlocked(
    home: &Path,
    include_legacy: bool,
    additionally_protected: &[PathBuf],
) -> Result<BackupCleanupResult, String> {
    cleanup_backups_unlocked_with_policy(
        home,
        include_legacy,
        additionally_protected,
        AUTOMATIC_BACKUP_LIMIT,
        MINIMUM_AUTOMATIC_BACKUPS,
        BACKUP_CAPACITY_LIMIT_BYTES,
        true,
    )
}

fn cleanup_backups_unlocked_light(
    home: &Path,
    include_legacy: bool,
    additionally_protected: &[PathBuf],
) -> Result<BackupCleanupResult, String> {
    cleanup_backups_unlocked_with_policy(
        home,
        include_legacy,
        additionally_protected,
        AUTOMATIC_BACKUP_LIMIT,
        MINIMUM_AUTOMATIC_BACKUPS,
        BACKUP_CAPACITY_LIMIT_BYTES,
        false,
    )
}

pub fn cleanup_backups_at(
    home: &Path,
    include_legacy: bool,
) -> Result<BackupCleanupResult, String> {
    let _guard = platform::acquire_operation_lock(home, "cleanup-backups")?;
    if let Some(pending) = load_pending_operation(home)? {
        return Err(format!(
            "backup cleanup is blocked until incomplete {} is recovered from {}",
            pending.command, pending.backup_path
        ));
    }
    cleanup_backups_unlocked(home, include_legacy, &[])
}

fn maintain_backups_at(home: &Path) -> Result<Option<BackupCleanupResult>, String> {
    if load_pending_operation(home)?.is_some() {
        return Ok(None);
    }
    let summary = backup_summary_at(home)?;
    let has_expired_incomplete = summary.entries.iter().any(incomplete_backup_is_expired);
    let managed_bytes: u64 = summary
        .entries
        .iter()
        .filter(|entry| entry.restorable && !entry.pinned)
        .map(|entry| entry.size_bytes)
        .sum();
    if summary.automatic_count <= AUTOMATIC_BACKUP_LIMIT
        && managed_bytes <= BACKUP_CAPACITY_LIMIT_BYTES
        && !has_expired_incomplete
    {
        return Ok(None);
    }
    let _guard = platform::acquire_operation_lock(home, "maintain-backups")?;
    if load_pending_operation(home)?.is_some() {
        return Ok(None);
    }
    // Over-limit / expired-incomplete only: prune with surface checks.
    // Deep hash integrity remains on manual cleanup, restore, and repair preflight.
    cleanup_backups_unlocked_light(home, false, &[]).map(Some)
}

fn estimate_manifest_bytes(home: &Path, rollout_updates: &[RolloutUpdate]) -> Result<u64, String> {
    let file_rows = relevant_manifest_paths(home)?
        .into_iter()
        .fold(0u64, |bytes, path| {
            let relative_length = path
                .strip_prefix(home)
                .unwrap_or(&path)
                .to_string_lossy()
                .len() as u64;
            bytes.saturating_add(relative_length.saturating_add(512))
        });
    let rollout_rows = rollout_updates.iter().try_fold(0u64, |bytes, update| {
        let relative = rollout_relative_path(home, &update.path)?;
        let provider_length = update
            .expected_provider
            .as_deref()
            .unwrap_or_default()
            .len()
            .saturating_add(update.provider.as_deref().unwrap_or_default().len());
        Ok::<_, String>(
            bytes.saturating_add(
                512u64
                    .saturating_add(update.thread_id.len() as u64)
                    .saturating_add(relative.len() as u64)
                    .saturating_add(provider_length as u64),
            ),
        )
    })?;
    Ok((64 * 1024u64)
        .saturating_add(file_rows)
        .saturating_add(rollout_rows))
}

fn estimate_backup_bytes(home: &Path, rollout_updates: &[RolloutUpdate]) -> Result<u64, String> {
    let state_path = home.join("state_5.sqlite");
    let connection = open_readonly(&state_path)?;
    let page_count: u64 = connection
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .map_err(|error| error.to_string())?;
    let page_size: u64 = connection
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .map_err(|error| error.to_string())?;
    let state_bytes = page_count.saturating_mul(page_size);
    let projection_bytes = fs::metadata(projection_state_path(home))
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    let manifest_bytes = estimate_manifest_bytes(home, rollout_updates)?;
    let payload = state_bytes
        .saturating_add(projection_bytes)
        .saturating_add(manifest_bytes);
    let margin = (payload / 10).max(8 * 1024 * 1024);
    Ok(payload.saturating_add(margin))
}

fn ensure_backup_free_space(
    home: &Path,
    rollout_updates: &[RolloutUpdate],
    available_override: Option<u64>,
) -> Result<(), String> {
    let root = ensure_backup_root(home, true)?;
    let expected = estimate_backup_bytes(home, rollout_updates)?;
    let required = expected.saturating_add(BACKUP_FREE_SPACE_RESERVE_BYTES);
    let available = match available_override {
        Some(available) => available,
        None => fs2::available_space(&root).map_err(|error| {
            format!(
                "cannot inspect backup disk space ({}): {error}",
                root.display()
            )
        })?,
    };
    if available < required {
        return Err(format!(
            "insufficient disk space for a safe recovery backup: {} MiB available, {} MiB required",
            available / 1024 / 1024,
            required / 1024 / 1024
        ));
    }
    Ok(())
}

pub fn set_backup_pinned_at(
    home: &Path,
    requested: &Path,
    pinned: bool,
) -> Result<BackupSummary, String> {
    let _guard = platform::acquire_operation_lock(home, "retain-backup")?;
    if load_pending_operation(home)?.is_some() {
        return Err("backup retention cannot change while recovery is pending".into());
    }
    let backup = safe_backup_path(home, Some(requested))?;
    let manifest_path = backup.join("manifest.json");
    let mut manifest: BackupManifest =
        serde_json::from_slice(&fs::read(&manifest_path).map_err(|error| error.to_string())?)
            .map_err(|error| format!("invalid backup manifest: {error}"))?;
    validate_backup_manifest_version(&manifest)?;
    manifest.pinned = pinned;
    let temporary = backup.join(format!(
        ".manifest-retain-{}-{}.tmp",
        std::process::id(),
        Local::now().format("%Y%m%d%H%M%S%3f")
    ));
    let result = (|| {
        let mut file = File::create(&temporary).map_err(|error| error.to_string())?;
        file.write_all(&serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        platform::atomic_replace_file(&temporary, &manifest_path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    backup_summary_at(home)
}

fn relevant_manifest_paths(home: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    for relative in [
        "config.toml",
        "auth.json",
        ".codex-global-state.json",
        "state_5.sqlite",
        "sqlite/codex-dev.db",
    ] {
        let path = home.join(relative);
        if path.is_file() {
            paths.push(path);
        }
    }
    for root in [home.join("sessions"), home.join("archived_sessions")] {
        if !root.is_dir() {
            continue;
        }
        for entry in WalkDir::new(root).follow_links(false) {
            let entry = entry.map_err(|error| format!("manifest traversal failed: {error}"))?;
            if entry.file_type().is_file()
                && entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl")
            {
                paths.push(entry.into_path());
            }
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn rollout_relative_path(home: &Path, path: &Path) -> Result<String, String> {
    let relative = path
        .strip_prefix(home)
        .map_err(|_| format!("rollout is outside CODEX_HOME: {}", path.display()))?;
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(format!(
            "rollout path is not normalized: {}",
            path.display()
        ));
    }
    let mut components = relative.components();
    let root = components
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .unwrap_or_default();
    if !matches!(root, "sessions" | "archived_sessions")
        || path
            .extension()
            .and_then(|value| value.to_str())
            .is_none_or(|value| !value.eq_ignore_ascii_case("jsonl"))
    {
        return Err(format!("unsafe rollout path: {}", path.display()));
    }
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn rollout_path_from_relative(home: &Path, relative: &str) -> Result<(PathBuf, bool), String> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(format!("unsafe rollout path in journal: {relative}"));
    }
    let root = relative_path
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .unwrap_or_default();
    let archived = match root {
        "sessions" => false,
        "archived_sessions" => true,
        _ => return Err(format!("unsafe rollout root in journal: {relative}")),
    };
    let path = home.join(relative_path);
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_none_or(|value| !value.eq_ignore_ascii_case("jsonl"))
    {
        return Err(format!("journal rollout is not JSONL: {relative}"));
    }
    Ok((path, archived))
}

fn rollout_journal_from_update(
    home: &Path,
    update: &RolloutUpdate,
) -> Result<RepairRolloutJournal, String> {
    Ok(RepairRolloutJournal {
        thread_id: update.thread_id.clone(),
        path: rollout_relative_path(home, &update.path)?,
        archived: update.archived,
        before_provider: update.expected_provider.clone(),
        after_provider: update.provider.clone(),
    })
}

fn validate_rollout_journal_path(
    home: &Path,
    row: &RepairRolloutJournal,
) -> Result<PathBuf, String> {
    let (path, archived) = rollout_path_from_relative(home, &row.path)?;
    if archived != row.archived {
        return Err(format!(
            "rollout archive flag does not match path: {}",
            row.path
        ));
    }
    let primary = rollout::read_primary_rollout(&path, archived).map_err(|issue| {
        format!(
            "{} ({}): {}",
            issue.code,
            issue.path.display(),
            issue.detail
        )
    })?;
    if primary.id != row.thread_id || primary.path != path || primary.archived != archived {
        return Err(format!("rollout identity changed: {}", row.thread_id));
    }
    Ok(path)
}

fn read_rollout_provider_slot(path: &Path, archived: bool) -> Result<Option<String>, String> {
    let primary = rollout::read_primary_rollout(path, archived).map_err(|issue| {
        format!(
            "{} ({}): {}",
            issue.code,
            issue.path.display(),
            issue.detail
        )
    })?;
    if primary.model_provider.is_none() && !primary.provider_field_missing {
        return Err(format!(
            "rollout provider field is present but invalid: {}",
            path.display()
        ));
    }
    Ok(primary.model_provider)
}

fn rollout_journal_state(
    home: &Path,
    row: &RepairRolloutJournal,
) -> Result<JournalImageState, String> {
    let path = validate_rollout_journal_path(home, row)?;
    let current = read_rollout_provider_slot(&path, row.archived)?;
    Ok(
        if row.before_provider == row.after_provider && current == row.before_provider {
            JournalImageState::Both
        } else if current == row.before_provider {
            JournalImageState::Before
        } else if current == row.after_provider {
            JournalImageState::After
        } else {
            JournalImageState::Conflict
        },
    )
}

fn rewrite_rollout_provider(
    home: &Path,
    row: &RepairRolloutJournal,
    expected_provider: &Option<String>,
    target_provider: &Option<String>,
) -> Result<bool, String> {
    let path = validate_rollout_journal_path(home, row)?;
    let current = read_rollout_provider_slot(&path, row.archived)?;
    if &current != expected_provider {
        return Err(format!(
            "rollout provider changed before write: {} (expected: {:?}, current: {:?})",
            row.thread_id, expected_provider, current
        ));
    }
    if &current == target_provider {
        return Ok(false);
    }
    let Some(target_provider) = target_provider.as_deref() else {
        let expected = current
            .as_deref()
            .ok_or_else(|| format!("rollout provider is already missing: {}", row.thread_id))?;
        return rollout::remove_provider_if_matches(&path, row.archived, expected)
            .map_err(|error| error.to_string());
    };
    let options = rollout::ProviderRewriteOptions {
        include_archived: row.archived,
    };
    let plan = rollout::plan_provider_rewrite(&path, row.archived, target_provider, options)
        .map_err(|error| error.to_string())?;
    if plan.previous_provider != current {
        return Err(format!(
            "rollout provider changed while planning write: {}",
            row.thread_id
        ));
    }
    match rollout::commit_provider_rewrite(plan).map_err(|error| error.to_string())? {
        rollout::ProviderRewriteCommit::Applied(_) => Ok(true),
        rollout::ProviderRewriteCommit::NoChange(rollout::ProviderRewriteStatus::Unchanged) => {
            Ok(false)
        }
        rollout::ProviderRewriteCommit::NoChange(status) => Err(format!(
            "rollout cannot be rewritten for {}: {status:?}",
            row.thread_id
        )),
    }
}

fn apply_rollout_updates(home: &Path, updates: &[RolloutUpdate]) -> Result<usize, String> {
    let mut rows = updates
        .iter()
        .map(|update| rollout_journal_from_update(home, update))
        .collect::<Result<Vec<_>, _>>()?;
    rows.sort_by(|left, right| left.path.cmp(&right.path));
    let mut changed = 0;
    for row in &rows {
        changed += usize::from(rewrite_rollout_provider(
            home,
            row,
            &row.before_provider,
            &row.after_provider,
        )?);
    }
    if changed != rows.len() {
        return Err(format!(
            "rollout update count mismatch: expected {}, got {changed}",
            rows.len()
        ));
    }
    Ok(changed)
}

fn sync_file_path(path: &Path) -> Result<(), String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| error.to_string())?
        .sync_all()
        .map_err(|error| error.to_string())
}

fn sqlite_online_copy(source: &Path, destination: &Path) -> Result<(), String> {
    let source_connection = open_readonly(source)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut destination_connection = Connection::open(destination)
        .map_err(|error| format!("{}: {error}", destination.display()))?;
    {
        let backup = Backup::new(&source_connection, &mut destination_connection)
            .map_err(|error| error.to_string())?;
        backup
            .run_to_completion(64, Duration::from_millis(20), None)
            .map_err(|error| error.to_string())?;
    }
    destination_connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .map_err(|error| {
            format!(
                "SQLite checkpoint failed ({}): {error}",
                destination.display()
            )
        })?;
    drop(destination_connection);
    sync_file_path(destination)
}

fn sqlite_online_copy_from_connection(
    source: &Connection,
    destination: &Path,
) -> Result<(), String> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut destination_connection = Connection::open(destination)
        .map_err(|error| format!("{}: {error}", destination.display()))?;
    {
        let backup =
            Backup::new(source, &mut destination_connection).map_err(|error| error.to_string())?;
        backup
            .run_to_completion(64, Duration::from_millis(20), None)
            .map_err(|error| error.to_string())?;
    }
    destination_connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .map_err(|error| error.to_string())?;
    drop(destination_connection);
    sync_file_path(destination)
}

fn sqlite_online_restore_to_connection(
    source: &Path,
    destination: &mut Connection,
) -> Result<(), String> {
    let source_connection = open_readonly(source)?;
    let backup = Backup::new(&source_connection, destination).map_err(|error| error.to_string())?;
    backup
        .run_to_completion(64, Duration::from_millis(20), None)
        .map_err(|error| error.to_string())
}

fn probe_sqlite_write_lock(path: &Path) -> Result<(), String> {
    let connection = Connection::open(path).map_err(|error| error.to_string())?;
    connection
        .busy_timeout(Duration::from_millis(500))
        .map_err(|error| error.to_string())?;
    connection
        .execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
        .map_err(|error| {
            format!(
                "database is locked or not writable ({}): {error}",
                path.display()
            )
        })
}

struct DualSqliteRestoreGuard {
    state: Connection,
    catalog: Connection,
}

struct StateSqliteRestoreGuard {
    state: Connection,
}

impl StateSqliteRestoreGuard {
    fn acquire(home: &Path) -> Result<Self, String> {
        let path = home.join("state_5.sqlite");
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| format!("{}: {error}", path.display()))?;
        connection
            .busy_timeout(Duration::from_millis(500))
            .map_err(|error| error.to_string())?;
        let mode: String = connection
            .query_row("PRAGMA locking_mode=EXCLUSIVE", [], |row| row.get(0))
            .map_err(|error| error.to_string())?;
        if !mode.eq_ignore_ascii_case("exclusive") {
            return Err(format!(
                "SQLite refused exclusive restore mode ({}): {mode}",
                path.display()
            ));
        }
        connection
            .execute_batch("BEGIN EXCLUSIVE; COMMIT;")
            .map_err(|error| format!("cannot acquire exclusive restore lock: {error}"))?;
        Ok(Self { state: connection })
    }

    fn save_current(&self, destination: &Path) -> Result<(), String> {
        sqlite_online_copy_from_connection(&self.state, destination)
    }

    fn restore(&mut self, source: &Path) -> Result<(), String> {
        sqlite_online_restore_to_connection(source, &mut self.state)
    }

    fn validate(&self) -> Result<(), String> {
        validate_state_repair_schema(&self.state)
    }
}

impl DualSqliteRestoreGuard {
    fn acquire(home: &Path) -> Result<Self, String> {
        fn open_exclusive(path: &Path) -> Result<Connection, String> {
            let connection = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|error| format!("{}: {error}", path.display()))?;
            connection
                .busy_timeout(Duration::from_millis(500))
                .map_err(|error| error.to_string())?;
            let mode: String = connection
                .query_row("PRAGMA locking_mode=EXCLUSIVE", [], |row| row.get(0))
                .map_err(|error| error.to_string())?;
            if !mode.eq_ignore_ascii_case("exclusive") {
                return Err(format!(
                    "SQLite refused exclusive restore mode ({}): {mode}",
                    path.display()
                ));
            }
            connection
                .execute_batch("BEGIN EXCLUSIVE; COMMIT;")
                .map_err(|error| {
                    format!(
                        "cannot acquire exclusive restore lock ({}): {error}",
                        path.display()
                    )
                })?;
            Ok(connection)
        }

        let state = open_exclusive(&home.join("state_5.sqlite"))?;
        let catalog = open_exclusive(&home.join("sqlite/codex-dev.db"))?;
        Ok(Self { state, catalog })
    }

    fn save_current(&self, state: &Path, catalog: &Path) -> Result<(), String> {
        sqlite_online_copy_from_connection(&self.state, state)?;
        sqlite_online_copy_from_connection(&self.catalog, catalog)
    }

    fn restore(&mut self, state: &Path, catalog: &Path) -> Result<(), String> {
        sqlite_online_restore_to_connection(state, &mut self.state)?;
        sqlite_online_restore_to_connection(catalog, &mut self.catalog)
    }

    fn validate(&self) -> Result<(), String> {
        validate_repair_schema_connections(&self.state, &self.catalog)
    }
}

fn online_write_busy_timeout() -> Duration {
    #[cfg(test)]
    {
        Duration::from_millis(75)
    }
    #[cfg(not(test))]
    {
        Duration::from_secs(3)
    }
}

fn is_sqlite_write_conflict(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(details, _)
            if matches!(details.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn sqlite_write_conflict(context: &str, error: &rusqlite::Error) -> String {
    format!(
        "SQLITE_BUSY: {context}; Codex is currently writing to its local index. Retry shortly, or close Codex only if the conflict continues: {error}"
    )
}

struct DualSqliteWriteFence {
    connection: Connection,
    active: bool,
}

impl Drop for DualSqliteWriteFence {
    fn drop(&mut self) {
        if self.active {
            let _ = self.connection.execute_batch("ROLLBACK");
        }
    }
}

impl DualSqliteWriteFence {
    fn commit(&mut self) -> Result<(), String> {
        self.connection.execute_batch("COMMIT").map_err(|error| {
            if is_sqlite_write_conflict(&error) {
                sqlite_write_conflict("cannot commit the online repair transaction", &error)
            } else {
                error.to_string()
            }
        })?;
        self.active = false;
        Ok(())
    }
}

fn acquire_dual_sqlite_write_fence(
    home: &Path,
    include_catalog: bool,
) -> Result<DualSqliteWriteFence, String> {
    let state = home.join("state_5.sqlite");
    let connection = Connection::open_with_flags(
        &state,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| {
        format!(
            "cannot open SQLite write fence ({}): {error}",
            state.display()
        )
    })?;
    connection
        .busy_timeout(online_write_busy_timeout())
        .map_err(|error| error.to_string())?;
    if include_catalog {
        let catalog = home.join("sqlite/codex-dev.db");
        connection
            .execute(
                "ATTACH DATABASE ?1 AS provider_catalog",
                params![catalog.to_string_lossy().as_ref()],
            )
            .map_err(|error| {
                format!(
                    "cannot attach catalog to SQLite write fence ({}): {error}",
                    catalog.display()
                )
            })?;
    }
    const ATTEMPTS: usize = 2;
    for attempt in 0..ATTEMPTS {
        match connection.execute_batch("BEGIN IMMEDIATE") {
            Ok(()) => {
                return Ok(DualSqliteWriteFence {
                    connection,
                    active: true,
                });
            }
            Err(error) if is_sqlite_write_conflict(&error) && attempt + 1 < ATTEMPTS => {
                std::thread::sleep(Duration::from_millis(125));
            }
            Err(error) if is_sqlite_write_conflict(&error) => {
                return Err(sqlite_write_conflict(
                    "cannot start the online repair transaction after retrying",
                    &error,
                ));
            }
            Err(error) => {
                return Err(format!(
                    "cannot start the online repair transaction: {error}"
                ));
            }
        }
    }
    unreachable!("online write retry loop always returns")
}

fn create_backup_snapshot(
    home: &Path,
    _expected_global_state_sha256: Option<&str>,
    _guarded_global_state: Option<&[u8]>,
    rollout_updates: &[RolloutUpdate],
) -> Result<BackupResult, String> {
    create_backup_snapshot_with_kind(
        home,
        _expected_global_state_sha256,
        _guarded_global_state,
        rollout_updates,
        BackupKind::Automatic,
        false,
    )
}

fn create_backup_snapshot_with_kind(
    home: &Path,
    _expected_global_state_sha256: Option<&str>,
    _guarded_global_state: Option<&[u8]>,
    rollout_updates: &[RolloutUpdate],
    kind: BackupKind,
    pinned: bool,
) -> Result<BackupResult, String> {
    create_backup_snapshot_with_kind_and_available(
        home,
        _expected_global_state_sha256,
        _guarded_global_state,
        rollout_updates,
        kind,
        pinned,
        None,
    )
}

fn create_backup_snapshot_with_kind_and_available(
    home: &Path,
    _expected_global_state_sha256: Option<&str>,
    _guarded_global_state: Option<&[u8]>,
    rollout_updates: &[RolloutUpdate],
    kind: BackupKind,
    pinned: bool,
    available_override: Option<u64>,
) -> Result<BackupResult, String> {
    ensure_backup_free_space(home, rollout_updates, available_override)?;
    let stamp = Local::now().format("repair-%Y%m%d-%H%M%S-%3f").to_string();
    let destination = ensure_backup_root(home, true)?.join(stamp);
    fs::create_dir(&destination).map_err(|error| error.to_string())?;
    let result = (|| {
        let relative = "state_5.sqlite";
        let source = home.join(relative);
        if !source.is_file() {
            return Err(format!(
                "complete backup unavailable: {} is missing",
                source.display()
            ));
        }
        let connection = open_readonly(&source)?;
        validate_state_repair_schema(&connection)
            .map_err(|error| format!("{}: {error}", source.display()))?;
        let target = destination.join(relative);
        sqlite_online_copy(&source, &target)?;
        let files = vec![relative.to_string()];
        let projection_state = projection_state_path(home);
        let projection_state_present = match fs::symlink_metadata(&projection_state) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => true,
            Ok(_) => {
                return Err(format!(
                    "projection state is not a regular file: {}",
                    projection_state.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.to_string()),
        };
        let projection_state_sha256 = if projection_state_present {
            let target = destination.join("projection-state.json");
            fs::copy(&projection_state, &target).map_err(|error| error.to_string())?;
            sync_file_path(&target)?;
            Some(
                hash_file(&target)
                    .ok_or_else(|| format!("cannot hash projection state: {}", target.display()))?,
            )
        } else {
            None
        };
        let manifest_files = relevant_manifest_paths(home)?
            .into_iter()
            .map(|path| -> Result<ManifestFile, String> {
                let relative = path
                    .strip_prefix(home)
                    .map_err(|error| error.to_string())?
                    .to_string_lossy()
                    .replace('\\', "/");
                let backed_up = files.iter().any(|item| item == &relative);
                let recorded_path = if backed_up {
                    destination.join(relative.replace('/', "_"))
                } else {
                    path.clone()
                };
                let size = fs::metadata(&recorded_path)
                    .map_err(|error| format!("{}: {error}", recorded_path.display()))?
                    .len();
                let sha256 = if backed_up {
                    Some(hash_file(&recorded_path).ok_or_else(|| {
                        format!("cannot hash backup file: {}", recorded_path.display())
                    })?)
                } else {
                    None
                };
                Ok(ManifestFile {
                    path: relative,
                    size,
                    modified: iso_modified(&recorded_path),
                    sha256,
                    backed_up,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let sqlite_user_versions = ["state_5.sqlite"]
            .into_iter()
            .filter_map(|relative| {
                sqlite_user_version(&home.join(relative)).map(|version| (relative.into(), version))
            })
            .collect();
        let mut rollout_provider_preimages = rollout_updates
            .iter()
            .map(|update| rollout_journal_from_update(home, update))
            .collect::<Result<Vec<_>, _>>()?;
        rollout_provider_preimages.sort_by(|left, right| left.path.cmp(&right.path));
        let mut rollout_paths = HashSet::new();
        for row in &rollout_provider_preimages {
            if !rollout_paths.insert(row.path.as_str()) {
                return Err(format!(
                    "duplicate rollout in backup write set: {}",
                    row.path
                ));
            }
            let path = validate_rollout_journal_path(home, row)?;
            let current = read_rollout_provider_slot(&path, row.archived)?;
            if current != row.before_provider {
                return Err(format!(
                    "rollout changed before backup: {} (expected: {:?}, current: {:?})",
                    row.thread_id, row.before_provider, current
                ));
            }
        }
        let manifest = BackupManifest {
            version: 6,
            created_at: Local::now().to_rfc3339(),
            source: home.to_string_lossy().to_string(),
            provider: current_provider(home),
            sqlite_user_versions,
            projection_state_present,
            projection_state_sha256,
            rollout_provider_preimages,
            kind,
            pinned,
            files: manifest_files,
        };
        let manifest_path = destination.join("manifest.json");
        let manifest_temporary = destination.join(".manifest.tmp");
        let mut manifest_file =
            File::create(&manifest_temporary).map_err(|error| error.to_string())?;
        manifest_file
            .write_all(&serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
        manifest_file
            .sync_all()
            .map_err(|error| error.to_string())?;
        drop(manifest_file);
        platform::atomic_replace_file(&manifest_temporary, &manifest_path)?;
        Ok(BackupResult {
            path: destination.to_string_lossy().to_string(),
            files: if projection_state_present {
                files
                    .into_iter()
                    .chain(["projection-state.json".into()])
                    .collect()
            } else {
                files
            },
            manifest: manifest_path.to_string_lossy().to_string(),
            cleanup: None,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&destination);
    }
    result
}

fn discard_aborted_repair_backup(backup: &BackupResult, reason: &str) -> String {
    match fs::remove_dir_all(&backup.path) {
        Ok(()) => reason.into(),
        Err(error) => format!(
            "{reason}; failed to discard the unused repair snapshot {}: {error}",
            backup.path
        ),
    }
}

#[cfg(test)]
fn create_backup_at_with_rollout_updates(
    home: &Path,
    rollout_updates: &[RolloutUpdate],
) -> Result<BackupResult, String> {
    create_backup_at_with_kind(home, rollout_updates, BackupKind::Automatic, false)
}

fn create_backup_at_with_kind(
    home: &Path,
    rollout_updates: &[RolloutUpdate],
    kind: BackupKind,
    pinned: bool,
) -> Result<BackupResult, String> {
    validate_repair_schema(home)?;
    let _write_fence = acquire_dual_sqlite_write_fence(home, false)?;
    create_backup_snapshot_with_kind(home, None, None, rollout_updates, kind, pinned)
}

pub fn create_backup_at(home: &Path) -> Result<BackupResult, String> {
    create_backup_at_with_kind(home, &[], BackupKind::Manual, true)
}

pub fn create_backup_safe_at(home: &Path) -> Result<BackupResult, String> {
    if let Some(pending) = load_pending_operation(home)? {
        return Err(format!(
            "backup blocked until incomplete {} is recovered from {}",
            pending.command, pending.backup_path
        ));
    }
    let blockers = platform::blocking_processes(home)?;
    if !blockers.is_empty() {
        return Err(format!(
            "backup blocked by active SQLite owners: {}",
            blockers
                .iter()
                .map(|process| format!("{} ({})", process.identity.name, process.identity.pid))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let _guard = platform::acquire_operation_lock(home, "backup")?;
    if load_pending_operation(home)?.is_some() {
        return Err(
            "backup blocked because recovery became required while waiting for the operation lock"
                .into(),
        );
    }
    let blockers = platform::blocking_processes(home)?;
    if !blockers.is_empty() {
        return Err("backup aborted because a SQLite owner started after locking".into());
    }
    let mut backup = create_backup_at(home)?;
    let external_owners = platform::blocking_processes(home)?
        .into_iter()
        .filter(|process| !process.identity.is_current)
        .collect::<Vec<_>>();
    if !external_owners.is_empty() {
        fs::remove_dir_all(&backup.path).map_err(|error| {
            format!(
                "backup discarded because a SQLite owner appeared during the snapshot; unsafe snapshot cleanup failed: {error}"
            )
        })?;
        return Err("backup discarded because a SQLite owner appeared during the snapshot".into());
    }
    backup.cleanup = Some(
        cleanup_backups_unlocked(home, false, &[PathBuf::from(&backup.path)]).unwrap_or_else(
            |error| BackupCleanupResult {
                warnings: vec![error],
                ..BackupCleanupResult::default()
            },
        ),
    );
    Ok(backup)
}

fn latest_backup(home: &Path) -> Option<PathBuf> {
    let root = ensure_backup_root(home, false).ok()?;
    let mut candidates = fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| is_restorable_backup(&entry.path()))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|right| std::cmp::Reverse(right.file_name()));
    candidates
        .into_iter()
        .map(|entry| entry.path())
        .find(|path| validate_backup_integrity(home, path).is_ok())
}

fn is_restorable_backup(path: &Path) -> bool {
    let manifest = fs::read(path.join("manifest.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BackupManifest>(&bytes).ok());
    let Some(manifest) = manifest else {
        return false;
    };
    if !matches!(manifest.version, 4..=6) {
        return false;
    }
    let paths: HashSet<_> = manifest
        .files
        .iter()
        .filter(|file| file.backed_up)
        .map(|file| file.path.as_str())
        .collect();
    let projection_valid = !manifest.projection_state_present
        || (manifest.projection_state_sha256.is_some()
            && path.join("projection-state.json").is_file());
    let rollout_preimages_valid = manifest.rollout_provider_preimages.iter().all(|row| {
        !row.thread_id.trim().is_empty()
            && row
                .before_provider
                .as_deref()
                .is_none_or(provider_id_is_safe)
            && row
                .after_provider
                .as_deref()
                .is_none_or(provider_id_is_safe)
            && row.before_provider != row.after_provider
            && rollout_path_from_relative(Path::new("."), &row.path)
                .map(|(_, archived)| archived == row.archived)
                .unwrap_or(false)
    });
    let expected_paths = if manifest.version == 6 {
        HashSet::from(["state_5.sqlite"])
    } else {
        HashSet::from([
            ".codex-global-state.json",
            "state_5.sqlite",
            "sqlite/codex-dev.db",
        ])
    };
    paths == expected_paths
        && projection_valid
        && rollout_preimages_valid
        && manifest
            .files
            .iter()
            .filter(|file| file.backed_up)
            .all(|file| {
                let backup_file = path.join(file.path.replace('/', "_"));
                file.sha256.is_some()
                    && fs::symlink_metadata(&backup_file)
                        .map(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
                        .unwrap_or(false)
            })
}

fn safe_backup_path(home: &Path, requested: Option<&Path>) -> Result<PathBuf, String> {
    let path = match requested {
        Some(value) if value.is_absolute() => value.to_path_buf(),
        Some(value) => home.join(value),
        None => latest_backup(home).ok_or("no backup found")?,
    };
    let root = ensure_backup_root(home, false)?;
    validate_backup_child(&root, &path)
}

fn restore_target_is_file(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(format!("restore target is a symlink: {}", path.display()))
        }
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(format!(
            "restore target is not a regular file: {}",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "restore target unavailable ({}): {error}",
            path.display()
        )),
    }
}

fn restore_projection_state(
    home: &Path,
    backup: &Path,
    manifest: &BackupManifest,
) -> Result<(), String> {
    let target = projection_state_path(home);
    if !manifest.projection_state_present {
        if target.exists() {
            fs::remove_file(&target).map_err(|error| error.to_string())?;
        }
        return Ok(());
    }
    let source = backup.join("projection-state.json");
    let metadata = fs::symlink_metadata(&source).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "unsafe projection state backup: {}",
            source.display()
        ));
    }
    let expected = manifest
        .projection_state_sha256
        .as_deref()
        .ok_or("projection state checksum is missing")?;
    let actual = hash_file(&source).ok_or("cannot hash projection state backup")?;
    if expected != actual {
        return Err("projection state checksum mismatch".into());
    }
    let store: ProjectionStore =
        serde_json::from_slice(&fs::read(&source).map_err(|error| error.to_string())?)
            .map_err(|error| format!("invalid projection state backup: {error}"))?;
    save_projection_store(home, &store)
}

fn restore_projection_bytes(home: &Path, previous: Option<&[u8]>) -> Result<(), String> {
    let path = projection_state_path(home);
    match previous {
        Some(bytes) => {
            let store: ProjectionStore = serde_json::from_slice(bytes)
                .map_err(|error| format!("invalid previous projection state: {error}"))?;
            save_projection_store(home, &store)
        }
        None if path.exists() => fs::remove_file(path).map_err(|error| error.to_string()),
        None => Ok(()),
    }
}

#[derive(Debug, Clone)]
struct RestoreRolloutImage {
    row: RepairRolloutJournal,
    path: PathBuf,
    entry_provider: Option<String>,
}

fn validate_backup_manifest_version(manifest: &BackupManifest) -> Result<(), String> {
    if matches!(manifest.version, 4..=6) {
        Ok(())
    } else {
        Err(format!(
            "unsupported backup manifest version: {}",
            manifest.version
        ))
    }
}

fn capture_restore_rollout_images(
    home: &Path,
    manifest: &BackupManifest,
) -> Result<Vec<RestoreRolloutImage>, String> {
    validate_backup_manifest_version(manifest)?;
    if manifest.version == 4 {
        if !manifest.rollout_provider_preimages.is_empty() {
            return Err("v4 backup cannot contain rollout provider preimages".into());
        }
        return Ok(Vec::new());
    }

    let mut rows = manifest.rollout_provider_preimages.clone();
    rows.sort_by(|left, right| left.path.cmp(&right.path));
    let mut paths = HashSet::new();
    let mut thread_ids = HashSet::new();
    let mut images = Vec::with_capacity(rows.len());
    for row in rows {
        if row.thread_id.trim().is_empty()
            || row
                .before_provider
                .as_deref()
                .is_some_and(|provider| !provider_id_is_safe(provider))
            || row
                .after_provider
                .as_deref()
                .is_some_and(|provider| !provider_id_is_safe(provider))
            || row.before_provider == row.after_provider
        {
            return Err(format!(
                "invalid rollout provider preimage: {}",
                row.thread_id
            ));
        }
        if !paths.insert(row.path.clone()) || !thread_ids.insert(row.thread_id.clone()) {
            return Err(format!(
                "duplicate rollout provider preimage: {}",
                row.thread_id
            ));
        }
        let path = validate_rollout_journal_path(home, &row)?;
        let entry_provider = read_rollout_provider_slot(&path, row.archived)?;
        if entry_provider
            .as_deref()
            .is_some_and(|provider| !provider_id_is_safe(provider))
        {
            return Err(format!(
                "unsafe current rollout provider for {}: {:?}",
                row.thread_id, entry_provider
            ));
        }
        images.push(RestoreRolloutImage {
            row,
            path,
            entry_provider,
        });
    }
    Ok(images)
}

fn apply_restore_rollout_images(home: &Path, images: &[RestoreRolloutImage]) -> Result<(), String> {
    for image in images {
        rewrite_rollout_provider(
            home,
            &image.row,
            &image.entry_provider,
            &image.row.before_provider,
        )?;
    }
    Ok(())
}

fn recover_restore_rollout_images(
    home: &Path,
    images: &[RestoreRolloutImage],
) -> Result<(), String> {
    let mut errors = Vec::new();
    for image in images.iter().rev() {
        let current = match read_rollout_provider_slot(&image.path, image.row.archived) {
            Ok(current) => current,
            Err(error) => {
                errors.push(format!("{}: {error}", image.row.thread_id));
                continue;
            }
        };
        if current == image.entry_provider {
            continue;
        }
        if current != image.row.before_provider {
            errors.push(format!(
                "{} changed during restore recovery (entry: {:?}, restore: {:?}, current: {:?})",
                image.row.thread_id, image.entry_provider, image.row.before_provider, current
            ));
            continue;
        }
        if let Err(error) = rewrite_rollout_provider(
            home,
            &image.row,
            &image.row.before_provider,
            &image.entry_provider,
        ) {
            errors.push(format!("{}: {error}", image.row.thread_id));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn safety_rollout_updates_for_restore(
    home: &Path,
    manifest: &BackupManifest,
) -> Result<Vec<RolloutUpdate>, String> {
    capture_restore_rollout_images(home, manifest).map(|images| {
        images
            .into_iter()
            .map(|image| RolloutUpdate {
                thread_id: image.row.thread_id,
                path: image.path,
                archived: image.row.archived,
                expected_provider: image.entry_provider,
                provider: image.row.before_provider,
            })
            .collect()
    })
}

fn restore_backup_v6(home: &Path, backup: &Path, manifest: &BackupManifest) -> Result<(), String> {
    let state = home.join("state_5.sqlite");
    let metadata = fs::symlink_metadata(&state)
        .map_err(|error| format!("restore target unavailable ({}): {error}", state.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("unsafe state restore target: {}", state.display()));
    }
    probe_sqlite_write_lock(&state)?;

    let backed_up = manifest
        .files
        .iter()
        .filter(|file| file.backed_up)
        .collect::<Vec<_>>();
    if backed_up.len() != 1 || backed_up[0].path != "state_5.sqlite" {
        return Err("v6 backup manifest must contain only the official state database".into());
    }
    let state_backup = backup.join("state_5.sqlite");
    let backup_metadata = fs::symlink_metadata(&state_backup)
        .map_err(|error| format!("backup file missing ({}): {error}", state_backup.display()))?;
    if backup_metadata.file_type().is_symlink() || !backup_metadata.is_file() {
        return Err(format!("unsafe state backup: {}", state_backup.display()));
    }
    let expected_hash = backed_up[0]
        .sha256
        .as_deref()
        .ok_or("state backup checksum is missing")?;
    if hash_file(&state_backup).as_deref() != Some(expected_hash) {
        return Err("state backup checksum mismatch".into());
    }
    let source = open_readonly(&state_backup)?;
    validate_state_repair_schema(&source)?;
    drop(source);

    let restore_rollouts = capture_restore_rollout_images(home, manifest)?;
    let previous_projection = match fs::read(projection_state_path(home)) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("cannot read current projection state: {error}")),
    };
    let temporary = ensure_backup_root(home, false)?.join(format!(
        ".restore-before-{}",
        Local::now().format("%Y%m%d%H%M%S%3f")
    ));
    fs::create_dir(&temporary).map_err(|error| error.to_string())?;
    let previous_state = temporary.join("state_5.sqlite");
    let mut guard = match StateSqliteRestoreGuard::acquire(home) {
        Ok(guard) => guard,
        Err(error) => {
            let _ = fs::remove_dir_all(&temporary);
            return Err(error);
        }
    };
    if let Err(error) = guard.save_current(&previous_state) {
        drop(guard);
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }

    let apply_result = (|| {
        guard.restore(&state_backup)?;
        apply_restore_rollout_images(home, &restore_rollouts)?;
        restore_projection_state(home, backup, manifest)?;
        guard.validate()
    })();
    if let Err(error) = apply_result {
        let state_recovery = guard
            .restore(&previous_state)
            .and_then(|_| guard.validate());
        let projection_recovery = restore_projection_bytes(home, previous_projection.as_deref());
        let rollout_recovery = recover_restore_rollout_images(home, &restore_rollouts);
        drop(guard);
        let _ = fs::remove_dir_all(&temporary);
        return match (state_recovery, projection_recovery, rollout_recovery) {
            (Ok(()), Ok(()), Ok(())) => Err(error),
            (state, projection, rollouts) => Err(format!(
                "restore failed: {error}; state recovery: {}; projection recovery: {}; rollout recovery: {}",
                state.err().unwrap_or_else(|| "ok".into()),
                projection.err().unwrap_or_else(|| "ok".into()),
                rollouts.err().unwrap_or_else(|| "ok".into())
            )),
        };
    }
    drop(guard);
    let _ = fs::remove_dir_all(&temporary);
    Ok(())
}

fn restore_backup_unchecked_with_options(
    home: &Path,
    requested: Option<&Path>,
    restore_global_state: bool,
) -> Result<(), String> {
    let backup = safe_backup_path(home, requested)?;
    let manifest: BackupManifest = serde_json::from_slice(
        &fs::read(backup.join("manifest.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    validate_backup_manifest_version(&manifest)?;
    if manifest.version == 6 {
        return restore_backup_v6(home, &backup, &manifest);
    }
    // Legacy snapshots restore both databases in place.
    ensure_home_sqlite_paths(home)?;
    for path in [
        home.join("state_5.sqlite"),
        home.join("sqlite/codex-dev.db"),
    ] {
        probe_sqlite_write_lock(&path)?;
    }
    let restore_rollouts = capture_restore_rollout_images(home, &manifest)?;
    let files = manifest
        .files
        .iter()
        .filter(|file| file.backed_up)
        .collect::<Vec<_>>();
    let write_set: HashSet<_> = files.iter().map(|file| file.path.as_str()).collect();
    if files.len() != 3
        || write_set
            != HashSet::from([
                ".codex-global-state.json",
                "state_5.sqlite",
                "sqlite/codex-dev.db",
            ])
    {
        return Err("backup manifest does not contain the complete repair write set".into());
    }
    let mut sources = Vec::new();
    for file in &files {
        if !matches!(
            file.path.as_str(),
            ".codex-global-state.json" | "state_5.sqlite" | "sqlite/codex-dev.db"
        ) {
            return Err(format!(
                "backup manifest contains unsafe path: {}",
                file.path
            ));
        }
        let source = backup.join(file.path.replace('/', "_"));
        if !source.is_file() {
            return Err(format!("backup file missing: {}", source.display()));
        }
        if fs::symlink_metadata(&source)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(true)
        {
            return Err(format!("backup file is a symlink: {}", source.display()));
        }
        let expected_hash = file
            .sha256
            .as_deref()
            .ok_or_else(|| format!("backup checksum missing: {}", source.display()))?;
        let actual_hash = hash_file(&source)
            .ok_or_else(|| format!("cannot hash backup file: {}", source.display()))?;
        if expected_hash != actual_hash {
            return Err(format!("backup checksum mismatch: {}", source.display()));
        }
        if file.path == ".codex-global-state.json" {
            serde_json::from_slice::<Value>(&fs::read(&source).map_err(|error| error.to_string())?)
                .map_err(|error| format!("invalid global state backup: {error}"))?;
        } else {
            let source_connection = open_readonly(&source)?;
            sqlite_quick_check(&source_connection)?;
        }
        sources.push((file.path.clone(), source));
    }
    let state_backup = sources
        .iter()
        .find(|(relative, _)| relative == "state_5.sqlite")
        .map(|(_, path)| path.as_path())
        .ok_or("state_5.sqlite backup missing")?;
    let catalog_backup = sources
        .iter()
        .find(|(relative, _)| relative == "sqlite/codex-dev.db")
        .map(|(_, path)| path.as_path())
        .ok_or("codex-dev.db backup missing")?;
    let global_state_backup = sources
        .iter()
        .find(|(relative, _)| relative == ".codex-global-state.json")
        .map(|(_, path)| path.as_path())
        .ok_or("global state backup missing")?;
    validate_repair_schema_files(state_backup, catalog_backup)?;
    let previous_global_state = if restore_global_state {
        read_regular_global_state(home)?
    } else {
        Vec::new()
    };
    let previous_projection = match fs::read(projection_state_path(home)) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("cannot read current projection state: {error}")),
    };
    let temporary = ensure_backup_root(home, false)?.join(format!(
        ".restore-before-{}",
        Local::now().format("%Y%m%d%H%M%S%3f")
    ));
    fs::create_dir(&temporary).map_err(|error| error.to_string())?;
    for (relative, _) in &sources {
        let target = home.join(relative);
        let is_file = match restore_target_is_file(&target) {
            Ok(is_file) => is_file,
            Err(error) => {
                let _ = fs::remove_dir_all(&temporary);
                return Err(error);
            }
        };
        if !is_file {
            let _ = fs::remove_dir_all(&temporary);
            return Err(format!("restore target is missing: {}", target.display()));
        }
    }
    let previous_state = temporary.join("state_5.sqlite");
    let previous_catalog = temporary.join("sqlite_codex-dev.db");
    let mut restore_guard = match DualSqliteRestoreGuard::acquire(home) {
        Ok(guard) => guard,
        Err(error) => {
            let _ = fs::remove_dir_all(&temporary);
            return Err(error);
        }
    };
    if let Err(error) = restore_guard.save_current(&previous_state, &previous_catalog) {
        drop(restore_guard);
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }
    let apply_result = (|| {
        restore_guard.restore(state_backup, catalog_backup)?;
        if restore_global_state {
            write_global_state_exclusive(
                home,
                &fs::read(global_state_backup).map_err(|error| error.to_string())?,
            )?;
        }
        apply_restore_rollout_images(home, &restore_rollouts)?;
        restore_projection_state(home, &backup, &manifest)?;
        restore_guard.validate()?;
        let expected_global_hash = files
            .iter()
            .find(|file| file.path == ".codex-global-state.json")
            .and_then(|file| file.sha256.as_deref())
            .ok_or("global state checksum is missing")?;
        if restore_global_state
            && hash_bytes(&read_regular_global_state(home)?) != expected_global_hash
        {
            return Err("global state restore verification failed".into());
        }
        Ok::<(), String>(())
    })();
    if let Err(error) = apply_result {
        let recovery = restore_guard
            .restore(&previous_state, &previous_catalog)
            .and_then(|_| restore_guard.validate());
        let projection_recovery = restore_projection_bytes(home, previous_projection.as_deref());
        let global_state_recovery = if restore_global_state {
            write_global_state_exclusive(home, &previous_global_state)
        } else {
            Ok(())
        };
        let rollout_recovery = recover_restore_rollout_images(home, &restore_rollouts);
        drop(restore_guard);
        let _ = fs::remove_dir_all(&temporary);
        return match (
            recovery,
            projection_recovery,
            global_state_recovery,
            rollout_recovery,
        ) {
            (Ok(()), Ok(()), Ok(()), Ok(())) => Err(error),
            (database, projection, global_state, rollouts) => Err(format!(
                "restore failed: {error}; database recovery: {}; projection recovery: {}; global-state recovery: {}; rollout recovery: {}",
                database.err().unwrap_or_else(|| "ok".into()),
                projection.err().unwrap_or_else(|| "ok".into()),
                global_state.err().unwrap_or_else(|| "ok".into()),
                rollouts.err().unwrap_or_else(|| "ok".into())
            )),
        };
    }
    drop(restore_guard);
    let _ = fs::remove_dir_all(&temporary);
    Ok(())
}

fn restore_backup_unchecked(home: &Path, requested: Option<&Path>) -> Result<(), String> {
    restore_backup_unchecked_with_options(home, requested, true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JournalImageState {
    Before,
    After,
    Both,
    CompatibleAfter,
    Conflict,
}

fn journal_image_state<T: PartialEq>(current: &T, before: &T, after: &T) -> JournalImageState {
    if before == after && current == before {
        JournalImageState::Both
    } else if current == before {
        JournalImageState::Before
    } else if current == after {
        JournalImageState::After
    } else {
        JournalImageState::Conflict
    }
}

fn catalog_journal_image_state(
    current: &Option<RepairCatalogImage>,
    before: &Option<RepairCatalogImage>,
    after: &Option<RepairCatalogImage>,
) -> JournalImageState {
    match (current.as_ref(), before.as_ref(), after.as_ref()) {
        (None, None, None) => JournalImageState::Both,
        (Some(current), Some(before), Some(after)) => {
            let current_key = (&current.model_provider, current.missing_candidate);
            let before_key = (&before.model_provider, before.missing_candidate);
            let after_key = (&after.model_provider, after.missing_candidate);
            if before_key == after_key && current_key == before_key {
                JournalImageState::Both
            } else if current_key == before_key {
                JournalImageState::Before
            } else if current_key == after_key {
                if current == after {
                    JournalImageState::After
                } else {
                    JournalImageState::CompatibleAfter
                }
            } else {
                JournalImageState::Conflict
            }
        }
        (None, None, Some(_)) => JournalImageState::Before,
        (Some(current), None, Some(after)) if current == after => JournalImageState::After,
        (Some(current), Some(before), None) if current == before => JournalImageState::Before,
        (None, Some(_), None) => JournalImageState::After,
        _ => JournalImageState::Conflict,
    }
}

fn watermark_journal_image_state(current: i64, before: i64, after: i64) -> JournalImageState {
    if before == after && current == before {
        JournalImageState::Both
    } else if current == before {
        JournalImageState::Before
    } else if current == after {
        JournalImageState::After
    } else if current > after {
        JournalImageState::CompatibleAfter
    } else {
        JournalImageState::Conflict
    }
}

fn state_is_before(state: JournalImageState) -> bool {
    matches!(state, JournalImageState::Before | JournalImageState::Both)
}

fn state_is_after(state: JournalImageState) -> bool {
    matches!(
        state,
        JournalImageState::After | JournalImageState::Both | JournalImageState::CompatibleAfter
    )
}

fn read_state_provider(connection: &Connection, thread_id: &str) -> Result<Option<String>, String> {
    connection
        .query_row(
            "SELECT model_provider FROM threads WHERE id=?1",
            params![thread_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())
}

fn insert_catalog_journal_image(
    connection: &Connection,
    image: &RepairCatalogImage,
) -> Result<(), String> {
    connection
        .execute(
            "INSERT INTO provider_catalog.local_thread_catalog (host_id, thread_id, display_title, source_created_at, source_updated_at, cwd, source_kind, source_detail, model_provider, git_branch, observation_sequence, missing_candidate) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                image.host_id,
                image.thread_id,
                image.display_title,
                image.source_created_at,
                image.source_updated_at,
                image.cwd,
                image.source_kind,
                image.source_detail,
                image.model_provider,
                image.git_branch,
                image.observation_sequence,
                i64::from(image.missing_candidate)
            ],
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn workspace_hints_from_bytes(bytes: &[u8]) -> Result<HashMap<String, Value>, String> {
    let snapshot = global_state_snapshot_from_bytes(bytes);
    if !snapshot.readable {
        return Err("global state workspace hints are unreadable".into());
    }
    Ok(snapshot.thread_workspace_root_hints)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingRepairResolution {
    BeforeCommitCleared,
    CommittedFinalized,
    Compensated,
    VerificationFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingRecoveryIntent {
    Resume,
    Rollback,
}

fn recover_pending_repair(
    home: &Path,
    pending: &PendingOperation,
    intent: PendingRecoveryIntent,
) -> Result<PendingRepairResolution, String> {
    let journal = pending
        .repair_journal
        .as_ref()
        .ok_or("online repair pending journal is missing")?;
    if !matches!(journal.version, 2..=5) {
        return Err(format!(
            "unsupported online repair journal version: {}",
            journal.version
        ));
    }
    let rollback_requested = intent == PendingRecoveryIntent::Rollback;
    let compensating = pending.phase == Some(RepairPhase::Compensating) || rollback_requested;
    match pending.phase {
        Some(RepairPhase::VerificationFailed) if !rollback_requested => {
            return Ok(PendingRepairResolution::VerificationFailed);
        }
        Some(RepairPhase::Prepared)
        | Some(RepairPhase::Compensating)
        | Some(RepairPhase::Committed)
        | Some(RepairPhase::VerificationFailed)
        | None => {}
    }

    let include_catalog = !journal.catalog_rows.is_empty() || journal.catalog_watermarks.is_some();
    let mut fence = acquire_dual_sqlite_write_fence(home, include_catalog)?;
    let mut global_state = if journal.workspace_hints.is_empty() {
        None
    } else {
        Some(ExclusiveGlobalState::acquire(home, None)?)
    };
    let workspace_hints = match global_state.as_ref() {
        Some(global_state) => workspace_hints_from_bytes(global_state.bytes())?,
        None => HashMap::new(),
    };
    let current_projection = load_projection_store(home)?;
    let mut states = Vec::<(String, JournalImageState)>::new();
    for row in &journal.state_rows {
        let state = match &row.row_images {
            Some(images) => journal_image_state(
                &read_state_journal_image(&fence.connection, &row.thread_id)?,
                &images.before,
                &images.after,
            ),
            None => journal_image_state(
                &read_state_provider(&fence.connection, &row.thread_id)?,
                &Some(row.before_provider.clone()),
                &Some(row.after_provider.clone()),
            ),
        };
        states.push((format!("state:{}", row.thread_id), state));
    }
    for row in &journal.rollout_rows {
        states.push((
            format!("rollout:{}", row.thread_id),
            rollout_journal_state(home, row)?,
        ));
    }
    for row in &journal.catalog_rows {
        states.push((
            format!("catalog:{}:{}", row.host_id, row.thread_id),
            catalog_journal_image_state(
                &read_catalog_journal_image(&fence.connection, &row.host_id, &row.thread_id)?,
                &row.before,
                &row.after,
            ),
        ));
    }
    if let Some(watermarks) = &journal.catalog_watermarks {
        let (observation_sequence, catalog_revision) = read_catalog_watermarks(&fence.connection)?;
        states.push((
            "catalog:observation-sequence".into(),
            watermark_journal_image_state(
                observation_sequence,
                watermarks.before_observation_sequence,
                watermarks.after_observation_sequence,
            ),
        ));
        states.push((
            "catalog:revision".into(),
            watermark_journal_image_state(
                catalog_revision,
                watermarks.before_catalog_revision,
                watermarks.after_catalog_revision,
            ),
        ));
    }
    for hint in &journal.workspace_hints {
        states.push((
            format!("workspace:{}", hint.thread_id),
            journal_image_state(
                &RepairJsonSlot::from_option(workspace_hints.get(&hint.thread_id).cloned()),
                &hint.before,
                &hint.after,
            ),
        ));
    }
    states.push((
        "projection-state".into(),
        journal_image_state(
            &current_projection,
            &journal.projection_before,
            &Some(journal.projection_after.clone()),
        ),
    ));
    let all_before = states.iter().all(|(_, state)| state_is_before(*state));
    let all_after = states.iter().all(|(_, state)| state_is_after(*state));
    if all_before {
        drop(global_state);
        drop(fence);
        clear_pending_operation(home)?;
        return Ok(PendingRepairResolution::BeforeCommitCleared);
    }
    if all_after && !compensating {
        drop(global_state);
        drop(fence);
        clear_pending_operation(home)?;
        return Ok(PendingRepairResolution::CommittedFinalized);
    }

    let conflicts = states
        .iter()
        .filter(|(_, state)| matches!(state, JournalImageState::Conflict))
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    if pending.phase != Some(RepairPhase::Compensating) {
        let mut compensating_pending = pending.clone();
        compensating_pending.phase = Some(RepairPhase::Compensating);
        write_pending_operation(home, &compensating_pending)?;
    }

    for row in &journal.state_rows {
        match &row.row_images {
            Some(images) => {
                let current = read_state_journal_image(&fence.connection, &row.thread_id)?;
                if journal_image_state(&current, &images.before, &images.after)
                    == JournalImageState::After
                {
                    restore_state_journal_image(
                        &fence.connection,
                        &row.thread_id,
                        images.before.as_ref(),
                        images.after.as_ref(),
                    )?;
                }
            }
            None => {
                let current = read_state_provider(&fence.connection, &row.thread_id)?;
                if journal_image_state(
                    &current,
                    &Some(row.before_provider.clone()),
                    &Some(row.after_provider.clone()),
                ) == JournalImageState::After
                {
                    let changed = fence
                        .connection
                        .execute(
                            "UPDATE threads SET model_provider=?1 WHERE id=?2 AND model_provider=?3",
                            params![row.before_provider, row.thread_id, row.after_provider],
                        )
                        .map_err(|error| error.to_string())?;
                    if changed != 1 {
                        return Err(format!("state CAS recovery failed: {}", row.thread_id));
                    }
                }
            }
        }
    }
    for row in &journal.rollout_rows {
        if rollout_journal_state(home, row)? == JournalImageState::After {
            rewrite_rollout_provider(home, row, &row.after_provider, &row.before_provider)?;
        }
    }
    for row in &journal.catalog_rows {
        let current = read_catalog_journal_image(&fence.connection, &row.host_id, &row.thread_id)?;
        if state_is_after(catalog_journal_image_state(
            &current,
            &row.before,
            &row.after,
        )) {
            match (&row.before, &row.after) {
                (Some(before), Some(after)) => {
                    let changed = fence.connection.execute(
                        "UPDATE provider_catalog.local_thread_catalog SET model_provider=?1, missing_candidate=?2, observation_sequence=CASE WHEN observation_sequence=?3 THEN ?4 ELSE observation_sequence END WHERE host_id=?5 AND thread_id=?6 AND model_provider=?7 AND missing_candidate=?8",
                        params![
                            before.model_provider,
                            i64::from(before.missing_candidate),
                            after.observation_sequence,
                            before.observation_sequence,
                            row.host_id,
                            row.thread_id,
                            after.model_provider,
                            i64::from(after.missing_candidate),
                        ],
                    ).map_err(|error| error.to_string())?;
                    if changed != 1 {
                        return Err(format!(
                            "catalog CAS recovery failed: {}:{}",
                            row.host_id, row.thread_id
                        ));
                    }
                }
                (None, Some(after)) => {
                    let changed = fence.connection.execute(
                        "DELETE FROM provider_catalog.local_thread_catalog WHERE host_id=?1 AND thread_id=?2 AND display_title=?3 AND source_created_at=?4 AND source_updated_at=?5 AND cwd=?6 AND source_kind=?7 AND source_detail IS ?8 AND model_provider=?9 AND git_branch IS ?10 AND observation_sequence=?11 AND missing_candidate=?12",
                        params![
                            after.host_id,
                            after.thread_id,
                            after.display_title,
                            after.source_created_at,
                            after.source_updated_at,
                            after.cwd,
                            after.source_kind,
                            after.source_detail,
                            after.model_provider,
                            after.git_branch,
                            after.observation_sequence,
                            i64::from(after.missing_candidate),
                        ],
                    ).map_err(|error| error.to_string())?;
                    if changed != 1 {
                        return Err(format!(
                            "catalog insert CAS recovery failed: {}:{}",
                            row.host_id, row.thread_id
                        ));
                    }
                }
                (Some(before), None) => {
                    insert_catalog_journal_image(&fence.connection, before)?;
                }
                (None, None) => {}
            }
        }
    }
    if let Some(watermarks) = &journal.catalog_watermarks {
        let (observation_sequence, catalog_revision) = read_catalog_watermarks(&fence.connection)?;
        if watermark_journal_image_state(
            observation_sequence,
            watermarks.before_observation_sequence,
            watermarks.after_observation_sequence,
        ) == JournalImageState::After
        {
            let changed = fence.connection.execute(
                "UPDATE provider_catalog.local_thread_catalog_sync_state SET observation_sequence=?1 WHERE host_id='local' AND observation_sequence=?2",
                params![watermarks.before_observation_sequence, watermarks.after_observation_sequence],
            ).map_err(|error| error.to_string())?;
            if changed != 1 {
                return Err("catalog observation sequence CAS recovery failed".into());
            }
        }
        if watermark_journal_image_state(
            catalog_revision,
            watermarks.before_catalog_revision,
            watermarks.after_catalog_revision,
        ) == JournalImageState::After
        {
            let changed = fence.connection.execute(
                "UPDATE provider_catalog.local_thread_catalog_metadata SET catalog_revision=?1 WHERE id=1 AND catalog_revision=?2",
                params![watermarks.before_catalog_revision, watermarks.after_catalog_revision],
            ).map_err(|error| error.to_string())?;
            if changed != 1 {
                return Err("catalog revision CAS recovery failed".into());
            }
        }
    }

    if let Some(global_state) = global_state.as_mut() {
        let mut global_value: Value =
            serde_json::from_slice(global_state.bytes()).map_err(|error| error.to_string())?;
        let object = global_value
            .as_object_mut()
            .ok_or("global state root is not a JSON object")?;
        let hints = object
            .entry("thread-workspace-root-hints")
            .or_insert_with(|| Value::Object(Default::default()))
            .as_object_mut()
            .ok_or("thread-workspace-root-hints is not a JSON object")?;
        let mut workspace_changed = false;
        for hint in &journal.workspace_hints {
            let current = RepairJsonSlot::from_option(hints.get(&hint.thread_id).cloned());
            if journal_image_state(&current, &hint.before, &hint.after) == JournalImageState::After
            {
                match hint.before.to_option() {
                    Some(before) => {
                        hints.insert(hint.thread_id.clone(), before);
                    }
                    None => {
                        hints.remove(&hint.thread_id);
                    }
                }
                workspace_changed = true;
            }
        }
        if workspace_changed {
            global_state.overwrite(
                &serde_json::to_vec(&global_value).map_err(|error| error.to_string())?,
            )?;
        }
    }
    if journal_image_state(
        &current_projection,
        &journal.projection_before,
        &Some(journal.projection_after.clone()),
    ) == JournalImageState::After
    {
        match &journal.projection_before {
            Some(store) => save_projection_store(home, store)?,
            None => restore_projection_bytes(home, None)?,
        }
    }
    fence.commit()?;
    drop(global_state);
    drop(fence);
    if conflicts.is_empty() {
        clear_pending_operation(home)?;
        Ok(PendingRepairResolution::Compensated)
    } else {
        Err(format!(
            "online repair recovery reverted unchanged after-images and preserved externally changed values: {}",
            conflicts.join(", ")
        ))
    }
}

fn finish_failed_online_repair(
    home: &Path,
    pending: &PendingOperation,
    backup: &BackupResult,
    cause: &str,
) -> String {
    match recover_pending_repair(home, pending, PendingRecoveryIntent::Resume) {
        Ok(PendingRepairResolution::BeforeCommitCleared)
        | Ok(PendingRepairResolution::Compensated) => {
            discard_aborted_repair_backup(backup, &format!("{cause}; all partial changes were reverted"))
        }
        Ok(PendingRepairResolution::CommittedFinalized) => format!(
            "{cause}; the SQLite commit was durable and the prepared journal was finalized. The repair snapshot remains at {}",
            backup.path
        ),
        Ok(PendingRepairResolution::VerificationFailed) => format!(
            "{cause}; verification had already failed, so the journal and snapshot remain at {}",
            backup.path
        ),
        Err(recovery_error) => format!(
            "{cause}; online journal recovery could not finish: {recovery_error}. The journal and snapshot remain at {}",
            backup.path
        ),
    }
}

fn restore_backup_operation_at(home: &Path, requested: Option<&Path>) -> Result<(), String> {
    let _guard = platform::acquire_operation_lock(home, "restore")?;
    if let Some(pending) = load_pending_operation(home)? {
        if requested.is_some() {
            return Err(format!(
                "historical restore is blocked while an incomplete {} operation is pending; recover or roll back the pending operation first",
                pending.command
            ));
        }
        if pending.command == "repair" {
            if pending.repair_journal.is_some() {
                if requested.is_none() {
                    return match recover_pending_repair(
                        home,
                        &pending,
                        PendingRecoveryIntent::Rollback,
                    )? {
                        PendingRepairResolution::BeforeCommitCleared
                        | PendingRepairResolution::Compensated => Ok(()),
                        PendingRepairResolution::CommittedFinalized => Err(
                            "the pending repair was finalized before the explicit rollback could run"
                                .into(),
                        ),
                        PendingRepairResolution::VerificationFailed => Err(
                            "the pending repair could not enter safe rollback mode".into(),
                        ),
                    };
                }
            } else if requested.is_none() {
                return Err(
                    "legacy repair pending state cannot be restored automatically; select an explicit backup for a full offline restore"
                        .into(),
                );
            }
        } else {
            let lock = inspect_lock(home);
            if !lock.active_processes.is_empty() {
                return Err(format!(
                    "restore blocked by active Codex processes: {}",
                    lock.active_processes.join(", ")
                ));
            }
            if !platform::blocking_processes(home)?.is_empty() {
                return Err("restore aborted because a SQLite owner started after locking".into());
            }
            let recovery = PathBuf::from(&pending.backup_path);
            let result = restore_backup_unchecked(home, Some(&recovery));
            if result.is_ok() {
                clear_pending_operation(home)?;
            }
            return result.map_err(|error| {
                format!(
                    "incomplete {} recovery from {} failed: {error}",
                    pending.command, pending.backup_path
                )
            });
        }
    }

    let lock = inspect_lock(home);
    if !lock.active_processes.is_empty() {
        return Err(format!(
            "restore blocked by active Codex processes: {}",
            lock.active_processes.join(", ")
        ));
    }
    if !platform::blocking_processes(home)?.is_empty() {
        return Err("restore aborted because a SQLite owner started after locking".into());
    }

    let target = safe_backup_path(home, requested)?;
    let target_manifest: BackupManifest = serde_json::from_slice(
        &fs::read(target.join("manifest.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("invalid backup manifest: {error}"))?;
    validate_backup_manifest_version(&target_manifest)?;
    let _ = cleanup_backups_unlocked(home, false, std::slice::from_ref(&target));
    let safety_rollout_updates = safety_rollout_updates_for_restore(home, &target_manifest)?;
    let safety = create_backup_at_with_kind(
        home,
        &safety_rollout_updates,
        BackupKind::RestoreSafety,
        false,
    )?;
    if !platform::blocking_processes(home)?.is_empty() {
        fs::remove_dir_all(&safety.path).map_err(|error| {
            format!(
                "restore aborted because a SQLite owner appeared during safety backup; unsafe snapshot cleanup failed: {error}"
            )
        })?;
        return Err("restore aborted because a SQLite owner appeared during safety backup; discarded the snapshot".into());
    }
    save_pending_operation(home, "restore", Path::new(&safety.path))?;
    let result = restore_backup_unchecked(home, Some(&target));
    match result {
        Ok(()) => {
            clear_pending_operation(home)?;
            let _ = cleanup_backups_unlocked(home, false, &[target, PathBuf::from(&safety.path)]);
            Ok(())
        }
        Err(error) => {
            let recovery = restore_backup_unchecked(home, Some(Path::new(&safety.path)));
            match recovery {
                Ok(()) => {
                    clear_pending_operation(home)?;
                    Err(format!(
                        "restore failed and previous state was recovered: {error}"
                    ))
                }
                Err(recovery_error) => Err(format!(
                    "restore failed: {error}; previous-state recovery failed: {recovery_error}"
                )),
            }
        }
    }
}

fn successful_restore_result() -> VerifyResult {
    VerifyResult {
        ok: true,
        checked: 1,
        remaining: 0,
        skipped: 0,
        reasons: Vec::new(),
    }
}

pub fn restore_backup_at(home: &Path, requested: Option<&Path>) -> Result<VerifyResult, String> {
    restore_backup_operation_at(home, requested)?;
    Ok(successful_restore_result())
}

fn sort_reasons(reasons: &mut [SkipReason]) {
    reasons.sort_by(|left, right| {
        left.thread_id
            .cmp(&right.thread_id)
            .then_with(|| left.reason.cmp(&right.reason))
    });
}

fn build_plan(snapshot: &Snapshot, target_provider: &str) -> RepairPlan {
    let target_value = canonical_provider(target_provider);
    let cohorts = session_cohorts(snapshot);
    let mut plan = RepairPlan {
        state_updates: Vec::new(),
        state_restores: Vec::new(),
        state_inserts: Vec::new(),
        state_deletes: Vec::new(),
        rollout_updates: Vec::new(),
        catalog_updates: Vec::new(),
        catalog_inserts: Vec::new(),
        catalog_deletes: Vec::new(),
        workspace_hint_updates: Vec::new(),
        expected_global_state_sha256: None,
        changed_ids: HashSet::new(),
        skipped: Vec::new(),
    };
    for (thread, state_present) in rollout_backed_threads(snapshot) {
        if let Some(reason) = repair_exclusion_reason(snapshot, &cohorts, &thread) {
            plan.skipped.push(SkipReason {
                thread_id: Some(thread.id.clone()),
                reason,
            });
            continue;
        }
        let Some(rollout_path) = snapshot.rollout_paths.get(&thread.id) else {
            plan.skipped.push(SkipReason {
                thread_id: Some(thread.id.clone()),
                reason: "rollout_path_missing".into(),
            });
            continue;
        };
        let primary = snapshot
            .primary_rollouts
            .get(&thread.id)
            .expect("rollout-backed thread has primary metadata");
        let rollout_provider = match snapshot.rollout_provider_values.get(&thread.id) {
            Some(provider) => Some(provider.clone()),
            None if primary.provider_field_missing => None,
            None => {
                plan.skipped.push(SkipReason {
                    thread_id: Some(thread.id.clone()),
                    reason: "rollout_provider_missing_or_invalid".into(),
                });
                continue;
            }
        };
        let desired_archived = primary.archived;
        let desired_rollout_path = rollout_path.to_string_lossy().to_string();
        let has_user_message = !thread.first_user_message.trim().is_empty()
            || primary
                .first_user_message
                .as_deref()
                .is_some_and(|message| !message.trim().is_empty());
        let desired_has_user_event = thread.has_user_event || has_user_message;
        let desired_thread_source = if thread.thread_source.trim().is_empty() && has_user_message {
            "user".to_string()
        } else {
            thread.thread_source.clone()
        };
        if !snapshot.threads_readable {
            plan.skipped.push(SkipReason {
                thread_id: Some(thread.id.clone()),
                reason: "state_unreadable".into(),
            });
        } else if !state_present {
            let Some(insert) = state_insert_from_rollout(snapshot, &thread, target_provider) else {
                plan.skipped.push(SkipReason {
                    thread_id: Some(thread.id.clone()),
                    reason: "rollout_state_metadata_missing".into(),
                });
                continue;
            };
            plan.state_inserts.push(insert);
            plan.changed_ids.insert(thread.id.clone());
        } else if thread.provider != target_value
            || thread.rollout_path != desired_rollout_path
            || thread.archived != desired_archived
            || thread.thread_source != desired_thread_source
            || thread.has_user_event != desired_has_user_event
        {
            plan.state_updates.push(StateUpdate {
                thread_id: thread.id.clone(),
                expected_rollout_path: thread.rollout_path.clone(),
                expected_provider: thread.provider.clone(),
                expected_archived: thread.archived,
                expected_thread_source: thread.thread_source.clone(),
                expected_has_user_event: thread.has_user_event,
                rollout_path: desired_rollout_path,
                provider: target_value.clone(),
                archived: desired_archived,
                thread_source: desired_thread_source,
                has_user_event: desired_has_user_event,
            });
            plan.changed_ids.insert(thread.id.clone());
        }
        if rollout_provider.as_deref() != Some(target_value.as_str()) {
            plan.rollout_updates.push(RolloutUpdate {
                thread_id: thread.id.clone(),
                path: rollout_path.clone(),
                archived: desired_archived,
                expected_provider: rollout_provider,
                provider: Some(target_value.clone()),
            });
            plan.changed_ids.insert(thread.id.clone());
        }
    }
    sort_reasons(&mut plan.skipped);
    plan
}

fn build_plan_for_preview(
    snapshot: &Snapshot,
    target_provider: &str,
    preview: &ProviderPlanPreview,
) -> RepairPlan {
    let desired = preview
        .sessions
        .iter()
        .map(|session| session.thread_id.as_str())
        .collect::<HashSet<_>>();
    let mut plan = build_plan(snapshot, target_provider);
    plan.state_updates
        .retain(|update| desired.contains(update.thread_id.as_str()));
    plan.state_inserts
        .retain(|insert| desired.contains(insert.thread_id.as_str()));
    plan.rollout_updates
        .retain(|update| desired.contains(update.thread_id.as_str()));
    plan.catalog_updates
        .retain(|update| desired.contains(update.thread_id.as_str()));
    plan.catalog_inserts
        .retain(|insert| desired.contains(insert.thread_id.as_str()));
    plan.workspace_hint_updates
        .retain(|update| desired.contains(update.thread_id.as_str()));
    if plan.workspace_hint_updates.is_empty() {
        plan.expected_global_state_sha256 = None;
    }
    plan.changed_ids
        .retain(|thread_id| desired.contains(thread_id.as_str()));
    plan.skipped.retain(|reason| {
        reason
            .thread_id
            .as_deref()
            .is_none_or(|thread_id| desired.contains(thread_id))
    });
    plan
}

#[allow(dead_code)]
fn add_projection_reconciliation(
    snapshot: &Snapshot,
    store: Option<&ProjectionStore>,
    preview: &ProviderPlanPreview,
    plan: &mut RepairPlan,
    selected_thread_ids: Option<&HashSet<String>>,
) -> HashSet<String> {
    let Some(store) = store else {
        return HashSet::new();
    };
    let desired = preview
        .sessions
        .iter()
        .map(|session| session.thread_id.as_str())
        .collect::<HashSet<_>>();
    let threads = snapshot
        .threads
        .iter()
        .map(|thread| (thread.id.as_str(), thread))
        .collect::<HashMap<_, _>>();
    let mut catalog: HashMap<&str, Vec<&CatalogRow>> = HashMap::new();
    for row in snapshot
        .catalog
        .iter()
        .filter(|row| catalog_row_is_local(row))
    {
        catalog.entry(&row.thread_id).or_default().push(row);
    }
    let mut reconciled = HashSet::new();
    let cohorts = session_cohorts(snapshot);
    for record in store
        .threads
        .values()
        .filter(|record| !desired.contains(record.thread_id.as_str()))
        .filter(|record| {
            selected_thread_ids.is_none_or(|selected| selected.contains(record.thread_id.as_str()))
        })
    {
        let state_thread = threads.get(record.thread_id.as_str()).copied();
        let candidate_thread = state_thread.cloned().or_else(|| {
            snapshot
                .primary_rollouts
                .get(&record.thread_id)
                .map(rollout_thread_row)
        });
        let Some(candidate_thread) = candidate_thread else {
            plan.skipped.push(SkipReason {
                thread_id: Some(record.thread_id.clone()),
                reason: "projection_reconcile_rollout_missing".into(),
            });
            continue;
        };
        if let Some(reason) = repair_exclusion_reason(snapshot, &cohorts, &candidate_thread) {
            plan.skipped.push(SkipReason {
                thread_id: Some(record.thread_id.clone()),
                reason: format!("projection_reconcile_ineligible_{reason}"),
            });
            continue;
        }
        let state_provider = match state_thread {
            Some(thread) => match source_provider(&thread.provider) {
                Some(provider) => Some(provider),
                None => {
                    plan.skipped.push(SkipReason {
                        thread_id: Some(record.thread_id.clone()),
                        reason: "projection_reconcile_state_untrusted".into(),
                    });
                    continue;
                }
            },
            None => None,
        };
        let rows = catalog
            .get(record.thread_id.as_str())
            .cloned()
            .unwrap_or_default();
        if rows.len() > 1 {
            plan.skipped.push(SkipReason {
                thread_id: Some(record.thread_id.clone()),
                reason: "projection_reconcile_catalog_ambiguous".into(),
            });
            continue;
        }
        let current_row = rows.first().copied();
        let current_catalog = match current_row {
            None => CatalogState::Missing,
            Some(row) => match source_provider(&row.provider) {
                Some(provider) if row.missing_candidate => {
                    CatalogState::MissingCandidate { provider }
                }
                Some(provider) => CatalogState::Present { provider },
                None => {
                    plan.skipped.push(SkipReason {
                        thread_id: Some(record.thread_id.clone()),
                        reason: "projection_reconcile_catalog_untrusted".into(),
                    });
                    continue;
                }
            },
        };
        let Some(current_rollout_value) = snapshot.rollout_provider_values.get(&record.thread_id)
        else {
            plan.skipped.push(SkipReason {
                thread_id: Some(record.thread_id.clone()),
                reason: "projection_reconcile_rollout_provider_missing".into(),
            });
            continue;
        };
        let Some(current_rollout_provider) = source_provider(current_rollout_value) else {
            plan.skipped.push(SkipReason {
                thread_id: Some(record.thread_id.clone()),
                reason: "projection_reconcile_rollout_provider_untrusted".into(),
            });
            continue;
        };
        let rollout_is_original = record
            .original_rollout_provider
            .as_ref()
            .is_none_or(|provider| &current_rollout_provider == provider);
        let state_is_original = if record.original_state_present {
            state_provider.as_ref() == Some(&record.original_state_provider)
        } else {
            state_provider.is_none()
        };
        if state_is_original && current_catalog == record.original_catalog && rollout_is_original {
            reconciled.insert(record.thread_id.clone());
            continue;
        }
        let state_is_projected = state_thread.is_some_and(|thread| {
            state_provider.as_ref() == Some(&record.projected_target) && !thread.archived
        });
        let state_is_absent_original = !record.original_state_present && state_thread.is_none();
        if (!state_is_projected && !state_is_absent_original)
            || current_catalog != record.expected_projected_catalog()
            || record
                .original_rollout_provider
                .as_ref()
                .is_some_and(|_| current_rollout_provider != record.projected_target)
        {
            plan.skipped.push(SkipReason {
                thread_id: Some(record.thread_id.clone()),
                reason: "projection_reconcile_conflict".into(),
            });
            continue;
        }
        if record.original_state_present
            && state_provider.as_ref() != Some(&record.original_state_provider)
        {
            let thread = state_thread.expect("projected state row was verified as present");
            plan.state_restores.push(StateUpdate {
                thread_id: record.thread_id.clone(),
                expected_rollout_path: thread.rollout_path.clone(),
                expected_provider: thread.provider.clone(),
                expected_archived: thread.archived,
                expected_thread_source: thread.thread_source.clone(),
                expected_has_user_event: thread.has_user_event,
                rollout_path: thread.rollout_path.clone(),
                provider: canonical_provider(record.original_state_provider.as_str()),
                archived: false,
                thread_source: thread.thread_source.clone(),
                has_user_event: thread.has_user_event,
            });
        } else if !record.original_state_present {
            if let Some(thread) = state_thread {
                let Some(path) = snapshot.rollout_paths.get(&record.thread_id) else {
                    plan.skipped.push(SkipReason {
                        thread_id: Some(record.thread_id.clone()),
                        reason: "projection_reconcile_rollout_path_missing".into(),
                    });
                    continue;
                };
                plan.state_deletes.push(StateDelete {
                    thread_id: record.thread_id.clone(),
                    expected_provider: thread.provider.clone(),
                    expected_rollout_path: path.to_string_lossy().to_string(),
                });
            }
        }
        if let Some(original_rollout) = &record.original_rollout_provider {
            if &current_rollout_provider != original_rollout {
                let Some(path) = snapshot.rollout_paths.get(&record.thread_id) else {
                    plan.skipped.push(SkipReason {
                        thread_id: Some(record.thread_id.clone()),
                        reason: "projection_reconcile_rollout_path_missing".into(),
                    });
                    continue;
                };
                plan.rollout_updates.push(RolloutUpdate {
                    thread_id: record.thread_id.clone(),
                    path: path.clone(),
                    archived: false,
                    expected_provider: Some(current_rollout_value.clone()),
                    provider: Some(canonical_provider(original_rollout.as_str())),
                });
            }
        }
        let current_row = current_row.expect("projected catalog was verified as present");
        match &record.original_catalog {
            CatalogState::Missing => plan.catalog_deletes.push(CatalogDelete {
                host_id: "local".into(),
                thread_id: record.thread_id.clone(),
                expected_provider: current_row.provider.clone(),
                expected_missing_candidate: current_row.missing_candidate,
            }),
            CatalogState::MissingCandidate { provider } => {
                plan.catalog_updates.push(CatalogUpdate {
                    host_id: "local".into(),
                    thread_id: record.thread_id.clone(),
                    expected_provider: current_row.provider.clone(),
                    expected_missing_candidate: current_row.missing_candidate,
                    provider: canonical_provider(provider.as_str()),
                    missing_candidate: true,
                })
            }
            CatalogState::Present { provider } => plan.catalog_updates.push(CatalogUpdate {
                host_id: "local".into(),
                thread_id: record.thread_id.clone(),
                expected_provider: current_row.provider.clone(),
                expected_missing_candidate: current_row.missing_candidate,
                provider: canonical_provider(provider.as_str()),
                missing_candidate: false,
            }),
        }
        plan.changed_ids.insert(record.thread_id.clone());
        reconciled.insert(record.thread_id.clone());
    }
    sort_reasons(&mut plan.skipped);
    reconciled
}

fn verify_projection_reconciliation(
    snapshot: &Snapshot,
    store: Option<&ProjectionStore>,
    reconciled: &HashSet<String>,
) -> Result<(), String> {
    let Some(store) = store else {
        return reconciled
            .is_empty()
            .then_some(())
            .ok_or_else(|| "projection store disappeared during verification".into());
    };
    let threads = snapshot
        .threads
        .iter()
        .map(|thread| (thread.id.as_str(), thread))
        .collect::<HashMap<_, _>>();
    let catalog = snapshot
        .catalog
        .iter()
        .filter(|row| catalog_row_is_local(row))
        .map(|row| (row.thread_id.as_str(), row))
        .collect::<HashMap<_, _>>();
    for thread_id in reconciled {
        let record = store
            .threads
            .get(thread_id)
            .ok_or_else(|| format!("projection record disappeared: {thread_id}"))?;
        let thread = threads.get(thread_id.as_str()).copied();
        if record.original_state_present {
            let thread =
                thread.ok_or_else(|| format!("restored thread disappeared: {thread_id}"))?;
            if source_provider(&thread.provider) != Some(record.original_state_provider.clone()) {
                return Err(format!("state restore verification failed: {thread_id}"));
            }
        } else if thread.is_some() {
            return Err(format!(
                "state insert rollback verification failed: {thread_id}"
            ));
        }
        let current_catalog = match catalog.get(thread_id.as_str()) {
            None => CatalogState::Missing,
            Some(row) => {
                let provider = source_provider(&row.provider)
                    .ok_or_else(|| format!("catalog provider became untrusted: {thread_id}"))?;
                if row.missing_candidate {
                    CatalogState::MissingCandidate { provider }
                } else {
                    CatalogState::Present { provider }
                }
            }
        };
        if current_catalog != record.original_catalog {
            return Err(format!("catalog restore verification failed: {thread_id}"));
        }
        if let Some(original_rollout) = &record.original_rollout_provider {
            let current_rollout = snapshot
                .rollout_provider_values
                .get(thread_id)
                .and_then(|provider| source_provider(provider))
                .ok_or_else(|| format!("rollout restore verification failed: {thread_id}"))?;
            if &current_rollout != original_rollout {
                return Err(format!("rollout restore verification failed: {thread_id}"));
            }
        }
    }
    Ok(())
}

fn apply_state_updates(transaction: &Connection, updates: &[StateUpdate]) -> Result<usize, String> {
    let mut statement = transaction
        .prepare(
            "UPDATE threads \
             SET model_provider=?1, archived=?2, rollout_path=?3, thread_source=?4, has_user_event=?5 \
             WHERE id=?6 AND model_provider=?7 AND archived=?8 AND rollout_path=?9 \
               AND COALESCE(thread_source, '')=?10 AND COALESCE(has_user_event, 0)=?11",
        )
        .map_err(|error| error.to_string())?;
    let mut count = 0;
    for update in updates {
        count += statement
            .execute(params![
                update.provider,
                i64::from(update.archived),
                update.rollout_path,
                update.thread_source,
                i64::from(update.has_user_event),
                update.thread_id,
                update.expected_provider,
                i64::from(update.expected_archived),
                update.expected_rollout_path,
                update.expected_thread_source,
                i64::from(update.expected_has_user_event),
            ])
            .map_err(|error| error.to_string())?;
    }
    Ok(count)
}

fn timestamp_millis(seconds: i64) -> i64 {
    seconds.saturating_mul(1_000)
}

fn state_insert_value(insert: &StateInsert, column: &str) -> Option<SqlValue> {
    let optional_text =
        |value: &Option<String>| value.clone().map(SqlValue::Text).unwrap_or(SqlValue::Null);
    Some(match column {
        "id" => SqlValue::Text(insert.thread_id.clone()),
        "rollout_path" => SqlValue::Text(insert.rollout_path.clone()),
        "created_at" => SqlValue::Integer(insert.created_at),
        "created_at_ms" => SqlValue::Integer(timestamp_millis(insert.created_at)),
        "updated_at" | "recency_at" => SqlValue::Integer(insert.updated_at),
        "updated_at_ms" | "recency_at_ms" => SqlValue::Integer(timestamp_millis(insert.updated_at)),
        "source" => SqlValue::Text(insert.source.clone()),
        "thread_source" => SqlValue::Text(insert.thread_source.clone()),
        "model_provider" => SqlValue::Text(insert.provider.clone()),
        "cwd" => SqlValue::Text(insert.cwd.clone()),
        "title" => SqlValue::Text(insert.title.clone()),
        "preview" => SqlValue::Text(insert.preview.clone()),
        "sandbox_policy" => SqlValue::Text(insert.sandbox_policy.clone()),
        "approval_mode" => SqlValue::Text(insert.approval_mode.clone()),
        "tokens_used" => SqlValue::Integer(0),
        "has_user_event" => SqlValue::Integer(i64::from(insert.has_user_event)),
        "archived" => SqlValue::Integer(i64::from(insert.archived)),
        "git_sha" => optional_text(&insert.git_sha),
        "git_branch" => optional_text(&insert.git_branch),
        "git_origin_url" => optional_text(&insert.git_origin_url),
        "cli_version" => SqlValue::Text(insert.cli_version.clone()),
        "first_user_message" => SqlValue::Text(insert.first_user_message.clone()),
        "agent_nickname" | "agent_role" => SqlValue::Null,
        "memory_mode" => SqlValue::Text("enabled".into()),
        _ => return None,
    })
}

fn quoted_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn apply_state_inserts(connection: &Connection, inserts: &[StateInsert]) -> Result<usize, String> {
    if inserts.is_empty() {
        return Ok(0);
    }
    let columns = table_column_info(connection, "threads")?;
    let selected = columns
        .iter()
        .filter(|column| state_insert_supports_column(&column.name))
        .map(|column| column.name.as_str())
        .collect::<Vec<_>>();
    let names = selected
        .iter()
        .map(|name| quoted_identifier(name))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders = (1..=selected.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("INSERT INTO threads ({names}) VALUES ({placeholders})");
    let mut count = 0;
    for insert in inserts {
        let values = selected
            .iter()
            .map(|column| {
                state_insert_value(insert, column)
                    .ok_or_else(|| format!("unsupported threads insert column selected: {column}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        count += connection
            .execute(&sql, rusqlite::params_from_iter(values))
            .map_err(|error| format!("state insert failed for {}: {error}", insert.thread_id))?;
    }
    Ok(count)
}

fn apply_state_deletes(connection: &Connection, deletes: &[StateDelete]) -> Result<usize, String> {
    let mut count = 0;
    for delete in deletes {
        count += connection
            .execute(
                "DELETE FROM threads WHERE id=?1 AND model_provider=?2 AND rollout_path=?3",
                params![
                    delete.thread_id,
                    delete.expected_provider,
                    delete.expected_rollout_path
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(count)
}

fn repair_sql_value(value: SqlValueRef<'_>) -> RepairSqlValue {
    match value {
        SqlValueRef::Null => RepairSqlValue::Null,
        SqlValueRef::Integer(value) => RepairSqlValue::Integer(value),
        SqlValueRef::Real(value) => RepairSqlValue::Real(value),
        SqlValueRef::Text(value) => {
            RepairSqlValue::Text(String::from_utf8_lossy(value).into_owned())
        }
        SqlValueRef::Blob(value) => RepairSqlValue::Blob(value.to_vec()),
    }
}

fn sqlite_value(value: &RepairSqlValue) -> SqlValue {
    match value {
        RepairSqlValue::Null => SqlValue::Null,
        RepairSqlValue::Integer(value) => SqlValue::Integer(*value),
        RepairSqlValue::Real(value) => SqlValue::Real(*value),
        RepairSqlValue::Text(value) => SqlValue::Text(value.clone()),
        RepairSqlValue::Blob(value) => SqlValue::Blob(value.clone()),
    }
}

fn read_state_journal_image(
    connection: &Connection,
    thread_id: &str,
) -> Result<Option<RepairStateImage>, String> {
    let mut statement = connection
        .prepare("SELECT * FROM threads WHERE id=?1")
        .map_err(|error| error.to_string())?;
    let columns = statement
        .column_names()
        .iter()
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();
    statement
        .query_row(params![thread_id], |row| {
            let mut values = BTreeMap::new();
            for (index, column) in columns.iter().enumerate() {
                values.insert(column.clone(), repair_sql_value(row.get_ref(index)?));
            }
            Ok(RepairStateImage { values })
        })
        .optional()
        .map_err(|error| error.to_string())
}

fn repair_state_keys(plan: &RepairPlan) -> Vec<String> {
    let mut keys = BTreeSet::new();
    keys.extend(
        plan.state_updates
            .iter()
            .map(|update| update.thread_id.clone()),
    );
    keys.extend(
        plan.state_restores
            .iter()
            .map(|update| update.thread_id.clone()),
    );
    keys.extend(
        plan.state_inserts
            .iter()
            .map(|insert| insert.thread_id.clone()),
    );
    keys.extend(
        plan.state_deletes
            .iter()
            .map(|delete| delete.thread_id.clone()),
    );
    keys.into_iter().collect()
}

fn capture_state_journal_images(
    connection: &Connection,
    keys: &[String],
) -> Result<HashMap<String, Option<RepairStateImage>>, String> {
    keys.iter()
        .map(|thread_id| {
            Ok((
                thread_id.clone(),
                read_state_journal_image(connection, thread_id)?,
            ))
        })
        .collect()
}

fn state_image_provider(image: Option<&RepairStateImage>) -> Option<String> {
    image
        .and_then(|image| image.values.get("model_provider"))
        .and_then(|value| match value {
            RepairSqlValue::Text(value) => Some(value.clone()),
            _ => None,
        })
}

fn insert_state_journal_image(
    connection: &Connection,
    image: &RepairStateImage,
) -> Result<(), String> {
    let columns = image.values.keys().cloned().collect::<Vec<_>>();
    let names = columns
        .iter()
        .map(|column| quoted_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders = (1..=columns.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let values = columns
        .iter()
        .map(|column| sqlite_value(&image.values[column]))
        .collect::<Vec<_>>();
    let changed = connection
        .execute(
            &format!("INSERT INTO threads ({names}) VALUES ({placeholders})"),
            rusqlite::params_from_iter(values),
        )
        .map_err(|error| error.to_string())?;
    (changed == 1)
        .then_some(())
        .ok_or_else(|| "state image insert did not create exactly one row".into())
}

fn restore_state_journal_image(
    connection: &Connection,
    thread_id: &str,
    before: Option<&RepairStateImage>,
    after: Option<&RepairStateImage>,
) -> Result<(), String> {
    match (before, after) {
        (None, Some(_)) => {
            let changed = connection
                .execute("DELETE FROM threads WHERE id=?1", params![thread_id])
                .map_err(|error| error.to_string())?;
            if changed != 1 {
                return Err(format!("state insert CAS recovery failed: {thread_id}"));
            }
        }
        (Some(before), None) => insert_state_journal_image(connection, before)?,
        (Some(before), Some(_)) => {
            let columns = before
                .values
                .keys()
                .filter(|column| column.as_str() != "id")
                .cloned()
                .collect::<Vec<_>>();
            let assignments = columns
                .iter()
                .enumerate()
                .map(|(index, column)| format!("{}=?{}", quoted_identifier(column), index + 1))
                .collect::<Vec<_>>()
                .join(", ");
            let mut values = columns
                .iter()
                .map(|column| sqlite_value(&before.values[column]))
                .collect::<Vec<_>>();
            values.push(SqlValue::Text(thread_id.to_string()));
            let changed = connection
                .execute(
                    &format!(
                        "UPDATE threads SET {assignments} WHERE id=?{}",
                        values.len()
                    ),
                    rusqlite::params_from_iter(values),
                )
                .map_err(|error| error.to_string())?;
            if changed != 1 {
                return Err(format!("state row CAS recovery failed: {thread_id}"));
            }
        }
        (None, None) => {}
    }
    Ok(())
}

fn read_catalog_journal_image(
    connection: &Connection,
    host_id: &str,
    thread_id: &str,
) -> Result<Option<RepairCatalogImage>, String> {
    connection
        .query_row(
            "SELECT host_id, thread_id, display_title, source_created_at, source_updated_at, cwd, source_kind, source_detail, model_provider, git_branch, observation_sequence, missing_candidate FROM provider_catalog.local_thread_catalog WHERE host_id=?1 AND thread_id=?2",
            params![host_id, thread_id],
            |row| {
                Ok(RepairCatalogImage {
                    host_id: row.get(0)?,
                    thread_id: row.get(1)?,
                    display_title: row.get(2)?,
                    source_created_at: row.get(3)?,
                    source_updated_at: row.get(4)?,
                    cwd: row.get(5)?,
                    source_kind: row.get(6)?,
                    source_detail: row.get(7)?,
                    model_provider: row.get(8)?,
                    git_branch: row.get(9)?,
                    observation_sequence: row.get(10)?,
                    missing_candidate: row.get::<_, i64>(11)? != 0,
                })
            },
        )
        .optional()
        .map_err(|error| error.to_string())
}

fn repair_catalog_keys(plan: &RepairPlan) -> Vec<(String, String)> {
    let mut keys = BTreeSet::new();
    for update in &plan.catalog_updates {
        keys.insert((update.host_id.clone(), update.thread_id.clone()));
    }
    for insert in &plan.catalog_inserts {
        keys.insert((insert.host_id.clone(), insert.thread_id.clone()));
    }
    for delete in &plan.catalog_deletes {
        keys.insert((delete.host_id.clone(), delete.thread_id.clone()));
    }
    keys.into_iter().collect()
}

fn capture_catalog_journal_images(
    connection: &Connection,
    keys: &[(String, String)],
) -> Result<HashMap<(String, String), Option<RepairCatalogImage>>, String> {
    keys.iter()
        .map(|(host_id, thread_id)| {
            Ok((
                (host_id.clone(), thread_id.clone()),
                read_catalog_journal_image(connection, host_id, thread_id)?,
            ))
        })
        .collect()
}

fn read_catalog_watermarks(connection: &Connection) -> Result<(i64, i64), String> {
    let observation_sequence = connection
        .query_row(
            "SELECT observation_sequence FROM provider_catalog.local_thread_catalog_sync_state WHERE host_id='local'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| format!("local catalog sync state unavailable: {error}"))?;
    let catalog_revision = connection
        .query_row(
            "SELECT catalog_revision FROM provider_catalog.local_thread_catalog_metadata WHERE id=1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| format!("local catalog metadata unavailable: {error}"))?;
    Ok((observation_sequence, catalog_revision))
}

#[allow(clippy::too_many_arguments)]
fn build_repair_journal(
    home: &Path,
    connection: &Connection,
    plan: &RepairPlan,
    state_before: &HashMap<String, Option<RepairStateImage>>,
    catalog_before: &HashMap<(String, String), Option<RepairCatalogImage>>,
    catalog_watermarks_before: Option<(i64, i64)>,
    projection_before: Option<&ProjectionStore>,
    projection_after: &ProjectionStore,
) -> Result<RepairJournal, String> {
    let mut state_rows = Vec::new();
    for thread_id in repair_state_keys(plan) {
        let before = state_before.get(&thread_id).cloned().unwrap_or(None);
        let after = read_state_journal_image(connection, &thread_id)?;
        state_rows.push(RepairStateJournal {
            thread_id,
            before_provider: state_image_provider(before.as_ref()).unwrap_or_default(),
            after_provider: state_image_provider(after.as_ref()).unwrap_or_default(),
            row_images: Some(RepairStateRowImages { before, after }),
        });
    }
    let mut rollout_rows = plan
        .rollout_updates
        .iter()
        .map(|update| rollout_journal_from_update(home, update))
        .collect::<Result<Vec<_>, _>>()?;
    rollout_rows.sort_by(|left, right| left.path.cmp(&right.path));

    let mut catalog_rows = Vec::new();
    for (host_id, thread_id) in repair_catalog_keys(plan) {
        let key = (host_id.clone(), thread_id.clone());
        catalog_rows.push(RepairCatalogJournal {
            host_id: host_id.clone(),
            thread_id: thread_id.clone(),
            before: catalog_before.get(&key).cloned().unwrap_or(None),
            after: read_catalog_journal_image(connection, &host_id, &thread_id)?,
        });
    }
    let workspace_hints = plan
        .workspace_hint_updates
        .iter()
        .map(|update| RepairWorkspaceJournal {
            thread_id: update.thread_id.clone(),
            before: RepairJsonSlot::from_option(update.expected_hint.clone()),
            after: RepairJsonSlot::Present(Value::String(update.workspace_root.clone())),
        })
        .collect();
    let catalog_watermarks = match catalog_watermarks_before {
        Some((before_observation_sequence, before_catalog_revision)) => {
            let (after_observation_sequence, after_catalog_revision) =
                read_catalog_watermarks(connection)?;
            Some(RepairCatalogWatermarkJournal {
                before_observation_sequence,
                after_observation_sequence,
                before_catalog_revision,
                after_catalog_revision,
            })
        }
        None => None,
    };
    Ok(RepairJournal {
        version: 5,
        target_provider: projection_after.target_provider.as_str().into(),
        state_rows,
        rollout_rows,
        catalog_rows,
        catalog_watermarks,
        workspace_hints,
        projection_before: projection_before.cloned(),
        projection_after: projection_after.clone(),
    })
}

#[cfg(test)]
fn apply_catalog_updates(
    transaction: &Connection,
    updates: &[CatalogUpdate],
    sequence: i64,
) -> Result<usize, String> {
    let mut count = 0;
    for update in updates {
        count += transaction.execute("UPDATE local_thread_catalog SET model_provider = ?1, observation_sequence = ?2, missing_candidate = ?3 WHERE host_id = ?4 AND thread_id = ?5 AND model_provider = ?6 AND missing_candidate = ?7", params![update.provider, sequence, i64::from(update.missing_candidate), update.host_id, update.thread_id, update.expected_provider, i64::from(update.expected_missing_candidate)]).map_err(|error| error.to_string())?;
    }
    Ok(count)
}

#[cfg(test)]
fn apply_catalog_deletes(
    transaction: &Connection,
    deletes: &[CatalogDelete],
) -> Result<usize, String> {
    let mut count = 0;
    for delete in deletes {
        count += transaction
            .execute(
                "DELETE FROM local_thread_catalog WHERE host_id=?1 AND thread_id=?2 AND model_provider=?3 AND missing_candidate=?4",
                params![
                    delete.host_id,
                    delete.thread_id,
                    delete.expected_provider,
                    i64::from(delete.expected_missing_candidate)
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(count)
}

#[cfg(test)]
fn apply_catalog_inserts(
    transaction: &Connection,
    inserts: &[CatalogInsert],
    sequence: i64,
) -> Result<usize, String> {
    let mut count = 0;
    for insert in inserts {
        count += transaction.execute("INSERT OR IGNORE INTO local_thread_catalog (host_id, thread_id, display_title, source_created_at, source_updated_at, cwd, source_kind, source_detail, model_provider, git_branch, observation_sequence, missing_candidate) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0)", params![insert.host_id, insert.thread_id, insert.title, insert.created_at, insert.updated_at, insert.cwd, insert.source_kind, insert.source_detail, insert.provider, insert.git_branch, sequence]).map_err(|error| error.to_string())?;
    }
    Ok(count)
}

#[cfg(test)]
fn table_exists(transaction: &Connection, table: &str) -> bool {
    transaction
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            params![table],
            |_| Ok(()),
        )
        .is_ok()
}

#[cfg(test)]
fn catalog_next_sequence(transaction: &Connection) -> Result<i64, String> {
    if table_exists(transaction, "local_thread_catalog_sync_state") {
        return transaction
            .query_row(
                "SELECT observation_sequence + 1 FROM local_thread_catalog_sync_state WHERE host_id='local'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| format!("local catalog sync state unavailable: {error}"));
    }
    Err("unsupported catalog schema: sync state table missing".into())
}

#[cfg(test)]
fn update_catalog_watermarks(transaction: &Connection, sequence: i64) -> Result<(), String> {
    let sync_changed = transaction
        .execute(
            "UPDATE local_thread_catalog_sync_state SET observation_sequence=?1 WHERE host_id='local'",
            params![sequence],
        )
        .map_err(|error| error.to_string())?;
    if sync_changed != 1 {
        return Err("local catalog sync state row missing".into());
    }
    let revision_changed = transaction
        .execute(
            "UPDATE local_thread_catalog_metadata SET catalog_revision=catalog_revision+1 WHERE id=1",
            [],
        )
        .map_err(|error| error.to_string())?;
    if revision_changed != 1 {
        return Err("local catalog metadata row missing".into());
    }
    Ok(())
}

fn apply_plan_with_fence(
    fence: &DualSqliteWriteFence,
    plan: &RepairPlan,
) -> Result<(usize, usize), String> {
    let connection = &fence.connection;
    let changed = apply_state_updates(connection, &plan.state_updates)?;
    if changed != plan.state_updates.len() {
        return Err(format!(
            "state update count mismatch: expected {}, got {changed}",
            plan.state_updates.len()
        ));
    }
    let restored = apply_state_updates(connection, &plan.state_restores)?;
    if restored != plan.state_restores.len() {
        return Err(format!(
            "state restore count mismatch: expected {}, got {restored}",
            plan.state_restores.len()
        ));
    }
    let deleted_state = apply_state_deletes(connection, &plan.state_deletes)?;
    if deleted_state != plan.state_deletes.len() {
        return Err(format!(
            "state delete count mismatch: expected {}, got {deleted_state}",
            plan.state_deletes.len()
        ));
    }
    let inserted_state = apply_state_inserts(connection, &plan.state_inserts)?;
    if inserted_state != plan.state_inserts.len() {
        return Err(format!(
            "state insert count mismatch: expected {}, got {inserted_state}",
            plan.state_inserts.len()
        ));
    }

    let has_catalog_changes = !plan.catalog_updates.is_empty()
        || !plan.catalog_inserts.is_empty()
        || !plan.catalog_deletes.is_empty();
    let mut catalog_updates = 0;
    let mut index_changes = 0;
    if has_catalog_changes {
        let sequence = connection
            .query_row(
                "SELECT observation_sequence + 1 FROM provider_catalog.local_thread_catalog_sync_state WHERE host_id='local'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| format!("local catalog sync state unavailable: {error}"))?;
        for update in &plan.catalog_updates {
            catalog_updates += connection.execute("UPDATE provider_catalog.local_thread_catalog SET model_provider = ?1, observation_sequence = ?2, missing_candidate = ?3 WHERE host_id = ?4 AND thread_id = ?5 AND model_provider = ?6 AND missing_candidate = ?7", params![update.provider, sequence, i64::from(update.missing_candidate), update.host_id, update.thread_id, update.expected_provider, i64::from(update.expected_missing_candidate)]).map_err(|error| error.to_string())?;
        }
        if catalog_updates != plan.catalog_updates.len() {
            return Err(format!(
                "catalog update count mismatch: expected {}, got {catalog_updates}",
                plan.catalog_updates.len()
            ));
        }
        let mut inserted = 0;
        for insert in &plan.catalog_inserts {
            inserted += connection.execute("INSERT OR IGNORE INTO provider_catalog.local_thread_catalog (host_id, thread_id, display_title, source_created_at, source_updated_at, cwd, source_kind, source_detail, model_provider, git_branch, observation_sequence, missing_candidate) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0)", params![insert.host_id, insert.thread_id, insert.title, insert.created_at, insert.updated_at, insert.cwd, insert.source_kind, insert.source_detail, insert.provider, insert.git_branch, sequence]).map_err(|error| error.to_string())?;
        }
        if inserted != plan.catalog_inserts.len() {
            return Err(format!(
                "catalog insert count mismatch: expected {}, got {inserted}",
                plan.catalog_inserts.len()
            ));
        }
        let mut deleted = 0;
        for delete in &plan.catalog_deletes {
            deleted += connection
                .execute(
                    "DELETE FROM provider_catalog.local_thread_catalog WHERE host_id=?1 AND thread_id=?2 AND model_provider=?3 AND missing_candidate=?4",
                    params![
                        delete.host_id,
                        delete.thread_id,
                        delete.expected_provider,
                        i64::from(delete.expected_missing_candidate)
                    ],
                )
                .map_err(|error| error.to_string())?;
        }
        if deleted != plan.catalog_deletes.len() {
            return Err(format!(
                "catalog delete count mismatch: expected {}, got {deleted}",
                plan.catalog_deletes.len()
            ));
        }
        let sync_changed = connection
            .execute(
                "UPDATE provider_catalog.local_thread_catalog_sync_state SET observation_sequence=?1 WHERE host_id='local'",
                params![sequence],
            )
            .map_err(|error| error.to_string())?;
        let revision_changed = connection
            .execute(
                "UPDATE provider_catalog.local_thread_catalog_metadata SET catalog_revision=catalog_revision+1 WHERE id=1",
                [],
            )
            .map_err(|error| error.to_string())?;
        if sync_changed != 1 || revision_changed != 1 {
            return Err("local catalog watermark rows are missing".into());
        }
        index_changes = inserted + deleted;
    }
    Ok((
        changed + restored + inserted_state + deleted_state + catalog_updates,
        index_changes,
    ))
}

#[cfg(test)]
fn apply_plan(home: &Path, plan: &RepairPlan) -> Result<(usize, usize), String> {
    ensure_home_sqlite_paths(home)?;
    apply_workspace_hint_updates(home, plan)?;
    let mut provider_count = 0;
    let mut index_count = 0;
    if !plan.state_updates.is_empty()
        || !plan.state_restores.is_empty()
        || !plan.state_inserts.is_empty()
        || !plan.state_deletes.is_empty()
    {
        let mut connection =
            Connection::open(home.join("state_5.sqlite")).map_err(|error| error.to_string())?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|error| error.to_string())?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let changed = apply_state_updates(&transaction, &plan.state_updates)?;
        if changed != plan.state_updates.len() {
            return Err(format!(
                "state update count mismatch: expected {}, got {changed}",
                plan.state_updates.len()
            ));
        }
        let restored = apply_state_updates(&transaction, &plan.state_restores)?;
        if restored != plan.state_restores.len() {
            return Err(format!(
                "state restore count mismatch: expected {}, got {restored}",
                plan.state_restores.len()
            ));
        }
        let deleted = apply_state_deletes(&transaction, &plan.state_deletes)?;
        if deleted != plan.state_deletes.len() {
            return Err(format!(
                "state delete count mismatch: expected {}, got {deleted}",
                plan.state_deletes.len()
            ));
        }
        let inserted = apply_state_inserts(&transaction, &plan.state_inserts)?;
        if inserted != plan.state_inserts.len() {
            return Err(format!(
                "state insert count mismatch: expected {}, got {inserted}",
                plan.state_inserts.len()
            ));
        }
        provider_count += changed + restored + inserted + deleted;
        transaction.commit().map_err(|error| error.to_string())?;
    }
    if !plan.catalog_updates.is_empty()
        || !plan.catalog_inserts.is_empty()
        || !plan.catalog_deletes.is_empty()
    {
        let mut connection = Connection::open(home.join("sqlite/codex-dev.db"))
            .map_err(|error| error.to_string())?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|error| error.to_string())?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let sequence = catalog_next_sequence(&transaction)?;
        let updated = apply_catalog_updates(&transaction, &plan.catalog_updates, sequence)?;
        if updated != plan.catalog_updates.len() {
            return Err(format!(
                "catalog update count mismatch: expected {}, got {updated}",
                plan.catalog_updates.len()
            ));
        }
        let inserted = apply_catalog_inserts(&transaction, &plan.catalog_inserts, sequence)?;
        if inserted != plan.catalog_inserts.len() {
            return Err(format!(
                "catalog insert count mismatch: expected {}, got {inserted}",
                plan.catalog_inserts.len()
            ));
        }
        let deleted = apply_catalog_deletes(&transaction, &plan.catalog_deletes)?;
        if deleted != plan.catalog_deletes.len() {
            return Err(format!(
                "catalog delete count mismatch: expected {}, got {deleted}",
                plan.catalog_deletes.len()
            ));
        }
        provider_count += updated;
        index_count += inserted + deleted;
        update_catalog_watermarks(&transaction, sequence)?;
        transaction.commit().map_err(|error| error.to_string())?;
    }
    Ok((provider_count, index_count))
}

fn verify_thread_ids(
    snapshot: &Snapshot,
    target_provider: &str,
    thread_ids: &HashSet<String>,
) -> VerifyResult {
    let target_value = canonical_provider(target_provider);
    let threads = snapshot
        .threads
        .iter()
        .map(|thread| (thread.id.as_str(), thread))
        .collect::<HashMap<_, _>>();
    let mut reasons = Vec::new();
    let mut remaining = 0;
    for thread_id in thread_ids {
        let Some(thread) = threads.get(thread_id.as_str()) else {
            remaining += 1;
            reasons.push(SkipReason {
                thread_id: Some(thread_id.clone()),
                reason: "thread_missing_after_write".into(),
            });
            continue;
        };
        let Some(primary) = snapshot.primary_rollouts.get(thread_id) else {
            remaining += 1;
            reasons.push(SkipReason {
                thread_id: Some(thread_id.clone()),
                reason: "rollout_missing_after_write".into(),
            });
            continue;
        };
        let expected_path = primary.path.to_string_lossy();
        let visibility_mismatch = !thread.first_user_message.trim().is_empty()
            && (!thread.has_user_event || thread.thread_source.trim().is_empty());
        let state_mismatch = thread.provider != target_value
            || thread.archived != primary.archived
            || thread.rollout_path != expected_path
            || visibility_mismatch;
        let rollout_issue = match snapshot.rollout_provider_values.get(thread_id) {
            None => Some("rollout_provider_missing"),
            Some(provider) if provider != &target_value => Some("rollout_provider_mismatch"),
            Some(_) => None,
        };
        if state_mismatch {
            reasons.push(SkipReason {
                thread_id: Some(thread_id.clone()),
                reason: "state_provider_mismatch".into(),
            });
        }
        if let Some(reason) = rollout_issue {
            reasons.push(SkipReason {
                thread_id: Some(thread_id.clone()),
                reason: reason.into(),
            });
        }
        if state_mismatch || rollout_issue.is_some() {
            remaining += 1;
        }
    }
    sort_reasons(&mut reasons);
    VerifyResult {
        ok: remaining == 0,
        checked: thread_ids.len(),
        remaining,
        skipped: 0,
        reasons,
    }
}

pub fn verify_projection_at(
    home: &Path,
    selected_sources: &[String],
    target_provider: &str,
    scope: ProjectionScope,
) -> Result<VerifyResult, String> {
    let preview = preview_projection_at(home, selected_sources, target_provider, scope)?;
    Ok(verify_projection_preview(preview))
}

pub fn verify_projection_selected_at(
    home: &Path,
    selected_sources: &[String],
    target_provider: &str,
    scope: ProjectionScope,
    selected_thread_ids: Option<&[String]>,
) -> Result<VerifyResult, String> {
    let preview = preview_projection_selected_at(
        home,
        selected_sources,
        target_provider,
        scope,
        selected_thread_ids,
    )?;
    Ok(verify_projection_preview(preview))
}

fn verify_projection_preview(preview: ProjectionPreviewResult) -> VerifyResult {
    let checked = preview.plan.considered
        + preview.reconcile_pending
        + preview.reconcile_conflicts
        + preview.workspace_conflicts;
    let remaining =
        preview.changed_threads + preview.reconcile_conflicts + preview.workspace_conflicts;
    let mut reasons = preview
        .plan
        .sessions
        .iter()
        .filter(|session| session.category != projection::PlanCategory::Aligned)
        .map(|session| SkipReason {
            thread_id: Some(session.thread_id.clone()),
            reason: format!("{:?}", session.category),
        })
        .collect::<Vec<_>>();
    if preview.reconcile_pending > 0 {
        reasons.push(SkipReason {
            thread_id: None,
            reason: format!(
                "{} out-of-scope projections need original values restored",
                preview.reconcile_pending
            ),
        });
    }
    if preview.changed_threads > 0 {
        reasons.push(SkipReason {
            thread_id: None,
            reason: format!(
                "{} selected sessions still require metadata or index repair",
                preview.changed_threads
            ),
        });
    }
    reasons.extend(preview.reconcile_reasons);
    reasons.extend(preview.workspace_conflict_reasons);
    VerifyResult {
        ok: remaining == 0,
        checked,
        remaining,
        skipped: preview.skipped,
        reasons,
    }
}

fn scan_result_for_snapshot(
    home: &Path,
    snapshot: &Snapshot,
    projection_store: Option<&ProjectionStore>,
    lock_detail: Option<LockSummary>,
) -> Result<ScanResult, String> {
    let cohorts = session_cohorts(snapshot);
    scan_result_for_snapshot_with_cohorts(
        home,
        snapshot,
        projection_store,
        lock_detail,
        &cohorts,
    )
}

fn scan_result_for_snapshot_with_cohorts(
    home: &Path,
    snapshot: &Snapshot,
    projection_store: Option<&ProjectionStore>,
    lock_detail: Option<LockSummary>,
    cohorts: &SessionCohorts,
) -> Result<ScanResult, String> {
    let current = current_provider(home);
    let thread_ids: HashSet<_> = snapshot.threads.iter().map(|row| row.id.clone()).collect();
    let local_rows: Vec<_> = snapshot
        .catalog
        .iter()
        .filter(|row| catalog_row_is_local(row))
        .collect();
    // Build all_ids once without cloning thread_ids first (insert threads directly).
    let mut all_ids = HashSet::with_capacity(
        thread_ids.len()
            + snapshot.catalog.len()
            + snapshot.rollouts.len()
            + snapshot.session_index.len(),
    );
    all_ids.extend(thread_ids.iter().cloned());
    all_ids.extend(snapshot.catalog.iter().map(|row| row.thread_id.clone()));
    all_ids.extend(snapshot.rollouts.iter().cloned());
    all_ids.extend(snapshot.session_index.iter().cloned());
    let orphaned_ids = all_ids
        .difference(&thread_ids)
        .filter(|id| !cohorts.remote_catalog_ids.contains(*id))
        .cloned()
        .collect::<HashSet<_>>();
    let automated_sessions = snapshot
        .threads
        .iter()
        .filter(|thread| {
            !thread.archived
                && (is_skipped_thread(thread).as_deref() == Some("subagent_or_automation")
                    || rollout_source_not_ordinary(snapshot, &thread.id))
        })
        .count();
    let skipped_state_ids = snapshot
        .threads
        .iter()
        .filter(|thread| !thread.archived)
        .filter(|thread| {
            is_skipped_thread(thread).is_some()
                || cohorts.remote_excluded_thread_ids.contains(&thread.id)
                || cohorts.missing_rollout_ids.contains(&thread.id)
        })
        .map(|thread| thread.id.clone())
        .collect::<HashSet<_>>();
    let missing_catalog = cohorts
        .recoverable_ids
        .difference(&cohorts.local_catalog_ids)
        .cloned()
        .collect::<HashSet<_>>();
    let rollout_provider_drift = snapshot
        .threads
        .iter()
        .filter(|thread| cohorts.recoverable_ids.contains(&thread.id))
        .filter(|thread| {
            snapshot
                .rollout_providers
                .get(&thread.id)
                .is_some_and(|providers| {
                    providers
                        .iter()
                        .any(|provider| provider != &thread.provider)
                })
        })
        .count();
    // O(n) index: avoid nested find over local catalog rows per thread.
    let mut local_row_by_thread: HashMap<&str, &CatalogRow> =
        HashMap::with_capacity(local_rows.len());
    for row in &local_rows {
        local_row_by_thread
            .entry(row.thread_id.as_str())
            .or_insert(*row);
    }
    let mut provider_drift_ids = HashSet::new();
    for thread in &snapshot.threads {
        if let Some(row) = local_row_by_thread.get(thread.id.as_str()) {
            if cohorts.recoverable_ids.contains(&thread.id)
                && !row.missing_candidate
                && thread.provider != row.provider
            {
                provider_drift_ids.insert(thread.id.clone());
            }
        }
    }
    let provider_drift = provider_drift_ids.len();
    let mut drift_ids = missing_catalog.clone();
    drift_ids.extend(provider_drift_ids);
    let current_source_provider =
        source_provider(&current).unwrap_or_else(|| SourceProvider::Other("unknown".into()));
    let (projection_sessions, _) = eligible_projection_sessions_with_cohorts(
        snapshot,
        projection_store,
        &current_source_provider,
        cohorts,
    );
    let mut provider_counts: HashMap<String, (usize, usize)> = HashMap::new();
    for session in &projection_sessions {
        let counts = provider_counts
            .entry(session.origin_provider.as_str().into())
            .or_default();
        counts.0 += 1;
        if let CatalogState::Present { provider } = &session.catalog {
            if *provider == session.state_provider && &current_source_provider == provider {
                counts.1 += 1;
            }
        }
    }
    let mut provider_ids = configured_providers(home);
    if let Some(current_provider) = configured_current_provider(home) {
        provider_ids.insert(current_provider);
    }
    provider_ids.extend(provider_counts.keys().cloned());
    provider_ids.extend(
        local_rows
            .iter()
            .filter_map(|row| validate_provider(&row.provider).ok()),
    );
    for thread_id in &cohorts.recoverable_ids {
        if let Some(providers) = snapshot.rollout_providers.get(thread_id) {
            provider_ids.extend(providers.iter().cloned());
        }
    }
    let providers = provider_ids
        .into_iter()
        .map(|id| {
            let (source_sessions, currently_visible) =
                provider_counts.get(&id).copied().unwrap_or_default();
            ProviderSummary {
                id: id.clone(),
                name: provider_name(&id),
                color: provider_color(&id).into(),
                source_sessions,
                currently_visible,
                status: if id == current {
                    "active".into()
                } else if source_sessions == 0 {
                    "available".into()
                } else {
                    "legacy".into()
                },
            }
        })
        .collect();
    let lock_detail = lock_detail.unwrap_or_else(|| inspect_lock(home));
    let mut sources = snapshot.sources.clone();
    sources.push(global_state_source(home));
    let needs_admin = lock_detail
        .active_processes
        .iter()
        .any(|process| process == "process-enumeration-failed")
        || sources.iter().any(|source| {
            let note = source.note.to_ascii_lowercase();
            note.contains("permission")
                || note.contains("access")
                || note.contains("denied")
                || note.contains("readonly")
                || note.contains("elevation")
                || ["拒绝访问", "需要提升权限", "不允许访问", "只读"]
                    .iter()
                    .any(|keyword| source.note.contains(keyword))
        });
    Ok(ScanResult {
        codex_home: home.to_string_lossy().to_string(),
        current_provider: current,
        providers,
        sessions: thread_ids.len(),
        discovered_sessions: all_ids.len(),
        orphaned_sessions: orphaned_ids.len(),
        archived_sessions: snapshot
            .threads
            .iter()
            .filter(|thread| thread.archived)
            .count(),
        ordinary_sessions: cohorts.ordinary_active_ids.len(),
        recoverable_sessions: cohorts.recoverable_ids.len(),
        recoverable_indexed: cohorts.recoverable_indexed_ids.len(),
        session_index_covered: cohorts.session_index_covered_ids.len(),
        remote_sessions: cohorts.remote_session_ids.len(),
        remote_excluded_sessions: cohorts.remote_excluded_thread_ids.len(),
        automated_sessions,
        rollout_sessions: snapshot.rollouts.len(),
        valid_rollout_sessions: snapshot.valid_active_rollouts.len()
            + snapshot.valid_archived_rollouts.len(),
        indexed: cohorts.local_catalog_ids.len(),
        session_indexed: snapshot.session_index.len(),
        drift: drift_ids.len(),
        provider_drift,
        rollout_provider_drift,
        missing_catalog: missing_catalog.len(),
        missing_rollout: cohorts.missing_rollout_ids.len(),
        skipped: skipped_state_ids.len()
            + orphaned_ids.difference(&cohorts.recoverable_ids).count(),
        sqlite: snapshot.sqlite_readable,
        jsonl: snapshot.jsonl_files.len(),
        lock: lock_detail.state.clone(),
        lock_detail,
        needs_admin,
        last_backup: latest_backup(home).map(|path| path.to_string_lossy().to_string()),
        pending_operation: load_pending_operation(home)?,
        sources,
    })
}

fn reconcile_pending_repair_on_startup(home: &Path) {
    let pending = match load_pending_operation(home) {
        Ok(Some(pending))
            if pending.command == "repair"
                && pending.repair_journal.is_some()
                && pending.phase != Some(RepairPhase::VerificationFailed) =>
        {
            pending
        }
        _ => return,
    };
    let Ok(_guard) = platform::acquire_operation_lock(home, "recover-repair") else {
        return;
    };
    let pending = match load_pending_operation(home) {
        Ok(Some(current))
            if current.command == pending.command
                && current.created_at == pending.created_at
                && current.repair_journal.is_some() =>
        {
            current
        }
        _ => return,
    };
    let _ = recover_pending_repair(home, &pending, PendingRecoveryIntent::Resume);
}

pub fn scan_at(home: &Path) -> Result<ScanResult, String> {
    reconcile_pending_repair_on_startup(home);
    let _ = maintain_backups_at(home);
    let snapshot = scan_snapshot(home);
    let projection_store = load_projection_store(home)?;
    scan_result_for_snapshot(home, &snapshot, projection_store.as_ref(), None)
}

pub fn refresh_desktop_at(
    home: &Path,
    _selected_sources: &[String],
    _target_provider: &str,
    _observed_provider: &str,
    _scope: ProjectionScope,
    initialize: bool,
) -> Result<DesktopRefreshResult, String> {
    // Common-path refresh: scan + list only. Full projection preview / plan_token
    // is computed on demand via preview_projection when recovery opens.
    reconcile_pending_repair_on_startup(home);
    let backup_cleanup = if initialize {
        match maintain_backups_at(home) {
            Ok(cleanup) => cleanup,
            Err(error) => Some(BackupCleanupResult {
                warnings: vec![error],
                ..BackupCleanupResult::default()
            }),
        }
    } else {
        None
    };
    let (snapshot, blocking_processes) = std::thread::scope(|scope| {
        let snapshot = scope.spawn(|| scan_snapshot(home));
        let processes = scope.spawn(|| platform::blocking_processes(home));
        (
            snapshot.join().expect("desktop snapshot scan panicked"),
            processes.join().expect("desktop process scan panicked"),
        )
    });
    let (blocking_processes, active_processes) = match blocking_processes {
        Ok(processes) => {
            let processes = processes
                .into_iter()
                .filter(|process| !process.identity.is_current)
                .collect::<Vec<_>>();
            let names = processes
                .iter()
                .map(|process| process.identity.name.clone())
                .collect();
            (processes, names)
        }
        Err(_) => (Vec::new(), vec!["process-enumeration-failed".into()]),
    };
    let lock_detail = lock_summary(home, active_processes);
    let projection_store = load_projection_store(home)?;
    // Shared cohorts for scan analytics + session list (avoid rebuilding twice).
    let cohorts = session_cohorts(&snapshot);
    let scan = scan_result_for_snapshot_with_cohorts(
        home,
        &snapshot,
        projection_store.as_ref(),
        Some(lock_detail),
        &cohorts,
    )?;
    let target_provider = configured_current_provider(home).ok_or_else(|| {
        "desktop repair unavailable: config.toml has no valid current model_provider".to_string()
    })?;
    let mut selected_sources = scan
        .providers
        .iter()
        .filter(|provider| provider.source_sessions > 0)
        .map(|provider| provider.id.clone())
        .collect::<Vec<_>>();
    if !selected_sources.contains(&target_provider) {
        selected_sources.push(target_provider.clone());
    }
    let local_sessions = local_session_summaries_with_cohorts(
        &snapshot,
        projection_store.as_ref(),
        &scan.current_provider,
        &cohorts,
    );
    Ok(DesktopRefreshResult {
        scan,
        preview: None,
        local_sessions,
        blocking_processes,
        selected_sources,
        target_provider,
        backup_cleanup,
    })
}

pub fn repair_projection_at(
    home: &Path,
    selected_sources: &[String],
    target_provider: &str,
    scope: ProjectionScope,
    dry_run: bool,
    require_plan_token: bool,
    expected_plan_token: Option<&str>,
) -> Result<RepairResult, String> {
    repair_projection_selected_at(
        home,
        selected_sources,
        target_provider,
        scope,
        None,
        dry_run,
        require_plan_token,
        expected_plan_token,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn repair_projection_selected_at(
    home: &Path,
    selected_sources: &[String],
    target_provider: &str,
    scope: ProjectionScope,
    selected_thread_ids: Option<&[String]>,
    dry_run: bool,
    require_plan_token: bool,
    expected_plan_token: Option<&str>,
) -> Result<RepairResult, String> {
    repair_projection_selected_at_with_progress(
        home,
        selected_sources,
        target_provider,
        scope,
        selected_thread_ids,
        dry_run,
        require_plan_token,
        expected_plan_token,
        |_| {},
    )
}

#[allow(clippy::too_many_arguments)]
pub fn repair_projection_selected_at_with_progress<F>(
    home: &Path,
    selected_sources: &[String],
    target_provider: &str,
    scope: ProjectionScope,
    selected_thread_ids: Option<&[String]>,
    dry_run: bool,
    require_plan_token: bool,
    expected_plan_token: Option<&str>,
    mut progress: F,
) -> Result<RepairResult, String>
where
    F: FnMut(RepairProgress),
{
    let mut reporter = RepairProgressReporter::new(&mut progress);
    repair_projection_selected_at_inner(
        home,
        selected_sources,
        target_provider,
        scope,
        selected_thread_ids,
        dry_run,
        require_plan_token,
        expected_plan_token,
        &mut reporter,
    )
}

#[allow(clippy::too_many_arguments)]
fn repair_projection_selected_at_inner(
    home: &Path,
    selected_sources: &[String],
    target_provider: &str,
    scope: ProjectionScope,
    selected_thread_ids: Option<&[String]>,
    dry_run: bool,
    require_plan_token: bool,
    expected_plan_token: Option<&str>,
    progress: &mut RepairProgressReporter<'_>,
) -> Result<RepairResult, String> {
    progress.report(
        RepairProgressStage::Planning,
        5,
        "Preparing the repair plan",
    );
    let target_provider = validate_current_target_provider(home, target_provider)?;
    let selected_thread_ids = selected_thread_filter(selected_thread_ids);
    if !dry_run && require_plan_token && expected_plan_token.is_none() {
        return Err("apply requires the plan token returned by the latest preview".into());
    }
    if !dry_run {
        let current = configured_current_provider(home)
            .ok_or_else(|| "config.toml has no valid current model_provider".to_string())?;
        if current != target_provider {
            return Err(format!(
                "target provider must match config.toml model_provider (current: {current})"
            ));
        }
    }
    // Apply/dry-run planning always re-reads disk; do not trust the short refresh cache.
    let snapshot = scan_snapshot_fresh(home);
    if !dry_run && !snapshot.threads_readable {
        return Err("repair unavailable: state_5.sqlite is not readable".into());
    }
    let store = load_projection_store(home)?;
    let (preview, _sessions) = projection_preview_for_snapshot(
        &snapshot,
        store.as_ref(),
        selected_sources,
        &target_provider,
        scope,
        selected_thread_ids.as_ref(),
    )?;
    let plan = build_plan_for_preview(&snapshot, &target_provider, &preview.plan);
    let reconciled: HashSet<String> = HashSet::new();
    let mut skipped_reasons = preview.skipped_reasons.clone();
    skipped_reasons.extend(plan.skipped.clone());
    let mut seen_skips = HashSet::new();
    skipped_reasons
        .retain(|reason| seen_skips.insert((reason.thread_id.clone(), reason.reason.clone())));
    sort_reasons(&mut skipped_reasons);
    if dry_run {
        let planned_sessions = preview.plan.sessions.len();
        progress.report_optional_count(
            RepairProgressStage::PlanValidated,
            90,
            "Repair preview is ready",
            planned_sessions,
            planned_sessions,
        );
        let result = RepairResult {
            changed_threads: plan.changed_ids.len(),
            restored_threads: reconciled.len(),
            state_updates: plan.state_updates.len()
                + plan.state_restores.len()
                + plan.state_inserts.len()
                + plan.state_deletes.len(),
            rollout_updates: plan.rollout_updates.len(),
            catalog_updates: plan.catalog_updates.len(),
            catalog_inserts: plan.catalog_inserts.len(),
            catalog_deletes: plan.catalog_deletes.len(),
            workspace_hint_updates: plan.workspace_hint_updates.len(),
            skipped: skipped_reasons.len(),
            skipped_reasons,
            dry_run: true,
            verified: false,
            backup_path: None,
            backup_cleanup: None,
            plan_token: Some(preview.plan_token),
            lock: inspect_lock(home).state,
            needs_admin: false,
        };
        progress.report(
            RepairProgressStage::Completed,
            100,
            "Repair preview completed",
        );
        return Ok(result);
    }

    progress.report(
        RepairProgressStage::AcquiringOperationLock,
        10,
        "Acquiring the repair operation lock",
    );
    let _lock = platform::acquire_operation_lock(home, "repair")?;
    if let Some(pending) = load_pending_operation(home)? {
        if pending.command == "repair" && pending.repair_journal.is_some() {
            match recover_pending_repair(home, &pending, PendingRecoveryIntent::Resume)? {
                PendingRepairResolution::VerificationFailed => {
                    return Err(
                        "repair blocked because the previous online commit failed verification; select its recorded backup explicitly if a full restore is required"
                            .into(),
                    );
                }
                PendingRepairResolution::BeforeCommitCleared
                | PendingRepairResolution::CommittedFinalized
                | PendingRepairResolution::Compensated => {}
            }
        } else {
            return Err(format!(
                "repair blocked by incomplete {} operation from {}",
                pending.command, pending.backup_path
            ));
        }
    }
    let pre_repair_cleanup = cleanup_backups_unlocked(home, false, &[]).ok();
    validate_repair_schema(home)?;
    let config_path = home.join("config.toml");
    let config_fingerprint = hash_file(&config_path).ok_or_else(|| {
        format!(
            "cannot fingerprint provider config: {}",
            config_path.display()
        )
    })?;

    // Recompute after acquiring the tool-level operation lock. A short SQLite
    // write fence is acquired only for the snapshot and final commit window.
    // Force a fresh scan so plan_token checks never reuse a pre-lock cache entry.
    let snapshot = scan_snapshot_fresh(home);
    if !snapshot.threads_readable {
        return Err("repair aborted: state_5.sqlite changed or became unreadable".into());
    }
    let store = load_projection_store(home)?;
    let (preview, sessions) = projection_preview_for_snapshot(
        &snapshot,
        store.as_ref(),
        selected_sources,
        &target_provider,
        scope,
        selected_thread_ids.as_ref(),
    )?;
    let plan = build_plan_for_preview(&snapshot, &target_provider, &preview.plan);
    let reconciled: HashSet<String> = HashSet::new();
    let mut skipped_reasons = preview.skipped_reasons.clone();
    skipped_reasons.extend(plan.skipped.clone());
    let mut seen_skips = HashSet::new();
    skipped_reasons
        .retain(|reason| seen_skips.insert((reason.thread_id.clone(), reason.reason.clone())));
    sort_reasons(&mut skipped_reasons);
    let has_reconcile_conflicts = preview.reconcile_conflicts > 0;
    let has_workspace_conflicts = preview.workspace_conflicts > 0;
    if let Some(expected) = expected_plan_token {
        if expected != preview.plan_token {
            return Err(
                "repair plan changed after preview; refresh the scan and preview before applying"
                    .into(),
            );
        }
    }
    let applied_plan_token = preview.plan_token.clone();
    let desired_ids = preview
        .plan
        .sessions
        .iter()
        .map(|session| session.thread_id.clone())
        .collect::<HashSet<_>>();
    let next_store = next_projection_store(
        store.as_ref(),
        &preview.plan,
        &sessions,
        &snapshot,
        &reconciled,
    )?;
    let planned_sessions = preview.plan.sessions.len();
    progress.report_optional_count(
        RepairProgressStage::PlanValidated,
        20,
        "Repair plan validated",
        planned_sessions,
        planned_sessions,
    );
    if plan.changed_ids.is_empty() {
        let projection_changed = store
            .as_ref()
            .is_some_and(|existing| existing != &next_store);
        if !reconciled.is_empty() {
            verify_projection_reconciliation(&snapshot, store.as_ref(), &reconciled)?;
        }
        if !reconciled.is_empty() || projection_changed {
            progress.report_counted(
                RepairProgressStage::MetadataSync,
                65,
                "Synchronizing recovery metadata",
                0,
                1,
            );
            if hash_file(&config_path).as_deref() != Some(config_fingerprint.as_str()) {
                return Err("repair aborted because config.toml changed after planning".into());
            }
            let provider_guard = acquire_target_provider_guard(home, &target_provider)?;
            if hash_file(&config_path).as_deref() != Some(config_fingerprint.as_str()) {
                drop(provider_guard);
                return Err(
                    "repair aborted because config.toml changed while acquiring the provider guard"
                        .into(),
                );
            }
            save_projection_store(home, &next_store)?;
            drop(provider_guard);
            progress.report_counted(
                RepairProgressStage::MetadataSync,
                75,
                "Recovery metadata synchronized",
                1,
                1,
            );
        }
        let verification_total = desired_ids.len();
        progress.report_optional_count(
            RepairProgressStage::Verification,
            90,
            "Verifying recovered sessions",
            0,
            verification_total,
        );
        let verification = verify_thread_ids(&snapshot, &target_provider, &desired_ids);
        progress.report_optional_count(
            RepairProgressStage::Verification,
            96,
            "Session verification completed",
            verification.checked,
            verification_total,
        );
        let result = RepairResult {
            changed_threads: 0,
            restored_threads: reconciled.len(),
            state_updates: 0,
            rollout_updates: 0,
            catalog_updates: 0,
            catalog_inserts: 0,
            catalog_deletes: 0,
            workspace_hint_updates: 0,
            skipped: skipped_reasons.len(),
            skipped_reasons,
            dry_run: false,
            verified: verification.ok && !has_reconcile_conflicts && !has_workspace_conflicts,
            backup_path: None,
            backup_cleanup: pre_repair_cleanup,
            plan_token: Some(applied_plan_token),
            lock: "clear".into(),
            needs_admin: false,
        };
        progress.report(
            RepairProgressStage::Completed,
            100,
            "Session recovery completed",
        );
        return Ok(result);
    }

    let state_updates = plan.state_updates.len()
        + plan.state_restores.len()
        + plan.state_inserts.len()
        + plan.state_deletes.len();
    let rollout_updates = plan.rollout_updates.len();
    let catalog_updates = plan.catalog_updates.len();
    let catalog_inserts = plan.catalog_inserts.len();
    let catalog_deletes = plan.catalog_deletes.len();
    let workspace_hint_updates = plan.workspace_hint_updates.len();
    let changed_threads = plan.changed_ids.len();
    let has_catalog_changes = !plan.catalog_updates.is_empty()
        || !plan.catalog_inserts.is_empty()
        || !plan.catalog_deletes.is_empty();
    progress.report(
        RepairProgressStage::AcquiringWriteFence,
        25,
        "Acquiring the SQLite write fence",
    );
    let mut write_fence = acquire_dual_sqlite_write_fence(home, has_catalog_changes)?;
    if !plan.state_inserts.is_empty() {
        if let Err(error) = validate_state_insert_schema(&write_fence.connection) {
            drop(write_fence);
            return Err(error);
        }
    }
    progress.report_counted(
        RepairProgressStage::Backup,
        32,
        "Creating a recovery backup",
        0,
        1,
    );
    let backup = match create_backup_snapshot(
        home,
        plan.expected_global_state_sha256.as_deref(),
        None,
        &plan.rollout_updates,
    ) {
        Ok(backup) => backup,
        Err(error) => {
            drop(write_fence);
            return Err(error);
        }
    };
    progress.report_counted(
        RepairProgressStage::Backup,
        40,
        "Recovery backup created",
        1,
        1,
    );

    let sqlite_updates = state_updates + catalog_updates + catalog_inserts + catalog_deletes;
    progress.report_optional_count(
        RepairProgressStage::SqliteStaging,
        45,
        "Staging SQLite session index updates",
        0,
        sqlite_updates,
    );
    let state_keys = repair_state_keys(&plan);
    let state_before = match capture_state_journal_images(&write_fence.connection, &state_keys) {
        Ok(images) => images,
        Err(error) => {
            drop(write_fence);
            return Err(discard_aborted_repair_backup(&backup, &error));
        }
    };
    let catalog_keys = repair_catalog_keys(&plan);
    let catalog_before =
        match capture_catalog_journal_images(&write_fence.connection, &catalog_keys) {
            Ok(images) => images,
            Err(error) => {
                drop(write_fence);
                return Err(discard_aborted_repair_backup(&backup, &error));
            }
        };
    let catalog_watermarks_before = if catalog_keys.is_empty() {
        None
    } else {
        match read_catalog_watermarks(&write_fence.connection) {
            Ok(watermarks) => Some(watermarks),
            Err(error) => {
                drop(write_fence);
                return Err(discard_aborted_repair_backup(&backup, &error));
            }
        }
    };
    if let Err(error) = apply_plan_with_fence(&write_fence, &plan) {
        drop(write_fence);
        return Err(discard_aborted_repair_backup(&backup, &error));
    }

    let journal = match build_repair_journal(
        home,
        &write_fence.connection,
        &plan,
        &state_before,
        &catalog_before,
        catalog_watermarks_before,
        store.as_ref(),
        &next_store,
    ) {
        Ok(journal) => journal,
        Err(error) => {
            drop(write_fence);
            return Err(discard_aborted_repair_backup(&backup, &error));
        }
    };
    progress.report_optional_count(
        RepairProgressStage::SqliteStaging,
        60,
        "SQLite session index updates staged",
        sqlite_updates,
        sqlite_updates,
    );
    if hash_file(&config_path).as_deref() != Some(config_fingerprint.as_str()) {
        drop(write_fence);
        return Err(discard_aborted_repair_backup(
            &backup,
            "repair aborted because config.toml changed after planning",
        ));
    }
    let provider_guard = match acquire_target_provider_guard(home, &target_provider) {
        Ok(guard) => guard,
        Err(error) => {
            drop(write_fence);
            return Err(discard_aborted_repair_backup(&backup, &error));
        }
    };
    if hash_file(&config_path).as_deref() != Some(config_fingerprint.as_str()) {
        drop(provider_guard);
        drop(write_fence);
        return Err(discard_aborted_repair_backup(
            &backup,
            "repair aborted because config.toml changed while acquiring the provider guard",
        ));
    }
    let mut global_state = if plan.workspace_hint_updates.is_empty() {
        None
    } else {
        match ExclusiveGlobalState::acquire(home, plan.expected_global_state_sha256.as_deref()) {
            Ok(global_state) => Some(global_state),
            Err(error) => {
                drop(provider_guard);
                drop(write_fence);
                return Err(discard_aborted_repair_backup(&backup, &error));
            }
        }
    };
    let mut pending = PendingOperation {
        command: "repair".into(),
        backup_path: backup.path.clone(),
        created_at: Local::now().to_rfc3339(),
        phase: Some(RepairPhase::Prepared),
        repair_journal: Some(journal),
    };
    if let Err(error) = write_pending_operation(home, &pending) {
        drop(global_state);
        drop(provider_guard);
        drop(write_fence);
        return Err(discard_aborted_repair_backup(&backup, &error));
    }

    let metadata_updates = rollout_updates + workspace_hint_updates + 1;
    progress.report_counted(
        RepairProgressStage::MetadataSync,
        65,
        "Synchronizing rollout and recovery metadata",
        0,
        metadata_updates,
    );
    let sidecar_result = (|| {
        apply_rollout_updates(home, &plan.rollout_updates)?;
        if let Some(global_state) = global_state.as_mut() {
            let updated = global_state.apply_workspace_hint_updates(&plan)?;
            if updated != plan.workspace_hint_updates.len() {
                return Err(format!(
                    "workspace hint update count mismatch: expected {}, got {updated}",
                    plan.workspace_hint_updates.len()
                ));
            }
        }
        save_projection_store(home, &next_store)
    })();
    if let Err(error) = sidecar_result {
        drop(global_state);
        drop(provider_guard);
        drop(write_fence);
        return Err(finish_failed_online_repair(
            home,
            &pending,
            &backup,
            &format!("repair failed before SQLite commit: {error}"),
        ));
    }
    progress.report_counted(
        RepairProgressStage::MetadataSync,
        75,
        "Rollout and recovery metadata synchronized",
        metadata_updates,
        metadata_updates,
    );
    progress.report_counted(
        RepairProgressStage::Commit,
        80,
        "Committing the SQLite repair transaction",
        0,
        1,
    );
    if let Err(error) = write_fence.commit() {
        drop(global_state);
        drop(provider_guard);
        drop(write_fence);
        return Err(finish_failed_online_repair(home, &pending, &backup, &error));
    }
    progress.report_counted(
        RepairProgressStage::Commit,
        85,
        "SQLite repair transaction committed",
        1,
        1,
    );
    drop(global_state);
    drop(provider_guard);
    drop(write_fence);

    pending.phase = Some(RepairPhase::Committed);
    let committed_phase_error = write_pending_operation(home, &pending).err();
    let verification_total = desired_ids.len();
    progress.report_optional_count(
        RepairProgressStage::Verification,
        90,
        "Verifying recovered sessions",
        0,
        verification_total,
    );
    invalidate_snapshot_cache();
    let after = scan_snapshot_fresh(home);
    let verification = verify_thread_ids(&after, &target_provider, &desired_ids);
    let reconciliation = verify_projection_reconciliation(&after, store.as_ref(), &reconciled);
    let result = if verification.ok && reconciliation.is_ok() {
        clear_pending_operation(home)?;
        progress.report_optional_count(
            RepairProgressStage::Verification,
            96,
            "Session verification completed",
            verification.checked,
            verification_total,
        );
        let backup_cleanup = Some(
            cleanup_backups_unlocked(home, false, &[PathBuf::from(&backup.path)]).unwrap_or_else(
                |error| BackupCleanupResult {
                    warnings: vec![error],
                    ..BackupCleanupResult::default()
                },
            ),
        );
        Ok(RepairResult {
            changed_threads,
            restored_threads: reconciled.len(),
            state_updates,
            rollout_updates,
            catalog_updates,
            catalog_inserts,
            catalog_deletes,
            workspace_hint_updates,
            skipped: skipped_reasons.len(),
            skipped_reasons,
            dry_run: false,
            verified: !has_reconcile_conflicts && !has_workspace_conflicts,
            backup_path: Some(backup.path),
            backup_cleanup,
            plan_token: Some(applied_plan_token),
            lock: "clear".into(),
            needs_admin: false,
        })
    } else {
        let reason = reconciliation
            .err()
            .unwrap_or_else(|| format!("{} selected records remain", verification.remaining));
        pending.phase = Some(RepairPhase::VerificationFailed);
        let phase_error = write_pending_operation(home, &pending).err();
        let journal_note = phase_error
            .map(|error| format!("; could not persist verificationFailed phase: {error}"))
            .unwrap_or_default();
        Err(format!(
            "verification failed after the online commit: {reason}. No automatic full-database restore was attempted because Codex may have written newer data. The snapshot remains at {}{}",
            backup.path, journal_note
        ))
    };
    let result = result.map_err(|mut error| {
        if let Some(phase_error) = committed_phase_error {
            error.push_str(&format!(
                "; could not persist committed journal phase: {phase_error}"
            ));
        }
        if error.to_ascii_lowercase().contains("permission")
            || error.to_ascii_lowercase().contains("access")
        {
            format!("{error}; administrator permission may be required")
        } else {
            error
        }
    })?;
    progress.report(
        RepairProgressStage::Completed,
        100,
        "Session recovery completed",
    );
    Ok(result)
}

pub fn restore_latest_at(home: &Path) -> Result<VerifyResult, String> {
    restore_backup_at(home, None)
}

pub fn run_cli() -> i32 {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "codex-provider-hub scan|backup|repair|verify|restore [BACKUP] \
             [--codex-home PATH] [--target-provider ID] \
             [--dry-run|--apply] [--plan-token TOKEN]\n\
             repair defaults to all sessions in dry-run mode; --apply requires the latest preview token"
        );
        return 0;
    }
    let command = args.remove(0);
    let mut home = default_codex_home();
    let mut target = None;
    let scope = ProjectionScope::All;
    let mut dry_run = true;
    let mut plan_token = None;
    let mut restore_path = None;
    let mut parse_error = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--codex-home" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    home = PathBuf::from(value);
                } else {
                    parse_error = Some("--codex-home requires a path".to_string());
                }
            }
            "--target-provider" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    target = Some(value.clone());
                } else {
                    parse_error = Some("--target-provider requires an ID".to_string());
                }
            }
            "--dry-run" => dry_run = true,
            "--apply" | "--write" => dry_run = false,
            "--plan-token" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    plan_token = Some(value.clone());
                } else {
                    parse_error = Some("--plan-token requires a token".to_string());
                }
            }
            value if command == "restore" && !value.starts_with('-') => {
                if restore_path.is_some() {
                    parse_error = Some("restore accepts at most one backup path".to_string());
                } else {
                    restore_path = Some(PathBuf::from(value));
                }
            }
            value => parse_error = Some(format!("unknown argument: {value}")),
        }
        if parse_error.is_some() {
            break;
        }
        index += 1;
    }
    let result: Result<Value, String> = if let Some(error) = parse_error {
        Err(error)
    } else {
        match command.as_str() {
            "scan" => scan_at(&home).map(|value| json!(value)),
            "backup" => create_backup_safe_at(&home).map(|value| json!(value)),
            "repair" => {
                let target = target.unwrap_or_else(|| current_provider(&home));
                if !dry_run && plan_token.is_none() {
                    Err("repair --apply requires --plan-token from a prior dry-run".into())
                } else {
                    repair_projection_at(
                        &home,
                        &[],
                        &target,
                        scope,
                        dry_run,
                        true,
                        plan_token.as_deref(),
                    )
                    .map(|value| json!(value))
                }
            }
            "verify" => {
                let target = target.unwrap_or_else(|| current_provider(&home));
                verify_projection_at(&home, &[], &target, scope).map(|value| json!(value))
            }
            "restore" => restore_backup_at(&home, restore_path.as_deref())
                .map(|verification| json!({ "ok": true, "verification": verification })),
            _ => Err(format!("unknown command: {command}")),
        }
    };
    match result {
        Ok(value) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into())
            );
            0
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "provider-hub-core-{}-{nonce}-{sequence}",
            std::process::id()
        ))
    }

    fn make_fixture() -> PathBuf {
        make_fixture_at(fixture())
    }

    fn make_fixture_at(home: PathBuf) -> PathBuf {
        fs::create_dir_all(home.join("sessions/2026/07/13")).unwrap();
        fs::create_dir_all(home.join("archived_sessions")).unwrap();
        fs::create_dir_all(home.join("sqlite")).unwrap();
        fs::write(
            home.join("config.toml"),
            "model_provider = \"openai\"\n[model_providers.custom]\nname = \"Custom\"\n",
        )
        .unwrap();
        fs::write(
            home.join(".codex-global-state.json"),
            r#"{"electron-saved-workspace-roots":["C:\\work"],"thread-workspace-root-hints":{}}"#,
        )
        .unwrap();
        for (id, minute) in [
            ("thread-one", "00"),
            ("thread-two", "01"),
            ("thread-three", "02"),
            ("thread-subagent", "03"),
            ("thread-explicit-remote", "04"),
        ] {
            fs::write(home.join(format!("sessions/2026/07/13/{id}.jsonl")), format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"timestamp\":\"2026-07-13T00:{minute}:00Z\",\"model_provider\":\"custom\",\"cwd\":\"C:\\\\work\\\\{id}\",\"source\":\"cli\"}}}}\n")).unwrap();
        }
        fs::write(home.join("archived_sessions/thread-archived.jsonl"), "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-archived\",\"timestamp\":\"2026-07-12T23:59:00Z\",\"model_provider\":\"custom\"}}\n").unwrap();
        fs::write(
            home.join("session_index.jsonl"),
            "{\"id\":\"thread-one\"}\n{\"id\":\"thread-two\"}\n{\"id\":\"thread-three\"}\n",
        )
        .unwrap();
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        state.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, source TEXT NOT NULL, model_provider TEXT NOT NULL, cwd TEXT NOT NULL, title TEXT NOT NULL, archived INTEGER NOT NULL DEFAULT 0, agent_role TEXT, thread_source TEXT, first_user_message TEXT NOT NULL DEFAULT '', has_user_event INTEGER NOT NULL DEFAULT 0);").unwrap();
        for id in [
            "thread-one",
            "thread-two",
            "thread-three",
            "thread-missing-rollout",
        ] {
            state.execute("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, archived, agent_role, thread_source) VALUES (?1, '', 0, 0, 'cli', 'custom', ?2, ?1, 0, NULL, 'user')", params![id, format!("C:\\work\\{id}")]).unwrap();
        }
        state
            .execute(
                "UPDATE threads SET source='vscode' WHERE id='thread-three'",
                [],
            )
            .unwrap();
        state.execute("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, archived, agent_role, thread_source) VALUES ('thread-explicit-remote', '', 0, 0, '{\"kind\":\"vscode\",\"remoteAuthority\":\"ssh-remote+devbox\"}', 'custom', '/work', 'Remote VS Code', 0, NULL, 'user')", []).unwrap();
        state.execute("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, archived, agent_role, thread_source) VALUES ('thread-archived', '', 0, 0, 'cli', 'custom', '', 'Archived', 1, NULL, 'user')", []).unwrap();
        state.execute("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, archived, agent_role, thread_source) VALUES ('thread-subagent', '', 0, 0, 'automation', 'custom', '', 'Subagent', 0, 'worker', 'subagent')", []).unwrap();
        drop(state);
        let catalog = Connection::open(home.join("sqlite/codex-dev.db")).unwrap();
        catalog.execute_batch("CREATE TABLE local_thread_catalog (host_id TEXT NOT NULL, thread_id TEXT NOT NULL, display_title TEXT NOT NULL, source_created_at REAL NOT NULL, source_updated_at REAL NOT NULL, cwd TEXT NOT NULL, source_kind TEXT NOT NULL, source_detail TEXT, model_provider TEXT NOT NULL, git_branch TEXT, observation_sequence INTEGER NOT NULL, missing_candidate INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(host_id, thread_id)); CREATE TABLE local_thread_catalog_sync_state (host_id TEXT PRIMARY KEY, watermark_updated_at REAL, initial_build_complete INTEGER NOT NULL DEFAULT 0, observation_sequence INTEGER NOT NULL DEFAULT 0); INSERT INTO local_thread_catalog_sync_state (host_id, observation_sequence) VALUES ('local', 1); CREATE TABLE local_thread_catalog_metadata (id INTEGER PRIMARY KEY, catalog_revision INTEGER NOT NULL DEFAULT 0); INSERT INTO local_thread_catalog_metadata (id, catalog_revision) VALUES (1, 1);").unwrap();
        catalog.execute("INSERT INTO local_thread_catalog (host_id, thread_id, display_title, source_created_at, source_updated_at, cwd, source_kind, model_provider, observation_sequence, missing_candidate) VALUES ('local', 'thread-one', 'One', 0, 0, 'C:\\work\\thread-one', 'local', 'custom', 1, 0)", []).unwrap();
        catalog.execute("INSERT INTO local_thread_catalog (host_id, thread_id, display_title, source_created_at, source_updated_at, cwd, source_kind, model_provider, observation_sequence, missing_candidate) VALUES ('remote-host', 'thread-two', 'Remote two', 0, 0, '', 'remote', 'CodexPilot', 1, 0)", []).unwrap();
        drop(catalog);
        home
    }

    fn all_provider_sources() -> Vec<String> {
        ALLOWED_PROVIDERS
            .iter()
            .map(|value| (*value).to_string())
            .collect()
    }

    fn prepare_v6_rollout_backup(home: &Path) -> (BackupResult, RepairRolloutJournal, Vec<u8>) {
        let path = home.join("sessions/2026/07/13/thread-one.jsonl");
        let original = fs::read(&path).unwrap();
        let update = RolloutUpdate {
            thread_id: "thread-one".into(),
            path,
            archived: false,
            expected_provider: Some("custom".into()),
            provider: Some("OpenAI".into()),
        };
        let backup =
            create_backup_at_with_rollout_updates(home, std::slice::from_ref(&update)).unwrap();
        let row = rollout_journal_from_update(home, &update).unwrap();
        assert!(rewrite_rollout_provider(
            home,
            &row,
            &Some("custom".into()),
            &Some("OpenAI".into()),
        )
        .unwrap());
        assert_eq!(
            rollout::read_provider_image(&update.path, false)
                .unwrap()
                .provider,
            "OpenAI"
        );
        (backup, row, original)
    }

    fn prepare_interrupted_online_repair(home: &Path, commit: bool) -> PendingOperation {
        let sources = all_provider_sources();
        let snapshot = scan_snapshot(home);
        let store = load_projection_store(home).unwrap();
        let (preview, sessions) = projection_preview_for_snapshot(
            &snapshot,
            store.as_ref(),
            &sources,
            "openai",
            ProjectionScope::All,
            None,
        )
        .unwrap();
        let mut plan = build_plan_for_preview(&snapshot, "openai", &preview.plan);
        let reconciled = add_projection_reconciliation(
            &snapshot,
            store.as_ref(),
            &preview.plan,
            &mut plan,
            None,
        );
        let next_store = next_projection_store(
            store.as_ref(),
            &preview.plan,
            &sessions,
            &snapshot,
            &reconciled,
        )
        .unwrap();
        assert!(!plan.changed_ids.is_empty());

        let mut fence = acquire_dual_sqlite_write_fence(home, true).unwrap();
        let backup = create_backup_snapshot(
            home,
            plan.expected_global_state_sha256.as_deref(),
            None,
            &plan.rollout_updates,
        )
        .unwrap();
        let state_keys = repair_state_keys(&plan);
        let state_before = capture_state_journal_images(&fence.connection, &state_keys).unwrap();
        let keys = repair_catalog_keys(&plan);
        let before = capture_catalog_journal_images(&fence.connection, &keys).unwrap();
        let watermarks =
            (!keys.is_empty()).then(|| read_catalog_watermarks(&fence.connection).unwrap());
        apply_plan_with_fence(&fence, &plan).unwrap();
        let journal = build_repair_journal(
            home,
            &fence.connection,
            &plan,
            &state_before,
            &before,
            watermarks,
            store.as_ref(),
            &next_store,
        )
        .unwrap();
        let pending = PendingOperation {
            command: "repair".into(),
            backup_path: backup.path,
            created_at: Local::now().to_rfc3339(),
            phase: Some(RepairPhase::Prepared),
            repair_journal: Some(journal),
        };
        write_pending_operation(home, &pending).unwrap();
        apply_rollout_updates(home, &plan.rollout_updates).unwrap();
        save_projection_store(home, &next_store).unwrap();
        if commit {
            fence.commit().unwrap();
        }
        drop(fence);
        pending
    }

    fn fixture_workspace_path(home: &Path, symbolic: &str) -> PathBuf {
        let relative = match symbolic.to_ascii_lowercase().as_str() {
            r"c:\current" => "workspaces/current",
            r"c:\current\repo" => "workspaces/current/repo",
            r"c:\legacy\repo" => "workspaces/legacy/repo",
            r"c:\unrelated" => "workspaces/unrelated",
            r"c:\other\repo" => "workspaces/other/repo",
            _ => panic!("unknown fixture workspace path: {symbolic}"),
        };
        home.join(relative)
    }

    fn rewrite_fixture_rollout_provider(home: &Path, thread_id: &str, provider: &str) {
        let active_path = home.join(format!("sessions/2026/07/13/{thread_id}.jsonl"));
        let archived_path = home.join(format!("archived_sessions/{thread_id}.jsonl"));
        let (path, archived) = if active_path.exists() {
            (active_path, false)
        } else {
            (archived_path, true)
        };
        let plan = rollout::plan_provider_rewrite(
            &path,
            archived,
            provider,
            rollout::ProviderRewriteOptions {
                include_archived: archived,
            },
        )
        .unwrap();
        rollout::commit_provider_rewrite(plan).unwrap();
        assert_eq!(
            rollout::read_provider_image(&path, archived)
                .unwrap()
                .provider,
            provider
        );
    }

    fn assert_provider_in_all_layers(home: &Path, thread_id: &str, provider: &str) {
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        assert_eq!(
            state
                .query_row(
                    "SELECT model_provider FROM threads WHERE id=?1",
                    params![thread_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            provider
        );
        drop(state);

        let active_path = home.join(format!("sessions/2026/07/13/{thread_id}.jsonl"));
        let archived_path = home.join(format!("archived_sessions/{thread_id}.jsonl"));
        let (rollout_path, archived) = if active_path.exists() {
            (active_path, false)
        } else {
            (archived_path, true)
        };
        assert_eq!(
            rollout::read_provider_image(&rollout_path, archived)
                .unwrap()
                .provider,
            provider
        );
    }

    fn add_rollout_only_fixture(home: &Path, thread_id: &str, provider: &str) {
        fs::write(
            home.join(format!("sessions/2026/07/13/{thread_id}.jsonl")),
            format!(
                "{}\n{}\n",
                json!({
                    "type": "session_meta",
                    "payload": {
                        "id": thread_id,
                        "timestamp": "2026-07-13T00:10:00Z",
                        "model_provider": provider,
                        "cwd": format!(r"C:\work\{thread_id}"),
                        "source": "cli",
                        "cli_version": "0.1.0",
                        "git": {"branch": "main"}
                    }
                }),
                json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "Recover rollout-only session"}]
                    }
                })
            ),
        )
        .unwrap();
    }

    fn configure_workspace_mismatch(home: &Path, saved_roots: &[&str], hint: Option<Value>) {
        let current_repo = fixture_workspace_path(home, r"C:\current\repo");
        let legacy_repo = fixture_workspace_path(home, r"C:\legacy\repo");
        fs::create_dir_all(&current_repo).unwrap();
        fs::create_dir_all(&legacy_repo).unwrap();
        let saved_roots = saved_roots
            .iter()
            .map(|root| {
                let path = fixture_workspace_path(home, root);
                fs::create_dir_all(&path).unwrap();
                path.to_string_lossy().to_string()
            })
            .collect::<Vec<_>>();
        fs::write(
            home.join("sessions/2026/07/13/thread-one.jsonl"),
            format!(
                "{}\n",
                json!({
                    "type": "session_meta",
                    "payload": {
                        "id": "thread-one",
                        "timestamp": "2026-07-13T00:00:00Z",
                        "model_provider": "custom",
                        "cwd": legacy_repo.to_string_lossy(),
                        "source": "cli"
                    }
                })
            ),
        )
        .unwrap();
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        state
            .execute(
                "UPDATE threads SET cwd=?1 WHERE id='thread-one'",
                params![current_repo.to_string_lossy().as_ref()],
            )
            .unwrap();
        drop(state);
        let catalog = Connection::open(home.join("sqlite/codex-dev.db")).unwrap();
        catalog
            .execute(
                "UPDATE local_thread_catalog SET cwd=?1 WHERE host_id='local' AND thread_id='thread-one'",
                params![current_repo.to_string_lossy().as_ref()],
            )
            .unwrap();
        drop(catalog);
        let mut hints = serde_json::Map::new();
        if let Some(hint) = hint {
            let hint = match hint {
                Value::String(path) => {
                    let path = fixture_workspace_path(home, &path);
                    fs::create_dir_all(&path).unwrap();
                    Value::String(path.to_string_lossy().to_string())
                }
                value => value,
            };
            hints.insert("thread-one".into(), hint);
        }
        fs::write(
            global_state_path(home),
            serde_json::to_vec(&json!({
                "electron-saved-workspace-roots": saved_roots,
                "thread-workspace-root-hints": hints,
                "unrelated-preserved-field": {"value": 7}
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn unknown_configured_provider_is_discovered_selected_and_repaired() {
        let home = make_fixture();
        fs::write(
            home.join("config.toml"),
            "model_provider = \"codex_local_access\"\n\
             [model_providers.OpenAI]\nname = \"OpenAI\"\n\
             [model_providers.codex_local_access]\nname = \"Local access\"\n",
        )
        .unwrap();

        let refreshed =
            refresh_desktop_at(&home, &[], "openai", "openai", ProjectionScope::All, true).unwrap();
        assert_eq!(refreshed.scan.current_provider, "codex_local_access");
        assert_eq!(refreshed.target_provider, "codex_local_access");
        assert!(refreshed
            .scan
            .providers
            .iter()
            .any(|provider| provider.id == "codex_local_access" && provider.status == "active"));
        assert!(refreshed.selected_sources.contains(&"custom".into()));
        assert!(refreshed
            .selected_sources
            .contains(&"codex_local_access".into()));
        assert!(refreshed.preview.is_none());
        let preview = preview_projection_at(
            &home,
            &refreshed.selected_sources,
            &refreshed.target_provider,
            ProjectionScope::All,
        )
        .unwrap();
        assert_eq!(preview.plan.target_provider.as_str(), "codex_local_access");
        assert_eq!(preview.plan.considered, 4);

        let refreshed_again = refresh_desktop_at(
            &home,
            &["openai".into()],
            "openai",
            "codex_local_access",
            ProjectionScope::All,
            false,
        )
        .unwrap();
        assert_eq!(refreshed_again.target_provider, "codex_local_access");
        assert!(refreshed_again.selected_sources.contains(&"custom".into()));
        assert!(refreshed_again
            .selected_sources
            .contains(&"codex_local_access".into()));

        let preview = repair_projection_at(
            &home,
            &refreshed.selected_sources,
            &refreshed.target_provider,
            ProjectionScope::All,
            true,
            false,
            None,
        )
        .unwrap();
        assert_eq!(preview.changed_threads, 4);
        let repaired = repair_projection_at(
            &home,
            &refreshed.selected_sources,
            &refreshed.target_provider,
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert!(repaired.verified);
        let idempotent = repair_projection_at(
            &home,
            &refreshed.selected_sources,
            &refreshed.target_provider,
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert!(idempotent.verified);
        assert_eq!(idempotent.changed_threads, 0);

        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        assert_eq!(
            state
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE id IN ('thread-one','thread-three','thread-subagent','thread-archived') AND model_provider='codex_local_access'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            4
        );
        assert_eq!(
            state
                .query_row(
                    "SELECT model_provider FROM threads WHERE id='thread-subagent'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "codex_local_access"
        );
        assert_eq!(
            state
                .query_row(
                    "SELECT model_provider FROM threads WHERE id='thread-explicit-remote'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "custom"
        );
        drop(state);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn rollout_only_session_is_inserted_for_dynamic_provider_and_is_idempotent() {
        let home = make_fixture();
        let thread_id = "thread-rollout-only";
        let target = "Nebula-Edge.7";
        add_rollout_only_fixture(&home, thread_id, "custom");
        fs::write(
            home.join("config.toml"),
            format!(
                "model_provider = \"{target}\"\n[model_providers.\"{target}\"]\nname = \"Nebula\"\n"
            ),
        )
        .unwrap();
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        state
            .execute_batch(
                "ALTER TABLE threads ADD COLUMN preview TEXT NOT NULL DEFAULT '';
                 ALTER TABLE threads ADD COLUMN recency_at INTEGER NOT NULL DEFAULT 0;",
            )
            .unwrap();
        drop(state);

        let sources = vec!["custom".into(), target.into()];
        let refreshed =
            refresh_desktop_at(&home, &[], target, target, ProjectionScope::All, true).unwrap();
        assert!(refreshed
            .local_sessions
            .iter()
            .any(|session| session.id == thread_id && session.status == "recoverable"));
        assert!(refreshed.preview.is_none());
        let preview = preview_projection_at(
            &home,
            &sources,
            target,
            ProjectionScope::All,
        )
        .unwrap();
        assert!(preview
            .plan
            .sessions
            .iter()
            .any(|session| session.thread_id == thread_id));

        let repaired = repair_projection_at(
            &home,
            &sources,
            target,
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert!(repaired.verified);
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        let row = state
            .query_row(
                "SELECT model_provider, archived, preview, first_user_message, has_user_event, recency_at FROM threads WHERE id=?1",
                params![thread_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row.0, target);
        assert_eq!(row.1, 0);
        assert!(!row.2.is_empty());
        assert_eq!(row.3, "Recover rollout-only session");
        assert_eq!(row.4, 1);
        assert!(row.5 > 0);
        drop(state);
        assert_eq!(
            Connection::open(home.join("sqlite/codex-dev.db"))
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM local_thread_catalog WHERE host_id='local' AND thread_id=?1",
                    params![thread_id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );

        let second = repair_projection_at(
            &home,
            &sources,
            target,
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert_eq!(second.changed_threads, 0);
        assert!(second.verified);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn missing_rollout_provider_is_repaired_idempotently_and_restored() {
        let home = make_fixture();
        let thread_id = "thread-one";
        let rollout_path = home.join("sessions/2026/07/13/thread-one.jsonl");
        let original = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{thread_id}\",\"timestamp\":\"2026-07-13T00:00:00Z\",\"cwd\":\"C:\\\\work\\\\thread-one\",\"source\":\"cli\"}}}}\n"
        );
        fs::write(&rollout_path, &original).unwrap();
        let before = rollout::read_primary_rollout(&rollout_path, false).unwrap();
        assert!(before.provider_field_missing);
        assert!(before.model_provider.is_none());

        let selected = vec![thread_id.to_string()];
        let preview = preview_projection_selected_at(
            &home,
            &all_provider_sources(),
            "openai",
            ProjectionScope::All,
            Some(&selected),
        )
        .unwrap();
        assert_eq!(preview.plan.considered, 1);
        assert_eq!(preview.changed_threads, 1);

        let repaired = repair_projection_selected_at(
            &home,
            &all_provider_sources(),
            "openai",
            ProjectionScope::All,
            Some(&selected),
            false,
            false,
            Some(&preview.plan_token),
        )
        .unwrap();
        assert!(repaired.verified);
        assert_eq!(repaired.changed_threads, 1);
        assert_eq!(repaired.state_updates, 1);
        assert_eq!(repaired.rollout_updates, 1);
        let backup_path = PathBuf::from(repaired.backup_path.as_ref().unwrap());
        assert_eq!(
            Connection::open(home.join("state_5.sqlite"))
                .unwrap()
                .query_row(
                    "SELECT model_provider FROM threads WHERE id=?1",
                    params![thread_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "openai"
        );
        let after = rollout::read_primary_rollout(&rollout_path, false).unwrap();
        assert!(!after.provider_field_missing);
        assert_eq!(after.model_provider.as_deref(), Some("openai"));

        let second = repair_projection_selected_at(
            &home,
            &all_provider_sources(),
            "openai",
            ProjectionScope::All,
            Some(&selected),
            false,
            false,
            None,
        )
        .unwrap();
        assert!(second.verified);
        assert_eq!(second.changed_threads, 0);

        restore_backup_unchecked(&home, Some(&backup_path)).unwrap();
        assert_eq!(
            Connection::open(home.join("state_5.sqlite"))
                .unwrap()
                .query_row(
                    "SELECT model_provider FROM threads WHERE id=?1",
                    params![thread_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "custom"
        );
        let restored = rollout::read_primary_rollout(&rollout_path, false).unwrap();
        assert!(restored.provider_field_missing);
        assert!(restored.model_provider.is_none());
        assert_eq!(fs::read(&rollout_path).unwrap(), original.as_bytes());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn pending_repair_rollback_deletes_an_unchanged_tool_inserted_state_row() {
        let home = make_fixture();
        let thread_id = "thread-rollout-only";
        add_rollout_only_fixture(&home, thread_id, "custom");
        let pending = prepare_interrupted_online_repair(&home, true);
        let inserted = pending
            .repair_journal
            .as_ref()
            .unwrap()
            .state_rows
            .iter()
            .find(|row| row.thread_id == thread_id)
            .and_then(|row| row.row_images.as_ref())
            .expect("state insert must be journaled with full row images");
        assert!(inserted.before.is_none());
        assert!(inserted.after.is_some());
        assert_eq!(
            Connection::open(home.join("state_5.sqlite"))
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE id=?1",
                    params![thread_id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );

        assert_eq!(
            recover_pending_repair(&home, &pending, PendingRecoveryIntent::Rollback).unwrap(),
            PendingRepairResolution::Compensated
        );
        assert_eq!(
            Connection::open(home.join("state_5.sqlite"))
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE id=?1",
                    params![thread_id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn pending_repair_rollback_preserves_an_externally_changed_inserted_state_row() {
        let home = make_fixture();
        let thread_id = "thread-rollout-only";
        add_rollout_only_fixture(&home, thread_id, "custom");
        let pending = prepare_interrupted_online_repair(&home, true);
        Connection::open(home.join("state_5.sqlite"))
            .unwrap()
            .execute(
                "UPDATE threads SET title='continued after repair' WHERE id=?1",
                params![thread_id],
            )
            .unwrap();

        let error =
            recover_pending_repair(&home, &pending, PendingRecoveryIntent::Rollback).unwrap_err();
        assert!(error.contains(&format!("state:{thread_id}")));
        assert_eq!(
            Connection::open(home.join("state_5.sqlite"))
                .unwrap()
                .query_row(
                    "SELECT title FROM threads WHERE id=?1",
                    params![thread_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "continued after repair"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn full_projection_retains_a_tool_inserted_state_row() {
        let home = make_fixture();
        let thread_id = "thread-rollout-only";
        add_rollout_only_fixture(&home, thread_id, "custom");
        let selected = vec![thread_id.to_string()];
        let projected = repair_projection_selected_at(
            &home,
            &["custom".into(), "openai".into()],
            "openai",
            ProjectionScope::All,
            Some(&selected),
            false,
            false,
            None,
        )
        .unwrap();
        assert!(projected.verified);
        assert!(
            !load_projection_store(&home).unwrap().unwrap().threads[thread_id]
                .original_state_present
        );
        Connection::open(home.join("state_5.sqlite"))
            .unwrap()
            .execute(
                "UPDATE threads SET updated_at=1 WHERE id=?1",
                params![thread_id],
            )
            .unwrap();
        for index in 0..50 {
            add_rollout_only_fixture(&home, &format!("newer-rollout-{index:02}"), "custom");
        }

        let restored = repair_projection_at(
            &home,
            &["openai".into()],
            "openai",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert_eq!(restored.changed_threads, 54);
        assert_eq!(restored.restored_threads, 0);
        assert!(restored.verified);
        assert_eq!(
            Connection::open(home.join("state_5.sqlite"))
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE id=?1",
                    params![thread_id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            Connection::open(home.join("sqlite/codex-dev.db"))
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM local_thread_catalog WHERE host_id='local' AND thread_id=?1",
                    params![thread_id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        assert_eq!(
            rollout::read_provider_image(
                &home.join(format!("sessions/2026/07/13/{thread_id}.jsonl")),
                false,
            )
            .unwrap()
            .provider,
            "openai"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn active_rollout_clears_a_stale_state_archived_flag() {
        let home = make_fixture();
        Connection::open(home.join("state_5.sqlite"))
            .unwrap()
            .execute("UPDATE threads SET archived=1 WHERE id='thread-one'", [])
            .unwrap();
        let sources = all_provider_sources();
        let repaired = repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert!(repaired.verified);
        assert_eq!(
            Connection::open(home.join("state_5.sqlite"))
                .unwrap()
                .query_row(
                    "SELECT archived FROM threads WHERE id='thread-one'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        let second = repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert_eq!(second.changed_threads, 0);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn rollout_only_provider_drift_is_repaired_and_idempotent() {
        let home = make_fixture();
        let sources = all_provider_sources();
        repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();

        rewrite_fixture_rollout_provider(&home, "thread-one", "custom");
        let preview =
            preview_projection_at(&home, &sources, "openai", ProjectionScope::All).unwrap();
        assert_eq!(preview.changed_threads, 1);
        assert_eq!(preview.rollout_updates, 1);

        let repaired = repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            false,
            false,
            Some(&preview.plan_token),
        )
        .unwrap();
        assert!(repaired.verified);
        assert_eq!(repaired.changed_threads, 1);
        assert_eq!(repaired.state_updates, 0);
        assert_eq!(repaired.rollout_updates, 1);
        assert_eq!(repaired.catalog_updates, 0);
        assert_eq!(repaired.catalog_inserts, 0);

        let second = repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert!(second.verified);
        assert_eq!(second.changed_threads, 0);
        assert_eq!(second.rollout_updates, 0);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn desktop_refresh_does_not_guess_a_target_without_current_provider() {
        let home = make_fixture();
        fs::write(
            home.join("config.toml"),
            "[model_providers.openai]\nname = \"OpenAI\"\n",
        )
        .unwrap();

        let error = refresh_desktop_at(&home, &[], "openai", "openai", ProjectionScope::All, true)
            .unwrap_err();
        assert!(error.contains("no valid current model_provider"));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn desktop_refresh_degrades_to_rollouts_when_sqlite_is_unreadable() {
        let home = make_fixture();
        fs::remove_file(home.join("state_5.sqlite")).unwrap();
        fs::remove_file(home.join("sqlite/codex-dev.db")).unwrap();

        let refreshed =
            refresh_desktop_at(&home, &[], "openai", "openai", ProjectionScope::All, true).unwrap();
        assert_eq!(refreshed.scan.sqlite, 0);
        assert!(refreshed
            .local_sessions
            .iter()
            .any(|session| session.id == "thread-one"));
        assert!(refreshed
            .scan
            .sources
            .iter()
            .filter(|source| matches!(source.name.as_str(), "threads" | "local_thread_catalog"))
            .all(|source| !source.readable));

        let dry_run = repair_projection_at(
            &home,
            &refreshed.selected_sources,
            "openai",
            ProjectionScope::All,
            true,
            false,
            None,
        )
        .unwrap();
        assert!(dry_run.dry_run);
        assert!(repair_projection_at(
            &home,
            &refreshed.selected_sources,
            "openai",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap_err()
        .contains("state_5.sqlite is not readable"));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn provider_validation_preserves_dynamic_ids_exactly() {
        assert_eq!(
            validate_provider("Codex_Local_Access").unwrap(),
            "Codex_Local_Access"
        );
        assert_eq!(validate_provider("OpenAI").unwrap(), "OpenAI");
        assert_eq!(validate_provider("Codex-Pilot").unwrap(), "Codex-Pilot");
        assert_eq!(validate_provider("open-ai").unwrap(), "open-ai");
        assert_eq!(
            validate_provider("local provider").unwrap(),
            "local provider"
        );
        assert_eq!(validate_provider("本地").unwrap(), "本地");
        assert_eq!(validate_provider("Custom").unwrap(), "Custom");
        for provider in ["", " leading", "trailing ", "line\nbreak"] {
            assert!(
                validate_provider(provider).is_err(),
                "accepted {provider:?}"
            );
        }
    }

    #[test]
    fn dynamic_target_preserves_case_without_requiring_a_provider_table() {
        let home = make_fixture();
        assert_ne!(source_provider("Custom"), source_provider("custom"));
        fs::write(
            home.join("config.toml"),
            "model_provider = \"Custom\"\n[model_providers.Custom]\nname = \"Case-sensitive gateway\"\n",
        )
        .unwrap();

        let refreshed =
            refresh_desktop_at(&home, &[], "openai", "openai", ProjectionScope::All, true).unwrap();
        assert_eq!(refreshed.target_provider, "Custom");
        assert!(refreshed.selected_sources.contains(&"Custom".into()));
        let repaired = repair_projection_at(
            &home,
            &refreshed.selected_sources,
            &refreshed.target_provider,
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert!(repaired.verified);
        for thread_id in [
            "thread-one",
            "thread-three",
            "thread-subagent",
            "thread-archived",
        ] {
            assert_provider_in_all_layers(&home, thread_id, "Custom");
        }
        let provider_ids = scan_at(&home)
            .unwrap()
            .providers
            .into_iter()
            .map(|provider| provider.id)
            .collect::<BTreeSet<_>>();
        assert!(provider_ids.contains("Custom"));
        assert!(provider_ids.contains("custom"));

        fs::write(
            home.join("config.toml"),
            "model_provider = \"Custom\"\n[model_providers.custom]\nname = \"Wrong case\"\n",
        )
        .unwrap();
        let refreshed =
            refresh_desktop_at(&home, &[], "openai", "openai", ProjectionScope::All, true).unwrap();
        assert_eq!(refreshed.target_provider, "Custom");
        let unchanged = repair_projection_at(
            &home,
            &refreshed.selected_sources,
            "Custom",
            ProjectionScope::All,
            true,
            false,
            None,
        )
        .unwrap();
        assert!(unchanged.dry_run);
        assert_eq!(unchanged.changed_threads, 0);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn all_local_sessions_follow_each_current_dynamic_provider_without_losing_origins() {
        let home = make_fixture();
        let config = |current: &str| {
            format!(
                "model_provider = \"{current}\"\n[model_providers.GatewayA]\nname = \"Gateway A\"\n[model_providers.GatewayB]\nname = \"Gateway B\"\n"
            )
        };

        for target in ["GatewayA", "GatewayB", "GatewayA"] {
            fs::write(home.join("config.toml"), config(target)).unwrap();
            let config_before = fs::read(home.join("config.toml")).unwrap();
            let result =
                repair_projection_at(&home, &[], target, ProjectionScope::All, false, false, None)
                    .unwrap();
            assert!(result.verified);
            assert_eq!(result.changed_threads, 4);
            assert_eq!(fs::read(home.join("config.toml")).unwrap(), config_before);
            for thread_id in [
                "thread-one",
                "thread-three",
                "thread-subagent",
                "thread-archived",
            ] {
                assert_provider_in_all_layers(&home, thread_id, target);
            }

            let store = load_projection_store(&home).unwrap().unwrap();
            assert_eq!(store.schema_version, 3);
            assert_eq!(store.target_provider.as_str(), target);
            for thread_id in [
                "thread-one",
                "thread-three",
                "thread-subagent",
                "thread-archived",
            ] {
                let record = &store.threads[thread_id];
                assert_eq!(record.projected_target.as_str(), target);
                assert_eq!(record.original_state_provider.as_str(), "custom");
                assert_eq!(
                    record
                        .original_rollout_provider
                        .as_ref()
                        .map(SourceProvider::as_str),
                    Some("custom")
                );
            }
        }

        let store_before_idempotent = load_projection_store(&home).unwrap().unwrap();
        let second = repair_projection_at(
            &home,
            &[],
            "GatewayA",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert_eq!(second.changed_threads, 0);
        assert!(second.verified);
        assert_eq!(
            load_projection_store(&home).unwrap().unwrap(),
            store_before_idempotent
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn external_pre_alignment_updates_the_ledger_even_when_repair_has_no_writes() {
        let home = make_fixture();
        let config = |current: &str| {
            format!(
                "model_provider = \"{current}\"\n[model_providers.GatewayA]\nname = \"Gateway A\"\n[model_providers.GatewayB]\nname = \"Gateway B\"\n"
            )
        };
        fs::write(home.join("config.toml"), config("GatewayA")).unwrap();
        repair_projection_at(
            &home,
            &[],
            "GatewayA",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();

        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        state
            .execute(
                "UPDATE threads SET model_provider='GatewayB' WHERE id IN ('thread-one','thread-three','thread-subagent','thread-archived')",
                [],
            )
            .unwrap();
        drop(state);
        for thread_id in [
            "thread-one",
            "thread-three",
            "thread-subagent",
            "thread-archived",
        ] {
            rewrite_fixture_rollout_provider(&home, thread_id, "GatewayB");
        }
        fs::write(home.join("config.toml"), config("GatewayB")).unwrap();
        let state_before = fs::read(home.join("state_5.sqlite")).unwrap();
        let catalog_before = fs::read(home.join("sqlite/codex-dev.db")).unwrap();
        let store_before = load_projection_store(&home).unwrap().unwrap();

        let aligned = repair_projection_at(
            &home,
            &[],
            "GatewayB",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert_eq!(aligned.changed_threads, 0);
        assert_eq!(aligned.state_updates, 0);
        assert_eq!(aligned.rollout_updates, 0);
        assert_eq!(aligned.catalog_updates, 0);
        assert_eq!(aligned.catalog_inserts, 0);
        assert_eq!(aligned.catalog_deletes, 0);
        assert!(aligned.backup_path.is_none());
        assert!(aligned.verified);
        let store = load_projection_store(&home).unwrap().unwrap();
        assert_eq!(
            store.projection_version,
            store_before.projection_version + 1
        );
        assert!(store
            .threads
            .values()
            .all(|record| record.projected_target.as_str() == "GatewayB"));
        assert_eq!(fs::read(home.join("state_5.sqlite")).unwrap(), state_before);
        assert_eq!(
            fs::read(home.join("sqlite/codex-dev.db")).unwrap(),
            catalog_before
        );
        for thread_id in [
            "thread-one",
            "thread-three",
            "thread-subagent",
            "thread-archived",
        ] {
            assert_provider_in_all_layers(&home, thread_id, "GatewayB");
            let record = &store.threads[thread_id];
            assert_eq!(record.original_state_provider.as_str(), "custom");
            assert_eq!(
                record
                    .original_rollout_provider
                    .as_ref()
                    .map(SourceProvider::as_str),
                Some("custom")
            );
        }

        fs::write(home.join("config.toml"), config("GatewayA")).unwrap();
        let switched_back = repair_projection_at(
            &home,
            &[],
            "GatewayA",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert_eq!(switched_back.changed_threads, 4);
        assert!(switched_back.verified);
        for thread_id in [
            "thread-one",
            "thread-three",
            "thread-subagent",
            "thread-archived",
        ] {
            assert_provider_in_all_layers(&home, thread_id, "GatewayA");
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn local_dynamic_sources_are_listed_without_promoting_remote_or_automation_sources() {
        let home = make_fixture();
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        state
            .execute(
                "UPDATE threads SET model_provider='local_gateway' WHERE id='thread-one'",
                [],
            )
            .unwrap();
        state
            .execute(
                "UPDATE threads SET model_provider='remote_gateway' WHERE id='thread-explicit-remote'",
                [],
            )
            .unwrap();
        state
            .execute(
                "UPDATE threads SET model_provider='automation_gateway' WHERE id='thread-subagent'",
                [],
            )
            .unwrap();
        drop(state);
        let catalog = Connection::open(home.join("sqlite/codex-dev.db")).unwrap();
        catalog
            .execute(
                "UPDATE local_thread_catalog SET model_provider='local_gateway' WHERE host_id='local' AND thread_id='thread-one'",
                [],
            )
            .unwrap();
        drop(catalog);
        fs::write(
            home.join("sessions/2026/07/13/thread-one.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-one\",\"timestamp\":\"2026-07-13T00:00:00Z\",\"model_provider\":\"local_gateway\",\"cwd\":\"C:\\\\work\\\\thread-one\",\"source\":\"cli\"}}\n",
        )
        .unwrap();

        let refreshed =
            refresh_desktop_at(&home, &[], "openai", "openai", ProjectionScope::All, true).unwrap();
        let provider = refreshed
            .scan
            .providers
            .iter()
            .find(|provider| provider.id == "local_gateway")
            .expect("local dynamic provider should be listed");
        assert_eq!(provider.source_sessions, 1);
        assert!(refreshed.selected_sources.contains(&"local_gateway".into()));
        assert!(!refreshed
            .scan
            .providers
            .iter()
            .any(|provider| provider.id == "remote_gateway"));
        assert!(!refreshed
            .scan
            .providers
            .iter()
            .any(|provider| provider.id == "automation_gateway"));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn dry_run_repair_verify_restore_is_idempotent() {
        let home = make_fixture();
        let sources = ALLOWED_PROVIDERS
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>();
        let scan = scan_at(&home).unwrap();
        assert_eq!(scan.sessions, 7);
        assert_eq!(scan.archived_sessions, 1);
        assert_eq!(scan.ordinary_sessions, 4);
        assert_eq!(scan.recoverable_sessions, 4);
        assert_eq!(scan.recoverable_indexed, 1);
        assert_eq!(scan.session_index_covered, 2);
        assert_eq!(scan.remote_sessions, 2);
        assert_eq!(scan.remote_excluded_sessions, 2);
        assert_eq!(scan.missing_rollout, 1);
        assert_eq!(scan.automated_sessions, 1);
        assert_eq!(scan.missing_catalog, 3);
        assert_eq!(scan.skipped, 4);
        let state_before = fs::read(home.join("state_5.sqlite")).unwrap();
        let catalog_before = fs::read(home.join("sqlite/codex-dev.db")).unwrap();
        let config_before = fs::read(home.join("config.toml")).unwrap();
        let index_before = fs::read(home.join("session_index.jsonl")).unwrap();
        let rollout_before = fs::read(home.join("sessions/2026/07/13/thread-two.jsonl")).unwrap();
        let dry = repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            true,
            false,
            None,
        )
        .unwrap();
        assert!(dry.dry_run);
        assert_eq!(dry.changed_threads, 4);
        assert!(dry
            .skipped_reasons
            .iter()
            .any(|reason| reason.reason == "remote_mapped"));
        assert_eq!(fs::read(home.join("state_5.sqlite")).unwrap(), state_before);
        assert_eq!(
            fs::read(home.join("sqlite/codex-dev.db")).unwrap(),
            catalog_before
        );
        let repaired = repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert!(repaired.verified);
        assert_eq!(repaired.catalog_inserts, 0);
        assert_eq!(repaired.catalog_updates, 0);
        assert_eq!(
            verify_projection_at(&home, &sources, "openai", ProjectionScope::All)
                .unwrap()
                .remaining,
            0
        );
        assert_eq!(repaired.rollout_updates, 4);
        assert_eq!(scan_at(&home).unwrap().rollout_provider_drift, 0);
        for id in [
            "thread-one",
            "thread-three",
            "thread-subagent",
            "thread-archived",
        ] {
            assert_provider_in_all_layers(&home, id, "openai");
        }
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        assert_eq!(
            state
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE id IN ('thread-one','thread-three','thread-subagent','thread-archived') AND model_provider='openai'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            4
        );
        for id in [
            "thread-two",
            "thread-explicit-remote",
            "thread-missing-rollout",
        ] {
            assert_eq!(
                state
                    .query_row(
                        "SELECT model_provider FROM threads WHERE id=?1",
                        params![id],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap(),
                "custom"
            );
        }
        drop(state);
        let catalog = Connection::open(home.join("sqlite/codex-dev.db")).unwrap();
        assert_eq!(
            catalog
                .query_row(
                    "SELECT COUNT(*) FROM local_thread_catalog WHERE host_id='local' AND model_provider='custom'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            catalog
                .query_row(
                    "SELECT model_provider FROM local_thread_catalog WHERE host_id='remote-host' AND thread_id='thread-two'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "CodexPilot"
        );
        assert_eq!(
            catalog
                .query_row(
                    "SELECT COUNT(*) FROM local_thread_catalog WHERE host_id='local' AND thread_id='thread-two'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        assert_eq!(
            catalog
                .query_row(
                    "SELECT observation_sequence FROM local_thread_catalog_sync_state WHERE host_id='local'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            catalog
                .query_row(
                    "SELECT catalog_revision FROM local_thread_catalog_metadata WHERE id=1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        drop(catalog);
        assert_eq!(fs::read(home.join("config.toml")).unwrap(), config_before);
        assert_eq!(
            fs::read(home.join("session_index.jsonl")).unwrap(),
            index_before
        );
        assert_eq!(
            fs::read(home.join("sessions/2026/07/13/thread-two.jsonl")).unwrap(),
            rollout_before
        );
        let second = repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert_eq!(second.changed_threads, 0);
        restore_backup_unchecked(&home, None).unwrap();
        fs::write(home.join("config.toml"), "model_provider = \"custom\"\n").unwrap();
        assert_eq!(
            verify_projection_at(&home, &sources, "custom", ProjectionScope::All)
                .unwrap()
                .remaining,
            4
        );
        let restored = Connection::open(home.join("state_5.sqlite")).unwrap();
        let provider: String = restored
            .query_row(
                "SELECT model_provider FROM threads WHERE id='thread-one'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(provider, "custom");
        drop(restored);
        let restored_catalog = Connection::open(home.join("sqlite/codex-dev.db")).unwrap();
        assert_eq!(
            restored_catalog
                .query_row(
                    "SELECT COUNT(*) FROM local_thread_catalog WHERE host_id='local'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            restored_catalog
                .query_row(
                    "SELECT observation_sequence FROM local_thread_catalog_sync_state WHERE host_id='local'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            restored_catalog
                .query_row(
                    "SELECT catalog_revision FROM local_thread_catalog_metadata WHERE id=1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        drop(restored_catalog);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn successful_snapshot_restore_is_not_misreported_as_projection_failure() {
        let home = make_fixture();
        let repaired = repair_projection_at(
            &home,
            &[],
            "openai",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        let backup_path = PathBuf::from(repaired.backup_path.unwrap());

        let restored = restore_backup_at(&home, Some(&backup_path)).unwrap();
        assert!(restored.ok);
        assert_eq!(restored.checked, 1);
        assert_eq!(restored.remaining, 0);
        assert!(
            !verify_projection_at(&home, &[], "openai", ProjectionScope::All)
                .unwrap()
                .ok,
            "restoring the pre-repair image should not be judged by current projection alignment"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn selected_repair_preserves_unselected_projection_records() {
        let home = make_fixture();
        let sources = ALLOWED_PROVIDERS
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>();
        let snapshot = scan_snapshot(&home);
        let listed = local_session_summaries(&snapshot, None, "openai");
        assert_eq!(
            listed
                .iter()
                .map(|session| session.id.as_str())
                .collect::<HashSet<_>>(),
            HashSet::from([
                "thread-one",
                "thread-three",
                "thread-subagent",
                "thread-archived",
            ])
        );

        repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        fs::write(
            home.join("config.toml"),
            "model_provider = \"custom\"\n[model_providers.custom]\nname = \"Custom\"\n",
        )
        .unwrap();

        let selected = vec!["thread-one".to_string()];
        let preview = preview_projection_selected_at(
            &home,
            &sources,
            "custom",
            ProjectionScope::All,
            Some(&selected),
        )
        .unwrap();
        assert_eq!(preview.plan.considered, 1);
        assert_eq!(preview.reconcile_pending, 0);
        repair_projection_selected_at(
            &home,
            &sources,
            "custom",
            ProjectionScope::All,
            Some(&selected),
            false,
            false,
            None,
        )
        .unwrap();

        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        {
            let provider = |thread_id: &str| {
                state
                    .query_row(
                        "SELECT model_provider FROM threads WHERE id=?1",
                        params![thread_id],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap()
            };
            assert_eq!(provider("thread-one"), "custom");
            assert_eq!(provider("thread-three"), "openai");
        }
        drop(state);
        let verification = verify_projection_selected_at(
            &home,
            &sources,
            "custom",
            ProjectionScope::All,
            Some(&selected),
        )
        .unwrap();
        assert!(verification.ok);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn explicit_empty_selection_is_a_safe_no_op() {
        let home = make_fixture();
        let sources = all_provider_sources();
        let selected = Vec::<String>::new();
        let preview = preview_projection_selected_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            Some(&selected),
        )
        .unwrap();
        assert_eq!(preview.plan.considered, 0);
        assert_eq!(preview.plan.pending, 0);

        let dry_run = repair_projection_selected_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            Some(&selected),
            true,
            false,
            None,
        )
        .unwrap();
        assert_eq!(dry_run.changed_threads, 0);
        assert_eq!(dry_run.restored_threads, 0);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn workspace_hints_are_outside_the_repair_write_set() {
        let home = make_fixture();
        configure_workspace_mismatch(&home, &[r"C:\current\repo"], None);
        let sources = ALLOWED_PROVIDERS
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>();
        let global_before = fs::read(global_state_path(&home)).unwrap();
        let rollout_before = fs::read(home.join("sessions/2026/07/13/thread-one.jsonl")).unwrap();
        let index_before = fs::read(home.join("session_index.jsonl")).unwrap();

        let preview =
            preview_projection_at(&home, &sources, "openai", ProjectionScope::All).unwrap();
        assert_eq!(preview.workspace_hint_updates, 0);
        assert_eq!(preview.workspace_conflicts, 0);
        let dry = repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            true,
            false,
            None,
        )
        .unwrap();
        assert_eq!(dry.workspace_hint_updates, 0);
        assert_eq!(fs::read(global_state_path(&home)).unwrap(), global_before);

        let repaired = repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            false,
            false,
            Some(&preview.plan_token),
        )
        .unwrap();
        assert!(repaired.verified);
        assert_eq!(repaired.workspace_hint_updates, 0);
        let backup_path = PathBuf::from(repaired.backup_path.as_ref().unwrap());
        let manifest: BackupManifest =
            serde_json::from_slice(&fs::read(backup_path.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest.version, 6);
        assert!(!manifest
            .files
            .iter()
            .find(|file| file.path == ".codex-global-state.json")
            .is_some_and(|file| file.backed_up));
        assert_eq!(fs::read(global_state_path(&home)).unwrap(), global_before);
        assert!(
            verify_projection_at(&home, &sources, "openai", ProjectionScope::All)
                .unwrap()
                .ok
        );

        let second_preview =
            preview_projection_at(&home, &sources, "openai", ProjectionScope::All).unwrap();
        assert_eq!(second_preview.workspace_hint_updates, 0);
        let second = repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            false,
            false,
            Some(&second_preview.plan_token),
        )
        .unwrap();
        assert_eq!(second.changed_threads, 0);
        assert_eq!(second.workspace_hint_updates, 0);

        restore_backup_unchecked(&home, Some(&backup_path)).unwrap();
        assert_eq!(fs::read(global_state_path(&home)).unwrap(), global_before);
        assert_eq!(
            fs::read(home.join("sessions/2026/07/13/thread-one.jsonl")).unwrap(),
            rollout_before
        );
        assert_eq!(
            fs::read(home.join("session_index.jsonl")).unwrap(),
            index_before
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn workspace_conflicts_do_not_exclude_valid_local_rollouts() {
        let home = make_fixture();
        configure_workspace_mismatch(&home, &[r"C:\unrelated"], None);
        let sources = ALLOWED_PROVIDERS
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>();
        let preview =
            preview_projection_at(&home, &sources, "openai", ProjectionScope::All).unwrap();
        assert_eq!(preview.workspace_hint_updates, 0);
        assert_eq!(preview.workspace_conflicts, 0);
        assert!(preview
            .plan
            .sessions
            .iter()
            .any(|session| session.thread_id == "thread-one"));
        let verification =
            verify_projection_at(&home, &sources, "openai", ProjectionScope::All).unwrap();
        assert!(!verification.ok);
        assert!(!verification
            .reasons
            .iter()
            .any(|reason| reason.reason == "workspace_conflict"));

        configure_workspace_mismatch(
            &home,
            &[r"C:\current\repo"],
            Some(Value::String(r"C:\other\repo".into())),
        );
        let conflicting_hint =
            preview_projection_at(&home, &sources, "openai", ProjectionScope::All).unwrap();
        assert_eq!(conflicting_hint.workspace_conflicts, 0);

        fs::write(
            home.join("sessions/2026/07/13/thread-one.jsonl"),
            format!(
                "{}\n",
                json!({
                    "type": "session_meta",
                    "payload": {
                        "id": "thread-one",
                        "timestamp": "2026-07-13T00:00:00Z",
                        "model_provider": "custom",
                        "cwd": fixture_workspace_path(&home, r"C:\current\repo").to_string_lossy(),
                        "source": "cli"
                    }
                })
            ),
        )
        .unwrap();
        let matching_cwd_with_conflicting_hint =
            preview_projection_at(&home, &sources, "openai", ProjectionScope::All).unwrap();
        assert_eq!(matching_cwd_with_conflicting_hint.workspace_conflicts, 0);

        configure_workspace_mismatch(&home, &[r"C:\current", r"C:\current\repo"], None);
        let ambiguous_root =
            preview_projection_at(&home, &sources, "openai", ProjectionScope::All).unwrap();
        assert_eq!(ambiguous_root.workspace_conflicts, 0);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn all_scope_does_not_truncate_local_sessions() {
        let home = make_fixture();
        configure_workspace_mismatch(&home, &[r"C:\unrelated"], None);
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        state
            .execute(
                "UPDATE threads SET updated_at=1000 WHERE id='thread-one'",
                [],
            )
            .unwrap();
        drop(state);
        let snapshot = scan_snapshot(&home);
        let (_, skipped) = eligible_projection_sessions(&snapshot, None, &SourceProvider::OpenAi);
        let synthetic = (0..75)
            .map(|index| EligibleSession {
                id: format!("eligible-{index:02}"),
                origin_provider: SourceProvider::Other("custom".into()),
                state_provider: SourceProvider::Other("custom".into()),
                state_present: true,
                catalog: CatalogState::Present {
                    provider: SourceProvider::Other("custom".into()),
                },
                updated_at: 999 - index,
            })
            .collect::<Vec<_>>();
        let selected = BTreeSet::from([SourceProvider::Other("custom".into())]);
        let (recent, conflicts) = scoped_projection_sessions(
            &snapshot,
            None,
            &synthetic,
            &skipped,
            &selected,
            SourceProvider::Other("custom".into()),
            ProjectionScope::All,
        );
        assert_eq!(recent.len(), 75);
        assert!(conflicts.is_empty());

        let openai_only = BTreeSet::from([SourceProvider::OpenAi]);
        let (recent, conflicts) = scoped_projection_sessions(
            &snapshot,
            None,
            &synthetic,
            &skipped,
            &openai_only,
            SourceProvider::OpenAi,
            ProjectionScope::All,
        );
        assert!(recent.is_empty());
        assert!(conflicts.is_empty());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn global_state_changes_do_not_invalidate_a_repair_plan() {
        let home = make_fixture();
        configure_workspace_mismatch(&home, &[r"C:\current\repo"], None);
        let sources = ALLOWED_PROVIDERS
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>();
        let first = preview_projection_at(&home, &sources, "openai", ProjectionScope::All).unwrap();
        let mut global: Value =
            serde_json::from_slice(&fs::read(global_state_path(&home)).unwrap()).unwrap();
        global["unrelated-preserved-field"]["value"] = json!(8);
        fs::write(
            global_state_path(&home),
            serde_json::to_vec(&global).unwrap(),
        )
        .unwrap();
        let second =
            preview_projection_at(&home, &sources, "openai", ProjectionScope::All).unwrap();
        assert_eq!(first.plan_token, second.plan_token);
        let repaired = repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            false,
            false,
            Some(&first.plan_token),
        )
        .unwrap();
        assert!(repaired.verified);
        assert_eq!(repaired.workspace_hint_updates, 0);
        let unchanged: Value =
            serde_json::from_slice(&fs::read(global_state_path(&home)).unwrap()).unwrap();
        assert_eq!(unchanged["unrelated-preserved-field"]["value"], json!(8));
        assert!(unchanged["thread-workspace-root-hints"]
            .get("thread-one")
            .is_none());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn desktop_refresh_matches_scan_and_preview_with_resolved_defaults() {
        let home = make_fixture();
        let refresh = refresh_desktop_at(
            &home,
            &["codexpilot".into()],
            "codexpilot",
            "unknown",
            ProjectionScope::All,
            true,
        )
        .unwrap();
        assert_eq!(refresh.target_provider, "openai");
        assert!(refresh
            .blocking_processes
            .iter()
            .all(|process| !process.identity.is_current));
        assert!(refresh.selected_sources.contains(&"custom".into()));
        assert!(refresh.selected_sources.contains(&"openai".into()));
        assert!(!refresh.selected_sources.contains(&"codexpilot".into()));
        // List refresh intentionally omits the heavy preview; recovery opens
        // preview_projection on demand.
        assert!(refresh.preview.is_none());

        let scan = scan_at(&home).unwrap();
        let preview = preview_projection_at(
            &home,
            &refresh.selected_sources,
            &refresh.target_provider,
            ProjectionScope::All,
        )
        .unwrap();
        assert_eq!(refresh.scan.recoverable_sessions, scan.recoverable_sessions);
        assert_eq!(refresh.scan.providers.len(), scan.providers.len());
        assert_eq!(preview.plan.target_provider.as_str(), "openai");
        assert!(!preview.plan_token.is_empty());
        assert!(!refresh.local_sessions.is_empty());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]

    #[test]
    fn snapshot_short_cache_reuses_identical_fingerprint() {
        let home = make_fixture();
        invalidate_snapshot_cache();
        let first = scan_snapshot(&home);
        let second = scan_snapshot(&home);
        assert_eq!(first.threads.len(), second.threads.len());
        assert_eq!(first.rollouts.len(), second.rollouts.len());
        // Fresh scan still works and repopulates the cache.
        let forced = scan_snapshot_fresh(&home);
        assert_eq!(forced.threads.len(), first.threads.len());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn maintain_backups_skips_when_within_limit() {
        let home = make_fixture();
        // Empty / under-limit backup root should not run cleanup work.
        let result = maintain_backups_at(&home).unwrap();
        assert!(result.is_none());
        fs::remove_dir_all(home).unwrap();
    }

    fn desktop_refresh_reuses_cohorts_without_building_preview() {
        let home = make_fixture();
        let refresh = refresh_desktop_at(
            &home,
            &[],
            "openai",
            "openai",
            ProjectionScope::All,
            false,
        )
        .unwrap();
        assert!(refresh.preview.is_none());
        assert!(!refresh.local_sessions.is_empty());
        // On-demand preview still works after a light refresh.
        let preview = preview_projection_at(
            &home,
            &refresh.selected_sources,
            &refresh.target_provider,
            ProjectionScope::All,
        )
        .unwrap();
        assert_eq!(
            preview.plan.target_provider.as_str(),
            refresh.target_provider
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn empty_source_filter_defaults_to_every_local_provider() {
        let home = make_fixture();
        let all = preview_projection_at(
            &home,
            &all_provider_sources(),
            "openai",
            ProjectionScope::All,
        )
        .unwrap();
        let empty = preview_projection_at(&home, &[], "openai", ProjectionScope::All).unwrap();

        assert_eq!(empty.plan.considered, all.plan.considered);
        assert_eq!(empty.plan.sessions, all.plan.sessions);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn scan_and_dry_run_do_not_create_sqlite_sidecars() {
        let home = make_fixture();
        let databases = [
            home.join("state_5.sqlite"),
            home.join("sqlite/codex-dev.db"),
        ];
        for database in &databases {
            let connection = Connection::open(database).unwrap();
            let mode: String = connection
                .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
                .unwrap();
            assert_eq!(mode.to_ascii_lowercase(), "wal");
            connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
            drop(connection);
            for suffix in ["-wal", "-shm"] {
                let sidecar = sidecar_path(database, suffix);
                if sidecar.exists() {
                    fs::remove_file(sidecar).unwrap();
                }
            }
        }

        scan_at(&home).unwrap();
        let sources = ALLOWED_PROVIDERS
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>();
        repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            true,
            false,
            None,
        )
        .unwrap();

        for database in &databases {
            for suffix in ["-wal", "-shm"] {
                assert!(
                    !sidecar_path(database, suffix).exists(),
                    "read-only flow created {suffix} for {}",
                    database.display()
                );
            }
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn source_filters_cannot_exclude_valid_local_rollouts() {
        let home = make_fixture();
        let all_sources = ALLOWED_PROVIDERS
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>();
        let projected = repair_projection_at(
            &home,
            &all_sources,
            "openai",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert_eq!(projected.changed_threads, 4);

        let only_openai = vec!["openai".to_string()];
        let preview =
            preview_projection_at(&home, &only_openai, "openai", ProjectionScope::All).unwrap();
        assert_eq!(preview.plan.considered, 4);
        assert_eq!(preview.reconcile_pending, 0);
        let unchanged = repair_projection_at(
            &home,
            &only_openai,
            "openai",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert_eq!(unchanged.changed_threads, 0);
        assert_eq!(unchanged.restored_threads, 0);
        assert!(unchanged.verified);
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        let projected_count: i64 = state
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE id IN ('thread-one','thread-three','thread-subagent','thread-archived') AND model_provider='openai'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(projected_count, 4);
        let catalog = Connection::open(home.join("sqlite/codex-dev.db")).unwrap();
        let projected_insert: i64 = catalog
            .query_row(
                "SELECT COUNT(*) FROM local_thread_catalog WHERE host_id='local' AND thread_id='thread-three'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(projected_insert, 0);
        drop(catalog);
        drop(state);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn remote_detection_requires_a_nonempty_remote_signal() {
        assert!(!has_explicit_remote_marker(
            r#"{"kind":"vscode","remoteAuthority":null}"#
        ));
        assert!(!has_explicit_remote_marker(
            r#"{"kind":"vscode","remoteAuthority":""}"#
        ));
        assert!(has_explicit_remote_marker(
            r#"{"kind":"vscode","remoteAuthority":"ssh-remote+devbox"}"#
        ));
        assert!(has_explicit_remote_marker(r#"{"kind":"dev-container"}"#));
        assert!(!has_explicit_remote_marker(
            r#"{"kind":"vscode","detail":"documentation mentions ssh-remote and codespaces"}"#
        ));
        assert!(!has_explicit_remote_marker(
            "documentation mentions devcontainer"
        ));
        assert!(catalog_row_is_remote(&CatalogRow {
            host_id: "local".into(),
            thread_id: "remote".into(),
            provider: "OpenAI".into(),
            missing_candidate: false,
            source_kind: "codespaces".into(),
            source_detail: String::new(),
            cwd: String::new(),
        }));
        assert_eq!(
            normalized_local_windows_path(r"\\?\C:\work\repo"),
            Some(r"c:\work\repo".into())
        );
        assert!(normalized_local_windows_path("/home/user/repo").is_none());
    }

    #[test]
    fn compare_and_swap_rejects_concurrent_provider_changes() {
        let home = make_fixture();
        let snapshot = scan_snapshot(&home);
        let sources = ALLOWED_PROVIDERS
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>();
        let (preview, _) = projection_preview_for_snapshot(
            &snapshot,
            None,
            &sources,
            "openai",
            ProjectionScope::All,
            None,
        )
        .unwrap();
        let plan = build_plan_for_preview(&snapshot, "openai", &preview.plan);
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        state
            .execute(
                "UPDATE threads SET model_provider='CodexPilot' WHERE id='thread-one'",
                [],
            )
            .unwrap();
        drop(state);
        assert!(apply_plan(&home, &plan).is_err());
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        let one: String = state
            .query_row(
                "SELECT model_provider FROM threads WHERE id='thread-one'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let three: String = state
            .query_row(
                "SELECT model_provider FROM threads WHERE id='thread-three'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(one, "CodexPilot");
        assert_eq!(three, "custom");
        drop(state);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn stale_preview_token_is_rejected_before_backup_or_apply() {
        let home = make_fixture();
        let sources = ALLOWED_PROVIDERS
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>();
        let preview =
            preview_projection_at(&home, &sources, "openai", ProjectionScope::All).unwrap();
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        state
            .execute(
                "UPDATE threads SET updated_at=updated_at+1 WHERE id='thread-three'",
                [],
            )
            .unwrap();
        drop(state);
        let error = repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            false,
            false,
            Some(&preview.plan_token),
        )
        .unwrap_err();
        assert!(error.contains("plan changed"));
        assert!(latest_backup(&home).is_none());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn repair_progress_follows_transaction_boundaries_monotonically() {
        let home = make_fixture();
        let sources = all_provider_sources();
        let mut events = Vec::new();
        let repaired = repair_projection_selected_at_with_progress(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            None,
            false,
            false,
            None,
            |event| events.push(event),
        )
        .unwrap();

        assert!(repaired.verified);
        assert!(events
            .windows(2)
            .all(|window| window[0].percent <= window[1].percent));
        assert_eq!(
            events.last().map(|event| event.stage),
            Some(RepairProgressStage::Completed)
        );
        assert_eq!(events.last().map(|event| event.percent), Some(100));

        let expected_stages = [
            RepairProgressStage::Planning,
            RepairProgressStage::AcquiringOperationLock,
            RepairProgressStage::PlanValidated,
            RepairProgressStage::AcquiringWriteFence,
            RepairProgressStage::Backup,
            RepairProgressStage::SqliteStaging,
            RepairProgressStage::MetadataSync,
            RepairProgressStage::Commit,
            RepairProgressStage::Verification,
            RepairProgressStage::Completed,
        ];
        let mut cursor = 0;
        for expected in expected_stages {
            let offset = events[cursor..]
                .iter()
                .position(|event| event.stage == expected)
                .unwrap_or_else(|| panic!("missing repair progress stage: {expected:?}"));
            cursor += offset + 1;
        }

        for stage in [
            RepairProgressStage::Backup,
            RepairProgressStage::SqliteStaging,
            RepairProgressStage::MetadataSync,
            RepairProgressStage::Commit,
            RepairProgressStage::Verification,
        ] {
            let stage_events = events
                .iter()
                .filter(|event| event.stage == stage)
                .collect::<Vec<_>>();
            assert_eq!(
                stage_events.first().and_then(|event| event.completed),
                Some(0)
            );
            let final_event = stage_events.last().unwrap();
            assert_eq!(final_event.completed, final_event.total);
            assert!(final_event.total.is_some_and(|total| total > 0));
        }

        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn repair_progress_never_reports_completion_after_an_error() {
        let home = make_fixture();
        let sources = all_provider_sources();
        let mut events = Vec::new();
        let error = repair_projection_selected_at_with_progress(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            None,
            false,
            false,
            Some("stale-plan-token"),
            |event| events.push(event),
        )
        .unwrap_err();

        assert!(error.contains("plan changed"));
        assert!(events
            .windows(2)
            .all(|window| window[0].percent <= window[1].percent));
        assert!(events
            .iter()
            .all(|event| event.stage != RepairProgressStage::Completed));
        assert!(latest_backup(&home).is_none());

        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn pending_repair_journal_is_persisted_but_not_exposed_in_scan_json() {
        let home = make_fixture();
        let pending = prepare_interrupted_online_repair(&home, false);
        let public_json = serde_json::to_value(&pending).unwrap();
        assert!(public_json.get("repairJournal").is_none());
        let persisted: Value =
            serde_json::from_slice(&fs::read(pending_operation_path(&home)).unwrap()).unwrap();
        assert!(persisted.get("repairJournal").is_some());
        assert!(load_pending_operation(&home)
            .unwrap()
            .is_some_and(|pending| pending.repair_journal.is_some()));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn prepared_commit_is_finalized_without_deleting_later_codex_rows() {
        let home = make_fixture();
        let pending = prepare_interrupted_online_repair(&home, true);
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        state.execute("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, archived, agent_role, thread_source) VALUES ('external-new', '', 0, 0, 'cli', 'OpenAI', 'C:\\work\\external-new', 'External new', 0, NULL, 'user')", []).unwrap();
        drop(state);
        let catalog = Connection::open(home.join("sqlite/codex-dev.db")).unwrap();
        let next_sequence: i64 = catalog
            .query_row(
                "SELECT observation_sequence + 1 FROM local_thread_catalog_sync_state WHERE host_id='local'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        catalog.execute("INSERT INTO local_thread_catalog (host_id, thread_id, display_title, source_created_at, source_updated_at, cwd, source_kind, source_detail, model_provider, git_branch, observation_sequence, missing_candidate) VALUES ('local', 'external-new', 'External new', 0, 0, 'C:\\work\\external-new', 'local', NULL, 'OpenAI', NULL, ?1, 0)", params![next_sequence]).unwrap();
        catalog.execute("UPDATE local_thread_catalog_sync_state SET observation_sequence=?1 WHERE host_id='local'", params![next_sequence]).unwrap();
        catalog.execute("UPDATE local_thread_catalog_metadata SET catalog_revision=catalog_revision+1 WHERE id=1", []).unwrap();
        drop(catalog);

        assert_eq!(
            recover_pending_repair(&home, &pending, PendingRecoveryIntent::Resume).unwrap(),
            PendingRepairResolution::CommittedFinalized
        );
        assert!(load_pending_operation(&home).unwrap().is_none());
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        assert_eq!(
            state
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE id='external-new'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        drop(state);
        let catalog = Connection::open(home.join("sqlite/codex-dev.db")).unwrap();
        assert_eq!(
            catalog
                .query_row(
                    "SELECT COUNT(*) FROM local_thread_catalog WHERE host_id='local' AND thread_id='external-new'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        drop(catalog);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn committed_phase_checks_live_images_before_finalizing() {
        let home = make_fixture();
        let mut pending = prepare_interrupted_online_repair(&home, true);
        pending.phase = Some(RepairPhase::Committed);
        write_pending_operation(&home, &pending).unwrap();
        let changed = pending
            .repair_journal
            .as_ref()
            .unwrap()
            .state_rows
            .first()
            .unwrap();
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        state
            .execute(
                "UPDATE threads SET model_provider='external-change' WHERE id=?1",
                params![changed.thread_id],
            )
            .unwrap();
        drop(state);

        let error =
            recover_pending_repair(&home, &pending, PendingRecoveryIntent::Resume).unwrap_err();
        assert!(error.contains(&format!("state:{}", changed.thread_id)));
        let persisted = load_pending_operation(&home).unwrap().unwrap();
        assert_eq!(persisted.phase, Some(RepairPhase::Compensating));
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        assert_eq!(
            state
                .query_row(
                    "SELECT model_provider FROM threads WHERE id=?1",
                    params![changed.thread_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "external-change"
        );
        drop(state);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn verification_failed_safe_rollback_preserves_later_codex_data() {
        let home = make_fixture();
        let mut pending = prepare_interrupted_online_repair(&home, true);
        pending.phase = Some(RepairPhase::VerificationFailed);
        write_pending_operation(&home, &pending).unwrap();
        assert!(pending
            .repair_journal
            .as_ref()
            .unwrap()
            .catalog_rows
            .is_empty());

        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        state.execute("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, archived, agent_role, thread_source) VALUES ('external-after-repair', '', 0, 0, 'cli', 'OpenAI', 'C:\\work\\external-after-repair', 'External after repair', 0, NULL, 'user')", []).unwrap();
        drop(state);
        let catalog = Connection::open(home.join("sqlite/codex-dev.db")).unwrap();
        let next_sequence: i64 = catalog
            .query_row(
                "SELECT observation_sequence + 5 FROM local_thread_catalog_sync_state WHERE host_id='local'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        catalog.execute("INSERT INTO local_thread_catalog (host_id, thread_id, display_title, source_created_at, source_updated_at, cwd, source_kind, source_detail, model_provider, git_branch, observation_sequence, missing_candidate) VALUES ('local', 'external-after-repair', 'External after repair', 0, 0, 'C:\\work\\external-after-repair', 'local', NULL, 'OpenAI', NULL, ?1, 0)", params![next_sequence]).unwrap();
        catalog
            .execute(
                "UPDATE local_thread_catalog SET display_title='Codex newer title', observation_sequence=?1 WHERE host_id=?2 AND thread_id=?3",
                params![next_sequence, "local", "thread-one"],
            )
            .unwrap();
        catalog
            .execute(
                "UPDATE local_thread_catalog_sync_state SET observation_sequence=?1 WHERE host_id='local'",
                params![next_sequence],
            )
            .unwrap();
        catalog
            .execute(
                "UPDATE local_thread_catalog_metadata SET catalog_revision=catalog_revision+5 WHERE id=1",
                [],
            )
            .unwrap();
        drop(catalog);

        restore_backup_at(&home, None).unwrap();
        assert!(load_pending_operation(&home).unwrap().is_none());
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        assert_eq!(
            state
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE id='external-after-repair'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        for row in &pending.repair_journal.as_ref().unwrap().state_rows {
            assert_eq!(
                state
                    .query_row(
                        "SELECT model_provider FROM threads WHERE id=?1",
                        params![row.thread_id],
                        |value| value.get::<_, String>(0),
                    )
                    .unwrap(),
                row.before_provider
            );
        }
        drop(state);
        let catalog = Connection::open(home.join("sqlite/codex-dev.db")).unwrap();
        assert_eq!(
            catalog
                .query_row(
                    "SELECT COUNT(*) FROM local_thread_catalog WHERE host_id='local' AND thread_id='external-after-repair'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        let (provider, title, sequence): (String, String, i64) = catalog
            .query_row(
                "SELECT model_provider, display_title, observation_sequence FROM local_thread_catalog WHERE host_id=?1 AND thread_id=?2",
                params!["local", "thread-one"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(provider, "custom");
        assert_eq!(title, "Codex newer title");
        assert_eq!(sequence, next_sequence);
        assert_eq!(
            catalog
                .query_row(
                    "SELECT observation_sequence FROM local_thread_catalog_sync_state WHERE host_id='local'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            next_sequence
        );
        drop(catalog);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn mixed_recovery_compensates_after_images_and_preserves_external_changes() {
        let home = make_fixture();
        let pending = prepare_interrupted_online_repair(&home, false);
        let journal = pending.repair_journal.as_ref().unwrap();
        assert!(journal.state_rows.len() >= 2);
        let reverted = &journal.state_rows[0];
        let external = &journal.state_rows[1];
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        let images = reverted
            .row_images
            .as_ref()
            .expect("updated state row must have complete before and after images");
        restore_state_journal_image(
            &state,
            &reverted.thread_id,
            images.after.as_ref(),
            images.before.as_ref(),
        )
        .unwrap();
        state
            .execute(
                "UPDATE threads SET model_provider='CodexPilot' WHERE id=?1",
                params![external.thread_id],
            )
            .unwrap();
        state.execute("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, archived, agent_role, thread_source) VALUES ('external-unrelated', '', 0, 0, 'cli', 'custom', 'C:\\work\\external-unrelated', 'External unrelated', 0, NULL, 'user')", []).unwrap();
        drop(state);

        let error =
            recover_pending_repair(&home, &pending, PendingRecoveryIntent::Resume).unwrap_err();
        assert!(error.contains(&format!("state:{}", external.thread_id)));
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        let provider = |thread_id: &str| {
            state
                .query_row(
                    "SELECT model_provider FROM threads WHERE id=?1",
                    params![thread_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap()
        };
        assert_eq!(provider(&reverted.thread_id), reverted.before_provider);
        assert_eq!(provider(&external.thread_id), "CodexPilot");
        assert_eq!(provider("external-unrelated"), "custom");
        drop(state);
        assert!(load_projection_store(&home).unwrap().is_none());
        assert_eq!(
            load_pending_operation(&home).unwrap().unwrap().phase,
            Some(RepairPhase::Compensating)
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn busy_state_writer_blocks_repair_without_artifacts() {
        let home = make_fixture();
        let sources = all_provider_sources();
        let writer = Connection::open(home.join("state_5.sqlite")).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let error = repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap_err();
        assert!(error.contains("SQLITE_BUSY"), "{error}");
        assert!(latest_backup(&home).is_none());
        assert!(load_pending_operation(&home).unwrap().is_none());
        writer.execute_batch("ROLLBACK").unwrap();
        drop(writer);
        assert_eq!(
            Connection::open(home.join("state_5.sqlite"))
                .unwrap()
                .query_row(
                    "SELECT model_provider FROM threads WHERE id='thread-one'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "custom"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn busy_catalog_writer_does_not_block_state_only_repair() {
        let home = make_fixture();
        let writer = Connection::open(home.join("sqlite/codex-dev.db")).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let repaired = repair_projection_at(
            &home,
            &all_provider_sources(),
            "openai",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert!(repaired.verified);
        assert_eq!(repaired.changed_threads, 4);
        assert_eq!(repaired.catalog_updates, 0);
        assert_eq!(repaired.catalog_inserts, 0);
        assert_eq!(repaired.catalog_deletes, 0);
        assert!(repaired.backup_path.is_some());
        writer.execute_batch("ROLLBACK").unwrap();
        drop(writer);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn dual_sqlite_write_fence_blocks_writers_for_both_databases() {
        let home = make_fixture();
        let fence = acquire_dual_sqlite_write_fence(&home, true).unwrap();
        let conflict = match acquire_dual_sqlite_write_fence(&home, true) {
            Ok(_) => panic!("a second online writer unexpectedly acquired the transaction"),
            Err(error) => error,
        };
        assert!(conflict.contains("SQLITE_BUSY"));
        for path in [
            home.join("state_5.sqlite"),
            home.join("sqlite/codex-dev.db"),
        ] {
            let writer = Connection::open(path).unwrap();
            writer.busy_timeout(Duration::from_millis(25)).unwrap();
            assert!(writer.execute_batch("BEGIN IMMEDIATE; ROLLBACK").is_err());
        }
        drop(fence);
        for path in [
            home.join("state_5.sqlite"),
            home.join("sqlite/codex-dev.db"),
        ] {
            let writer = Connection::open(path).unwrap();
            writer.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn online_repair_succeeds_while_codex_style_wal_readers_remain_open() {
        let home = make_fixture();
        let sources = ALLOWED_PROVIDERS
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>();
        let preview =
            preview_projection_at(&home, &sources, "openai", ProjectionScope::All).unwrap();

        let state_reader = Connection::open(home.join("state_5.sqlite")).unwrap();
        let catalog_reader = Connection::open(home.join("sqlite/codex-dev.db")).unwrap();
        for connection in [&state_reader, &catalog_reader] {
            let mode: String = connection
                .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
                .unwrap();
            assert_eq!(mode.to_ascii_lowercase(), "wal");
            connection.execute_batch("BEGIN").unwrap();
        }
        state_reader
            .query_row("SELECT COUNT(*) FROM threads", [], |_| Ok(()))
            .unwrap();
        catalog_reader
            .query_row("SELECT COUNT(*) FROM local_thread_catalog", [], |_| Ok(()))
            .unwrap();

        let repaired = repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            false,
            true,
            Some(&preview.plan_token),
        )
        .unwrap();
        assert!(repaired.verified);

        state_reader.execute_batch("ROLLBACK").unwrap();
        catalog_reader.execute_batch("ROLLBACK").unwrap();
        drop(state_reader);
        drop(catalog_reader);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn restore_guard_holds_both_wal_databases_exclusively() {
        let home = make_fixture();
        for path in [
            home.join("state_5.sqlite"),
            home.join("sqlite/codex-dev.db"),
        ] {
            let connection = Connection::open(path).unwrap();
            let mode: String = connection
                .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
                .unwrap();
            assert_eq!(mode.to_ascii_lowercase(), "wal");
            connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
        }
        let source_root = home.join("restore-source");
        fs::create_dir_all(&source_root).unwrap();
        let source_state = source_root.join("state_5.sqlite");
        let source_catalog = source_root.join("codex-dev.db");
        sqlite_online_copy(&home.join("state_5.sqlite"), &source_state).unwrap();
        sqlite_online_copy(&home.join("sqlite/codex-dev.db"), &source_catalog).unwrap();
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        state
            .execute(
                "UPDATE threads SET model_provider='OpenAI' WHERE id='thread-one'",
                [],
            )
            .unwrap();
        drop(state);
        let catalog = Connection::open(home.join("sqlite/codex-dev.db")).unwrap();
        catalog
        .execute(
            "UPDATE local_thread_catalog SET model_provider='OpenAI' WHERE host_id='local' AND thread_id='thread-one'",
            [],
        )
        .unwrap();
        drop(catalog);
        let guard = DualSqliteRestoreGuard::acquire(&home).unwrap();
        for (path, table) in [
            (home.join("state_5.sqlite"), "threads"),
            (home.join("sqlite/codex-dev.db"), "local_thread_catalog"),
        ] {
            let competing = Connection::open(path).unwrap();
            competing.busy_timeout(Duration::from_millis(25)).unwrap();
            assert!(competing
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |_| Ok(()))
                .is_err());
            assert!(competing
                .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
                .is_err());
        }
        let mut guard = guard;
        guard.restore(&source_state, &source_catalog).unwrap();
        guard.validate().unwrap();
        drop(guard);
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        let provider: String = state
            .query_row(
                "SELECT model_provider FROM threads WHERE id='thread-one'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(provider, "custom");
        drop(state);
        fs::remove_dir_all(home).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn target_provider_guard_denies_config_replacement() {
        let home = make_fixture();
        let guard = acquire_target_provider_guard(&home, "openai").unwrap();
        assert!(fs::write(home.join("config.toml"), "model_provider = \"custom\"\n").is_err());
        drop(guard);
        fs::write(home.join("config.toml"), "model_provider = \"custom\"\n").unwrap();
        assert_eq!(current_provider(&home), "custom");
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn externally_restored_rows_are_reprojected_to_the_current_provider() {
        let home = make_fixture();
        let all_sources = ALLOWED_PROVIDERS
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>();
        repair_projection_at(
            &home,
            &all_sources,
            "openai",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();

        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        state
            .execute(
                "UPDATE threads SET model_provider='custom' WHERE id IN ('thread-one','thread-three')",
                [],
            )
            .unwrap();
        drop(state);
        rewrite_fixture_rollout_provider(&home, "thread-one", "custom");
        rewrite_fixture_rollout_provider(&home, "thread-three", "custom");

        let only_openai = vec!["openai".to_string()];
        let result = repair_projection_at(
            &home,
            &only_openai,
            "openai",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert_eq!(result.changed_threads, 2);
        assert_eq!(result.restored_threads, 0);
        assert!(result.verified);
        assert!(load_projection_store(&home).unwrap().is_some_and(|store| {
            store.threads.keys().cloned().collect::<BTreeSet<_>>()
                == BTreeSet::from([
                    "thread-one".to_string(),
                    "thread-three".to_string(),
                    "thread-subagent".to_string(),
                    "thread-archived".to_string(),
                ])
        }));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn remote_projection_record_is_never_reconciled_or_removed() {
        let home = make_fixture();
        let sources = ALLOWED_PROVIDERS
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>();
        repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        state
            .execute(
                r#"UPDATE threads SET source='{"kind":"vscode","remoteAuthority":"ssh-remote+host"}' WHERE id='thread-three'"#,
                [],
            )
            .unwrap();
        drop(state);

        let result = repair_projection_at(
            &home,
            &sources,
            "openai",
            ProjectionScope::All,
            false,
            false,
            None,
        )
        .unwrap();
        assert_eq!(result.changed_threads, 0);
        assert!(result.skipped_reasons.iter().any(|reason| {
            reason.thread_id.as_deref() == Some("thread-three") && reason.reason == "remote_mapped"
        }));
        assert!(load_projection_store(&home)
            .unwrap()
            .is_some_and(|store| store.threads.contains_key("thread-three")));
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        let provider: String = state
            .query_row(
                "SELECT model_provider FROM threads WHERE id='thread-three'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(provider, "openai");
        drop(state);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn restore_pending_operation_recovers_from_its_recorded_backup() {
        let home = make_fixture();
        let backup = create_backup_at(&home).unwrap();
        save_pending_operation(&home, "restore", Path::new(&backup.path)).unwrap();
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        state
            .execute(
                "UPDATE threads SET model_provider='OpenAI' WHERE id='thread-one'",
                [],
            )
            .unwrap();
        drop(state);
        assert!(scan_at(&home).unwrap().pending_operation.is_some());
        assert!(create_backup_safe_at(&home).is_err());
        restore_backup_at(&home, None).unwrap();
        assert!(load_pending_operation(&home).unwrap().is_none());
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        let provider: String = state
            .query_row(
                "SELECT model_provider FROM threads WHERE id='thread-one'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(provider, "custom");
        drop(state);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn relative_codex_home_restore_pending_recovers_recorded_backup() {
        let relative_home =
            PathBuf::from(fixture().file_name().expect("fixture path has a file name"));
        let home = make_fixture_at(relative_home.clone());
        let backup = create_backup_at(&home).unwrap();
        assert!(Path::new(&backup.path).is_absolute());
        let pending = save_pending_operation(&home, "restore", Path::new(&backup.path)).unwrap();
        assert!(Path::new(&pending.backup_path).is_absolute());
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        state
            .execute(
                "UPDATE threads SET model_provider='OpenAI' WHERE id='thread-one'",
                [],
            )
            .unwrap();
        drop(state);

        restore_backup_at(&home, None).unwrap();
        assert!(load_pending_operation(&home).unwrap().is_none());
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        let provider: String = state
            .query_row(
                "SELECT model_provider FROM threads WHERE id='thread-one'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(provider, "custom");
        drop(state);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn v6_explicit_restore_restores_the_state_database() {
        let home = make_fixture();
        let backup = create_backup_at(&home).unwrap();
        let manifest_path = Path::new(&backup.path).join("manifest.json");
        let manifest: BackupManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest.version, 6);
        assert!(manifest.rollout_provider_preimages.is_empty());
        assert_eq!(
            manifest
                .files
                .iter()
                .filter(|file| file.backed_up)
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["state_5.sqlite"]
        );
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        state
            .execute(
                "UPDATE threads SET model_provider='OpenAI' WHERE id='thread-one'",
                [],
            )
            .unwrap();
        drop(state);

        restore_backup_unchecked(&home, Some(Path::new(&backup.path))).unwrap();
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        assert_eq!(
            state
                .query_row(
                    "SELECT model_provider FROM threads WHERE id='thread-one'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "custom"
        );
        drop(state);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn v6_explicit_restore_restores_rollout_provider_preimage() {
        let home = make_fixture();
        let (backup, row, _) = prepare_v6_rollout_backup(&home);
        let manifest: BackupManifest = serde_json::from_slice(
            &fs::read(Path::new(&backup.path).join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.version, 6);
        assert_eq!(manifest.rollout_provider_preimages, vec![row]);

        restore_backup_unchecked(&home, Some(Path::new(&backup.path))).unwrap();
        assert_eq!(
            rollout::read_provider_image(&home.join("sessions/2026/07/13/thread-one.jsonl"), false)
                .unwrap()
                .provider,
            "custom"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn v6_rollout_restore_preserves_messages_appended_after_backup() {
        let home = make_fixture();
        let (backup, _, original) = prepare_v6_rollout_backup(&home);
        let rollout_path = home.join("sessions/2026/07/13/thread-one.jsonl");
        let appended = b"{\"type\":\"event_msg\",\"payload\":{\"message\":\"after backup\"}}\n";
        OpenOptions::new()
            .append(true)
            .open(&rollout_path)
            .unwrap()
            .write_all(appended)
            .unwrap();

        restore_backup_unchecked(&home, Some(Path::new(&backup.path))).unwrap();
        let restored = fs::read(&rollout_path).unwrap();
        let mut expected = original;
        expected.extend_from_slice(appended);
        assert_eq!(restored, expected);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn failed_v6_restore_safety_snapshot_recovers_entry_provider() {
        let home = make_fixture();
        let (backup, _, _) = prepare_v6_rollout_backup(&home);
        let backup_path = PathBuf::from(&backup.path);
        let manifest_path = backup_path.join("manifest.json");
        let mut manifest: BackupManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest.projection_state_present = true;
        manifest.projection_state_sha256 = Some("0".repeat(64));
        fs::write(backup_path.join("projection-state.json"), b"{}").unwrap();
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let error = restore_backup_at(&home, Some(&backup_path)).unwrap_err();
        assert!(
            error.contains("restore failed and previous state was recovered"),
            "{error}"
        );
        assert!(load_pending_operation(&home).unwrap().is_none());
        assert_eq!(
            rollout::read_provider_image(&home.join("sessions/2026/07/13/thread-one.jsonl"), false)
                .unwrap()
                .provider,
            "OpenAI"
        );

        let safety_manifest = fs::read_dir(ensure_backup_root(&home, false).unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path != &backup_path)
            .find_map(|path| {
                fs::read(path.join("manifest.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<BackupManifest>(&bytes).ok())
                    .filter(|candidate| !candidate.rollout_provider_preimages.is_empty())
            })
            .expect("restore safety backup must include rollout preimages");
        assert_eq!(safety_manifest.version, 6);
        assert_eq!(safety_manifest.kind, BackupKind::RestoreSafety);
        assert_eq!(safety_manifest.rollout_provider_preimages.len(), 1);
        assert_eq!(
            safety_manifest.rollout_provider_preimages[0]
                .before_provider
                .as_deref(),
            Some("OpenAI")
        );
        assert_eq!(
            safety_manifest.rollout_provider_preimages[0]
                .after_provider
                .as_deref(),
            Some("custom")
        );
        fs::remove_dir_all(home).unwrap();
    }

    fn automatic_test_backups(home: &Path, count: usize) -> Vec<PathBuf> {
        (0..count)
            .map(|index| {
                std::thread::sleep(Duration::from_millis(2));
                let backup = create_backup_snapshot(home, None, None, &[]).unwrap();
                let path = PathBuf::from(backup.path);
                let manifest_path = path.join("manifest.json");
                let mut manifest: BackupManifest =
                    serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
                manifest.created_at = format!("2026-07-15T00:{index:02}:00+08:00");
                fs::write(manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
                path
            })
            .collect()
    }

    fn set_test_backup_pinned(path: &Path, pinned: bool) {
        let manifest_path = path.join("manifest.json");
        let mut manifest: BackupManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest.pinned = pinned;
        fs::write(manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    }

    #[test]
    fn backup_retention_keeps_the_five_newest_and_is_idempotent() {
        let home = make_fixture();
        let backups = automatic_test_backups(&home, 7);

        let cleanup =
            cleanup_backups_unlocked_with_policy(&home, false, &[], 5, 2, u64::MAX, true).unwrap();
        assert_eq!(cleanup.removed_count, 2);
        assert!(!backups[0].exists());
        assert!(!backups[1].exists());
        assert!(backups[2..].iter().all(|path| path.exists()));
        assert_eq!(backup_summary_at(&home).unwrap().automatic_count, 5);

        let repeated =
            cleanup_backups_unlocked_with_policy(&home, false, &[], 5, 2, u64::MAX, true).unwrap();
        assert_eq!(repeated.removed_count, 0);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn backup_retention_protects_pending_and_retained_backups() {
        let home = make_fixture();
        let backups = automatic_test_backups(&home, 7);
        set_test_backup_pinned(&backups[0], true);
        save_pending_operation(&home, "repair", &backups[1]).unwrap();

        let cleanup =
            cleanup_backups_unlocked_with_policy(&home, false, &[], 5, 2, u64::MAX, true).unwrap();
        assert_eq!(cleanup.removed_count, 1);
        assert!(backups[0].exists());
        assert!(backups[1].exists());
        assert!(!backups[2].exists());
        assert_eq!(backup_summary_at(&home).unwrap().automatic_count, 5);

        clear_pending_operation(&home).unwrap();
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn backup_retention_capacity_never_drops_below_two_healthy_backups() {
        let home = make_fixture();
        let backups = automatic_test_backups(&home, 3);

        let cleanup = cleanup_backups_unlocked_with_policy(&home, false, &[], 5, 2, 1, true).unwrap();
        assert_eq!(cleanup.removed_count, 1);
        assert!(!backups[0].exists());
        assert!(backups[1].exists());
        assert!(backups[2].exists());
        assert_eq!(backup_summary_at(&home).unwrap().automatic_count, 2);
        assert!(cleanup
            .warnings
            .iter()
            .any(|warning| warning.contains("above its limit")));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn backup_retention_requires_explicit_legacy_cleanup() {
        let home = make_fixture();
        let backups = automatic_test_backups(&home, 3);
        let manifest_path = backups[0].join("manifest.json");
        let mut manifest: BackupManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest.version = 3;
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let automatic =
            cleanup_backups_unlocked_with_policy(&home, false, &[], 5, 2, u64::MAX, true).unwrap();
        assert_eq!(automatic.removed_legacy_count, 0);
        assert!(backups[0].exists());

        let explicit =
            cleanup_backups_unlocked_with_policy(&home, true, &[], 5, 2, u64::MAX, true).unwrap();
        assert_eq!(explicit.removed_legacy_count, 1);
        assert!(!backups[0].exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn backup_retention_removes_damaged_backups_automatically() {
        let home = make_fixture();
        let backups = automatic_test_backups(&home, 6);
        fs::write(backups[0].join("state_5.sqlite"), b"damaged").unwrap();

        let cleanup =
            cleanup_backups_unlocked_with_policy(&home, false, &[], 5, 2, u64::MAX, true).unwrap();
        // 1 damaged removed + 1 healthy pruned to enforce automatic limit of 5.
        assert!(cleanup.removed_count >= 1);
        assert!(!backups[0].exists());
        assert!(cleanup
            .warnings
            .iter()
            .any(|warning| warning.contains("removed damaged backup")));
        let summary = list_backups_at(&home).unwrap();
        assert!(summary
            .entries
            .iter()
            .all(|entry| Path::new(&entry.path) != backups[0]));
        assert!(summary.restorable_count <= 5);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn backup_retention_removes_damaged_backups_and_keeps_healthy_minimum() {
        let home = make_fixture();
        let backups = automatic_test_backups(&home, 6);
        for backup in &backups[..4] {
            fs::write(backup.join("state_5.sqlite"), b"damaged").unwrap();
        }

        let cleanup =
            cleanup_backups_unlocked_with_policy(&home, false, &[], 5, 2, u64::MAX, true).unwrap();
        assert_eq!(cleanup.removed_count, 4);
        assert!(backups[..4].iter().all(|path| !path.exists()));
        assert!(backups[4].exists());
        assert!(backups[5].exists());
        assert_eq!(list_backups_at(&home).unwrap().restorable_count, 2);
        assert!(
            cleanup
                .warnings
                .iter()
                .filter(|warning| warning.contains("removed damaged backup"))
                .count()
                >= 4
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn backup_retention_auto_deletes_an_unparseable_manifest() {
        let home = make_fixture();
        let backup = automatic_test_backups(&home, 1).remove(0);
        fs::write(backup.join("manifest.json"), b"{ truncated").unwrap();

        let summary = backup_summary_at(&home).unwrap();
        let entry = summary
            .entries
            .iter()
            .find(|entry| Path::new(&entry.path) == backup)
            .unwrap();
        assert_eq!(entry.status, "corrupt");
        assert!(!incomplete_backup_is_expired(entry));

        let automatic =
            cleanup_backups_unlocked_with_policy(&home, false, &[], 5, 2, u64::MAX, true).unwrap();
        assert_eq!(automatic.removed_count, 1);
        assert!(!backup.exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn backup_retention_auto_deletes_a_final_backup_with_a_missing_manifest() {
        let home = make_fixture();
        let backup = automatic_test_backups(&home, 1).remove(0);
        fs::remove_file(backup.join("manifest.json")).unwrap();

        let summary = backup_summary_at(&home).unwrap();
        let entry = summary
            .entries
            .iter()
            .find(|entry| Path::new(&entry.path) == backup)
            .unwrap();
        assert_eq!(entry.status, "corrupt");
        assert!(!incomplete_backup_is_expired(entry));

        let cleanup =
            cleanup_backups_unlocked_with_policy(&home, false, &[], 5, 2, u64::MAX, true).unwrap();
        assert_eq!(cleanup.removed_count, 1);
        assert!(!backup.exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn latest_backup_skips_a_newer_snapshot_that_fails_integrity_validation() {
        let home = make_fixture();
        let backups = automatic_test_backups(&home, 3);
        fs::write(backups[2].join("state_5.sqlite"), b"damaged").unwrap();

        let cleanup =
            cleanup_backups_unlocked_with_policy(&home, false, &[], 5, 2, u64::MAX, true).unwrap();
        assert_eq!(cleanup.removed_count, 1);
        assert!(!backups[2].exists());
        assert_eq!(latest_backup(&home).as_deref(), Some(backups[1].as_path()));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn schema_incompatible_sqlite_is_not_a_healthy_or_latest_backup() {
        let home = make_fixture();
        let backups = automatic_test_backups(&home, 3);
        let newest = &backups[2];
        let state_path = newest.join("state_5.sqlite");
        fs::remove_file(&state_path).unwrap();
        let incompatible = Connection::open(&state_path).unwrap();
        incompatible
            .execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY);")
            .unwrap();
        drop(incompatible);

        let manifest_path = newest.join("manifest.json");
        let mut manifest: BackupManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        let state_entry = manifest
            .files
            .iter_mut()
            .find(|file| file.path == "state_5.sqlite")
            .unwrap();
        state_entry.size = fs::metadata(&state_path).unwrap().len();
        state_entry.sha256 = hash_file(&state_path);
        fs::write(manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

        let summary = list_backups_at(&home).unwrap();
        let incompatible_entry = summary
            .entries
            .iter()
            .find(|entry| Path::new(&entry.path) == newest)
            .unwrap();
        assert_eq!(incompatible_entry.status, "corrupt");
        assert!(!incompatible_entry.restorable);

        let cleanup = cleanup_backups_unlocked_with_policy(&home, false, &[], 5, 2, 1, true).unwrap();
        // Damaged newest is deleted; keep at least two healthy points despite capacity=1.
        assert!(cleanup.removed_count >= 1);
        assert!(!newest.exists());
        assert!(backups[0].exists());
        assert!(backups[1].exists());
        assert_eq!(latest_backup(&home).as_deref(), Some(backups[1].as_path()));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn explicit_historical_restore_is_blocked_until_pending_recovery_finishes() {
        let home = make_fixture();
        let backups = automatic_test_backups(&home, 2);

        for command in ["repair", "restore"] {
            save_pending_operation(&home, command, &backups[0]).unwrap();
            let error = restore_backup_at(&home, Some(&backups[1])).unwrap_err();
            assert!(error.contains("historical restore is blocked"), "{error}");
            let pending = load_pending_operation(&home).unwrap().unwrap();
            assert_eq!(pending.command, command);
            assert!(paths_refer_to_same_file(
                Path::new(&pending.backup_path),
                &backups[0]
            ));
            clear_pending_operation(&home).unwrap();
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn insufficient_backup_space_is_rejected_before_a_snapshot_directory_is_created() {
        let home = make_fixture();
        let result = create_backup_snapshot_with_kind_and_available(
            &home,
            None,
            None,
            &[],
            BackupKind::Automatic,
            false,
            Some(0),
        );
        assert!(result
            .unwrap_err()
            .contains("insufficient disk space for a safe recovery backup"));
        assert!(backup_summary_at(&home).unwrap().entries.is_empty());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn backup_retention_pins_explicit_backups_by_default() {
        let home = make_fixture();
        let backup = create_backup_at(&home).unwrap();
        let manifest: BackupManifest = serde_json::from_slice(
            &fs::read(Path::new(&backup.path).join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.kind, BackupKind::Manual);
        assert!(manifest.pinned);
        let summary = set_backup_pinned_at(&home, Path::new(&backup.path), false).unwrap();
        assert!(
            !summary
                .entries
                .iter()
                .find(|entry| entry.path == backup.path)
                .unwrap()
                .pinned
        );
        fs::remove_dir_all(home).unwrap();
    }
}
