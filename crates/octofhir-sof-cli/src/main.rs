//! `octofhir-sof` — command-line interface over the `octofhir-sof` library.
//!
//! The CLI is a thin shell: it parses arguments and delegates to the library's
//! `SqlGenerator`, `ViewRunner` and output writers. All real work lives in the
//! library so it stays embeddable.

mod diagnostic;

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use octofhir_sof::output::get_writer;
use octofhir_sof::{SqlGenerator, ViewDefinition, ViewRunner};
use octofhir_sof_lint::{FhirSchemaProvider, Severity, lint, validate_structure};
use sqlx_postgres::PgPool;

#[derive(Parser)]
#[command(name = "octofhir-sof", version, about = "SQL on FHIR toolkit")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Disable coloured output (also honours the NO_COLOR environment variable).
    #[arg(long, global = true)]
    no_color: bool,
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

        /// Emit findings as machine-readable JSON instead of a report.
        #[arg(long)]
        json: bool,
    },

    /// Run a SQL-on-FHIR test-case file (the official content-test format:
    /// `{resources, tests:[{title, view, expect|expectCount|expectColumns|
    /// expectError}]}`) in memory and report pass/fail. Exits non-zero on any
    /// failure.
    Test {
        /// Path to the test-case JSON file (or a directory of them).
        manifest: PathBuf,
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

        /// Emit findings as machine-readable JSON instead of a report.
        #[arg(long)]
        json: bool,
    },
}

/// Read a ViewDefinition file, returning both its raw text (for diagnostics
/// spans) and the parsed view.
fn read_view(path: &PathBuf) -> Result<(String, ViewDefinition)> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading ViewDefinition {}", path.display()))?;
    let view = ViewDefinition::parse(&text)
        .with_context(|| format!("parsing ViewDefinition {}", path.display()))?;
    Ok((text, view))
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

/// Run one SQL-on-FHIR test-case file in memory, printing per-test results.
/// Returns `(passed, total)`.
fn run_test_file(path: &PathBuf, color: bool) -> Result<(usize, usize)> {
    let doc: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
    )
    .with_context(|| format!("parsing {}", path.display()))?;

    let resources: Vec<serde_json::Value> = doc
        .get("resources")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let empty = Vec::new();
    let tests = doc
        .get("tests")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);

    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (mut pass, mut total) = (0usize, 0usize);
    for test in tests {
        total += 1;
        let title = test
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("<untitled>");
        let (ok, detail) = run_one_test(test, &resources);
        let marker = diagnostic::marker(ok, color);
        if ok {
            pass += 1;
            println!("  {marker} [{name}] {title}");
        } else {
            println!("  {marker} [{name}] {title}: {detail}");
        }
    }
    Ok((pass, total))
}

fn run_one_test(test: &serde_json::Value, resources: &[serde_json::Value]) -> (bool, String) {
    use serde_json::Value;
    let expect_error = test
        .get("expectError")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let view = match ViewDefinition::from_json(&test.get("view").cloned().unwrap_or(Value::Null)) {
        Ok(v) => v,
        Err(e) => return (expect_error, format!("parse error: {e}")),
    };
    let result = match octofhir_sof::execute(&view, resources) {
        Ok(r) => r,
        Err(e) => return (expect_error, format!("execute error: {e}")),
    };
    if expect_error {
        return (false, "expected an error but execution succeeded".into());
    }

    if let Some(expect) = test.get("expect").and_then(Value::as_array) {
        let actual = result.to_json_array();
        if multiset_eq(&actual, expect) {
            (true, String::new())
        } else {
            (false, format!("rows differ: got {}", Value::Array(actual)))
        }
    } else if let Some(count) = test.get("expectCount").and_then(Value::as_u64) {
        let got = result.row_count as u64;
        (got == count, format!("want {count} rows, got {got}"))
    } else if let Some(cols) = test.get("expectColumns").and_then(Value::as_array) {
        let got: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        let want: Vec<&str> = cols.iter().filter_map(Value::as_str).collect();
        (got == want, format!("want columns {want:?}, got {got:?}"))
    } else {
        (true, String::new())
    }
}

/// Order-insensitive comparison of two JSON row sets.
fn multiset_eq(a: &[serde_json::Value], b: &[serde_json::Value]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a: Vec<String> = a.iter().map(canonical_json).collect();
    let mut b: Vec<String> = b.iter().map(canonical_json).collect();
    a.sort();
    b.sort();
    a == b
}

/// Canonical JSON string with object keys sorted, for set comparison.
fn canonical_json(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<&String, &Value> = map.iter().collect();
            let parts: Vec<String> = sorted
                .iter()
                .map(|(k, val)| format!("{k:?}:{}", canonical_json(val)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(canonical_json).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

/// Print findings (JSON or rustc-style) and report whether any are errors.
fn report_findings(
    origin: &str,
    source: &str,
    findings: &[octofhir_sof_lint::Finding],
    json: bool,
    color: bool,
) -> bool {
    if json {
        println!("{}", diagnostic::render_findings_json(findings));
    } else {
        print!(
            "{}",
            diagnostic::render_findings(origin, source, findings, color)
        );
    }
    findings.iter().any(|f| f.severity == Severity::Error)
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let color = diagnostic::use_color(cli.no_color);
    if let Err(e) = run(cli, color).await {
        eprintln!("{} {e:#}", diagnostic::error_prefix(color));
        std::process::exit(1);
    }
}

async fn run(cli: Cli, color: bool) -> Result<()> {
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
        Command::Validate { view, json } => {
            let origin = view.display().to_string();
            let (source, view) = read_view(&view)?;
            let findings = validate_structure(&view);
            if findings.is_empty() && !json {
                println!("{}", diagnostic::ok("valid", color));
            } else {
                let has_error = report_findings(&origin, &source, &findings, json, color);
                if has_error {
                    std::process::exit(1);
                }
            }
        }
        Command::Test { manifest } => {
            let mut files = Vec::new();
            if manifest.is_dir() {
                let mut entries: Vec<PathBuf> = fs::read_dir(&manifest)
                    .with_context(|| format!("reading directory {}", manifest.display()))?
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().is_some_and(|x| x == "json"))
                    .collect();
                entries.sort();
                files.extend(entries);
            } else {
                files.push(manifest.clone());
            }
            let (mut pass, mut total) = (0usize, 0usize);
            for file in &files {
                let (p, t) = run_test_file(file, color)?;
                pass += p;
                total += t;
            }
            let summary = format!("{pass}/{total} passed");
            println!(
                "{}",
                if pass == total {
                    diagnostic::ok(&summary, color)
                } else {
                    summary
                }
            );
            if pass < total {
                std::process::exit(1);
            }
        }
        Command::Lint {
            view,
            package,
            version,
            json,
        } => {
            let origin = view.display().to_string();
            let (source, view) = read_view(&view)?;
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
            if findings.is_empty() && !json {
                println!("{}", diagnostic::ok("no findings", color));
            } else {
                let has_error = report_findings(&origin, &source, &findings, json, color);
                if has_error {
                    std::process::exit(1);
                }
            }
        }
    }
    Ok(())
}
