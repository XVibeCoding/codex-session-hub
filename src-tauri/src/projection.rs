use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceProvider {
    OpenAi,
    Other(String),
}

impl SourceProvider {
    pub fn from_id(id: impl Into<String>) -> Self {
        let id = id.into();
        match id.as_str() {
            "openai" => Self::OpenAi,
            _ => Self::Other(id),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::OpenAi => "openai",
            Self::Other(id) => id,
        }
    }
}

impl Serialize for SourceProvider {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SourceProvider {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let id = String::deserialize(deserializer)?;
        if id.is_empty() || id.len() > 256 || id.trim() != id || id.chars().any(char::is_control) {
            return Err(de::Error::custom("provider id is invalid"));
        }
        Ok(Self::from_id(id))
    }
}

impl fmt::Display for SourceProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProjectionScope {
    All,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum CatalogState {
    Missing,
    MissingCandidate { provider: SourceProvider },
    Present { provider: SourceProvider },
}

impl CatalogState {
    pub fn projected(target: SourceProvider) -> Self {
        Self::Present { provider: target }
    }

    fn target_status(&self, target: &SourceProvider) -> CatalogTargetStatus {
        match self {
            Self::Missing => CatalogTargetStatus::Missing,
            Self::Present { provider } if provider == target => CatalogTargetStatus::Aligned,
            Self::Present { .. } | Self::MissingCandidate { .. } => {
                CatalogTargetStatus::NeedsUpdate
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CatalogTargetStatus {
    Aligned,
    NeedsUpdate,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EligibleSession {
    pub id: String,
    pub origin_provider: SourceProvider,
    pub state_present: bool,
    pub state_provider: SourceProvider,
    pub catalog: CatalogState,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanCategory {
    Aligned,
    CatalogUpdateOnly,
    CatalogInsertOnly,
    StateUpdateOnly,
    StateAndCatalogUpdate,
    StateAndCatalogInsert,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedSession {
    pub thread_id: String,
    pub origin_provider: SourceProvider,
    pub updated_at: i64,
    pub category: PlanCategory,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanMatrix {
    pub aligned: usize,
    pub catalog_update_only: usize,
    pub catalog_insert_only: usize,
    pub state_update_only: usize,
    pub state_and_catalog_update: usize,
    pub state_and_catalog_insert: usize,
}

impl PlanMatrix {
    pub const fn total(&self) -> usize {
        self.aligned
            + self.catalog_update_only
            + self.catalog_insert_only
            + self.state_update_only
            + self.state_and_catalog_update
            + self.state_and_catalog_insert
    }

    pub const fn pending(&self) -> usize {
        self.catalog_update_only
            + self.catalog_insert_only
            + self.state_update_only
            + self.state_and_catalog_update
            + self.state_and_catalog_insert
    }

    pub const fn state_updates(&self) -> usize {
        self.state_update_only + self.state_and_catalog_update + self.state_and_catalog_insert
    }

    pub const fn catalog_updates(&self) -> usize {
        self.catalog_update_only + self.state_and_catalog_update
    }

    pub const fn catalog_inserts(&self) -> usize {
        self.catalog_insert_only + self.state_and_catalog_insert
    }

    fn increment(&mut self, category: PlanCategory) {
        match category {
            PlanCategory::Aligned => self.aligned += 1,
            PlanCategory::CatalogUpdateOnly => self.catalog_update_only += 1,
            PlanCategory::CatalogInsertOnly => self.catalog_insert_only += 1,
            PlanCategory::StateUpdateOnly => self.state_update_only += 1,
            PlanCategory::StateAndCatalogUpdate => self.state_and_catalog_update += 1,
            PlanCategory::StateAndCatalogInsert => self.state_and_catalog_insert += 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanOperations {
    pub changed_threads: usize,
    pub state_updates: usize,
    pub catalog_updates: usize,
    pub catalog_inserts: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderPlanPreview {
    pub target_provider: SourceProvider,
    pub scope: ProjectionScope,
    pub selected_sources: BTreeSet<SourceProvider>,
    pub total_candidates: usize,
    pub considered: usize,
    pub pending: usize,
    pub matrix: PlanMatrix,
    pub operations: PlanOperations,
    pub sessions: Vec<PlannedSession>,
}

impl ProviderPlanPreview {
    pub fn validate_invariants(&self) -> Result<(), ProjectionError> {
        if !self.selected_sources.contains(&self.target_provider) {
            return Err(ProjectionError::InvariantViolation(
                "target provider is absent from selected sources".into(),
            ));
        }
        if self.considered != self.sessions.len() || self.considered != self.matrix.total() {
            return Err(ProjectionError::InvariantViolation(
                "considered sessions do not equal the six matrix categories".into(),
            ));
        }
        let mut actual_matrix = PlanMatrix::default();
        let mut seen = HashSet::new();
        for session in &self.sessions {
            if !seen.insert(session.thread_id.as_str()) {
                return Err(ProjectionError::DuplicateSessionId(
                    session.thread_id.clone(),
                ));
            }
            if !self.selected_sources.contains(&session.origin_provider) {
                return Err(ProjectionError::InvariantViolation(format!(
                    "planned session has an unselected origin: {}",
                    session.thread_id
                )));
            }
            actual_matrix.increment(session.category);
        }
        if actual_matrix != self.matrix {
            return Err(ProjectionError::InvariantViolation(
                "serialized matrix does not match planned session categories".into(),
            ));
        }
        if self.pending != self.matrix.pending()
            || self.pending != self.considered.saturating_sub(self.matrix.aligned)
        {
            return Err(ProjectionError::InvariantViolation(
                "pending sessions do not equal considered minus aligned".into(),
            ));
        }
        if self.operations.changed_threads != self.pending
            || self.operations.state_updates != self.matrix.state_updates()
            || self.operations.catalog_updates != self.matrix.catalog_updates()
            || self.operations.catalog_inserts != self.matrix.catalog_inserts()
        {
            return Err(ProjectionError::InvariantViolation(
                "operation totals do not match the plan matrix".into(),
            ));
        }
        let expected_considered = self.total_candidates;
        if self.considered != expected_considered {
            return Err(ProjectionError::InvariantViolation(format!(
                "scope expected {expected_considered} sessions but considered {}",
                self.considered
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionError {
    DuplicateSessionId(String),
    InvariantViolation(String),
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateSessionId(id) => write!(formatter, "duplicate session id: {id}"),
            Self::InvariantViolation(message) => formatter.write_str(message),
        }
    }
}

impl Error for ProjectionError {}

fn classify(session: &EligibleSession, target: &SourceProvider) -> PlanCategory {
    match (
        session.state_present && &session.state_provider == target,
        session.catalog.target_status(target),
    ) {
        (true, CatalogTargetStatus::Aligned) => PlanCategory::Aligned,
        (true, CatalogTargetStatus::NeedsUpdate) => PlanCategory::CatalogUpdateOnly,
        (true, CatalogTargetStatus::Missing) => PlanCategory::CatalogInsertOnly,
        (false, CatalogTargetStatus::Aligned) => PlanCategory::StateUpdateOnly,
        (false, CatalogTargetStatus::NeedsUpdate) => PlanCategory::StateAndCatalogUpdate,
        (false, CatalogTargetStatus::Missing) => PlanCategory::StateAndCatalogInsert,
    }
}

pub fn build_provider_plan_preview(
    sessions: &[EligibleSession],
    selected_sources: &BTreeSet<SourceProvider>,
    target_provider: SourceProvider,
    scope: ProjectionScope,
) -> Result<ProviderPlanPreview, ProjectionError> {
    let mut seen = HashSet::new();
    for session in sessions {
        if !seen.insert(session.id.as_str()) {
            return Err(ProjectionError::DuplicateSessionId(session.id.clone()));
        }
    }

    let mut effective_sources = selected_sources.clone();
    effective_sources.insert(target_provider.clone());
    let mut selected = sessions
        .iter()
        .filter(|session| effective_sources.contains(&session.origin_provider))
        .collect::<Vec<_>>();
    selected.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    let total_candidates = selected.len();
    let mut matrix = PlanMatrix::default();
    let planned_sessions = selected
        .into_iter()
        .map(|session| {
            let category = classify(session, &target_provider);
            matrix.increment(category);
            PlannedSession {
                thread_id: session.id.clone(),
                origin_provider: session.origin_provider.clone(),
                updated_at: session.updated_at,
                category,
            }
        })
        .collect::<Vec<_>>();
    let considered = planned_sessions.len();
    let pending = matrix.pending();
    let preview = ProviderPlanPreview {
        target_provider,
        scope,
        selected_sources: effective_sources,
        total_candidates,
        considered,
        pending,
        operations: PlanOperations {
            changed_threads: pending,
            state_updates: matrix.state_updates(),
            catalog_updates: matrix.catalog_updates(),
            catalog_inserts: matrix.catalog_inserts(),
        },
        matrix,
        sessions: planned_sessions,
    };
    preview.validate_invariants()?;
    Ok(preview)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionRecord {
    pub thread_id: String,
    pub origin_provider: SourceProvider,
    #[serde(default = "default_true")]
    pub original_state_present: bool,
    pub original_state_provider: SourceProvider,
    pub original_catalog: CatalogState,
    #[serde(default)]
    pub original_rollout_provider: Option<SourceProvider>,
    pub projected_target: SourceProvider,
    pub version: u64,
    pub timestamp: i64,
}

const fn default_true() -> bool {
    true
}

impl ProjectionRecord {
    pub fn expected_projected_catalog(&self) -> CatalogState {
        CatalogState::projected(self.projected_target.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionStore {
    pub schema_version: u32,
    pub projection_version: u64,
    pub target_provider: SourceProvider,
    pub timestamp: i64,
    pub threads: BTreeMap<String, ProjectionRecord>,
}

impl ProjectionStore {
    pub fn capture(
        preview: &ProviderPlanPreview,
        sessions: &[EligibleSession],
        projection_version: u64,
        timestamp: i64,
    ) -> Result<Self, ProjectionError> {
        preview.validate_invariants()?;
        let by_id = sessions
            .iter()
            .map(|session| (session.id.as_str(), session))
            .collect::<HashMap<_, _>>();
        if by_id.len() != sessions.len() {
            let mut seen = HashSet::new();
            let duplicate = sessions
                .iter()
                .find(|session| !seen.insert(session.id.as_str()))
                .map(|session| session.id.clone())
                .unwrap_or_default();
            return Err(ProjectionError::DuplicateSessionId(duplicate));
        }
        let mut threads = BTreeMap::new();
        for planned in &preview.sessions {
            if planned.category == PlanCategory::Aligned {
                continue;
            }
            let Some(session) = by_id.get(planned.thread_id.as_str()) else {
                return Err(ProjectionError::InvariantViolation(format!(
                    "preview session is absent from capture input: {}",
                    planned.thread_id
                )));
            };
            threads.insert(
                session.id.clone(),
                ProjectionRecord {
                    thread_id: session.id.clone(),
                    origin_provider: session.origin_provider.clone(),
                    original_state_present: session.state_present,
                    original_state_provider: session.state_provider.clone(),
                    original_catalog: session.catalog.clone(),
                    original_rollout_provider: None,
                    projected_target: preview.target_provider.clone(),
                    version: projection_version,
                    timestamp,
                },
            );
        }
        Ok(Self {
            schema_version: 1,
            projection_version,
            target_provider: preview.target_provider.clone(),
            timestamp,
            threads,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentProjectionState {
    pub thread_id: String,
    pub state_provider: SourceProvider,
    pub catalog: CatalogState,
    pub projection_version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileAction {
    pub thread_id: String,
    pub expected_version: u64,
    pub next_version: u64,
    pub expected_state_provider: SourceProvider,
    pub expected_catalog: CatalogState,
    pub restore_state_provider: SourceProvider,
    pub restore_catalog: CatalogState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "camelCase")]
pub enum ReconcileConflictReason {
    VersionMismatch {
        expected: u64,
        actual: u64,
    },
    CurrentValueChanged {
        actual_state_provider: SourceProvider,
        actual_catalog: CatalogState,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileConflict {
    pub thread_id: String,
    pub detail: ReconcileConflictReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcilePlan {
    pub projection_version: u64,
    pub timestamp: i64,
    pub restores: Vec<ReconcileAction>,
    pub already_restored: Vec<String>,
    pub conflicts: Vec<ReconcileConflict>,
    pub missing: Vec<String>,
}

pub fn plan_reconcile(
    store: &ProjectionStore,
    current: &[CurrentProjectionState],
    timestamp: i64,
) -> Result<ReconcilePlan, ProjectionError> {
    let mut current_by_id = HashMap::new();
    for state in current {
        if current_by_id
            .insert(state.thread_id.as_str(), state)
            .is_some()
        {
            return Err(ProjectionError::DuplicateSessionId(state.thread_id.clone()));
        }
    }

    let mut plan = ReconcilePlan {
        projection_version: store.projection_version,
        timestamp,
        restores: Vec::new(),
        already_restored: Vec::new(),
        conflicts: Vec::new(),
        missing: Vec::new(),
    };
    for record in store.threads.values() {
        let Some(current) = current_by_id.get(record.thread_id.as_str()) else {
            plan.missing.push(record.thread_id.clone());
            continue;
        };
        if current.state_provider == record.original_state_provider
            && current.catalog == record.original_catalog
        {
            plan.already_restored.push(record.thread_id.clone());
            continue;
        }
        if current.projection_version != record.version {
            plan.conflicts.push(ReconcileConflict {
                thread_id: record.thread_id.clone(),
                detail: ReconcileConflictReason::VersionMismatch {
                    expected: record.version,
                    actual: current.projection_version,
                },
            });
            continue;
        }
        let expected_catalog = record.expected_projected_catalog();
        if current.state_provider != record.projected_target || current.catalog != expected_catalog
        {
            plan.conflicts.push(ReconcileConflict {
                thread_id: record.thread_id.clone(),
                detail: ReconcileConflictReason::CurrentValueChanged {
                    actual_state_provider: current.state_provider.clone(),
                    actual_catalog: current.catalog.clone(),
                },
            });
            continue;
        }
        let Some(next_version) = record.version.checked_add(1) else {
            return Err(ProjectionError::InvariantViolation(format!(
                "projection version overflow for {}",
                record.thread_id
            )));
        };
        plan.restores.push(ReconcileAction {
            thread_id: record.thread_id.clone(),
            expected_version: record.version,
            next_version,
            expected_state_provider: record.projected_target.clone(),
            expected_catalog,
            restore_state_provider: record.original_state_provider.clone(),
            restore_catalog: record.original_catalog.clone(),
        });
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn present(provider: SourceProvider) -> CatalogState {
        CatalogState::Present { provider }
    }

    fn provider(id: &str) -> SourceProvider {
        SourceProvider::Other(id.to_string())
    }

    fn screenshot_sessions() -> Vec<EligibleSession> {
        let mut sessions = Vec::new();
        for index in 0..50 {
            sessions.push(EligibleSession {
                id: format!("openai-{index:03}"),
                origin_provider: SourceProvider::OpenAi,
                state_present: true,
                state_provider: SourceProvider::OpenAi,
                catalog: present(provider("custom")),
                updated_at: 1_000 - index,
            });
        }
        for index in 0..141 {
            sessions.push(EligibleSession {
                id: format!("custom-{index:03}"),
                origin_provider: provider("custom"),
                state_present: true,
                state_provider: provider("custom"),
                catalog: present(provider("custom")),
                updated_at: 900 - index,
            });
        }
        sessions
    }

    #[test]
    fn screenshot_range_reconciles_for_each_target() {
        let sessions = screenshot_sessions();
        let selected = BTreeSet::from([
            SourceProvider::OpenAi,
            provider("custom"),
            provider("codexpilot"),
        ]);
        let openai = build_provider_plan_preview(
            &sessions,
            &selected,
            SourceProvider::OpenAi,
            ProjectionScope::All,
        )
        .unwrap();
        assert_eq!(openai.considered, 191);
        assert_eq!(openai.matrix.aligned, 0);
        assert_eq!(openai.matrix.catalog_update_only, 50);
        assert_eq!(openai.matrix.state_and_catalog_update, 141);
        assert_eq!(openai.pending, 191);
        assert_eq!(openai.operations.state_updates, 141);
        assert_eq!(openai.operations.catalog_updates, 191);

        let custom = build_provider_plan_preview(
            &sessions,
            &selected,
            provider("custom"),
            ProjectionScope::All,
        )
        .unwrap();
        assert_eq!(custom.considered, 191);
        assert_eq!(custom.matrix.aligned, 141);
        assert_eq!(custom.matrix.state_update_only, 50);
        assert_eq!(custom.pending, 50);
        assert_eq!(custom.operations.state_updates, 50);
        assert_eq!(custom.operations.catalog_updates, 0);
    }

    #[test]
    fn all_scope_includes_every_selected_and_target_session() {
        let mut sessions = Vec::new();
        for index in 0..35 {
            sessions.push(EligibleSession {
                id: format!("a-openai-{index:02}"),
                origin_provider: SourceProvider::OpenAi,
                state_present: true,
                state_provider: SourceProvider::OpenAi,
                catalog: present(SourceProvider::OpenAi),
                updated_at: index,
            });
            sessions.push(EligibleSession {
                id: format!("b-custom-{index:02}"),
                origin_provider: provider("custom"),
                state_present: true,
                state_provider: provider("custom"),
                catalog: present(provider("custom")),
                updated_at: index,
            });
        }
        let selected = BTreeSet::from([provider("custom")]);
        let preview = build_provider_plan_preview(
            &sessions,
            &selected,
            SourceProvider::OpenAi,
            ProjectionScope::All,
        )
        .unwrap();

        assert_eq!(preview.total_candidates, 70);
        assert_eq!(preview.considered, 70);
        assert!(preview.selected_sources.contains(&SourceProvider::OpenAi));
        assert_eq!(preview.sessions.first().unwrap().thread_id, "a-openai-34");
        assert_eq!(preview.sessions.last().unwrap().updated_at, 0);
        assert_eq!(
            preview
                .sessions
                .iter()
                .map(|session| session.origin_provider.clone())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([SourceProvider::OpenAi, provider("custom")])
        );
        preview.validate_invariants().unwrap();
    }

    #[test]
    fn missing_state_row_is_planned_even_when_its_provider_matches() {
        let sessions = vec![EligibleSession {
            id: "rollout-only".into(),
            origin_provider: SourceProvider::Other("gateway".into()),
            state_present: false,
            state_provider: SourceProvider::Other("gateway".into()),
            catalog: CatalogState::Missing,
            updated_at: 1,
        }];
        let selected = BTreeSet::from([SourceProvider::Other("gateway".into())]);

        let preview = build_provider_plan_preview(
            &sessions,
            &selected,
            SourceProvider::Other("gateway".into()),
            ProjectionScope::All,
        )
        .unwrap();

        assert_eq!(preview.matrix.state_and_catalog_insert, 1);
        assert_eq!(preview.operations.state_updates, 1);
        assert_eq!(preview.operations.catalog_inserts, 1);
    }

    #[test]
    fn reconcile_restores_only_unchanged_projection_values() {
        let records = [
            (
                "restore",
                SourceProvider::OpenAi,
                present(SourceProvider::OpenAi),
                7,
            ),
            (
                "already",
                provider("custom"),
                present(provider("custom")),
                99,
            ),
            (
                "version",
                SourceProvider::OpenAi,
                present(SourceProvider::OpenAi),
                8,
            ),
            (
                "changed",
                provider("codexpilot"),
                present(provider("codexpilot")),
                7,
            ),
        ];
        let mut threads = BTreeMap::new();
        for (id, _, _, _) in &records {
            threads.insert(
                (*id).into(),
                ProjectionRecord {
                    thread_id: (*id).into(),
                    origin_provider: provider("custom"),
                    original_state_present: true,
                    original_state_provider: provider("custom"),
                    original_catalog: present(provider("custom")),
                    original_rollout_provider: None,
                    projected_target: SourceProvider::OpenAi,
                    version: 7,
                    timestamp: 1_000,
                },
            );
        }
        threads.insert(
            "missing".into(),
            ProjectionRecord {
                thread_id: "missing".into(),
                origin_provider: provider("custom"),
                original_state_present: true,
                original_state_provider: provider("custom"),
                original_catalog: CatalogState::Missing,
                original_rollout_provider: None,
                projected_target: SourceProvider::OpenAi,
                version: 7,
                timestamp: 1_000,
            },
        );
        let store = ProjectionStore {
            schema_version: 1,
            projection_version: 7,
            target_provider: SourceProvider::OpenAi,
            timestamp: 1_000,
            threads,
        };
        let current = records
            .into_iter()
            .map(
                |(id, state_provider, catalog, projection_version)| CurrentProjectionState {
                    thread_id: id.into(),
                    state_provider,
                    catalog,
                    projection_version,
                },
            )
            .collect::<Vec<_>>();

        let plan = plan_reconcile(&store, &current, 2_000).unwrap();
        assert_eq!(
            plan.restores
                .iter()
                .map(|action| action.thread_id.as_str())
                .collect::<Vec<_>>(),
            vec!["restore"]
        );
        assert_eq!(plan.already_restored, vec!["already"]);
        assert_eq!(plan.missing, vec!["missing"]);
        assert_eq!(plan.conflicts.len(), 2);
        assert!(matches!(
            plan.conflicts[0].detail,
            ReconcileConflictReason::CurrentValueChanged { .. }
        ));
        assert!(matches!(
            plan.conflicts[1].detail,
            ReconcileConflictReason::VersionMismatch { .. }
        ));
    }

    #[test]
    fn dynamic_provider_round_trips_as_a_plain_provider_id() {
        let provider = SourceProvider::from_id("MyGateway");
        assert_eq!(serde_json::to_string(&provider).unwrap(), "\"MyGateway\"");
        assert_eq!(
            serde_json::from_str::<SourceProvider>("\"MyGateway\"").unwrap(),
            provider
        );
        assert_eq!(
            serde_json::from_str::<SourceProvider>("\"open-ai\"").unwrap(),
            SourceProvider::Other("open-ai".into())
        );
        assert_eq!(
            serde_json::from_str::<SourceProvider>("\"Custom\"").unwrap(),
            SourceProvider::Other("Custom".into())
        );
        assert_eq!(
            serde_json::from_str::<SourceProvider>("\"本地 Provider\"").unwrap(),
            SourceProvider::Other("本地 Provider".into())
        );
        assert!(serde_json::from_str::<SourceProvider>("\" trailing \"").is_err());
    }
}
