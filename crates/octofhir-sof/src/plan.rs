//! The dependency graph of a SQL on FHIR job (`$sql-run`, `$sql-export`).
//!
//! [`Resolver`] walks the `depends-on` entries of every subject as SQL on
//! FHIR, Common Operation Behavior, "Matching supplied artifacts to
//! dependencies" specifies: each canonical is resolved once per job, a
//! `context` entry (url, and version where pinned) first, else an artifact
//! the host resolves through its [`ArtifactSource`]; a SQLView adds its own
//! dependencies; ViewDefinitions, ValueSets and ConceptMaps are leaves. A
//! ValueSet or ConceptMap is turned into its relation's rows right away, so
//! content that cannot be determined fails before any SQL. A `context` entry
//! without a url, two sharing a url, or one no dependency selected is
//! refused.
//!
//! The result, a [`ResolvedJob`], is plain data (serde): an asynchronous job
//! can store it and run exactly what its kick-off resolved. [`closure`] gives
//! each query its dependencies as CTEs in dependency order.
//!
//! What the host keeps: where artifacts come from (its data, packages, a
//! terminology server), how a ValueSet's membership is obtained, how tables
//! are materialized and which SQL is allowed to run.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ViewDefinition;
use crate::expansion::{Resolved, split_canonical};
use crate::library::{SqlLibrary, SqlLibraryKind};
use crate::relations::{concept_map_record, concept_map_rows};

/// SQLViews depending on SQLViews: deeper than this is refused.
pub const MAX_DEPENDENCY_DEPTH: usize = 16;

/// Why a job cannot run, by the status the operations answer with.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// Invalid input (400): a `context` entry without a url, two sharing one,
    /// one matching nothing.
    #[error("{0}")]
    Invalid(String),
    /// A canonical nothing resolves, or resolves to more than one version
    /// (404).
    #[error("{0}")]
    NotFound(String),
    /// An artifact that does not conform, a cycle, a label naming two
    /// artifacts, or terminology content that cannot be determined (422).
    #[error("{0}")]
    Unprocessable(String),
    /// The host failed; passed through as it is.
    #[error(transparent)]
    Host(Box<dyn std::error::Error + Send + Sync>),
}

/// A label of a SQL subject or SQLView, bound to an artifact of the job.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Edge {
    pub label: String,
    /// Index into [`ResolvedJob::artifacts`].
    pub artifact: usize,
}

/// Which relation a terminology artifact is.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TerminologyKind {
    ValueSet,
    ConceptMap,
}

/// A resolved dependency.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Artifact {
    /// A ViewDefinition: a table over the data.
    View { resource: Value },
    /// A SQLView: its SQL (of the job's dialect) over its own dependencies.
    SqlView {
        display: String,
        sql: String,
        dependencies: Vec<Edge>,
    },
    /// A ValueSet or ConceptMap: the relation's rows, fixed for the job.
    Terminology {
        relation: TerminologyKind,
        rows: Value,
    },
}

/// Every artifact a job's subjects depend on, each resolved once.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResolvedJob {
    pub artifacts: Vec<Artifact>,
    /// What produced each terminology artifact's content.
    pub provenance: Vec<Value>,
}

/// The content of a terminology dependency.
#[derive(Clone, Debug)]
pub struct TerminologyContent {
    pub rows: Vec<Value>,
    /// The provenance record (see [`crate::relations::value_set_record`]).
    pub provenance: Value,
}

/// Where a job's dependencies come from, besides the request's `context`.
#[async_trait]
pub trait ArtifactSource: Send + Sync {
    /// The artifact `canonical` (`url` or `url|version`) names among the
    /// host's own: [`PlanError::NotFound`] when there is none, or more than
    /// one version and none pinned.
    async fn artifact(&self, canonical: &str) -> Result<Resolved, PlanError>;

    /// The rows and provenance of the ValueSet dependency `label`, resolved
    /// to `value_set` (see [`crate::expansion::Expander`] and
    /// [`crate::relations::value_set_rows`]); `context` holds the request's
    /// supplied artifacts, which an expansion may also use.
    async fn value_set(
        &self,
        label: &str,
        value_set: &Resolved,
        context: &ContextArtifacts,
    ) -> Result<TerminologyContent, PlanError>;
}

/// A job's `context` entries: validated, matched by url (and version), and
/// tracked so an entry nothing selected can be refused.
#[derive(Debug, Default)]
pub struct ContextArtifacts {
    entries: Vec<Value>,
    used: Mutex<HashSet<usize>>,
}

impl ContextArtifacts {
    /// The `context` parameters (0..*) of a `Parameters` resource. An entry
    /// without a url, or two sharing one, is [`PlanError::Invalid`].
    pub fn from_parameters(params: &Value) -> Result<Self, PlanError> {
        let mut entries: Vec<Value> = Vec::new();
        for param in params["parameter"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|param| param["name"] == "context")
        {
            let resource = param
                .get("resource")
                .filter(|resource| resource["url"].is_string())
                .ok_or_else(|| {
                    PlanError::Invalid("context requires a canonical resource with a url".into())
                })?;
            if entries.iter().any(|known| known["url"] == resource["url"]) {
                return Err(PlanError::Invalid(format!(
                    "context: two entries share the url {}",
                    resource["url"].as_str().unwrap_or_default()
                )));
            }
            entries.push(resource.clone());
        }
        Ok(Self {
            entries,
            used: Mutex::new(HashSet::new()),
        })
    }

    /// Whether the request supplied any entry.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry for `url` (and `version`), of `resource_type` if given;
    /// finding it selects it.
    pub fn find(
        &self,
        resource_type: Option<&str>,
        url: &str,
        version: Option<&str>,
    ) -> Option<Value> {
        let index = self.entries.iter().position(|entry| {
            resource_type.is_none_or(|resource_type| entry["resourceType"] == resource_type)
                && matches_canonical(entry, url, version)
        })?;
        self.used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(index);
        Some(self.entries[index].clone())
    }

    /// [`PlanError::Invalid`] for the first entry nothing selected.
    pub fn check_all_used(&self) -> Result<(), PlanError> {
        let used = self
            .used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match (0..self.entries.len()).find(|index| !used.contains(index)) {
            Some(index) => Err(PlanError::Invalid(format!(
                "context: {} matches no dependency of the request's subjects",
                self.entries[index]["url"].as_str().unwrap_or_default()
            ))),
            None => Ok(()),
        }
    }
}

/// Whether `artifact` has `url` and, where given, `version`.
pub fn matches_canonical(artifact: &Value, url: &str, version: Option<&str>) -> bool {
    artifact["url"] == url && version.is_none_or(|version| artifact["version"] == version)
}

/// Resolves the dependencies of a job's subjects.
pub struct Resolver<'a> {
    source: &'a dyn ArtifactSource,
    context: &'a ContextArtifacts,
    /// The SQL dialect SQLViews are read in.
    dialect: String,
    job: ResolvedJob,
    /// Canonical as written → artifact: one resolution per canonical.
    by_canonical: HashMap<String, usize>,
    /// Terminology identity (type|url|version) → artifact, so a canonical
    /// written with and without its version shares one content.
    by_identity: HashMap<String, usize>,
}

type BoxFuture<'s, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 's>>;

impl<'a> Resolver<'a> {
    pub fn new(
        source: &'a dyn ArtifactSource,
        context: &'a ContextArtifacts,
        dialect: &str,
    ) -> Self {
        Self {
            source,
            context,
            dialect: dialect.to_owned(),
            job: ResolvedJob::default(),
            by_canonical: HashMap::new(),
            by_identity: HashMap::new(),
        }
    }

    /// Resolve the dependencies of a SQL subject, checking that every label
    /// of its closure names one artifact.
    pub async fn subject(&mut self, library: &SqlLibrary) -> Result<Vec<Edge>, PlanError> {
        let edges = self.dependencies(library, &mut Vec::new()).await?;
        closure(&self.job, &edges)?;
        Ok(edges)
    }

    /// Add a ViewDefinition subject as an artifact of the job.
    pub fn view_subject(&mut self, resource: Value) -> usize {
        self.job.artifacts.push(Artifact::View { resource });
        self.job.artifacts.len() - 1
    }

    /// The resolved job; a `context` entry nothing selected is refused.
    pub fn finish(self) -> Result<ResolvedJob, PlanError> {
        self.context.check_all_used()?;
        Ok(self.job)
    }

    fn dependencies<'s>(
        &'s mut self,
        library: &'s SqlLibrary,
        path: &'s mut Vec<String>,
    ) -> BoxFuture<'s, Result<Vec<Edge>, PlanError>> {
        Box::pin(async move {
            if path.len() > MAX_DEPENDENCY_DEPTH {
                return Err(PlanError::Unprocessable(format!(
                    "{}: SQLViews nest deeper than {MAX_DEPENDENCY_DEPTH} levels",
                    library.display
                )));
            }
            let mut edges = Vec::with_capacity(library.dependencies.len());
            for dependency in &library.dependencies {
                if path.contains(&dependency.canonical) {
                    return Err(PlanError::Unprocessable(format!(
                        "SQLView {} depends on itself",
                        dependency.canonical
                    )));
                }
                let artifact = match self.by_canonical.get(&dependency.canonical) {
                    Some(&artifact) => artifact,
                    None => {
                        let artifact = self
                            .resolve(&dependency.label, &dependency.canonical, path)
                            .await?;
                        self.by_canonical
                            .insert(dependency.canonical.clone(), artifact);
                        artifact
                    }
                };
                edges.push(Edge {
                    label: dependency.label.clone(),
                    artifact,
                });
            }
            Ok(edges)
        })
    }

    /// Resolve one canonical to a new artifact (or a shared terminology one).
    async fn resolve(
        &mut self,
        label: &str,
        canonical: &str,
        path: &mut Vec<String>,
    ) -> Result<usize, PlanError> {
        let (url, version) = split_canonical(canonical);
        let resolved = match self.context.find(None, url, version) {
            Some(resource) => Resolved {
                resource,
                origin: "context".into(),
            },
            None => self.source.artifact(canonical).await?,
        };
        let resource = &resolved.resource;
        let artifact = match resource["resourceType"].as_str() {
            Some("ViewDefinition") => {
                ViewDefinition::from_json(resource).map_err(|error| {
                    PlanError::Unprocessable(format!(
                        "Dependency {canonical} is not a valid ViewDefinition: {error}"
                    ))
                })?;
                Artifact::View {
                    resource: resource.clone(),
                }
            }
            Some("Library") => {
                let view = SqlLibrary::from_json(resource, &self.dialect).map_err(|error| {
                    PlanError::Unprocessable(format!("Dependency {canonical}: {error}"))
                })?;
                if view.kind != SqlLibraryKind::View {
                    return Err(PlanError::Unprocessable(format!(
                        "Dependency {canonical} is a SQLQuery; a dependency is a ViewDefinition, SQLView, ValueSet or ConceptMap"
                    )));
                }
                path.push(canonical.to_owned());
                let dependencies = self.dependencies(&view, path).await?;
                path.pop();
                Artifact::SqlView {
                    display: view.display,
                    sql: view.sql,
                    dependencies,
                }
            }
            Some(kind @ ("ValueSet" | "ConceptMap")) => {
                let identity = identity(resource);
                if let Some(&shared) = self.by_identity.get(&identity) {
                    return Ok(shared);
                }
                let (relation, content) = if kind == "ValueSet" {
                    (
                        TerminologyKind::ValueSet,
                        self.source
                            .value_set(label, &resolved, self.context)
                            .await?,
                    )
                } else {
                    let rows = concept_map_rows(resource).map_err(|reason| {
                        PlanError::Unprocessable(format!(
                            "ConceptMap {canonical} ({label}): {reason}"
                        ))
                    })?;
                    let provenance =
                        concept_map_record(label, resource, &resolved.origin, rows.len());
                    (
                        TerminologyKind::ConceptMap,
                        TerminologyContent { rows, provenance },
                    )
                };
                self.job.provenance.push(content.provenance);
                self.job.artifacts.push(Artifact::Terminology {
                    relation,
                    rows: Value::Array(content.rows),
                });
                let index = self.job.artifacts.len() - 1;
                self.by_identity.insert(identity, index);
                return Ok(index);
            }
            _ => {
                return Err(PlanError::Unprocessable(format!(
                    "Dependency {canonical} is not a ViewDefinition, SQLView, ValueSet or ConceptMap"
                )));
            }
        };
        self.job.artifacts.push(artifact);
        Ok(self.job.artifacts.len() - 1)
    }
}

/// A query's dependencies with everything they depend on, as CTEs in
/// dependency order: tables (views and relations) first, then SQLViews after
/// what they read. A label naming two artifacts in one closure is refused.
pub fn closure(job: &ResolvedJob, edges: &[Edge]) -> Result<Vec<(String, usize)>, PlanError> {
    fn visit(
        job: &ResolvedJob,
        edges: &[Edge],
        bound: &mut HashMap<String, usize>,
        tables: &mut Vec<(String, usize)>,
        views: &mut Vec<(String, usize)>,
    ) -> Result<(), PlanError> {
        for edge in edges {
            match bound.get(&edge.label) {
                Some(&artifact) if artifact == edge.artifact => continue,
                Some(_) => {
                    return Err(PlanError::Unprocessable(format!(
                        "Table {} names two different artifacts in one query; use distinct labels",
                        edge.label
                    )));
                }
                None => {}
            }
            bound.insert(edge.label.clone(), edge.artifact);
            match job.artifacts.get(edge.artifact) {
                Some(Artifact::SqlView { dependencies, .. }) => {
                    visit(job, dependencies, bound, tables, views)?;
                    views.push((edge.label.clone(), edge.artifact));
                }
                Some(_) => tables.push((edge.label.clone(), edge.artifact)),
                None => {
                    return Err(PlanError::Unprocessable(format!(
                        "Table {} names no artifact of the job",
                        edge.label
                    )));
                }
            }
        }
        Ok(())
    }
    let mut bound = HashMap::new();
    let mut tables = Vec::new();
    let mut views = Vec::new();
    visit(job, edges, &mut bound, &mut tables, &mut views)?;
    tables.extend(views);
    Ok(tables)
}

/// The identity of a terminology artifact: type, url and version.
fn identity(artifact: &Value) -> String {
    format!(
        "{}|{}|{}",
        artifact["resourceType"].as_str().unwrap_or_default(),
        artifact["url"].as_str().unwrap_or_default(),
        artifact["version"].as_str().unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde_json::json;

    fn job() -> ResolvedJob {
        ResolvedJob {
            artifacts: vec![
                Artifact::View {
                    resource: json!({}),
                },
                Artifact::Terminology {
                    relation: TerminologyKind::ValueSet,
                    rows: json!([]),
                },
                Artifact::SqlView {
                    display: "v".into(),
                    sql: "SELECT 1".into(),
                    dependencies: vec![edge("p", 0)],
                },
            ],
            provenance: Vec::new(),
        }
    }

    fn edge(label: &str, artifact: usize) -> Edge {
        Edge {
            label: label.into(),
            artifact,
        }
    }

    #[test]
    fn a_closure_lists_tables_then_views_after_what_they_read() {
        let order = closure(&job(), &[edge("v", 2), edge("codes", 1)]).unwrap();
        assert_eq!(
            order,
            [
                ("p".to_owned(), 0),
                ("codes".to_owned(), 1),
                ("v".to_owned(), 2)
            ]
        );
        let order = closure(&job(), &[edge("p", 0), edge("v", 2)]).unwrap();
        assert_eq!(order.len(), 2);
        assert!(closure(&job(), &[edge("p", 1), edge("v", 2)]).is_err());
    }

    #[test]
    fn a_resolved_job_round_trips_as_json() {
        let text = serde_json::to_string(&job()).unwrap();
        let back: ResolvedJob = serde_json::from_str(&text).unwrap();
        assert_eq!(back.artifacts.len(), 3);
        assert!(matches!(
            back.artifacts[1],
            Artifact::Terminology {
                relation: TerminologyKind::ValueSet,
                ..
            }
        ));
    }

    #[test]
    fn context_entries_need_a_unique_url_and_a_dependency() {
        let params = |entries: Value| json!({"parameter": entries});
        assert!(
            ContextArtifacts::from_parameters(&params(json!([
                {"name": "context", "resource": {"resourceType": "ValueSet"}}
            ])))
            .is_err()
        );
        assert!(
            ContextArtifacts::from_parameters(&params(json!([
                {"name": "context", "resource": {"resourceType": "ValueSet", "url": "u", "version": "1"}},
                {"name": "context", "resource": {"resourceType": "ValueSet", "url": "u", "version": "2"}}
            ])))
            .is_err()
        );
        let inline = ContextArtifacts::from_parameters(&params(json!([
            {"name": "context", "resource": {"resourceType": "ValueSet", "url": "a", "version": "1"}},
            {"name": "context", "resource": {"resourceType": "ConceptMap", "url": "b"}}
        ])))
        .unwrap();
        assert!(inline.find(None, "a", Some("2")).is_none());
        assert!(inline.find(Some("ValueSet"), "a", Some("1")).is_some());
        let error = inline.check_all_used().unwrap_err();
        assert!(error.to_string().contains('b'), "{error}");
        assert!(inline.find(None, "b", None).is_some());
        inline.check_all_used().unwrap();
    }

    /// A host that knows one SQLView and one ConceptMap.
    struct Host;

    #[async_trait]
    impl ArtifactSource for Host {
        async fn artifact(&self, canonical: &str) -> Result<Resolved, PlanError> {
            let resource = match canonical {
                "http://x/Library/v" => json!({
                    "resourceType": "Library", "url": "http://x/Library/v",
                    "type": {"coding": [{"system": crate::library::LIBRARY_TYPES, "code": "sql-view"}]},
                    "relatedArtifact": [
                        {"type": "depends-on", "label": "p", "resource": "http://x/ViewDefinition/p"},
                        {"type": "depends-on", "label": "map", "resource": "http://x/ConceptMap/m|1"}],
                    "content": [
                        {"contentType": "application/sql", "data": STANDARD.encode("SELECT 'portable'")},
                        {"contentType": "application/sql;dialect=duckdb", "data": STANDARD.encode("SELECT 'duck'")}]
                }),
                "http://x/ConceptMap/m|1" | "http://x/ConceptMap/m" => json!({
                    "resourceType": "ConceptMap", "url": "http://x/ConceptMap/m", "version": "1",
                    "group": [{"source": "s", "target": "t", "element": [
                        {"code": "a", "target": [{"code": "b", "relationship": "equivalent"}]}]}]
                }),
                _ => return Err(PlanError::NotFound(format!("{canonical} not found"))),
            };
            Ok(Resolved {
                resource,
                origin: "host".into(),
            })
        }

        async fn value_set(
            &self,
            _label: &str,
            _value_set: &Resolved,
            _context: &ContextArtifacts,
        ) -> Result<TerminologyContent, PlanError> {
            Err(PlanError::Unprocessable("no value sets here".into()))
        }
    }

    fn query(dependencies: Value) -> SqlLibrary {
        SqlLibrary::from_json(
            &json!({
                "resourceType": "Library", "url": "http://x/Library/q",
                "type": {"coding": [{"system": crate::library::LIBRARY_TYPES, "code": "sql-query"}]},
                "relatedArtifact": dependencies,
                "content": [{"contentType": "application/sql", "data": STANDARD.encode("SELECT 1")}]
            }),
            "duckdb",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn dependencies_resolve_once_with_context_first() {
        let context = ContextArtifacts::from_parameters(&json!({"parameter": [
            {"name": "context", "resource": {"resourceType": "ViewDefinition", "url": "http://x/ViewDefinition/p",
             "status": "active", "resource": "Patient", "select": [{"column": [{"name": "id", "path": "id"}]}]}}
        ]}))
        .unwrap();
        let mut resolver = Resolver::new(&Host, &context, "duckdb");
        let edges = resolver
            .subject(&query(json!([
                {"type": "depends-on", "label": "v", "resource": "http://x/Library/v"},
                {"type": "depends-on", "label": "same_map", "resource": "http://x/ConceptMap/m"}
            ])))
            .await
            .unwrap();
        let job = resolver.finish().unwrap();
        // p, the map (shared by both canonicals), v.
        assert_eq!(job.artifacts.len(), 3);
        assert_eq!(edges[1].artifact, 1);
        let Artifact::SqlView { sql, .. } = &job.artifacts[2] else {
            panic!("not a SQLView");
        };
        assert_eq!(sql, "SELECT 'duck'");
        assert_eq!(job.provenance[0]["mappings"], 1);
        let order: Vec<String> = closure(&job, &edges)
            .unwrap()
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        assert_eq!(order, ["p", "map", "same_map", "v"]);
    }

    #[tokio::test]
    async fn unresolved_and_unused_artifacts_are_refused() {
        let empty = ContextArtifacts::default();
        let mut resolver = Resolver::new(&Host, &empty, "postgresql");
        let missing = resolver
            .subject(&query(json!([
                {"type": "depends-on", "label": "x", "resource": "http://x/nowhere"}
            ])))
            .await;
        assert!(matches!(missing, Err(PlanError::NotFound(_))));

        let unused = ContextArtifacts::from_parameters(&json!({"parameter": [
            {"name": "context", "resource": {"resourceType": "ValueSet", "url": "http://x/vs"}}
        ]}))
        .unwrap();
        let resolver = Resolver::new(&Host, &unused, "postgresql");
        assert!(matches!(resolver.finish(), Err(PlanError::Invalid(_))));
    }
}
