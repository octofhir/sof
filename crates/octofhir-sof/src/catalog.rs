//! Artifacts and terminology from files: a [`Catalog`] of canonical
//! resources, and [`CatalogSource`], the job's [`ArtifactSource`] over it.
//!
//! A catalog indexes the canonical resources a SQL on FHIR job can depend on
//! ([`CATALOG_TYPES`]: ViewDefinitions, SQL Libraries, value sets, code
//! systems, concept maps) from JSON and NDJSON files, Bundles, directories
//! and FHIR packages (an extracted package directory, or a `.tgz` with the
//! `packages` feature). A package directory's `.index.json` is used to skip
//! files of other types without parsing them.
//!
//! # Versions
//!
//! A canonical with `|version` selects that version. Without one, the
//! latest is used (by the resources' `versionAlgorithm[x]`: `semver`,
//! `integer`, `date` or `alpha`, otherwise a natural comparison of the
//! version segments), and recorded in the job's provenance. SQL on FHIR
//! leaves the choice to the runner; what it requires (Terminology in SQL,
//! "Errors") is that a canonical that cannot be resolved to a single
//! version is rejected: two different resources with the latest (or the
//! pinned) version are [`Lookup::Ambiguous`]. The same resource loaded twice
//! (a package present in two places) is one entry.
//!
//! # Terminology
//!
//! A ValueSet's membership is its own expansion when it carries one, else
//! computed by the strict [`Expander`] over the job's `context` and the
//! catalog. With a terminology server (the ecosystem's
//! [`TerminologyProvider`], the same one that answers `memberOf()` in views),
//! a value set whose definition is
//! beyond the local expander (a SNOMED CT or LOINC filter, say) is expanded
//! by the terminology server, by canonical, and so is a value set the
//! catalog does not hold at all; see [`value_set_content`] for the rule
//! hosts share.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs;
#[cfg(feature = "packages")]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use octofhir_fhir_model::terminology::{ExpandedValueSet, TerminologyProvider};
use regex::Regex;
use serde_json::Value;

use crate::expansion::{
    Expander, ExpansionError, ExpansionStamp, Lookup, Resolved, TerminologySource, dated_by,
    split_canonical,
};
use crate::plan::{ArtifactSource, ContextArtifacts, PlanError, TerminologyContent};
use crate::relations::{value_set_record, value_set_rows};

/// The resource types a catalog keeps; others are skipped when loading.
pub const CATALOG_TYPES: [&str; 5] = [
    "Library",
    "ViewDefinition",
    "ValueSet",
    "CodeSystem",
    "ConceptMap",
];

/// The types a job dependency may resolve to, in lookup order.
const DEPENDENCY_TYPES: [&str; 4] = ["Library", "ViewDefinition", "ValueSet", "ConceptMap"];

/// Origin prefix of a value set a terminology server expanded.
pub const REMOTE_ORIGIN_PREFIX: &str = "terminology server ";

/// The first `resourceType` of a JSON text.
static FIRST_TYPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""resourceType"\s*:\s*"([A-Za-z]+)""#).expect("valid pattern"));

/// Why a catalog could not load a file.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {message}")]
    Content { path: PathBuf, message: String },
}

impl CatalogError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }

    fn content(path: &Path, message: impl Into<String>) -> Self {
        Self::Content {
            path: path.to_owned(),
            message: message.into(),
        }
    }
}

/// One canonical resource of a catalog.
#[derive(Debug)]
struct Entry {
    resource_type: String,
    version: Option<String>,
    origin: String,
    resource: Arc<Value>,
}

/// Canonical resources indexed by url.
#[derive(Debug, Default)]
pub struct Catalog {
    entries: Vec<Entry>,
    by_url: HashMap<String, Vec<usize>>,
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of resources held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Add `resource`, or every entry of a Bundle; `origin` says where it
    /// came from, for provenance. Returns how many resources were added:
    /// resources without a url, of another type than [`CATALOG_TYPES`], or
    /// already held are not.
    pub fn add(&mut self, resource: Value, origin: &str) -> usize {
        if resource["resourceType"] == "Bundle" {
            let entries = match resource {
                Value::Object(mut bundle) => bundle.remove("entry"),
                _ => None,
            };
            return entries
                .and_then(|entries| match entries {
                    Value::Array(entries) => Some(entries),
                    _ => None,
                })
                .unwrap_or_default()
                .into_iter()
                .filter_map(|mut entry| entry.get_mut("resource").map(Value::take))
                .map(|resource| self.add(resource, origin))
                .sum();
        }
        let (Some(resource_type), Some(url)) =
            (resource["resourceType"].as_str(), resource["url"].as_str())
        else {
            return 0;
        };
        if !CATALOG_TYPES.contains(&resource_type) {
            return 0;
        }
        let (resource_type, url) = (resource_type.to_owned(), url.to_owned());
        let version = resource["version"].as_str().map(str::to_owned);
        let known = self.by_url.entry(url).or_default();
        if known.iter().any(|&index| {
            let entry = &self.entries[index];
            entry.resource_type == resource_type
                && entry.version == version
                && *entry.resource == resource
        }) {
            return 0;
        }
        known.push(self.entries.len());
        self.entries.push(Entry {
            resource_type,
            version,
            origin: origin.to_owned(),
            resource: Arc::new(resource),
        });
        1
    }

    /// Load a JSON or NDJSON file, a directory (recursively) or, with the
    /// `packages` feature, a FHIR package `.tgz`. Returns how many resources
    /// were added. Within a directory, files other than `.json`, `.ndjson`
    /// and `.tgz` are skipped; a file that does not parse is an error.
    pub fn load(&mut self, path: &Path) -> Result<usize, CatalogError> {
        let metadata = fs::metadata(path).map_err(|error| CatalogError::io(path, error))?;
        if metadata.is_dir() {
            let package = package_origin(path);
            return self.load_dir(path, package.as_deref());
        }
        let origin = file_origin(path);
        self.load_file(path, &origin)
    }

    /// A directory's files, recorded as from `package` when inside one, else
    /// each from its own path.
    fn load_dir(&mut self, dir: &Path, package: Option<&str>) -> Result<usize, CatalogError> {
        let index = read_package_index(dir);
        let mut children: Vec<PathBuf> = fs::read_dir(dir)
            .map_err(|error| CatalogError::io(dir, error))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        children.sort();
        let mut added = 0;
        for child in children {
            let name = child
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if child.is_dir() {
                let nested = package_origin(&child);
                added += self.load_dir(&child, nested.as_deref().or(package))?;
                continue;
            }
            if name == "package.json" || name.starts_with('.') {
                continue;
            }
            if let Some(index) = &index
                && name.ends_with(".json")
                && !index.wants(name)
            {
                continue;
            }
            if has_extension(&child, &["json", "ndjson", "tgz"]) {
                let origin = package.map_or_else(|| file_origin(&child), str::to_owned);
                added += self.load_file(&child, &origin)?;
            }
        }
        Ok(added)
    }

    fn load_file(&mut self, path: &Path, origin: &str) -> Result<usize, CatalogError> {
        if has_extension(path, &["tgz"]) || path.to_string_lossy().ends_with(".tar.gz") {
            return self.load_package_archive(path);
        }
        let text = fs::read_to_string(path).map_err(|error| CatalogError::io(path, error))?;
        if has_extension(path, &["ndjson"]) {
            let mut added = 0;
            for (number, line) in text.lines().enumerate() {
                if line.trim().is_empty() || !wanted_text(line) {
                    continue;
                }
                let resource: Value = serde_json::from_str(line).map_err(|error| {
                    CatalogError::content(path, format!("line {}: {error}", number + 1))
                })?;
                added += self.add(resource, origin);
            }
            return Ok(added);
        }
        if !wanted_text(&text) {
            return Ok(0);
        }
        let resource: Value = serde_json::from_str(&text)
            .map_err(|error| CatalogError::content(path, error.to_string()))?;
        Ok(self.add(resource, origin))
    }

    #[cfg(feature = "packages")]
    fn load_package_archive(&mut self, path: &Path) -> Result<usize, CatalogError> {
        let file = fs::File::open(path).map_err(|error| CatalogError::io(path, error))?;
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
        let mut origin = file_origin(path);
        let mut resources = Vec::new();
        let entries = archive
            .entries()
            .map_err(|error| CatalogError::io(path, error))?;
        for entry in entries {
            let mut entry = entry.map_err(|error| CatalogError::io(path, error))?;
            let name = entry
                .path()
                .map_err(|error| CatalogError::io(path, error))?
                .to_string_lossy()
                .into_owned();
            // Top-level package files only: `package/examples/` and the
            // like are not the package's conformance resources.
            let Some(file) = name.strip_prefix("package/") else {
                continue;
            };
            if file.contains('/') || !file.ends_with(".json") || file.starts_with('.') {
                continue;
            }
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .map_err(|error| CatalogError::content(path, format!("{name}: {error}")))?;
            if file == "package.json" {
                if let Some(package) = package_identity(&text) {
                    origin = package;
                }
                continue;
            }
            if wanted_text(&text) {
                let resource: Value = serde_json::from_str(&text)
                    .map_err(|error| CatalogError::content(path, format!("{name}: {error}")))?;
                resources.push(resource);
            }
        }
        Ok(resources
            .into_iter()
            .map(|resource| self.add(resource, &origin))
            .sum())
    }

    #[cfg(not(feature = "packages"))]
    fn load_package_archive(&mut self, path: &Path) -> Result<usize, CatalogError> {
        Err(CatalogError::content(
            path,
            "reading a package archive needs the `packages` feature; extract it and load the directory",
        ))
    }

    /// The resource of `resource_type` (any of [`CATALOG_TYPES`] if `None`)
    /// with `url`, and `version` where given (see the module documentation
    /// for the version rule).
    pub fn find(&self, resource_type: Option<&str>, url: &str, version: Option<&str>) -> Lookup {
        let mut matching: Vec<&Entry> = self
            .by_url
            .get(url)
            .into_iter()
            .flatten()
            .map(|&index| &self.entries[index])
            .filter(|entry| resource_type.is_none_or(|wanted| entry.resource_type == wanted))
            .filter(|entry| version.is_none_or(|wanted| entry.version.as_deref() == Some(wanted)))
            .collect();
        if matching.is_empty() {
            return Lookup::Missing;
        }
        matching.sort_by(|a, b| compare_versions(&a.resource, &b.resource));
        let latest = matching[matching.len() - 1];
        let ties: Vec<&&Entry> = matching
            .iter()
            .filter(|entry| entry.version == latest.version)
            .collect();
        if ties.len() > 1 {
            let origins: Vec<&str> = ties.iter().map(|entry| entry.origin.as_str()).collect();
            let version = latest.version.as_deref().unwrap_or("(none)");
            return Lookup::Ambiguous(format!(
                "{url} has {} different resources with version {version} ({})",
                ties.len(),
                origins.join(", ")
            ));
        }
        Lookup::Found(Resolved {
            resource: (*latest.resource).clone(),
            origin: latest.origin.clone(),
        })
    }

    /// The latest version of the `resource_type` with `url` dated on or before
    /// `date`, for a `compose.lockedDate`.
    pub fn find_as_of(&self, resource_type: &str, url: &str, date: &str) -> Lookup {
        let dated: Option<&Entry> = self
            .by_url
            .get(url)
            .into_iter()
            .flatten()
            .map(|&index| &self.entries[index])
            .filter(|entry| entry.resource_type == resource_type)
            .filter(|entry| dated_by(&entry.resource, date))
            .max_by(|a, b| compare_versions(&a.resource, &b.resource));
        dated.map_or(Lookup::Missing, |entry| {
            Lookup::Found(Resolved {
                resource: (*entry.resource).clone(),
                origin: entry.origin.clone(),
            })
        })
    }

    /// The resource a canonical (`url` or `url|version`) names among
    /// `resource_types`, the first type that has it.
    pub fn find_canonical(&self, resource_types: &[&str], canonical: &str) -> Lookup {
        let (url, version) = split_canonical(canonical);
        for resource_type in resource_types {
            match self.find(Some(resource_type), url, version) {
                Lookup::Missing => {}
                found => return found,
            }
        }
        Lookup::Missing
    }
}

/// Orders two canonical resources by version: by their shared
/// `versionAlgorithm[x]` where they declare one (`semver`, `integer`,
/// `date`, `alpha`), else by a natural comparison of the version's
/// `.`, `-` and `+` separated segments (numbers as numbers). A resource
/// without a version sorts first.
pub fn compare_versions(a: &Value, b: &Value) -> Ordering {
    let algorithm = |resource: &Value| {
        resource["versionAlgorithmString"]
            .as_str()
            .or_else(|| resource["versionAlgorithmCoding"]["code"].as_str())
            .map(str::to_owned)
    };
    let (va, vb) = (a["version"].as_str(), b["version"].as_str());
    let (va, vb) = match (va, vb) {
        (Some(va), Some(vb)) => (va, vb),
        (left, right) => return left.is_some().cmp(&right.is_some()),
    };
    let natural = || natural_order(va, vb);
    match (algorithm(a), algorithm(b)) {
        (Some(x), Some(y)) if x == y => match x.as_str() {
            "semver" => match (
                semver::Version::parse(va.trim_start_matches('v')),
                semver::Version::parse(vb.trim_start_matches('v')),
            ) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                _ => natural(),
            },
            "integer" => match (va.parse::<i64>(), vb.parse::<i64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                _ => natural(),
            },
            "date" | "alpha" => va.cmp(vb),
            _ => natural(),
        },
        _ => natural(),
    }
}

fn natural_order(a: &str, b: &str) -> Ordering {
    let segments = |version: &str| -> Vec<(Option<u64>, String)> {
        version
            .split(['.', '-', '+'])
            .map(|part| (part.parse::<u64>().ok(), part.to_owned()))
            .collect()
    };
    let (a, b) = (segments(a), segments(b));
    for (x, y) in a.iter().zip(&b) {
        let order = match (x.0, y.0) {
            (Some(x), Some(y)) => x.cmp(&y),
            // A numeric segment is a release; a textual one a pre-release.
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => x.1.cmp(&y.1),
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    // `1.0.1` follows `1.0`, `1.0-ballot` precedes it.
    let released = |longer: &[(Option<u64>, String)], at: usize| longer[at].0.is_some();
    match a.len().cmp(&b.len()) {
        Ordering::Greater if released(&a, b.len()) => Ordering::Greater,
        Ordering::Greater => Ordering::Less,
        Ordering::Less if released(&b, a.len()) => Ordering::Less,
        Ordering::Less => Ordering::Greater,
        Ordering::Equal => Ordering::Equal,
    }
}

/// Whether a JSON text may hold a resource of [`CATALOG_TYPES`] (or a
/// Bundle): judged by its first `resourceType`, which serializers write
/// first, so a large StructureDefinition is skipped unparsed.
fn wanted_text(text: &str) -> bool {
    match FIRST_TYPE.captures(text) {
        Some(found) => {
            let resource_type = &found[1];
            resource_type == "Bundle" || CATALOG_TYPES.contains(&resource_type)
        }
        None => false,
    }
}

/// A package directory's `.index.json`: which files hold which types.
struct PackageIndex {
    wanted: HashSet<String>,
}

impl PackageIndex {
    fn wants(&self, file: &str) -> bool {
        self.wanted.contains(file)
    }
}

fn read_package_index(dir: &Path) -> Option<PackageIndex> {
    let text = fs::read_to_string(dir.join(".index.json")).ok()?;
    let index: Value = serde_json::from_str(&text).ok()?;
    let wanted = index["files"]
        .as_array()?
        .iter()
        .filter(|file| {
            file["resourceType"]
                .as_str()
                .is_some_and(|resource_type| CATALOG_TYPES.contains(&resource_type))
        })
        .filter_map(|file| file["filename"].as_str().map(str::to_owned))
        .collect();
    Some(PackageIndex { wanted })
}

/// `name#version` of the package whose `package.json` is in `dir`.
fn package_origin(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join("package.json")).ok()?;
    package_identity(&text).map(|identity| format!("package {identity}"))
}

fn package_identity(text: &str) -> Option<String> {
    let manifest: Value = serde_json::from_str(text).ok()?;
    Some(format!(
        "{}#{}",
        manifest["name"].as_str()?,
        manifest["version"].as_str()?
    ))
}

fn file_origin(path: &Path) -> String {
    format!("file {}", path.display())
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extensions.contains(&extension))
}

/// Members asked of a terminology server: as many as a relation may hold.
const REMOTE_COUNT: u32 = crate::relations::MAX_RELATION_ROWS as u32;

/// How a host lets a terminology server stand in for the local expander.
#[derive(Clone, Copy)]
pub struct RemoteFallback<'a> {
    /// The server, as the ecosystem's [`TerminologyProvider`] (with
    /// `tx-remote`, `HttpTerminologyProvider`, cached or not): the strict
    /// `expand_valueset_resource` / `expand_valueset_definition` are used,
    /// which keep the whole expansion and tell an unknown value set from a
    /// failure.
    pub provider: &'a dyn TerminologyProvider,
    /// Also send a value set's definition when the server does not know its
    /// canonical (the definition leaves the host: opt in).
    pub send_definitions: bool,
}

/// How a provider is named in provenance and messages: its endpoint.
pub fn provider_name(provider: &dyn TerminologyProvider) -> &str {
    provider.endpoint().unwrap_or("(terminology provider)")
}

/// The origin recorded for a value set `provider` expanded.
pub fn remote_origin(provider: &dyn TerminologyProvider) -> String {
    format!("{REMOTE_ORIGIN_PREFIX}{}", provider_name(provider))
}

/// The rows and provenance of the ValueSet dependency `label`: its own
/// expansion when it carries one, else the strict [`Expander`]'s over
/// `source`, else, where the definition is beyond the expander and `remote`
/// is given, the terminology server's expansion by canonical (or, with
/// [`RemoteFallback::send_definitions`], of the definition when the server
/// does not know the canonical). `local` names who expands locally, for
/// provenance (`this server`, `octofhir-sof`).
///
/// A host passes `remote` only where the value set may be named to the
/// terminology server (a server sends published canonicals, never its own
/// or the caller's).
pub async fn value_set_content(
    label: &str,
    value_set: &Resolved,
    source: &dyn TerminologySource,
    local: &str,
    remote: Option<RemoteFallback<'_>>,
) -> Result<TerminologyContent, PlanError> {
    let resource = &value_set.resource;
    let named = match (resource["url"].as_str(), resource["version"].as_str()) {
        (Some(url), Some(version)) => format!("{url}|{version}"),
        (Some(url), None) => url.to_owned(),
        _ => resource["id"].as_str().unwrap_or("(inline)").to_owned(),
    };
    let expanded_by = if value_set.origin.starts_with(REMOTE_ORIGIN_PREFIX) {
        value_set.origin.clone()
    } else if resource.get("expansion").is_some() {
        "the artifact's expansion".to_owned()
    } else {
        local.to_owned()
    };
    let (expansion, expanded_by) = match Expander::new(source)
        .expand(resource, &ExpansionStamp::now())
        .await
    {
        Ok(expansion) => (expansion, expanded_by),
        Err(ExpansionError::Undeterminable(reason)) => {
            // A server's own answer is not asked for again.
            let Some(fallback) =
                remote.filter(|_| !value_set.origin.starts_with(REMOTE_ORIGIN_PREFIX))
            else {
                return Err(PlanError::Unprocessable(format!(
                    "ValueSet {named} ({label}): membership cannot be determined: {reason}"
                )));
            };
            let provider = fallback.provider;
            let server = provider_name(provider);
            let unexpanded = |why: String| {
                PlanError::Unprocessable(format!(
                    "ValueSet {named} ({label}): membership cannot be determined: {reason}; {why}"
                ))
            };
            let by_canonical = match resource["url"].as_str() {
                Some(url) => {
                    provider
                        .expand_valueset_resource(
                            url,
                            resource["version"].as_str(),
                            Some(REMOTE_COUNT),
                        )
                        .await
                }
                None => Ok(ExpandedValueSet::Unknown),
            };
            let answer = match by_canonical {
                Ok(ExpandedValueSet::Unknown) if fallback.send_definitions => {
                    provider
                        .expand_valueset_definition(resource, Some(REMOTE_COUNT))
                        .await
                }
                other => other,
            };
            match answer {
                Ok(ExpandedValueSet::Found(remote)) => {
                    (remote["expansion"].clone(), remote_origin(provider))
                }
                Ok(ExpandedValueSet::Unknown) if fallback.send_definitions => {
                    return Err(unexpanded(format!(
                        "the terminology server {server} could not expand its definition"
                    )));
                }
                Ok(ExpandedValueSet::Unknown) => {
                    return Err(unexpanded(format!(
                        "the terminology server {server} does not know it (sending definitions is off)"
                    )));
                }
                Err(error) => {
                    return Err(unexpanded(format!(
                        "the terminology server {server} could not expand it either: {error}"
                    )));
                }
            }
        }
        Err(ExpansionError::Source(error)) => return Err(PlanError::Host(error)),
    };
    let rows = value_set_rows(&expansion).map_err(|reason| {
        PlanError::Unprocessable(format!("ValueSet {named} ({label}): {reason}"))
    })?;
    let provenance = value_set_record(
        label,
        resource,
        &value_set.origin,
        &expanded_by,
        &expansion,
        rows.len(),
    );
    Ok(TerminologyContent { rows, provenance })
}

/// Value sets and code systems of a job: its `context` first, then the
/// catalog.
pub struct CatalogTerminology<'a> {
    pub catalog: &'a Catalog,
    pub context: &'a ContextArtifacts,
}

#[async_trait]
impl TerminologySource for CatalogTerminology<'_> {
    async fn find(
        &self,
        resource_type: &str,
        url: &str,
        version: Option<&str>,
    ) -> Result<Lookup, ExpansionError> {
        if let Some(resource) = self.context.find(Some(resource_type), url, version) {
            return Ok(Lookup::Found(Resolved {
                resource,
                origin: "context".into(),
            }));
        }
        Ok(self.catalog.find(Some(resource_type), url, version))
    }

    async fn find_as_of(
        &self,
        resource_type: &str,
        url: &str,
        date: &str,
    ) -> Result<Lookup, ExpansionError> {
        if let Some(resource) = self.context.find(Some(resource_type), url, None)
            && dated_by(&resource, date)
        {
            return Ok(Lookup::Found(Resolved {
                resource,
                origin: "context".into(),
            }));
        }
        Ok(self.catalog.find_as_of(resource_type, url, date))
    }
}

#[async_trait]
impl TerminologySource for Catalog {
    async fn find(
        &self,
        resource_type: &str,
        url: &str,
        version: Option<&str>,
    ) -> Result<Lookup, ExpansionError> {
        Ok(Catalog::find(self, Some(resource_type), url, version))
    }

    async fn find_as_of(
        &self,
        resource_type: &str,
        url: &str,
        date: &str,
    ) -> Result<Lookup, ExpansionError> {
        Ok(Catalog::find_as_of(self, resource_type, url, date))
    }
}

/// The [`ArtifactSource`] of a job over a catalog and, optionally, a
/// terminology server: a canonical the catalog lacks is asked of the server
/// as a value set; a value set beyond the local expander is expanded there.
pub struct CatalogSource<'a> {
    catalog: &'a Catalog,
    remote: Option<RemoteFallback<'a>>,
    local: String,
}

impl<'a> CatalogSource<'a> {
    pub fn new(catalog: &'a Catalog) -> Self {
        Self {
            catalog,
            remote: None,
            local: "octofhir-sof".into(),
        }
    }

    /// Fall back on `provider` for value sets, by canonical.
    pub fn with_remote(mut self, provider: &'a dyn TerminologyProvider) -> Self {
        self.remote = Some(RemoteFallback {
            provider,
            send_definitions: false,
        });
        self
    }

    /// Fall back on `provider` for value sets, sending a definition the
    /// server does not know by canonical.
    pub fn with_remote_definitions(mut self, provider: &'a dyn TerminologyProvider) -> Self {
        self.remote = Some(RemoteFallback {
            provider,
            send_definitions: true,
        });
        self
    }

    /// Who expands locally, as recorded in provenance.
    pub fn with_local_name(mut self, name: impl Into<String>) -> Self {
        self.local = name.into();
        self
    }
}

#[async_trait]
impl ArtifactSource for CatalogSource<'_> {
    async fn artifact(&self, canonical: &str) -> Result<Resolved, PlanError> {
        let not_found =
            |reason: String| PlanError::NotFound(format!("Dependency {canonical}: {reason}"));
        match self.catalog.find_canonical(&DEPENDENCY_TYPES, canonical) {
            Lookup::Found(resolved) => return Ok(resolved),
            Lookup::Ambiguous(reason) => return Err(not_found(reason)),
            Lookup::Missing => {}
        }
        let Some(RemoteFallback { provider, .. }) = self.remote else {
            return Err(not_found("not found".into()));
        };
        let (url, version) = split_canonical(canonical);
        let server = provider_name(provider);
        match provider
            .expand_valueset_resource(url, version, Some(REMOTE_COUNT))
            .await
        {
            Ok(ExpandedValueSet::Found(value_set)) => Ok(Resolved {
                resource: value_set,
                origin: remote_origin(provider),
            }),
            Ok(ExpandedValueSet::Unknown) => Err(not_found(format!(
                "not found here nor on the terminology server {server}"
            ))),
            Err(error) => Err(not_found(format!(
                "not found here, and the terminology server {server} could not be asked: {error}"
            ))),
        }
    }

    async fn value_set(
        &self,
        label: &str,
        value_set: &Resolved,
        context: &ContextArtifacts,
    ) -> Result<TerminologyContent, PlanError> {
        let source = CatalogTerminology {
            catalog: self.catalog,
            context,
        };
        value_set_content(label, value_set, &source, &self.local, self.remote).await
    }
}

#[cfg(test)]
mod tests {
    use octofhir_fhir_model::terminology::{
        ConnectionStatus, ExpansionParameters, LookupResult, NoOpTerminologyProvider,
        SubsumptionResult, TranslationResult, ValidationResult as TerminologyValidation,
        ValueSetExpansion,
    };
    use serde_json::json;

    use super::*;

    fn value_set(version: &str, algorithm: Option<&str>) -> Value {
        let mut resource = json!({
            "resourceType": "ValueSet", "url": "http://x/vs", "version": version,
            "expansion": {"contains": [{"system": "s", "code": version}]}
        });
        if let Some(algorithm) = algorithm {
            resource["versionAlgorithmString"] = json!(algorithm);
        }
        resource
    }

    fn found_version(lookup: Lookup) -> String {
        match lookup {
            Lookup::Found(resolved) => resolved.resource["version"].as_str().unwrap().to_owned(),
            other => panic!("not found: {other:?}"),
        }
    }

    #[test]
    fn the_latest_version_is_chosen_unless_one_is_pinned() {
        let mut catalog = Catalog::new();
        for version in ["1.9.0", "1.10.0", "1.10.0-ballot"] {
            catalog.add(value_set(version, None), "test");
        }
        assert_eq!(
            found_version(catalog.find(None, "http://x/vs", None)),
            "1.10.0"
        );
        assert_eq!(
            found_version(catalog.find(Some("ValueSet"), "http://x/vs", Some("1.9.0"))),
            "1.9.0"
        );
        assert!(matches!(
            catalog.find(Some("CodeSystem"), "http://x/vs", None),
            Lookup::Missing
        ));
    }

    #[test]
    fn declared_algorithms_order_versions() {
        let mut catalog = Catalog::new();
        catalog.add(value_set("2026-02-01", Some("date")), "test");
        catalog.add(value_set("2025-12-31", Some("date")), "test");
        assert_eq!(
            found_version(catalog.find(None, "http://x/vs", None)),
            "2026-02-01"
        );
        assert_eq!(
            compare_versions(
                &value_set("10", Some("integer")),
                &value_set("9", Some("integer"))
            ),
            Ordering::Greater
        );
    }

    #[test]
    fn the_same_resource_twice_is_one_entry_and_two_different_ones_are_ambiguous() {
        let mut catalog = Catalog::new();
        assert_eq!(catalog.add(value_set("1", None), "a"), 1);
        assert_eq!(catalog.add(value_set("1", None), "b"), 0);
        let mut other = value_set("1", None);
        other["name"] = json!("Other");
        assert_eq!(catalog.add(other, "c"), 1);
        assert!(matches!(
            catalog.find(None, "http://x/vs", None),
            Lookup::Ambiguous(reason) if reason.contains("a, c")
        ));
    }

    #[test]
    fn bundles_unpack_and_other_types_are_skipped() {
        let mut catalog = Catalog::new();
        let added = catalog.add(
            json!({"resourceType": "Bundle", "entry": [
                {"resource": value_set("1", None)},
                {"resource": {"resourceType": "StructureDefinition", "url": "http://x/sd"}},
                {"resource": {"resourceType": "Patient", "id": "p"}}
            ]}),
            "bundle",
        );
        assert_eq!(added, 1);
        assert!(!wanted_text(
            r#"{"resourceType": "StructureDefinition", "contained": [{"resourceType": "ValueSet"}]}"#
        ));
        assert!(wanted_text(r#"{ "resourceType" : "CodeSystem" }"#));
    }

    #[test]
    fn directories_load_with_their_package_identity_and_index() {
        let dir = std::env::temp_dir().join(format!("sof-catalog-{}", std::process::id()));
        let package = dir.join("package");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("package.json"),
            r#"{"name": "acme.terms", "version": "1.0.0"}"#,
        )
        .unwrap();
        fs::write(
            package.join(".index.json"),
            r#"{"files": [{"filename": "ValueSet-vs.json", "resourceType": "ValueSet"},
                          {"filename": "big.json", "resourceType": "StructureDefinition"}]}"#,
        )
        .unwrap();
        fs::write(
            package.join("ValueSet-vs.json"),
            value_set("1", None).to_string(),
        )
        .unwrap();
        // Not parsed: the index says it is a StructureDefinition.
        fs::write(package.join("big.json"), "not json").unwrap();
        fs::write(
            dir.join("maps.ndjson"),
            format!(
                "{}\n{}\n",
                json!({"resourceType": "ConceptMap", "url": "http://x/cm"}),
                json!({"resourceType": "Patient", "id": "p"})
            ),
        )
        .unwrap();
        let mut catalog = Catalog::new();
        assert_eq!(catalog.load(&dir).unwrap(), 2);
        match catalog.find(None, "http://x/vs", None) {
            Lookup::Found(resolved) => assert_eq!(resolved.origin, "package acme.terms#1.0.0"),
            other => panic!("{other:?}"),
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A terminology server knowing two value sets by canonical and
    /// expanding any definition sent to it.
    #[derive(Debug)]
    struct Tx;

    #[async_trait]
    impl TerminologyProvider for Tx {
        fn endpoint(&self) -> Option<&str> {
            Some("https://tx.example")
        }

        async fn expand_valueset_resource(
            &self,
            url: &str,
            _version: Option<&str>,
            _count: Option<u32>,
        ) -> octofhir_fhir_model::Result<ExpandedValueSet> {
            Ok(match url {
                "http://snomed.info/sct?fhir_vs=isa/1" | "http://x/filtered" => {
                    ExpandedValueSet::Found(json!({
                        "resourceType": "ValueSet", "url": url,
                        "expansion": {"total": 1, "contains": [{"system": "http://snomed.info/sct", "code": "2"}]}
                    }))
                }
                _ => ExpandedValueSet::Unknown,
            })
        }

        async fn expand_valueset_definition(
            &self,
            value_set: &Value,
            _count: Option<u32>,
        ) -> octofhir_fhir_model::Result<ExpandedValueSet> {
            let mut expanded = value_set.clone();
            expanded["expansion"] = json!({"total": 1, "contains": [{"system": "http://snomed.info/sct", "code": "3"}]});
            Ok(ExpandedValueSet::Found(expanded))
        }

        async fn validate_code(
            &self,
            code: &str,
            system: &str,
            version: Option<&str>,
        ) -> octofhir_fhir_model::Result<bool> {
            NoOpTerminologyProvider
                .validate_code(code, system, version)
                .await
        }

        async fn expand_valueset(
            &self,
            url: &str,
            parameters: Option<&ExpansionParameters>,
        ) -> octofhir_fhir_model::Result<ValueSetExpansion> {
            NoOpTerminologyProvider
                .expand_valueset(url, parameters)
                .await
        }

        async fn translate_code(
            &self,
            code: &str,
            target: &str,
            map: Option<&str>,
        ) -> octofhir_fhir_model::Result<TranslationResult> {
            NoOpTerminologyProvider
                .translate_code(code, target, map)
                .await
        }

        async fn lookup_code(
            &self,
            system: &str,
            code: &str,
            version: Option<&str>,
            properties: Option<Vec<&str>>,
        ) -> octofhir_fhir_model::Result<LookupResult> {
            NoOpTerminologyProvider
                .lookup_code(system, code, version, properties)
                .await
        }

        async fn validate_code_vs(
            &self,
            value_set: &str,
            system: Option<&str>,
            code: &str,
            display: Option<&str>,
        ) -> octofhir_fhir_model::Result<TerminologyValidation> {
            NoOpTerminologyProvider
                .validate_code_vs(value_set, system, code, display)
                .await
        }

        async fn subsumes(
            &self,
            system: &str,
            parent: &str,
            child: &str,
        ) -> octofhir_fhir_model::Result<SubsumptionResult> {
            NoOpTerminologyProvider
                .subsumes(system, parent, child)
                .await
        }

        async fn test_connection(&self) -> octofhir_fhir_model::Result<ConnectionStatus> {
            NoOpTerminologyProvider.test_connection().await
        }
    }

    #[tokio::test]
    async fn the_terminology_server_covers_what_the_catalog_cannot() {
        let mut catalog = Catalog::new();
        catalog.add(
            json!({"resourceType": "ValueSet", "url": "http://x/filtered", "compose": {"include": [
                {"system": "http://snomed.info/sct", "filter": [{"property": "concept", "op": "is-a", "value": "1"}]}
            ]}}),
            "file vs.json",
        );
        let context = ContextArtifacts::default();
        let local = CatalogSource::new(&catalog);
        let filtered = local.artifact("http://x/filtered").await.unwrap();
        assert!(matches!(
            local.value_set("vs", &filtered, &context).await,
            Err(PlanError::Unprocessable(_))
        ));
        assert!(matches!(
            local.artifact("http://snomed.info/sct?fhir_vs=isa/1").await,
            Err(PlanError::NotFound(_))
        ));

        let remote = CatalogSource::new(&catalog).with_remote(&Tx);
        let content = remote.value_set("vs", &filtered, &context).await.unwrap();
        assert_eq!(content.rows.len(), 1);
        assert_eq!(
            content.provenance["expandedBy"],
            "terminology server https://tx.example"
        );
        let implicit = remote
            .artifact("http://snomed.info/sct?fhir_vs=isa/1")
            .await
            .unwrap();
        assert_eq!(implicit.origin, "terminology server https://tx.example");
        let content = remote.value_set("isa", &implicit, &context).await.unwrap();
        assert_eq!(content.provenance["source"], implicit.origin);
        assert!(matches!(
            remote.artifact("http://x/unknown").await,
            Err(PlanError::NotFound(_))
        ));

        // A value set the server cannot know by canonical: only its
        // definition, sent on request, expands there.
        let private = Resolved {
            resource: json!({"resourceType": "ValueSet", "url": "http://x/private", "compose": {"include": [
                {"system": "http://snomed.info/sct", "filter": [{"property": "concept", "op": "is-a", "value": "1"}]}
            ]}}),
            origin: "file private.json".into(),
        };
        match remote.value_set("p", &private, &context).await {
            Err(PlanError::Unprocessable(message)) => {
                assert!(message.contains("sending definitions is off"), "{message}")
            }
            other => panic!("{:?}", other.map(|content| content.rows)),
        }
        let sending = CatalogSource::new(&catalog).with_remote_definitions(&Tx);
        let content = sending.value_set("p", &private, &context).await.unwrap();
        assert_eq!(content.rows[0]["code"], "3");
    }
}
