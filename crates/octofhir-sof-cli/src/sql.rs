//! `octofhir-sof sql …`: SQLQuery / SQLView Libraries from the command line.
//!
//! - `build` turns annotated SQL into a Library;
//! - `check` resolves a Library's dependencies (one resolution per job, as
//!   the operations do) and, with the `duckdb` feature, checks its SQL
//!   against their tables without data;
//! - `run` runs a SQLQuery over FHIR resources in an embedded DuckDB;
//! - `export` runs several SQLQueries over the same resources into a
//!   directory, with a manifest.
//!
//! Dependencies come from `--context` resources first, then `--artifacts`
//! (files, directories, FHIR packages), then, for value sets, the `--tx`
//! terminology server.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use octofhir_sof::TerminologyProvider;
use octofhir_sof::catalog::{Catalog, CatalogSource};
use octofhir_sof::library::{
    AnnotatedOptions, ParameterValue, SqlLibrary, SqlLibraryKind, bind_parameters, build_annotated,
};
use octofhir_sof::plan::{Artifact, ContextArtifacts, Edge, ResolvedJob, Resolver};
use serde_json::{Value, json};

/// The SQL dialect Libraries are read in: the one the embedded engine runs.
const DIALECT: &str = "duckdb";

#[derive(Subcommand)]
pub enum SqlCommand {
    /// Build a SQLQuery (or, with --view, a SQLView) Library from SQL
    /// carrying annotations in its comments (@name, @title, @version,
    /// @param, @relatedDependency, ...).
    Build {
        /// The annotated SQL file.
        sql: PathBuf,

        /// Build a SQLView instead of a SQLQuery.
        #[arg(long)]
        view: bool,

        /// Library.url of the built Library.
        #[arg(long)]
        url: Option<String>,

        /// Dialect of the SQL (postgresql, duckdb, ...); portable if omitted.
        #[arg(long)]
        dialect: Option<String>,

        /// Write the Library here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Resolve a Library's dependencies and check its SQL against their
    /// tables, with no data. Exits non-zero when anything does not resolve
    /// or the SQL does not compile.
    Check {
        /// A Library (JSON) or annotated SQL file.
        library: PathBuf,

        #[command(flatten)]
        artifacts: ArtifactArgs,

        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Run a SQLQuery over FHIR resources in an embedded DuckDB and write
    /// the rows.
    Run {
        /// A SQLQuery Library (JSON) or annotated SQL file.
        library: PathBuf,

        #[command(flatten)]
        artifacts: ArtifactArgs,

        #[command(flatten)]
        data: DataArgs,

        /// Write to this file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,

        /// Write the terminology provenance (which value sets and concept
        /// maps, which versions, expanded by whom) to this JSON file.
        #[arg(long)]
        provenance: Option<PathBuf>,
    },

    /// Run several SQLQueries over the same resources (one job: every
    /// dependency resolved once) into a directory: one file per query and a
    /// manifest.json.
    Export {
        /// SQLQuery Libraries (JSON) or annotated SQL files.
        #[arg(required = true)]
        libraries: Vec<PathBuf>,

        #[command(flatten)]
        artifacts: ArtifactArgs,

        #[command(flatten)]
        data: DataArgs,

        /// The output directory.
        #[arg(long)]
        out: PathBuf,
    },
}

/// Where a job's dependencies come from.
#[derive(Args)]
pub struct ArtifactArgs {
    /// ViewDefinitions, SQLViews, ValueSets, CodeSystems and ConceptMaps:
    /// a JSON or NDJSON file, a Bundle, a directory, or a FHIR package
    /// (extracted, or .tgz). Repeatable.
    #[arg(long = "artifacts", short = 'a', value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// A resource supplied with the request (`context`), preferred over
    /// --artifacts; a context entry no dependency uses is an error.
    /// Repeatable.
    #[arg(long = "context", value_name = "FILE")]
    context: Vec<PathBuf>,

    /// FHIR terminology server base URL: expands value sets the local
    /// expander cannot (SNOMED CT, LOINC filters) or the artifacts lack, and
    /// resolves memberOf() in views (needs the tx-remote feature).
    #[arg(long, env = "SOF_TX")]
    tx: Option<String>,

    /// Send the definition of a value set the terminology server does not
    /// know by canonical (it leaves this machine), instead of failing.
    #[arg(long, requires = "tx")]
    tx_definitions: bool,
}

/// The data and parameters of a run.
#[derive(Args)]
pub struct DataArgs {
    /// FHIR resources the ViewDefinitions read: an NDJSON file, a Bundle, a
    /// JSON resource or array, a directory of such files, or `-` for stdin.
    #[arg(long)]
    input: PathBuf,

    /// A parameter value, `name=value`, converted to the declared type.
    /// Repeatable.
    #[arg(long = "param", short = 'p', value_name = "NAME=VALUE")]
    params: Vec<String>,

    /// A FHIR Parameters resource with parameter values.
    #[arg(long)]
    parameters: Option<PathBuf>,

    /// Output format: csv, ndjson, json (parquet with the parquet feature).
    #[arg(long, default_value = "csv")]
    output: String,

    /// Parquet temporal encoding (text or native); see `run --help`.
    #[arg(long, default_value = "text", value_parser = ["text", "native"])]
    parquet_temporal: String,

    /// Threads evaluating the ViewDefinitions (default: the number of CPUs).
    #[arg(long)]
    threads: Option<usize>,
}

pub async fn run(command: SqlCommand) -> Result<()> {
    match command {
        SqlCommand::Build {
            sql,
            view,
            url,
            dialect,
            out,
        } => build(&sql, view, url, dialect, out.as_deref()),
        SqlCommand::Check {
            library,
            artifacts,
            json,
        } => check(&library, &artifacts, json).await,
        SqlCommand::Run {
            library,
            artifacts,
            data,
            out,
            provenance,
        } => {
            run_query(
                &library,
                &artifacts,
                &data,
                out.as_deref(),
                provenance.as_deref(),
            )
            .await
        }
        SqlCommand::Export {
            libraries,
            artifacts,
            data,
            out,
        } => export(&libraries, &artifacts, &data, &out).await,
    }
}

fn build(
    sql: &Path,
    view: bool,
    url: Option<String>,
    dialect: Option<String>,
    out: Option<&Path>,
) -> Result<()> {
    let text = fs::read_to_string(sql).with_context(|| format!("reading {}", sql.display()))?;
    let built = build_annotated(
        &text,
        &AnnotatedOptions {
            view,
            name: file_name(sql),
            url,
            dialect,
        },
    )
    .with_context(|| format!("building a Library from {}", sql.display()))?;
    for warning in &built.warnings {
        eprintln!("warning: {}: {warning}", sql.display());
    }
    let mut text = serde_json::to_string_pretty(&built.library)?;
    text.push('\n');
    match out {
        Some(path) => fs::write(path, text).with_context(|| format!("writing {}", path.display())),
        None => io::stdout()
            .write_all(text.as_bytes())
            .context("writing the Library"),
    }
}

/// A Library read from JSON, or built from annotated SQL.
struct Subject {
    path: PathBuf,
    resource: Value,
    library: SqlLibrary,
}

fn read_subject(path: &Path) -> Result<Subject> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let resource = if path.extension().is_some_and(|extension| extension == "sql") {
        let built = build_annotated(
            &text,
            &AnnotatedOptions {
                name: file_name(path),
                ..AnnotatedOptions::default()
            },
        )
        .with_context(|| format!("reading annotated SQL {}", path.display()))?;
        for warning in &built.warnings {
            eprintln!("warning: {}: {warning}", path.display());
        }
        built.library
    } else {
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
    };
    let library = SqlLibrary::from_json(&resource, DIALECT)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(Subject {
        path: path.to_owned(),
        resource,
        library,
    })
}

fn file_name(path: &Path) -> Option<String> {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
}

/// The catalog, context and terminology server of a job.
struct Sources {
    catalog: Catalog,
    context: ContextArtifacts,
    remote: Option<Arc<dyn TerminologyProvider>>,
    send_definitions: bool,
    options: octofhir_sof::CompileOptions,
}

fn sources(args: &ArtifactArgs) -> Result<Sources> {
    let mut catalog = Catalog::new();
    for path in &args.paths {
        let added = catalog
            .load(path)
            .with_context(|| format!("loading artifacts from {}", path.display()))?;
        if added == 0 {
            eprintln!(
                "warning: {} holds no ViewDefinition, Library, ValueSet, CodeSystem or ConceptMap",
                path.display()
            );
        }
    }
    let mut supplied = Vec::new();
    for path in &args.context {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let resource: Value =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        match resource["entry"].as_array() {
            Some(entries) if resource["resourceType"] == "Bundle" => supplied.extend(
                entries
                    .iter()
                    .filter_map(|entry| entry.get("resource").cloned()),
            ),
            _ => supplied.push(resource),
        }
    }
    let context =
        ContextArtifacts::from_resources(supplied).map_err(|error| anyhow::anyhow!("{error}"))?;
    let remote = super::terminology(args.tx.as_deref())?;
    Ok(Sources {
        catalog,
        context,
        options: super::compile_options(remote.as_ref()),
        remote,
        send_definitions: args.tx_definitions,
    })
}

/// Resolve the dependencies of `subjects` as one job.
async fn resolve(sources: &Sources, subjects: &[Subject]) -> Result<(ResolvedJob, Vec<Vec<Edge>>)> {
    let mut source = CatalogSource::new(&sources.catalog);
    source = match &sources.remote {
        Some(remote) if sources.send_definitions => source.with_remote_definitions(remote.as_ref()),
        Some(remote) => source.with_remote(remote.as_ref()),
        None => source,
    };
    let mut resolver = Resolver::new(&source, &sources.context, DIALECT);
    let mut edges = Vec::with_capacity(subjects.len());
    for subject in subjects {
        edges.push(
            resolver
                .subject(&subject.library)
                .await
                .map_err(|error| anyhow::anyhow!("{}: {error}", subject.path.display()))?,
        );
    }
    let job = resolver
        .finish()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok((job, edges))
}

/// One line per dependency of a subject: label, what it is, where from.
fn dependency_report(job: &ResolvedJob, edges: &[Edge]) -> Vec<Value> {
    let mut terminology = job.provenance.iter();
    let mut seen_terminology: HashMap<usize, Value> = HashMap::new();
    for (index, artifact) in job.artifacts.iter().enumerate() {
        if matches!(artifact, Artifact::Terminology { .. })
            && let Some(record) = terminology.next()
        {
            seen_terminology.insert(index, record.clone());
        }
    }
    edges
        .iter()
        .map(|edge| match &job.artifacts[edge.artifact] {
            Artifact::View { resource } => json!({
                "label": edge.label,
                "kind": "ViewDefinition",
                "url": resource["url"],
                "resource": resource["resource"],
            }),
            Artifact::SqlView {
                display,
                dependencies,
                ..
            } => json!({
                "label": edge.label,
                "kind": "SQLView",
                "url": display,
                "dependencies": dependency_report(job, dependencies),
            }),
            Artifact::Terminology { .. } => {
                let mut record = seen_terminology
                    .get(&edge.artifact)
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                record["label"] = json!(edge.label);
                record
            }
        })
        .collect()
}

fn print_dependencies(report: &[Value], indent: usize) {
    for dependency in report {
        let pad = " ".repeat(indent);
        let label = dependency["label"].as_str().unwrap_or_default();
        let kind = dependency["kind"]
            .as_str()
            .or_else(|| dependency["resourceType"].as_str())
            .unwrap_or_default();
        let url = dependency["url"].as_str().unwrap_or_default();
        let version = dependency["version"]
            .as_str()
            .map(|version| format!("|{version}"))
            .unwrap_or_default();
        let detail = if let Some(members) = dependency["members"].as_u64() {
            format!(
                "  {members} members, from {}, expanded by {}",
                dependency["source"].as_str().unwrap_or_default(),
                dependency["expandedBy"].as_str().unwrap_or_default()
            )
        } else if let Some(mappings) = dependency["mappings"].as_u64() {
            format!(
                "  {mappings} mappings, from {}",
                dependency["source"].as_str().unwrap_or_default()
            )
        } else if let Some(resource) = dependency["resource"].as_str() {
            format!("  over {resource}")
        } else {
            String::new()
        };
        println!("{pad}{label:<16} {kind:<15} {url}{version}{detail}");
        if let Some(nested) = dependency["dependencies"].as_array() {
            print_dependencies(nested, indent + 2);
        }
    }
}

async fn check(path: &Path, args: &ArtifactArgs, as_json: bool) -> Result<()> {
    let subject = read_subject(path)?;
    let sources = sources(args)?;
    let (job, edges) = resolve(&sources, std::slice::from_ref(&subject)).await?;
    let dependencies = dependency_report(&job, &edges[0]);
    let columns = check_sql(&subject, &job, &edges[0], &sources)?;
    let postgresql = postgres_guard(&subject);
    let kind = match subject.library.kind {
        SqlLibraryKind::Query => "SQLQuery",
        SqlLibraryKind::View => "SQLView",
    };
    if as_json {
        let report = json!({
            "library": subject.library.display,
            "kind": kind,
            "parameters": subject.library.parameters.iter().map(|parameter| json!({
                "name": parameter.name, "type": parameter.fhir_type, "required": parameter.required
            })).collect::<Vec<_>>(),
            "dependencies": dependencies,
            "columns": columns,
            "postgresql": postgresql,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("{kind} {}", subject.library.display);
    for parameter in &subject.library.parameters {
        println!(
            "  :{} {}{}",
            parameter.name,
            parameter.fhir_type,
            if parameter.required {
                " (required)"
            } else {
                ""
            }
        );
    }
    print_dependencies(&dependencies, 2);
    if let Some(columns) = columns.as_array() {
        let list: Vec<String> = columns
            .iter()
            .map(|column| {
                format!(
                    "{} {}",
                    column["name"].as_str().unwrap_or_default(),
                    column["type"].as_str().unwrap_or_default()
                )
            })
            .collect();
        println!("  → {}", list.join(", "));
    }
    match (
        postgresql["passes"].as_bool(),
        postgresql["reason"].as_str(),
    ) {
        (Some(true), _) => println!("  postgresql: passes the static SQL check"),
        (Some(false), Some(reason)) => println!("  postgresql: a server would refuse it: {reason}"),
        _ => {}
    }
    println!("ok");
    Ok(())
}

/// The result columns of the subject over empty dependency tables, or null
/// without the duckdb feature.
#[cfg(feature = "duckdb")]
fn check_sql(
    subject: &Subject,
    job: &ResolvedJob,
    edges: &[Edge],
    sources: &Sources,
) -> Result<Value> {
    use octofhir_sof::embedded::DuckDbJob;

    let mut duck = DuckDbJob::open_in_memory()?;
    duck.load(job, &[], 1, &sources.options)?;
    duck.sandbox()?;
    let parameters: HashMap<String, ParameterValue> = subject
        .library
        .parameters
        .iter()
        .map(|parameter| (parameter.name.clone(), ParameterValue::Null))
        .collect();
    let columns = duck
        .describe(job, edges, &subject.library, &parameters)
        .with_context(|| format!("checking the SQL of {}", subject.path.display()))?;
    Ok(Value::Array(
        columns
            .iter()
            .map(|column| json!({"name": column.name, "type": column.col_type.fhir_type().unwrap_or("json")}))
            .collect(),
    ))
}

#[cfg(not(feature = "duckdb"))]
fn check_sql(
    _subject: &Subject,
    _job: &ResolvedJob,
    _edges: &[Edge],
    _sources: &Sources,
) -> Result<Value> {
    eprintln!("note: the SQL itself is checked in a build with the duckdb feature");
    Ok(Value::Null)
}

/// Whether the PostgreSQL SQL of the subject passes the static check a
/// server applies before running it on its database (`guard` feature).
#[cfg(feature = "guard")]
fn postgres_guard(subject: &Subject) -> Value {
    let library = match SqlLibrary::from_json(&subject.resource, "postgresql") {
        Ok(library) => library,
        Err(error) => return json!({"passes": false, "reason": error.to_string()}),
    };
    match octofhir_sof::guard::check_library(&library) {
        Ok(_) => json!({"passes": true}),
        Err(reason) => json!({"passes": false, "reason": reason}),
    }
}

#[cfg(not(feature = "guard"))]
fn postgres_guard(_subject: &Subject) -> Value {
    Value::Null
}

/// The `Parameters` a run binds: the --parameters file, then --param values
/// converted to the types `library` declares.
fn parameters_of(library: &SqlLibrary, data: &DataArgs) -> Result<HashMap<String, ParameterValue>> {
    let mut parameters: Vec<Value> = match &data.parameters {
        Some(path) => {
            let text =
                fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            let resource: Value = serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?;
            if resource["resourceType"] != "Parameters" {
                bail!("{} is not a Parameters resource", path.display());
            }
            resource["parameter"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        }
        None => Vec::new(),
    };
    for param in &data.params {
        let (name, raw) = param
            .split_once('=')
            .with_context(|| format!("--param {param}: expected name=value"))?;
        let declared = library
            .parameters
            .iter()
            .find(|declared| declared.name == name)
            .with_context(|| format!("{} declares no parameter {name}", library.display))?;
        let value = match declared.sql_type() {
            Some("integer" | "bigint") => json!(
                raw.parse::<i64>()
                    .with_context(|| format!("--param {name}: {raw} is not an integer"))?
            ),
            Some("boolean") => json!(
                raw.parse::<bool>()
                    .with_context(|| format!("--param {name}: {raw} is not true or false"))?
            ),
            Some("numeric") => serde_json::from_str::<serde_json::Number>(raw)
                .map(Value::Number)
                .with_context(|| format!("--param {name}: {raw} is not a decimal"))?,
            _ => json!(raw),
        };
        parameters.retain(|known| known["name"] != name);
        parameters.push(json!({"name": name, declared.value_field(): value}));
    }
    bind_parameters(library, Some(&json!({ "parameter": parameters })))
        .map_err(|error| anyhow::anyhow!("{error}"))
}

#[cfg(feature = "duckdb")]
async fn run_query(
    path: &Path,
    args: &ArtifactArgs,
    data: &DataArgs,
    out: Option<&Path>,
    provenance: Option<&Path>,
) -> Result<()> {
    let subject = read_subject(path)?;
    require_query(&subject)?;
    let parameters = parameters_of(&subject.library, data)?;
    let sources = sources(args)?;
    let (job, edges) = resolve(&sources, std::slice::from_ref(&subject)).await?;
    let duck = load_job(&job, &sources, data)?;
    let mut sink = rows::FileRows::new(
        &data.output,
        &data.parquet_temporal,
        match out {
            Some(path) => Box::new(io::BufWriter::new(
                fs::File::create(path).with_context(|| format!("creating {}", path.display()))?,
            )),
            None => Box::new(io::BufWriter::new(io::stdout().lock())),
        },
    )?;
    let count = duck
        .run(&job, &edges[0], &subject.library, &parameters, &mut sink)
        .with_context(|| format!("running {}", subject.path.display()))?;
    sink.finish()?;
    if let Some(path) = provenance {
        fs::write(path, serde_json::to_string_pretty(&job.provenance)?)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    eprintln!("{}", row_count(count));
    Ok(())
}

#[cfg(feature = "duckdb")]
async fn export(paths: &[PathBuf], args: &ArtifactArgs, data: &DataArgs, out: &Path) -> Result<()> {
    let subjects: Vec<Subject> = paths
        .iter()
        .map(|path| read_subject(path))
        .collect::<Result<_>>()?;
    let mut bound = Vec::with_capacity(subjects.len());
    for subject in &subjects {
        require_query(subject)?;
        bound.push(parameters_of(&subject.library, data)?);
    }
    let sources = sources(args)?;
    let (job, edges) = resolve(&sources, &subjects).await?;
    let duck = load_job(&job, &sources, data)?;
    fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let extension = rows::extension(&data.output)?;
    let mut used: Vec<String> = Vec::new();
    let mut outputs = Vec::with_capacity(subjects.len());
    for ((subject, edges), parameters) in subjects.iter().zip(&edges).zip(&bound) {
        let stem = subject.resource["name"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| file_name(&subject.path))
            .unwrap_or_else(|| "query".into());
        let mut name = stem.clone();
        let mut suffix = 1;
        while used.contains(&name) {
            suffix += 1;
            name = format!("{stem}_{suffix}");
        }
        used.push(name.clone());
        let file = out.join(format!("{name}.{extension}"));
        let mut sink = rows::FileRows::new(
            &data.output,
            &data.parquet_temporal,
            Box::new(io::BufWriter::new(
                fs::File::create(&file).with_context(|| format!("creating {}", file.display()))?,
            )),
        )?;
        let count = duck
            .run(&job, edges, &subject.library, parameters, &mut sink)
            .with_context(|| format!("running {}", subject.path.display()))?;
        sink.finish()?;
        eprintln!(
            "{} -> {} ({})",
            subject.library.display,
            file.display(),
            row_count(count)
        );
        outputs.push(json!({
            "name": name,
            "library": subject.library.display,
            "file": format!("{name}.{extension}"),
            "rows": count,
        }));
    }
    let manifest = json!({
        "transactionTime": now(),
        "format": data.output,
        "output": outputs,
        "terminology": job.provenance,
    });
    let path = out.join("manifest.json");
    fs::write(&path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(not(feature = "duckdb"))]
async fn run_query(
    _path: &Path,
    _args: &ArtifactArgs,
    _data: &DataArgs,
    _out: Option<&Path>,
    _provenance: Option<&Path>,
) -> Result<()> {
    bail!("sql run needs a build with the duckdb feature (it is on by default)")
}

#[cfg(not(feature = "duckdb"))]
async fn export(
    _paths: &[PathBuf],
    _args: &ArtifactArgs,
    _data: &DataArgs,
    _out: &Path,
) -> Result<()> {
    bail!("sql export needs a build with the duckdb feature (it is on by default)")
}

#[cfg(feature = "duckdb")]
fn require_query(subject: &Subject) -> Result<()> {
    if subject.library.kind != SqlLibraryKind::Query {
        bail!(
            "{} is a SQLView: run a SQLQuery that reads it",
            subject.path.display()
        );
    }
    Ok(())
}

/// The job's tables in a sandboxed in-memory DuckDB.
#[cfg(feature = "duckdb")]
fn load_job(
    job: &ResolvedJob,
    sources: &Sources,
    data: &DataArgs,
) -> Result<octofhir_sof::embedded::DuckDbJob> {
    let threads = data
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let resources = super::load_resources(&data.input, threads)?;
    let mut duck = octofhir_sof::embedded::DuckDbJob::open_in_memory()?;
    // Evaluation runs on threads of its own; let this worker wait for them.
    tokio::task::block_in_place(|| duck.load(job, &resources, threads, &sources.options))
        .context("evaluating the ViewDefinitions")?;
    duck.sandbox()?;
    Ok(duck)
}

#[cfg(feature = "duckdb")]
fn row_count(count: usize) -> String {
    match count {
        1 => "1 row".to_owned(),
        _ => format!("{count} rows"),
    }
}

#[cfg(feature = "duckdb")]
fn now() -> String {
    octofhir_sof::expansion::ExpansionStamp::now().timestamp
}

/// Rows of a query written to a file in one of the output formats.
#[cfg(feature = "duckdb")]
mod rows {
    use std::io::Write;

    use anyhow::{Result, bail};
    use octofhir_sof::embedded::RowSink;
    use octofhir_sof::output::{RowEncoder, RowFormat};
    use octofhir_sof::{ColumnInfo, Error};
    use serde_json::{Map, Value};

    enum Encoder {
        Rows(RowEncoder),
        #[cfg(feature = "parquet")]
        Parquet(Box<octofhir_sof::output::ParquetStreamEncoder>),
    }

    pub struct FileRows {
        format: String,
        #[cfg_attr(not(feature = "parquet"), allow(dead_code))]
        temporal: String,
        out: Box<dyn Write>,
        encoder: Option<Encoder>,
    }

    pub fn extension(format: &str) -> Result<&'static str> {
        Ok(match format.to_ascii_lowercase().as_str() {
            "csv" => "csv",
            "ndjson" => "ndjson",
            "json" => "json",
            "parquet" if cfg!(feature = "parquet") => "parquet",
            "parquet" => bail!("Parquet output requires a build with --features parquet"),
            other => bail!("unknown output format {other} (csv, ndjson, json, parquet)"),
        })
    }

    impl FileRows {
        pub fn new(format: &str, temporal: &str, out: Box<dyn Write>) -> Result<Self> {
            extension(format)?;
            Ok(Self {
                format: format.to_ascii_lowercase(),
                temporal: temporal.to_owned(),
                out,
                encoder: None,
            })
        }

        /// The end of the file.
        pub fn finish(mut self) -> Result<()> {
            let tail = match self.encoder.take() {
                Some(Encoder::Rows(encoder)) => encoder.finish().into_bytes(),
                #[cfg(feature = "parquet")]
                Some(Encoder::Parquet(encoder)) => encoder.finish()?,
                None => Vec::new(),
            };
            self.out.write_all(&tail)?;
            self.out.flush()?;
            Ok(())
        }

        fn write(&mut self, bytes: &[u8]) -> octofhir_sof::Result<()> {
            self.out
                .write_all(bytes)
                .map_err(|error| Error::Output(error.to_string()))
        }
    }

    impl RowSink for FileRows {
        fn start(&mut self, columns: &[ColumnInfo]) -> octofhir_sof::Result<()> {
            let names: Vec<String> = columns.iter().map(|column| column.name.clone()).collect();
            let format = match self.format.as_str() {
                "ndjson" => RowFormat::Ndjson,
                "json" => RowFormat::Json,
                "parquet" => {
                    #[cfg(feature = "parquet")]
                    {
                        use octofhir_sof::output::{
                            ParquetStreamEncoder, ParquetWriter, TemporalEncoding,
                        };
                        let settings = ParquetWriter::new()
                            .with_temporal(TemporalEncoding::parse(&self.temporal)?);
                        self.encoder = Some(Encoder::Parquet(Box::new(ParquetStreamEncoder::new(
                            columns.to_vec(),
                            &settings,
                        )?)));
                        return Ok(());
                    }
                    #[cfg(not(feature = "parquet"))]
                    return Err(Error::Output(
                        "Parquet output needs the parquet feature".into(),
                    ));
                }
                _ => RowFormat::Csv { header: true },
            };
            let encoder = RowEncoder::new(format, names);
            let start = encoder.start();
            self.encoder = Some(Encoder::Rows(encoder));
            self.write(start.as_bytes())
        }

        fn row(&mut self, row: Map<String, Value>) -> octofhir_sof::Result<()> {
            let bytes = match self.encoder.as_mut() {
                Some(Encoder::Rows(encoder)) => encoder.row(&row).into_bytes(),
                #[cfg(feature = "parquet")]
                Some(Encoder::Parquet(encoder)) => {
                    let values = encoder
                        .columns()
                        .iter()
                        .map(|column| row.get(&column.name).cloned().unwrap_or(Value::Null))
                        .collect();
                    encoder.row(values)?
                }
                None => return Err(Error::Output("a row before the columns".into())),
            };
            self.write(&bytes)
        }
    }
}
