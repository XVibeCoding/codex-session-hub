use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    ops::Range,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};
use walkdir::WalkDir;

const MAX_EMPTY_LINES: usize = 8;
const MAX_EMPTY_PREFIX_BYTES: usize = 64 * 1024;
const MAX_PRIMARY_RECORD_BYTES: usize = 1024 * 1024;
const MAX_ENRICHMENT_RECORD_BYTES: usize = 1024 * 1024;
const MAX_ENRICHMENT_SCAN_BYTES: usize = 1024 * 1024;
const MAX_ENRICHMENT_RECORDS: usize = 64;
const MAX_FIRST_USER_MESSAGE_CHARS: usize = 16 * 1024;
const MAX_TITLE_CHARS: usize = 120;
const MAX_PREVIEW_CHARS: usize = 240;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub len: u64,
    pub modified: Option<SystemTime>,
    pub file_id: Option<u128>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PrimaryRollout {
    pub id: String,
    pub path: PathBuf,
    pub archived: bool,
    pub model_provider: Option<String>,
    pub provider_field_missing: bool,
    pub source: Option<Value>,
    pub thread_source: Option<Value>,
    pub cwd: Option<String>,
    pub locality: Value,
    pub session_timestamp: Option<String>,
    pub cli_version: Option<String>,
    pub git: Option<RolloutGitInfo>,
    pub sandbox_policy: Option<Value>,
    pub approval_mode: Option<String>,
    pub first_user_message: Option<String>,
    pub title: Option<String>,
    pub preview: Option<String>,
    pub updated_at_fallback: Option<SystemTime>,
    pub fingerprint: Fingerprint,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RolloutGitInfo {
    pub commit_hash: Option<String>,
    pub branch: Option<String>,
    pub repository_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub path: PathBuf,
    pub code: &'static str,
    pub detail: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RolloutInventory {
    pub rollouts: BTreeMap<String, PrimaryRollout>,
    pub issues: Vec<Issue>,
    pub file_count: usize,
    pub active_file_count: usize,
    pub archived_file_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentFingerprint {
    pub len: u64,
    pub sha256: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderField {
    SnakeCase,
    CamelCase,
}

impl ProviderField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SnakeCase => "model_provider",
            Self::CamelCase => "modelProvider",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderRewriteStatus {
    Changed,
    Unchanged,
    MissingSessionMeta,
    MissingProvider,
    ArchivedExcluded,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProviderRewriteOptions {
    pub include_archived: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderRewritePlan {
    pub path: PathBuf,
    pub archived: bool,
    pub status: ProviderRewriteStatus,
    pub provider_field: Option<ProviderField>,
    pub previous_provider: Option<String>,
    pub target_provider: String,
    pub before_fingerprint: Option<ContentFingerprint>,
    pub after_fingerprint: Option<ContentFingerprint>,
    preimage: Vec<u8>,
    replacement: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderImage {
    pub path: PathBuf,
    pub archived: bool,
    pub provider_field: ProviderField,
    pub provider: String,
    pub fingerprint: ContentFingerprint,
}

impl ProviderRewritePlan {
    pub fn has_change(&self) -> bool {
        self.status == ProviderRewriteStatus::Changed
    }

    pub fn preimage(&self) -> Option<&[u8]> {
        self.has_change().then_some(self.preimage.as_slice())
    }

    pub fn replacement(&self) -> Option<&[u8]> {
        self.has_change().then_some(self.replacement.as_slice())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderRewriteStats {
    pub examined: usize,
    pub changed: usize,
    pub unchanged: usize,
    pub missing_session_meta: usize,
    pub missing_provider: usize,
    pub archived_excluded: usize,
    pub changed_bytes: u64,
}

impl ProviderRewriteStats {
    pub fn record(&mut self, plan: &ProviderRewritePlan) {
        self.examined += 1;
        match plan.status {
            ProviderRewriteStatus::Changed => {
                self.changed += 1;
                self.changed_bytes = self
                    .changed_bytes
                    .saturating_add(plan.replacement.len() as u64);
            }
            ProviderRewriteStatus::Unchanged => self.unchanged += 1,
            ProviderRewriteStatus::MissingSessionMeta => self.missing_session_meta += 1,
            ProviderRewriteStatus::MissingProvider => self.missing_provider += 1,
            ProviderRewriteStatus::ArchivedExcluded => self.archived_excluded += 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedProviderRewrite {
    pub path: PathBuf,
    pub archived: bool,
    pub provider_field: ProviderField,
    pub previous_provider: Option<String>,
    pub target_provider: String,
    pub before_fingerprint: ContentFingerprint,
    pub after_fingerprint: ContentFingerprint,
    preimage: Vec<u8>,
}

impl AppliedProviderRewrite {
    pub fn preimage(&self) -> &[u8] {
        &self.preimage
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderRewriteCommit {
    Applied(AppliedProviderRewrite),
    NoChange(ProviderRewriteStatus),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderRewriteError {
    pub path: PathBuf,
    pub code: &'static str,
    pub detail: String,
}

impl fmt::Display for ProviderRewriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} ({}): {}",
            self.code,
            self.path.display(),
            self.detail
        )
    }
}

impl std::error::Error for ProviderRewriteError {}

struct ExtractedPrimary {
    id: String,
    model_provider: Option<String>,
    provider_field_missing: bool,
    source: Option<Value>,
    thread_source: Option<Value>,
    cwd: Option<String>,
    locality: Value,
    session_timestamp: Option<String>,
    cli_version: Option<String>,
    git: Option<RolloutGitInfo>,
    sandbox_policy: Option<Value>,
    approval_mode: Option<String>,
    first_user_message: Option<String>,
    title: Option<String>,
    preview: Option<String>,
}

struct ReadIssue {
    code: &'static str,
    detail: String,
}

enum BoundedLine {
    Eof,
    Line(Vec<u8>),
    TooLong,
}

struct StableBytes {
    bytes: Vec<u8>,
    fingerprint: ContentFingerprint,
}

enum LocatedProvider {
    Found {
        field: ProviderField,
        value: String,
        token: Range<usize>,
        removal: Range<usize>,
    },
    MissingSessionMeta,
    MissingProvider {
        insertion: usize,
        needs_comma: bool,
    },
    InvalidProvider,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ObjectMemberSpan {
    key: String,
    value: Range<usize>,
    removal: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ObjectLayout {
    members: Vec<ObjectMemberSpan>,
    insertion: usize,
}

struct JsonCursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn read_provider_image(
    path: &Path,
    archived: bool,
) -> Result<ProviderImage, ProviderRewriteError> {
    let StableBytes { bytes, fingerprint } = read_stable_bytes(path)?;
    match locate_first_session_provider(&bytes)
        .map_err(|detail| provider_rewrite_error(path, "provider_location_error", detail))?
    {
        LocatedProvider::Found { field, value, .. } => Ok(ProviderImage {
            path: path.to_path_buf(),
            archived,
            provider_field: field,
            provider: value,
            fingerprint,
        }),
        LocatedProvider::MissingSessionMeta => Err(provider_rewrite_error(
            path,
            "missing_session_meta",
            "rollout has no session_meta record",
        )),
        LocatedProvider::MissingProvider { .. } => Err(provider_rewrite_error(
            path,
            "missing_provider",
            "session_meta has no provider field",
        )),
        LocatedProvider::InvalidProvider => Err(provider_rewrite_error(
            path,
            "invalid_provider",
            "session_meta provider field is not a string",
        )),
    }
}

pub fn remove_provider_if_matches(
    path: &Path,
    _archived: bool,
    expected_provider: &str,
) -> Result<bool, ProviderRewriteError> {
    validate_target_provider(path, expected_provider)?;
    let StableBytes { bytes, fingerprint } = read_stable_bytes(path)?;
    let removal = match locate_first_session_provider(&bytes)
        .map_err(|detail| provider_rewrite_error(path, "provider_location_error", detail))?
    {
        LocatedProvider::Found {
            field: ProviderField::SnakeCase,
            value,
            removal,
            ..
        } if value == expected_provider => removal,
        LocatedProvider::Found { field, value, .. } => {
            return Err(provider_rewrite_error(
                path,
                "provider_remove_conflict",
                format!(
                    "expected inserted model_provider {expected_provider:?}, found {} {value:?}",
                    field.as_str()
                ),
            ));
        }
        LocatedProvider::MissingProvider { .. } => return Ok(false),
        LocatedProvider::InvalidProvider => {
            return Err(provider_rewrite_error(
                path,
                "invalid_provider",
                "session_meta provider field is not a string",
            ));
        }
        LocatedProvider::MissingSessionMeta => {
            return Err(provider_rewrite_error(
                path,
                "missing_session_meta",
                "rollout has no session_meta record",
            ));
        }
    };

    if bytes.get(removal.clone()).is_none() {
        return Err(provider_rewrite_error(
            path,
            "invalid_provider_removal",
            "located provider member is outside the rollout content",
        ));
    }
    let mut replacement = Vec::with_capacity(bytes.len().saturating_sub(removal.len()));
    replacement.extend_from_slice(&bytes[..removal.start]);
    replacement.extend_from_slice(&bytes[removal.end..]);
    if !matches!(
        locate_first_session_provider(&replacement).map_err(|detail| provider_rewrite_error(
            path,
            "provider_location_error",
            detail
        ))?,
        LocatedProvider::MissingProvider { .. }
    ) {
        return Err(provider_rewrite_error(
            path,
            "invalid_provider_removal",
            "provider member removal did not produce a provider-free session_meta payload",
        ));
    }
    let expected_after = content_fingerprint(&replacement);
    replace_file_if_matches(path, &fingerprint, &replacement, &expected_after)?;
    Ok(true)
}

pub fn plan_provider_rewrite(
    path: &Path,
    archived: bool,
    target_provider: &str,
    options: ProviderRewriteOptions,
) -> Result<ProviderRewritePlan, ProviderRewriteError> {
    validate_target_provider(path, target_provider)?;
    if archived && !options.include_archived {
        return Ok(ProviderRewritePlan {
            path: path.to_path_buf(),
            archived,
            status: ProviderRewriteStatus::ArchivedExcluded,
            provider_field: None,
            previous_provider: None,
            target_provider: target_provider.to_owned(),
            before_fingerprint: None,
            after_fingerprint: None,
            preimage: Vec::new(),
            replacement: Vec::new(),
        });
    }

    let StableBytes { bytes, fingerprint } = read_stable_bytes(path)?;
    let located = locate_first_session_provider(&bytes)
        .map_err(|detail| provider_rewrite_error(path, "provider_location_error", detail))?;
    let (status, provider_field, previous_provider, replacement, after_fingerprint) = match located
    {
        LocatedProvider::MissingSessionMeta => (
            ProviderRewriteStatus::MissingSessionMeta,
            None,
            None,
            Vec::new(),
            None,
        ),
        LocatedProvider::MissingProvider {
            insertion,
            needs_comma,
        } => {
            let encoded_provider = serde_json::to_vec(target_provider).map_err(|error| {
                provider_rewrite_error(path, "provider_encoding_error", error.to_string())
            })?;
            let mut inserted = Vec::with_capacity(encoded_provider.len() + 20);
            if needs_comma {
                inserted.push(b',');
            }
            inserted.extend_from_slice(br#""model_provider":"#);
            inserted.extend_from_slice(&encoded_provider);
            let mut replacement = Vec::with_capacity(bytes.len().saturating_add(inserted.len()));
            replacement.extend_from_slice(&bytes[..insertion]);
            replacement.extend_from_slice(&inserted);
            replacement.extend_from_slice(&bytes[insertion..]);
            match locate_first_session_provider(&replacement) {
                Ok(LocatedProvider::Found {
                    field: ProviderField::SnakeCase,
                    value,
                    ..
                }) if value == target_provider => {}
                Ok(_) => {
                    return Err(provider_rewrite_error(
                        path,
                        "invalid_rewrite_plan",
                        "provider insertion did not produce the requested direct model_provider field",
                    ));
                }
                Err(detail) => {
                    return Err(provider_rewrite_error(
                        path,
                        "invalid_rewrite_plan",
                        format!("provider insertion produced invalid JSON: {detail}"),
                    ));
                }
            }
            let after_fingerprint = content_fingerprint(&replacement);
            (
                ProviderRewriteStatus::Changed,
                Some(ProviderField::SnakeCase),
                None,
                replacement,
                Some(after_fingerprint),
            )
        }
        LocatedProvider::InvalidProvider => (
            ProviderRewriteStatus::MissingProvider,
            None,
            None,
            Vec::new(),
            None,
        ),
        LocatedProvider::Found {
            field,
            value,
            token: _,
            removal: _,
        } if value == target_provider => (
            ProviderRewriteStatus::Unchanged,
            Some(field),
            Some(value),
            Vec::new(),
            Some(fingerprint.clone()),
        ),
        LocatedProvider::Found {
            field,
            value,
            token,
            removal: _,
        } => {
            let encoded_provider = serde_json::to_vec(target_provider).map_err(|error| {
                provider_rewrite_error(path, "provider_encoding_error", error.to_string())
            })?;
            let mut replacement = Vec::with_capacity(
                bytes
                    .len()
                    .saturating_sub(token.len())
                    .saturating_add(encoded_provider.len()),
            );
            replacement.extend_from_slice(&bytes[..token.start]);
            replacement.extend_from_slice(&encoded_provider);
            replacement.extend_from_slice(&bytes[token.end..]);
            let after_fingerprint = content_fingerprint(&replacement);
            (
                ProviderRewriteStatus::Changed,
                Some(field),
                Some(value),
                replacement,
                Some(after_fingerprint),
            )
        }
    };
    let preimage = if status == ProviderRewriteStatus::Changed {
        bytes
    } else {
        Vec::new()
    };
    Ok(ProviderRewritePlan {
        path: path.to_path_buf(),
        archived,
        status,
        provider_field,
        previous_provider,
        target_provider: target_provider.to_owned(),
        before_fingerprint: Some(fingerprint),
        after_fingerprint,
        preimage,
        replacement,
    })
}

pub fn commit_provider_rewrite(
    plan: ProviderRewritePlan,
) -> Result<ProviderRewriteCommit, ProviderRewriteError> {
    if !plan.has_change() {
        return Ok(ProviderRewriteCommit::NoChange(plan.status));
    }
    let provider_field = plan.provider_field.ok_or_else(|| {
        provider_rewrite_error(
            &plan.path,
            "invalid_rewrite_plan",
            "changed plan does not identify a provider field",
        )
    })?;
    let previous_provider = plan.previous_provider.clone();
    let before_fingerprint = plan.before_fingerprint.clone().ok_or_else(|| {
        provider_rewrite_error(
            &plan.path,
            "invalid_rewrite_plan",
            "changed plan does not contain its preimage fingerprint",
        )
    })?;
    let expected_after = plan.after_fingerprint.clone().ok_or_else(|| {
        provider_rewrite_error(
            &plan.path,
            "invalid_rewrite_plan",
            "changed plan does not contain its replacement fingerprint",
        )
    })?;
    let after_fingerprint = replace_file_if_matches(
        &plan.path,
        &before_fingerprint,
        &plan.replacement,
        &expected_after,
    )?;
    Ok(ProviderRewriteCommit::Applied(AppliedProviderRewrite {
        path: plan.path,
        archived: plan.archived,
        provider_field,
        previous_provider,
        target_provider: plan.target_provider,
        before_fingerprint,
        after_fingerprint,
        preimage: plan.preimage,
    }))
}

pub fn rollback_provider_rewrite(
    applied: AppliedProviderRewrite,
) -> Result<ContentFingerprint, ProviderRewriteError> {
    replace_file_if_matches(
        &applied.path,
        &applied.after_fingerprint,
        &applied.preimage,
        &applied.before_fingerprint,
    )
}

fn validate_target_provider(
    path: &Path,
    target_provider: &str,
) -> Result<(), ProviderRewriteError> {
    if target_provider.is_empty() || target_provider.trim() != target_provider {
        return Err(provider_rewrite_error(
            path,
            "invalid_target_provider",
            "target provider must be non-empty and cannot have leading or trailing whitespace",
        ));
    }
    Ok(())
}

fn provider_rewrite_error(
    path: &Path,
    code: &'static str,
    detail: impl Into<String>,
) -> ProviderRewriteError {
    ProviderRewriteError {
        path: path.to_path_buf(),
        code,
        detail: detail.into(),
    }
}

fn content_fingerprint(bytes: &[u8]) -> ContentFingerprint {
    let digest = Sha256::digest(bytes);
    ContentFingerprint {
        len: bytes.len() as u64,
        sha256: digest.iter().map(|byte| format!("{byte:02x}")).collect(),
    }
}

fn read_stable_bytes(path: &Path) -> Result<StableBytes, ProviderRewriteError> {
    for attempt in 0..3 {
        let before = regular_file_fingerprint(path)?;
        let bytes = fs::read(path)
            .map_err(|error| provider_rewrite_error(path, "read_error", error.to_string()))?;
        let after = regular_file_fingerprint(path)?;
        if before == after && after.len == bytes.len() as u64 {
            return Ok(StableBytes {
                fingerprint: content_fingerprint(&bytes),
                bytes,
            });
        }
        if attempt == 2 {
            return Err(provider_rewrite_error(
                path,
                "volatile_file",
                "rollout changed during all three full-file reads",
            ));
        }
    }
    unreachable!("three stable read attempts must return")
}

fn regular_file_fingerprint(path: &Path) -> Result<Fingerprint, ProviderRewriteError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| provider_rewrite_error(path, "metadata_error", error.to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(provider_rewrite_error(
            path,
            "not_regular_file",
            "rollout must be a regular file and cannot be a symbolic link",
        ));
    }
    Ok(Fingerprint {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        file_id: platform_file_id(&metadata),
    })
}

fn locate_first_session_provider(bytes: &[u8]) -> Result<LocatedProvider, String> {
    let mut line_start = 0;
    while line_start < bytes.len() {
        let line_end = bytes[line_start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |offset| line_start + offset + 1);
        let line = &bytes[line_start..line_end];
        let bom_len = usize::from(line.starts_with(&[0xef, 0xbb, 0xbf])) * 3;
        let record = &line[bom_len..];
        line_start = line_end;
        if record.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(record) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let Some(payload) = value.get("payload").and_then(Value::as_object) else {
            continue;
        };
        if payload
            .get("id")
            .and_then(Value::as_str)
            .is_none_or(|id| id.trim().is_empty())
        {
            continue;
        }
        let record_layout = direct_object_layout(record)?;
        let payload_members = record_layout
            .members
            .iter()
            .filter(|member| member.key == "payload")
            .collect::<Vec<_>>();
        if payload_members.len() != 1 {
            return Err("session_meta must contain exactly one direct payload field".into());
        }
        let payload_span = payload_members[0].value.clone();
        let payload_bytes = &record[payload_span.clone()];
        let payload_layout = direct_object_layout(payload_bytes)?;
        let provider_members = payload_layout
            .members
            .iter()
            .filter(|member| matches!(member.key.as_str(), "model_provider" | "modelProvider"))
            .collect::<Vec<_>>();
        if provider_members.len() > 1 {
            return Err("session_meta payload contains duplicate provider fields".into());
        }
        let record_start = line_start
            .saturating_sub(line.len())
            .saturating_add(bom_len);
        let payload_start = record_start.saturating_add(payload_span.start);
        let Some(member) = provider_members.first() else {
            return Ok(LocatedProvider::MissingProvider {
                insertion: payload_start.saturating_add(payload_layout.insertion),
                needs_comma: !payload_layout.members.is_empty(),
            });
        };
        let field = if member.key == "model_provider" {
            ProviderField::SnakeCase
        } else {
            ProviderField::CamelCase
        };
        let Some(previous_provider) =
            serde_json::from_slice::<Value>(&payload_bytes[member.value.clone()])
                .map_err(|error| error.to_string())?
                .as_str()
                .map(str::to_owned)
        else {
            return Ok(LocatedProvider::InvalidProvider);
        };
        let token = payload_start.saturating_add(member.value.start)
            ..payload_start.saturating_add(member.value.end);
        let removal = payload_start.saturating_add(member.removal.start)
            ..payload_start.saturating_add(member.removal.end);
        if bytes.get(token.clone()).is_none_or(|value| {
            serde_json::from_slice::<String>(value).ok().as_deref()
                != Some(previous_provider.as_str())
        }) {
            return Err("located provider token does not match parsed provider value".into());
        }
        return Ok(LocatedProvider::Found {
            field,
            value: previous_provider,
            token,
            removal,
        });
    }
    Ok(LocatedProvider::MissingSessionMeta)
}

fn direct_object_layout(bytes: &[u8]) -> Result<ObjectLayout, String> {
    let mut cursor = JsonCursor { bytes, position: 0 };
    cursor.skip_whitespace();
    cursor.expect(b'{')?;
    cursor.skip_whitespace();
    let mut raw_members = Vec::new();
    let mut preceding_comma = None;
    if cursor.consume(b'}') {
        return Ok(ObjectLayout {
            members: Vec::new(),
            insertion: cursor.position - 1,
        });
    }
    loop {
        cursor.skip_whitespace();
        let member_start = cursor.position;
        let key = cursor.parse_string()?;
        cursor.skip_whitespace();
        cursor.expect(b':')?;
        cursor.skip_whitespace();
        let value_start = cursor.position;
        cursor.skip_value(0)?;
        let value = value_start..cursor.position;
        let insertion = value.end;
        cursor.skip_whitespace();
        if cursor.consume(b'}') {
            raw_members.push((key, value, member_start, preceding_comma, None));
            let members = raw_members
                .into_iter()
                .map(|(key, value, member_start, comma_before, comma_after)| {
                    let removal = if let Some(comma_after) = comma_after {
                        member_start..comma_after + 1
                    } else if let Some(comma_before) = comma_before {
                        comma_before..value.end
                    } else {
                        member_start..value.end
                    };
                    ObjectMemberSpan {
                        key,
                        value,
                        removal,
                    }
                })
                .collect();
            return Ok(ObjectLayout { members, insertion });
        }
        let comma_after = cursor.position;
        cursor.expect(b',')?;
        raw_members.push((key, value, member_start, preceding_comma, Some(comma_after)));
        preceding_comma = Some(comma_after);
    }
}

impl JsonCursor<'_> {
    fn skip_whitespace(&mut self) {
        while self
            .bytes
            .get(self.position)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.position += 1;
        }
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.bytes.get(self.position) == Some(&expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: u8) -> Result<(), String> {
        if self.consume(expected) {
            Ok(())
        } else {
            Err(format!(
                "expected JSON byte {:?} at offset {}",
                expected as char, self.position
            ))
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        let start = self.position;
        self.expect(b'"')?;
        let mut escaped = false;
        while let Some(byte) = self.bytes.get(self.position).copied() {
            self.position += 1;
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                return serde_json::from_slice::<String>(&self.bytes[start..self.position])
                    .map_err(|error| error.to_string());
            }
        }
        Err("unterminated JSON string".into())
    }

    fn skip_value(&mut self, depth: usize) -> Result<(), String> {
        if depth > 256 {
            return Err("JSON nesting exceeds rewrite parser limit".into());
        }
        self.skip_whitespace();
        match self.bytes.get(self.position).copied() {
            Some(b'"') => self.parse_string().map(|_| ()),
            Some(b'{') => self.skip_object(depth + 1),
            Some(b'[') => self.skip_array(depth + 1),
            Some(_) => {
                let start = self.position;
                while self.bytes.get(self.position).is_some_and(|byte| {
                    !byte.is_ascii_whitespace() && !matches!(*byte, b',' | b']' | b'}')
                }) {
                    self.position += 1;
                }
                if self.position == start {
                    Err(format!("missing JSON value at offset {start}"))
                } else {
                    Ok(())
                }
            }
            None => Err("missing JSON value at end of record".into()),
        }
    }

    fn skip_object(&mut self, depth: usize) -> Result<(), String> {
        self.expect(b'{')?;
        self.skip_whitespace();
        if self.consume(b'}') {
            return Ok(());
        }
        loop {
            self.skip_whitespace();
            self.parse_string()?;
            self.skip_whitespace();
            self.expect(b':')?;
            self.skip_value(depth)?;
            self.skip_whitespace();
            if self.consume(b'}') {
                return Ok(());
            }
            self.expect(b',')?;
        }
    }

    fn skip_array(&mut self, depth: usize) -> Result<(), String> {
        self.expect(b'[')?;
        self.skip_whitespace();
        if self.consume(b']') {
            return Ok(());
        }
        loop {
            self.skip_value(depth)?;
            self.skip_whitespace();
            if self.consume(b']') {
                return Ok(());
            }
            self.expect(b',')?;
        }
    }
}

fn replace_file_if_matches(
    path: &Path,
    expected_before: &ContentFingerprint,
    replacement: &[u8],
    expected_after: &ContentFingerprint,
) -> Result<ContentFingerprint, ProviderRewriteError> {
    if &content_fingerprint(replacement) != expected_after {
        return Err(provider_rewrite_error(
            path,
            "invalid_rewrite_plan",
            "replacement bytes do not match their planned fingerprint",
        ));
    }
    let current = read_stable_bytes(path)?;
    if &current.fingerprint != expected_before {
        return Err(provider_rewrite_error(
            path,
            "fingerprint_conflict",
            "rollout content changed after the rewrite was planned",
        ));
    }
    let permissions = fs::metadata(path)
        .map_err(|error| provider_rewrite_error(path, "metadata_error", error.to_string()))?
        .permissions();
    let (temporary_path, mut temporary_file) = create_temporary_file(path)?;
    let write_result = (|| {
        temporary_file.write_all(replacement).map_err(|error| {
            provider_rewrite_error(path, "temporary_write_error", error.to_string())
        })?;
        temporary_file.sync_all().map_err(|error| {
            provider_rewrite_error(path, "temporary_sync_error", error.to_string())
        })?;
        fs::set_permissions(&temporary_path, permissions).map_err(|error| {
            provider_rewrite_error(path, "temporary_permission_error", error.to_string())
        })?;
        temporary_file.sync_all().map_err(|error| {
            provider_rewrite_error(path, "temporary_sync_error", error.to_string())
        })?;
        drop(temporary_file);

        let temporary = read_stable_bytes(&temporary_path)?;
        if &temporary.fingerprint != expected_after {
            return Err(provider_rewrite_error(
                path,
                "temporary_verification_error",
                "temporary rollout does not match the planned replacement",
            ));
        }
        let live = read_stable_bytes(path)?;
        if &live.fingerprint != expected_before {
            return Err(provider_rewrite_error(
                path,
                "fingerprint_conflict",
                "rollout changed while its atomic replacement was being prepared",
            ));
        }
        crate::platform::atomic_replace_file(&temporary_path, path)
            .map_err(|detail| provider_rewrite_error(path, "atomic_replace_error", detail))?;
        sync_parent_directory(path)?;
        let written = read_stable_bytes(path)?;
        if &written.fingerprint != expected_after {
            return Err(provider_rewrite_error(
                path,
                "post_write_conflict",
                "rollout changed immediately after atomic replacement",
            ));
        }
        Ok(written.fingerprint)
    })();
    if temporary_path.exists() {
        let _ = fs::remove_file(&temporary_path);
    }
    write_result
}

fn create_temporary_file(path: &Path) -> Result<(PathBuf, File), ProviderRewriteError> {
    let parent = path.parent().ok_or_else(|| {
        provider_rewrite_error(
            path,
            "invalid_rollout_path",
            "rollout has no parent directory",
        )
    })?;
    for _ in 0..32 {
        let nonce = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary_path = parent.join(format!(
            ".provider-hub-rollout-{}-{nonce}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((temporary_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(provider_rewrite_error(
                    path,
                    "temporary_create_error",
                    error.to_string(),
                ));
            }
        }
    }
    Err(provider_rewrite_error(
        path,
        "temporary_create_error",
        "could not allocate a unique temporary rollout path",
    ))
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) -> Result<(), ProviderRewriteError> {
    let parent = path.parent().ok_or_else(|| {
        provider_rewrite_error(
            path,
            "invalid_rollout_path",
            "rollout has no parent directory",
        )
    })?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| provider_rewrite_error(path, "directory_sync_error", error.to_string()))
}

#[cfg(windows)]
fn sync_parent_directory(_path: &Path) -> Result<(), ProviderRewriteError> {
    Ok(())
}

pub fn scan_rollouts(home: &Path) -> RolloutInventory {
    let mut inventory = RolloutInventory::default();
    let mut candidates: BTreeMap<String, Vec<PrimaryRollout>> = BTreeMap::new();

    for (directory, archived) in [("sessions", false), ("archived_sessions", true)] {
        let root = home.join(directory);
        if !root.is_dir() {
            continue;
        }

        for entry in WalkDir::new(&root).follow_links(false) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    inventory.issues.push(Issue {
                        path: error.path().unwrap_or(&root).to_path_buf(),
                        code: "walk_error",
                        detail: error.to_string(),
                    });
                    continue;
                }
            };
            if !entry.file_type().is_file()
                || !entry
                    .path()
                    .extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|value| value.eq_ignore_ascii_case("jsonl"))
            {
                continue;
            }

            inventory.file_count += 1;
            if archived {
                inventory.archived_file_count += 1;
            } else {
                inventory.active_file_count += 1;
            }

            match read_stable_primary(entry.path(), archived) {
                Ok(primary) => candidates
                    .entry(primary.id.clone())
                    .or_default()
                    .push(primary),
                Err(issue) => inventory.issues.push(issue),
            }
        }
    }

    for (id, mut matches) in candidates {
        if matches.len() == 1 {
            let primary = matches.pop().expect("single primary rollout");
            inventory.rollouts.insert(id, primary);
            continue;
        }

        matches.sort_by(|left, right| left.path.cmp(&right.path));
        let paths = matches
            .iter()
            .map(|primary| primary.path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        for primary in matches {
            inventory.issues.push(Issue {
                path: primary.path,
                code: "duplicate_primary_id",
                detail: format!("primary id {id} appears in multiple files: {paths}"),
            });
        }
    }

    inventory.issues.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.code.cmp(right.code))
    });
    inventory
}

pub fn read_primary_rollout(path: &Path, archived: bool) -> Result<PrimaryRollout, Issue> {
    read_stable_primary(path, archived)
}

fn read_stable_primary(path: &Path, archived: bool) -> Result<PrimaryRollout, Issue> {
    for attempt in 0..2 {
        let before = file_fingerprint(path).map_err(|error| Issue {
            path: path.to_path_buf(),
            code: "metadata_error",
            detail: error.to_string(),
        })?;
        let parsed = read_primary_record(path);
        let after = file_fingerprint(path);

        match after {
            Ok(after) if before == after => {
                return parsed
                    .map(|primary| PrimaryRollout {
                        id: primary.id,
                        path: path.to_path_buf(),
                        archived,
                        model_provider: primary.model_provider,
                        provider_field_missing: primary.provider_field_missing,
                        source: primary.source,
                        thread_source: primary.thread_source,
                        cwd: primary.cwd,
                        locality: primary.locality,
                        session_timestamp: primary.session_timestamp,
                        cli_version: primary.cli_version,
                        git: primary.git,
                        sandbox_policy: primary.sandbox_policy,
                        approval_mode: primary.approval_mode,
                        first_user_message: primary.first_user_message,
                        title: primary.title,
                        preview: primary.preview,
                        updated_at_fallback: after.modified,
                        fingerprint: after,
                    })
                    .map_err(|issue| Issue {
                        path: path.to_path_buf(),
                        code: issue.code,
                        detail: issue.detail,
                    });
            }
            Ok(_) if attempt == 0 => continue,
            Ok(_) => {
                return Err(Issue {
                    path: path.to_path_buf(),
                    code: "volatile_file",
                    detail: "file length or modification identity changed during both reads".into(),
                });
            }
            Err(_) if attempt == 0 => continue,
            Err(error) => {
                return Err(Issue {
                    path: path.to_path_buf(),
                    code: "volatile_file",
                    detail: format!("file metadata became unavailable while reading: {error}"),
                });
            }
        }
    }
    unreachable!("two rollout read attempts must return")
}

fn read_primary_record(path: &Path) -> Result<ExtractedPrimary, ReadIssue> {
    let file = File::open(path).map_err(|error| ReadIssue {
        code: "read_error",
        detail: error.to_string(),
    })?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut empty_lines = 0;
    let mut empty_prefix_bytes = 0;

    loop {
        let line =
            match read_bounded_line(&mut reader, MAX_PRIMARY_RECORD_BYTES).map_err(|error| {
                ReadIssue {
                    code: "read_error",
                    detail: error.to_string(),
                }
            })? {
                BoundedLine::Eof => {
                    return Err(ReadIssue {
                        code: "empty_file",
                        detail: "no primary JSON record was found".into(),
                    });
                }
                BoundedLine::TooLong => {
                    return Err(ReadIssue {
                        code: "primary_record_too_large",
                        detail: format!(
                            "primary JSON record exceeds {MAX_PRIMARY_RECORD_BYTES} bytes"
                        ),
                    });
                }
                BoundedLine::Line(line) => line,
            };

        let record = strip_utf8_bom(&line);
        if record.iter().all(u8::is_ascii_whitespace) {
            empty_lines += 1;
            empty_prefix_bytes += line.len();
            if empty_lines > MAX_EMPTY_LINES {
                return Err(ReadIssue {
                    code: "too_many_empty_lines",
                    detail: format!(
                        "more than {MAX_EMPTY_LINES} empty lines precede primary record"
                    ),
                });
            }
            if empty_prefix_bytes > MAX_EMPTY_PREFIX_BYTES {
                return Err(ReadIssue {
                    code: "empty_prefix_too_large",
                    detail: format!("empty prefix exceeds {MAX_EMPTY_PREFIX_BYTES} bytes"),
                });
            }
            continue;
        }

        let mut primary = parse_primary(path, record)?;
        scan_enrichment_records(&mut reader, &mut primary);
        return Ok(primary);
    }
}

fn parse_primary(path: &Path, record: &[u8]) -> Result<ExtractedPrimary, ReadIssue> {
    let value = serde_json::from_slice::<Value>(record).map_err(|error| ReadIssue {
        code: "invalid_primary_json",
        detail: error.to_string(),
    })?;
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return Err(ReadIssue {
            code: "primary_not_session_meta",
            detail: "first non-empty JSON record is not session_meta".into(),
        });
    }
    let payload = value.get("payload").ok_or_else(|| ReadIssue {
        code: "missing_primary_id",
        detail: "session_meta payload is missing".into(),
    })?;
    let id = payload
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| ReadIssue {
            code: "missing_primary_id",
            detail: "session_meta payload.id is missing".into(),
        })?
        .to_owned();
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if !stem.ends_with(&id) {
        return Err(ReadIssue {
            code: "filename_id_mismatch",
            detail: format!("file stem does not end with primary id {id}"),
        });
    }

    let provider_field_missing =
        payload.get("model_provider").is_none() && payload.get("modelProvider").is_none();
    let model_provider = payload
        .get("model_provider")
        .or_else(|| payload.get("modelProvider"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let source = payload.get("source").cloned();
    let thread_source = payload
        .get("thread_source")
        .or_else(|| payload.get("threadSource"))
        .cloned();
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let session_timestamp = payload
        .get("timestamp")
        .or_else(|| value.get("timestamp"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let cli_version = payload
        .get("cli_version")
        .or_else(|| payload.get("cliVersion"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let git = payload
        .get("git")
        .or_else(|| payload.get("git_info"))
        .or_else(|| payload.get("gitInfo"))
        .and_then(extract_git_info);
    let mut locality = serde_json::Map::new();
    for key in [
        "remote",
        "remoteAuthority",
        "remote_authority",
        "hostId",
        "host_id",
        "kind",
        "type",
        "sourceKind",
        "source_kind",
        "workspaceUri",
        "workspace_uri",
    ] {
        if let Some(value) = payload.get(key) {
            locality.insert(key.into(), value.clone());
        }
    }
    if let Some(value) = &source {
        locality.insert("source".into(), value.clone());
    }
    Ok(ExtractedPrimary {
        id,
        model_provider,
        provider_field_missing,
        source,
        thread_source,
        cwd,
        locality: Value::Object(locality),
        session_timestamp,
        cli_version,
        git,
        sandbox_policy: None,
        approval_mode: None,
        first_user_message: None,
        title: None,
        preview: None,
    })
}

fn extract_git_info(value: &Value) -> Option<RolloutGitInfo> {
    let object = value.as_object()?;
    let string = |snake: &str, camel: &str| {
        object
            .get(snake)
            .or_else(|| object.get(camel))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let info = RolloutGitInfo {
        commit_hash: string("commit_hash", "commitHash"),
        branch: string("branch", "branch"),
        repository_url: string("repository_url", "repositoryUrl"),
    };
    (info != RolloutGitInfo::default()).then_some(info)
}

fn scan_enrichment_records<R: BufRead>(reader: &mut R, primary: &mut ExtractedPrimary) {
    let mut scanned_bytes = 0usize;
    for _ in 0..MAX_ENRICHMENT_RECORDS {
        let line = match read_bounded_line(reader, MAX_ENRICHMENT_RECORD_BYTES) {
            Ok(BoundedLine::Line(line)) => line,
            Ok(BoundedLine::Eof | BoundedLine::TooLong) | Err(_) => break,
        };
        scanned_bytes = scanned_bytes.saturating_add(line.len());
        if scanned_bytes > MAX_ENRICHMENT_SCAN_BYTES {
            break;
        }
        let record = strip_utf8_bom(&line);
        if record.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(record) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("turn_context") {
            extract_turn_context(&value, primary);
        }
        if primary.first_user_message.is_none() {
            if let Some(message) = extract_real_user_message(&value) {
                populate_user_text(primary, &message);
            }
        }
        // A real user message follows the session's initial turn context in
        // supported rollout formats. Stop here so scan cost stays independent
        // of the rest of a potentially very large conversation.
        if primary.first_user_message.is_some() {
            break;
        }
    }
}

fn extract_turn_context(value: &Value, primary: &mut ExtractedPrimary) {
    let Some(payload) = value.get("payload") else {
        return;
    };
    if primary.sandbox_policy.is_none() {
        primary.sandbox_policy = payload
            .get("sandbox_policy")
            .or_else(|| payload.get("sandboxPolicy"))
            .filter(|value| !value.is_null())
            .cloned();
    }
    if primary.approval_mode.is_none() {
        primary.approval_mode = payload
            .get("approval_mode")
            .or_else(|| payload.get("approvalMode"))
            .or_else(|| payload.get("approval_policy"))
            .or_else(|| payload.get("approvalPolicy"))
            .and_then(|value| {
                value.as_str().or_else(|| {
                    value
                        .get("mode")
                        .or_else(|| value.get("type"))
                        .and_then(Value::as_str)
                })
            })
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
    }
}

fn extract_real_user_message(value: &Value) -> Option<String> {
    let record_type = value.get("type").and_then(Value::as_str)?;
    let payload = value.get("payload")?;
    let message = match record_type {
        "event_msg" if payload.get("type").and_then(Value::as_str) == Some("user_message") => {
            payload.get("message").and_then(Value::as_str)?.to_owned()
        }
        "response_item"
            if payload.get("type").and_then(Value::as_str) == Some("message")
                && payload.get("role").and_then(Value::as_str) == Some("user") =>
        {
            extract_response_user_content(payload)?
        }
        _ => return None,
    };
    let message = message.trim();
    if message.is_empty() || is_synthetic_user_context(message) {
        None
    } else {
        Some(message.to_owned())
    }
}

fn extract_response_user_content(payload: &Value) -> Option<String> {
    if let Some(content) = payload.get("content").and_then(Value::as_str) {
        return Some(content.to_owned());
    }
    let content = payload.get("content")?.as_array()?;
    let parts = content
        .iter()
        .filter(|part| {
            matches!(
                part.get("type").and_then(Value::as_str),
                Some("input_text" | "text")
            )
        })
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn is_synthetic_user_context(message: &str) -> bool {
    const SYNTHETIC_PREFIXES: &[&str] = &[
        "<environment_context>",
        "<permissions instructions>",
        "<app-context>",
        "<collaboration_mode>",
        "<skills_instructions>",
        "<plugins_instructions>",
        "<turn_aborted>",
    ];
    SYNTHETIC_PREFIXES
        .iter()
        .any(|prefix| message.starts_with(prefix))
}

fn populate_user_text(primary: &mut ExtractedPrimary, message: &str) {
    let first_user_message = truncate_chars(message.trim(), MAX_FIRST_USER_MESSAGE_CHARS);
    if first_user_message.is_empty() {
        return;
    }
    let title_source = first_user_message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(first_user_message.as_str());
    let title = truncate_chars(
        &title_source
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        MAX_TITLE_CHARS,
    );
    let preview = truncate_chars(
        &first_user_message
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        MAX_PREVIEW_CHARS,
    );
    primary.first_user_message = Some(first_user_message);
    primary.title = (!title.is_empty()).then_some(title);
    primary.preview = (!preview.is_empty()).then_some(preview);
}

fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn read_bounded_line<R: BufRead>(reader: &mut R, limit: usize) -> io::Result<BoundedLine> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if line.is_empty() {
                BoundedLine::Eof
            } else {
                BoundedLine::Line(line)
            });
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(consumed) > limit {
            return Ok(BoundedLine::TooLong);
        }
        line.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(BoundedLine::Line(line));
        }
    }
}

fn strip_utf8_bom(value: &[u8]) -> &[u8] {
    value.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(value)
}

fn file_fingerprint(path: &Path) -> io::Result<Fingerprint> {
    let metadata = fs::metadata(path)?;
    Ok(Fingerprint {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        file_id: platform_file_id(&metadata),
    })
}

fn platform_file_id(_metadata: &Metadata) -> Option<u128> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestHome(PathBuf);

    impl TestHome {
        fn new() -> Self {
            let nonce = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "codex-session-hub-rollout-test-{}-{timestamp}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(path.join("sessions/2026/07/13")).unwrap();
            fs::create_dir_all(path.join("archived_sessions")).unwrap();
            Self(path)
        }

        fn active(&self, id: &str) -> PathBuf {
            self.0
                .join("sessions/2026/07/13")
                .join(format!("rollout-2026-07-13T00-00-00-{id}.jsonl"))
        }

        fn archived(&self, id: &str) -> PathBuf {
            self.0
                .join("archived_sessions")
                .join(format!("rollout-2026-07-12T00-00-00-{id}.jsonl"))
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn metadata(id: &str, source: Value) -> String {
        serde_json::json!({
            "type": "session_meta",
            "payload": {
                "id": id,
                "model_provider": "OpenAI",
                "source": source
            }
        })
        .to_string()
    }

    #[test]
    fn accepts_large_primary_record_below_limit() {
        let home = TestHome::new();
        let id = "019f0000-0000-7000-8000-000000000001";
        let record = metadata(id, Value::String("x".repeat(128 * 1024)));
        fs::write(home.active(id), format!("{record}\n")).unwrap();

        let inventory = scan_rollouts(&home.0);
        let primary = inventory.rollouts.get(id).unwrap();
        assert!(!primary.archived);
        assert_eq!(primary.model_provider.as_deref(), Some("OpenAI"));
        assert!(inventory.issues.is_empty());
    }

    #[test]
    fn ignores_parent_metadata_after_primary_record() {
        let home = TestHome::new();
        let child = "019f0000-0000-7000-8000-000000000002";
        let parent = "019f0000-0000-7000-8000-000000000003";
        let content = format!(
            "{}\n{}\n{{\"type\":\"event_msg\",\"payload\":{{}}}}\n",
            metadata(
                child,
                serde_json::json!({"subagent": {"parent_thread_id": parent}})
            ),
            metadata(parent, Value::String("vscode".into()))
        );
        fs::write(home.active(child), content).unwrap();

        let inventory = scan_rollouts(&home.0);
        assert!(inventory.rollouts.contains_key(child));
        assert!(!inventory.rollouts.contains_key(parent));
        assert!(inventory.issues.is_empty());
    }

    #[test]
    fn rejects_filename_mismatch_and_duplicate_primary_ids() {
        let home = TestHome::new();
        let id = "019f0000-0000-7000-8000-000000000004";
        let other = "019f0000-0000-7000-8000-000000000005";
        fs::write(
            home.active(other),
            format!("{}\n", metadata(id, Value::Null)),
        )
        .unwrap();

        let duplicate = "019f0000-0000-7000-8000-000000000006";
        fs::write(
            home.active(duplicate),
            format!("{}\n", metadata(duplicate, Value::Null)),
        )
        .unwrap();
        fs::write(
            home.archived(duplicate),
            format!("{}\n", metadata(duplicate, Value::Null)),
        )
        .unwrap();

        let inventory = scan_rollouts(&home.0);
        assert!(!inventory.rollouts.contains_key(id));
        assert!(!inventory.rollouts.contains_key(duplicate));
        assert_eq!(
            inventory
                .issues
                .iter()
                .filter(|issue| issue.code == "filename_id_mismatch")
                .count(),
            1
        );
        assert_eq!(
            inventory
                .issues
                .iter()
                .filter(|issue| issue.code == "duplicate_primary_id")
                .count(),
            2
        );
    }

    #[test]
    fn accepts_bom_after_empty_prefix_and_reports_empty_file() {
        let home = TestHome::new();
        let id = "019f0000-0000-7000-8000-000000000007";
        let mut content = b"\n\r\n".to_vec();
        content.extend_from_slice(&[0xef, 0xbb, 0xbf]);
        content.extend_from_slice(metadata(id, Value::String("cli".into())).as_bytes());
        content.push(b'\n');
        fs::write(home.active(id), content).unwrap();
        fs::write(home.active("019f0000-0000-7000-8000-000000000008"), []).unwrap();

        let inventory = scan_rollouts(&home.0);
        assert!(inventory.rollouts.contains_key(id));
        assert_eq!(
            inventory
                .issues
                .iter()
                .filter(|issue| issue.code == "empty_file")
                .count(),
            1
        );
    }

    #[test]
    fn rewrites_only_first_valid_session_provider_and_can_rollback() {
        let home = TestHome::new();
        let id = "019f0000-0000-7000-8000-000000000009";
        let later_id = "019f0000-0000-7000-8000-000000000010";
        let original = format!(
            "not-json\r\n{{\"type\":\"event_msg\",\"payload\":{{}}}}\r\n  {{ \"type\" : \"session_meta\", \"payload\" : {{ \"id\" : \"{id}\", \"source\":{{\"model_provider\":\"nested\"}}, \"model_provider\" : \"OpenAI\", \"cwd\":\"C:\\\\work\" }} }}\r\n{}\n",
            metadata(later_id, Value::String("cli".into()))
        );
        let path = home.active(id);
        fs::write(&path, original.as_bytes()).unwrap();

        let plan = plan_provider_rewrite(&path, false, "custom", ProviderRewriteOptions::default())
            .unwrap();
        assert_eq!(plan.status, ProviderRewriteStatus::Changed);
        assert_eq!(plan.provider_field, Some(ProviderField::SnakeCase));
        assert_eq!(plan.previous_provider.as_deref(), Some("OpenAI"));
        let expected = original.replacen(
            "\"model_provider\" : \"OpenAI\"",
            "\"model_provider\" : \"custom\"",
            1,
        );
        assert_eq!(plan.replacement().unwrap(), expected.as_bytes());

        let applied = match commit_provider_rewrite(plan).unwrap() {
            ProviderRewriteCommit::Applied(applied) => applied,
            other => panic!("expected applied rewrite, got {other:?}"),
        };
        assert_eq!(fs::read(&path).unwrap(), expected.as_bytes());
        let restored = rollback_provider_rewrite(applied).unwrap();
        assert_eq!(restored, content_fingerprint(original.as_bytes()));
        assert_eq!(fs::read(path).unwrap(), original.as_bytes());
    }

    #[test]
    fn inserts_missing_provider_idempotently_and_supports_both_rollback_paths() {
        let home = TestHome::new();
        let id = "019f0000-0000-7000-8000-000000000011";
        let path = home.active(id);
        let original = format!(
            "  {{ \"type\" : \"session_meta\", \"payload\" : {{ \"id\" : \"{id}\", \"source\" : \"cli\"   }} }}\r\n"
        );
        fs::write(&path, &original).unwrap();

        let primary = read_primary_rollout(&path, false).unwrap();
        assert!(primary.provider_field_missing);
        assert!(primary.model_provider.is_none());

        let plan = plan_provider_rewrite(&path, false, "custom", ProviderRewriteOptions::default())
            .unwrap();
        assert_eq!(plan.status, ProviderRewriteStatus::Changed);
        assert_eq!(plan.provider_field, Some(ProviderField::SnakeCase));
        assert!(plan.previous_provider.is_none());
        let expected_inserted = original.replacen(
            r#""cli"   } }"#,
            r#""cli","model_provider":"custom"   } }"#,
            1,
        );
        assert_eq!(plan.replacement().unwrap(), expected_inserted.as_bytes());
        let applied = match commit_provider_rewrite(plan).unwrap() {
            ProviderRewriteCommit::Applied(applied) => applied,
            other => panic!("expected applied insertion, got {other:?}"),
        };
        assert!(applied.previous_provider.is_none());
        let inserted = fs::read(&path).unwrap();
        assert_eq!(inserted, expected_inserted.as_bytes());
        let primary = read_primary_rollout(&path, false).unwrap();
        assert!(!primary.provider_field_missing);
        assert_eq!(primary.model_provider.as_deref(), Some("custom"));

        let second =
            plan_provider_rewrite(&path, false, "custom", ProviderRewriteOptions::default())
                .unwrap();
        assert_eq!(second.status, ProviderRewriteStatus::Unchanged);
        assert!(matches!(
            commit_provider_rewrite(second).unwrap(),
            ProviderRewriteCommit::NoChange(ProviderRewriteStatus::Unchanged)
        ));

        rollback_provider_rewrite(applied).unwrap();
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());

        let plan = plan_provider_rewrite(&path, false, "custom", ProviderRewriteOptions::default())
            .unwrap();
        assert!(matches!(
            commit_provider_rewrite(plan).unwrap(),
            ProviderRewriteCommit::Applied(_)
        ));
        let appended = b"{\"type\":\"event_msg\",\"payload\":{\"message\":\"after repair\"}}\r\n";
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(appended)
            .unwrap();
        assert!(remove_provider_if_matches(&path, false, "custom").unwrap());
        let mut expected = original.into_bytes();
        expected.extend_from_slice(appended);
        assert_eq!(fs::read(&path).unwrap(), expected);
        assert!(!remove_provider_if_matches(&path, false, "custom").unwrap());
    }

    #[test]
    fn does_not_repair_a_present_non_string_provider_field() {
        let home = TestHome::new();
        let id = "019f0000-0000-7000-8000-000000000110";
        let path = home.active(id);
        let original = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"model_provider\":{{\"invalid\":true}}}}}}\n"
        );
        fs::write(&path, &original).unwrap();

        let primary = read_primary_rollout(&path, false).unwrap();
        assert!(!primary.provider_field_missing);
        assert!(primary.model_provider.is_none());
        let plan = plan_provider_rewrite(&path, false, "custom", ProviderRewriteOptions::default())
            .unwrap();
        assert_eq!(plan.status, ProviderRewriteStatus::MissingProvider);
        assert!(plan.preimage().is_none());
        assert!(matches!(
            commit_provider_rewrite(plan).unwrap(),
            ProviderRewriteCommit::NoChange(ProviderRewriteStatus::MissingProvider)
        ));
        assert_eq!(
            remove_provider_if_matches(&path, false, "custom")
                .unwrap_err()
                .code,
            "invalid_provider"
        );
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn removes_matching_snake_case_provider_from_any_object_position() {
        let home = TestHome::new();
        let cases = [
            (
                "019f0000-0000-7000-8000-000000000111",
                r#""model_provider":"custom", "id":"{id}","source":"cli""#,
            ),
            (
                "019f0000-0000-7000-8000-000000000112",
                r#""id":"{id}", "model_provider" : "custom" , "source":"cli""#,
            ),
            (
                "019f0000-0000-7000-8000-000000000113",
                r#""id":"{id}", "source":"cli", "model_provider":"custom"  "#,
            ),
        ];
        for (id, payload) in cases {
            let path = home.active(id);
            let payload = payload.replace("{id}", id);
            fs::write(
                &path,
                format!("{{\"type\":\"session_meta\",\"payload\":{{{payload}}}}}\r\n"),
            )
            .unwrap();

            assert!(remove_provider_if_matches(&path, false, "custom").unwrap());
            let bytes = fs::read(&path).unwrap();
            assert!(bytes.ends_with(b"\r\n"));
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            let payload = value["payload"].as_object().unwrap();
            assert_eq!(payload.get("id").and_then(Value::as_str), Some(id));
            assert!(!payload.contains_key("model_provider"));
            assert_eq!(payload.get("source").and_then(Value::as_str), Some("cli"));
        }
    }

    #[test]
    fn provider_removal_rejects_value_alias_and_duplicate_conflicts() {
        let home = TestHome::new();
        let id = "019f0000-0000-7000-8000-000000000114";
        let path = home.active(id);
        let different = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"model_provider\":\"other\"}}}}\n"
        );
        fs::write(&path, &different).unwrap();
        assert_eq!(
            remove_provider_if_matches(&path, false, "custom")
                .unwrap_err()
                .code,
            "provider_remove_conflict"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), different);

        let camel = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"modelProvider\":\"custom\"}}}}\n"
        );
        fs::write(&path, &camel).unwrap();
        assert_eq!(
            remove_provider_if_matches(&path, false, "custom")
                .unwrap_err()
                .code,
            "provider_remove_conflict"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), camel);

        let duplicate = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"model_provider\":\"custom\",\"modelProvider\":\"custom\"}}}}\n"
        );
        fs::write(&path, &duplicate).unwrap();
        assert_eq!(
            remove_provider_if_matches(&path, false, "custom")
                .unwrap_err()
                .code,
            "provider_location_error"
        );
        assert_eq!(fs::read_to_string(path).unwrap(), duplicate);
    }

    #[test]
    fn rewrites_camel_case_provider_field() {
        let home = TestHome::new();
        let id = "019f0000-0000-7000-8000-000000000012";
        let path = home.active(id);
        let original = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"modelProvider\":\"OpenAI\"}}}}\n"
        );
        fs::write(&path, &original).unwrap();

        let plan = plan_provider_rewrite(&path, false, "custom", ProviderRewriteOptions::default())
            .unwrap();
        assert_eq!(plan.provider_field, Some(ProviderField::CamelCase));
        let applied = match commit_provider_rewrite(plan).unwrap() {
            ProviderRewriteCommit::Applied(applied) => applied,
            other => panic!("expected applied rewrite, got {other:?}"),
        };
        assert_eq!(applied.provider_field, ProviderField::CamelCase);
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            original.replacen("\"OpenAI\"", "\"custom\"", 1)
        );
    }

    #[test]
    fn refuses_commit_after_fingerprint_conflict() {
        let home = TestHome::new();
        let id = "019f0000-0000-7000-8000-000000000013";
        let path = home.active(id);
        let original = format!("{}\n", metadata(id, Value::String("cli".into())));
        fs::write(&path, &original).unwrap();
        let plan = plan_provider_rewrite(&path, false, "custom", ProviderRewriteOptions::default())
            .unwrap();
        let external = format!("{original}{{\"type\":\"event_msg\",\"payload\":{{}}}}\n");
        fs::write(&path, &external).unwrap();

        let error = commit_provider_rewrite(plan).unwrap_err();
        assert_eq!(error.code, "fingerprint_conflict");
        assert_eq!(fs::read_to_string(path).unwrap(), external);
    }

    #[test]
    fn rewrite_is_idempotent_after_first_commit() {
        let home = TestHome::new();
        let id = "019f0000-0000-7000-8000-000000000014";
        let path = home.active(id);
        fs::write(
            &path,
            format!("{}\n", metadata(id, Value::String("cli".into()))),
        )
        .unwrap();
        let first =
            plan_provider_rewrite(&path, false, "custom", ProviderRewriteOptions::default())
                .unwrap();
        assert!(matches!(
            commit_provider_rewrite(first).unwrap(),
            ProviderRewriteCommit::Applied(_)
        ));

        let second =
            plan_provider_rewrite(&path, false, "custom", ProviderRewriteOptions::default())
                .unwrap();
        assert_eq!(second.status, ProviderRewriteStatus::Unchanged);
        assert!(second.preimage().is_none());
        assert!(matches!(
            commit_provider_rewrite(second).unwrap(),
            ProviderRewriteCommit::NoChange(ProviderRewriteStatus::Unchanged)
        ));
    }

    #[test]
    fn archived_rollout_requires_explicit_opt_in() {
        let home = TestHome::new();
        let id = "019f0000-0000-7000-8000-000000000015";
        let path = home.archived(id);
        let original = format!("{}\n", metadata(id, Value::String("cli".into())));
        fs::write(&path, &original).unwrap();

        let excluded =
            plan_provider_rewrite(&path, true, "custom", ProviderRewriteOptions::default())
                .unwrap();
        assert_eq!(excluded.status, ProviderRewriteStatus::ArchivedExcluded);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);

        let included = plan_provider_rewrite(
            &path,
            true,
            "custom",
            ProviderRewriteOptions {
                include_archived: true,
            },
        )
        .unwrap();
        assert_eq!(included.status, ProviderRewriteStatus::Changed);
        assert!(matches!(
            commit_provider_rewrite(included).unwrap(),
            ProviderRewriteCommit::Applied(_)
        ));
        assert!(fs::read_to_string(path)
            .unwrap()
            .contains("\"model_provider\":\"custom\""));
    }

    #[test]
    fn extracts_session_metadata_turn_context_and_event_user_message() {
        let home = TestHome::new();
        let id = "019f0000-0000-7000-8000-000000000016";
        let path = home.active(id);
        let records = [
            serde_json::json!({
                "timestamp": "2026-07-13T00:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-07-13T00:00:01Z",
                    "cli_version": "1.2.3",
                    "model_provider": "OpenAI",
                    "git": {
                        "commit_hash": "abc123",
                        "branch": "main",
                        "repository_url": "https://example.test/repo"
                    }
                }
            }),
            serde_json::json!({
                "type": "turn_context",
                "payload": {
                    "sandbox_policy": {"type": "workspace-write"},
                    "approval_policy": "on-request"
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "developer",
                    "content": [{"type": "input_text", "text": "do not use this"}]
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "user_message",
                    "message": "Restore my missing sessions\nPlease keep the messages."
                }
            }),
        ];
        let content = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(&path, content).unwrap();

        let primary = scan_rollouts(&home.0).rollouts.remove(id).unwrap();
        assert_eq!(
            primary.session_timestamp.as_deref(),
            Some("2026-07-13T00:00:01Z")
        );
        assert_eq!(primary.cli_version.as_deref(), Some("1.2.3"));
        let git = primary.git.unwrap();
        assert_eq!(git.commit_hash.as_deref(), Some("abc123"));
        assert_eq!(git.branch.as_deref(), Some("main"));
        assert_eq!(
            git.repository_url.as_deref(),
            Some("https://example.test/repo")
        );
        assert_eq!(primary.approval_mode.as_deref(), Some("on-request"));
        assert_eq!(
            primary.sandbox_policy,
            Some(serde_json::json!({"type": "workspace-write"}))
        );
        assert_eq!(
            primary.first_user_message.as_deref(),
            Some("Restore my missing sessions\nPlease keep the messages.")
        );
        assert_eq!(
            primary.title.as_deref(),
            Some("Restore my missing sessions")
        );
        assert_eq!(
            primary.preview.as_deref(),
            Some("Restore my missing sessions Please keep the messages.")
        );
        assert!(primary.updated_at_fallback.is_some());
    }

    #[test]
    fn extracts_response_item_user_message_but_skips_developer_content() {
        let home = TestHome::new();
        let id = "019f0000-0000-7000-8000-000000000017";
        let path = home.active(id);
        let records = [
            serde_json::json!({
                "type": "session_meta",
                "payload": {"id": id, "model_provider": "OpenAI"}
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "developer",
                    "content": [{"type": "input_text", "text": "developer text"}]
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "first part"},
                        {"type": "input_text", "text": "second part"}
                    ]
                }
            }),
        ];
        fs::write(
            &path,
            records
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();

        let primary = scan_rollouts(&home.0).rollouts.remove(id).unwrap();
        assert_eq!(
            primary.first_user_message.as_deref(),
            Some("first part\nsecond part")
        );
        assert_eq!(primary.title.as_deref(), Some("first part"));
    }

    #[test]
    fn leaves_user_metadata_empty_when_no_real_user_message_exists() {
        let home = TestHome::new();
        let id = "019f0000-0000-7000-8000-000000000018";
        let path = home.active(id);
        let records = [
            serde_json::json!({
                "type": "session_meta",
                "payload": {"id": id, "model_provider": "OpenAI"}
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "assistant"}]
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_started", "message": "not a user message"}
            }),
        ];
        fs::write(
            &path,
            records
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();

        let primary = scan_rollouts(&home.0).rollouts.remove(id).unwrap();
        assert!(primary.first_user_message.is_none());
        assert!(primary.title.is_none());
        assert!(primary.preview.is_none());
    }

    #[test]
    fn bounds_long_user_input_and_stops_on_oversized_secondary_record() {
        let home = TestHome::new();
        let id = "019f0000-0000-7000-8000-000000000019";
        let path = home.active(id);
        let long_message = "x".repeat(MAX_FIRST_USER_MESSAGE_CHARS + 1024);
        let session = serde_json::json!({
            "type": "session_meta",
            "payload": {"id": id, "model_provider": "OpenAI"}
        });
        let user = serde_json::json!({
            "type": "event_msg",
            "payload": {"type": "user_message", "message": long_message}
        });
        fs::write(&path, format!("{}\n{}\n", session, user)).unwrap();
        let primary = scan_rollouts(&home.0).rollouts.remove(id).unwrap();
        assert_eq!(
            primary.first_user_message.as_ref().unwrap().chars().count(),
            MAX_FIRST_USER_MESSAGE_CHARS
        );
        assert!(primary.title.as_ref().unwrap().chars().count() <= MAX_TITLE_CHARS);
        assert!(primary.preview.as_ref().unwrap().chars().count() <= MAX_PREVIEW_CHARS);

        let oversized_id = "019f0000-0000-7000-8000-000000000020";
        let oversized_path = home.active(oversized_id);
        let oversized = format!(
            "{}\n{}\n",
            serde_json::json!({
                "type": "session_meta",
                "payload": {"id": oversized_id, "model_provider": "OpenAI"}
            }),
            "{".to_string() + &"x".repeat(MAX_ENRICHMENT_RECORD_BYTES + 32)
        );
        fs::write(&oversized_path, oversized).unwrap();
        let primary = scan_rollouts(&home.0)
            .rollouts
            .remove(oversized_id)
            .unwrap();
        assert!(primary.first_user_message.is_none());
    }
}
