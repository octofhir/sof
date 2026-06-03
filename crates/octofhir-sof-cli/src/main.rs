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

    /// Execute a ViewDefinition against a database and write the rows.
    Run {
        /// Path to the ViewDefinition JSON file.
        view: PathBuf,

        /// PostgreSQL connection URL (or set DATABASE_URL).
        #[arg(long, env = "DATABASE_URL")]
        db: String,

        /// Output format: csv, ndjson, json (parquet with the parquet feature).
        #[arg(long, default_value = "csv")]
        output: String,

        /// Write to this file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
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
            db,
            output,
            out,
        } => {
            let view = load_view(&view)?;
            let pool = PgPool::connect(&db)
                .await
                .with_context(|| format!("connecting to {db}"))?;
            let result = ViewRunner::new(pool)
                .run(&view)
                .await
                .context("executing view")?;

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
