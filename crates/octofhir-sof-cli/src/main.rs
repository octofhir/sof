//! `octofhir-sof` — command-line interface over the `octofhir-sof` library.
//!
//! The CLI is a thin shell: it parses arguments and delegates to the library's
//! `SqlGenerator`, `ViewRunner` and output writers. All real work lives in the
//! library so it stays embeddable.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use octofhir_sof::output::get_writer;
use octofhir_sof::{SqlGenerator, ViewDefinition, ViewRunner};
use octofhir_sof_lint::{FhirSchemaProvider, Severity, lint};
use sqlx_postgres::PgPool;

#[derive(Parser)]
#[command(name = "octofhir-sof", version, about = "SQL on FHIR toolkit")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate PostgreSQL from a ViewDefinition (offline, no database).
    Generate {
        /// Path to the ViewDefinition JSON file.
        view: PathBuf,
    },

    /// Execute a ViewDefinition and write the rows. Runs against FHIR files with
    /// `--input` (no database) or against PostgreSQL with `--db`.
    Run {
        /// Path to the ViewDefinition JSON file.
        view: PathBuf,

        /// FHIR resources to run against, with no database: an NDJSON file, a
        /// Bundle, a JSON resource or array, or a directory of such files.
        #[arg(long, conflicts_with = "db")]
        input: Option<PathBuf>,

        /// PostgreSQL connection URL (or set DATABASE_URL).
        #[arg(long, env = "DATABASE_URL")]
        db: Option<String>,

        /// Output format: csv, ndjson, json (parquet with the parquet feature).
        #[arg(long, default_value = "csv")]
        output: String,

        /// Write to this file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Validate a ViewDefinition's structure against the spec, offline (no FHIR
    /// package and no database): SQL-safe names, one iteration construct per
    /// select, unique column names, consistent unionAll branches.
    Validate {
        /// Path to the ViewDefinition JSON file.
        view: PathBuf,
    },

    /// Validate a ViewDefinition's FHIRPath selectors and generated SQL against
    /// a FHIR package.
    Lint {
        /// Path to the ViewDefinition JSON file.
        view: PathBuf,

        /// FHIR package name (e.g. hl7.fhir.r4.core).
        #[arg(long)]
        package: String,

        /// Package version. When given, the package is installed if missing;
        /// otherwise only an already-present package is used (offline).
        #[arg(long)]
        version: Option<String>,
    },
}

fn load_view(path: &PathBuf) -> Result<ViewDefinition> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading ViewDefinition {}", path.display()))?;
    ViewDefinition::parse(&text)
        .with_context(|| format!("parsing ViewDefinition {}", path.display()))
}

/// Load FHIR resources from a file or a directory of files. Supports NDJSON
/// (`.ndjson`), single resources, resource arrays, and Bundles.
fn load_resources(path: &PathBuf) -> Result<Vec<serde_json::Value>> {
    let mut resources = Vec::new();
    if path.is_dir() {
        let mut entries: Vec<PathBuf> = fs::read_dir(path)
            .with_context(|| format!("reading directory {}", path.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json" || x == "ndjson"))
            .collect();
        entries.sort();
        for entry in entries {
            load_file(&entry, &mut resources)?;
        }
    } else {
        load_file(path, &mut resources)?;
    }
    if resources.is_empty() {
        anyhow::bail!("no FHIR resources found in {}", path.display());
    }
    Ok(resources)
}

fn load_file(path: &PathBuf, out: &mut Vec<serde_json::Value>) -> Result<()> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if path.extension().is_some_and(|x| x == "ndjson") {
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(line)
                .with_context(|| format!("parsing {} line {}", path.display(), i + 1))?;
            push_resource(value, out);
        }
    } else {
        let value: serde_json::Value =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        push_resource(value, out);
    }
    Ok(())
}

/// Expand a JSON value into resources: a Bundle becomes its entries, an array
/// becomes its elements, anything else is a single resource.
fn push_resource(value: serde_json::Value, out: &mut Vec<serde_json::Value>) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                push_resource(item, out);
            }
        }
        serde_json::Value::Object(ref map)
            if map.get("resourceType").and_then(|v| v.as_str()) == Some("Bundle") =>
        {
            if let Some(entries) = map.get("entry").and_then(|e| e.as_array()) {
                for entry in entries {
                    if let Some(resource) = entry.get("resource") {
                        push_resource(resource.clone(), out);
                    }
                }
            }
        }
        other => out.push(other),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Generate { view } => {
            let view = load_view(&view)?;
            let generated = SqlGenerator::new()
                .generate(&view)
                .context("generating SQL")?;
            println!("{}", generated.sql);
        }
        Command::Run {
            view,
            input,
            db,
            output,
            out,
        } => {
            let view = load_view(&view)?;
            let result = match (input, db) {
                (Some(path), _) => {
                    let resources = load_resources(&path)?;
                    octofhir_sof::execute(&view, &resources).context("executing view")?
                }
                (None, Some(db)) => {
                    let pool = PgPool::connect(&db)
                        .await
                        .with_context(|| format!("connecting to {db}"))?;
                    ViewRunner::new(pool)
                        .run(&view)
                        .await
                        .context("executing view")?
                }
                (None, None) => {
                    anyhow::bail!("provide --input <file|dir> to run on files, or --db <url>")
                }
            };

            let writer = get_writer(&output).context("selecting output format")?;
            let mut sink: Box<dyn Write> = match out {
                Some(path) => Box::new(
                    fs::File::create(&path)
                        .with_context(|| format!("creating {}", path.display()))?,
                ),
                None => Box::new(io::stdout().lock()),
            };
            writer.write(&result, &mut sink).context("writing output")?;
            sink.flush().ok();
        }
        Command::Validate { view } => {
            let view = load_view(&view)?;
            let findings = octofhir_sof_lint::validate_structure(&view);
            for finding in &findings {
                println!("{finding}");
            }
            if findings.is_empty() {
                println!("valid");
            } else {
                std::process::exit(1);
            }
        }
        Command::Lint {
            view,
            package,
            version,
        } => {
            let view = load_view(&view)?;
            let provider = FhirSchemaProvider::load(&package, version.as_deref())
                .await
                .with_context(|| format!("loading package {package}"))?;
            if provider.is_empty() {
                anyhow::bail!(
                    "package `{package}` has no StructureDefinitions in the store; \
                     pass --version to install it"
                );
            }

            let findings = lint(&view, &provider);
            let errors = findings
                .iter()
                .filter(|f| f.severity == Severity::Error)
                .count();
            for finding in &findings {
                println!("{finding}");
            }
            if findings.is_empty() {
                println!("no findings");
            }
            if errors > 0 {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
