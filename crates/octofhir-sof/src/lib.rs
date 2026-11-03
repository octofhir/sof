//! SQL on FHIR implementation for OctoFHIR.
//!
//! This crate provides support for the SQL on FHIR specification, which enables
//! transformation of FHIR resources into tabular data using ViewDefinition resources.
//!
//! # Overview
//!
//! SQL on FHIR allows you to define views over FHIR resources that can be materialized
//! as relational tables. This is useful for analytics, reporting, and integration with
//! SQL-based tools.
//!
//! # Components
//!
//! - [`ViewDefinition`] - Parsed representation of a FHIR ViewDefinition resource
//! - [`SqlGenerator`] - Generates SQL from ViewDefinitions
//! - [`CompiledView`] / [`execute`] - Database-free in-memory evaluation
//! - [`SqlExecutor`] / [`run_with`] - Run generated SQL through any database layer
//! - `ViewRunner` - Executes views against PostgreSQL (feature `postgres`)
//! - [`library`] - SQLQuery / SQLView Libraries (v3): SQL by dialect,
//!   dependencies, parameter binding
//! - [`plan`] - the dependency graph of a `$sql-run` / `$sql-export` job:
//!   one resolution per canonical, `context` matching, CTE order
//! - [`relations`] / [`expansion`] - Terminology in SQL: ValueSet and
//!   ConceptMap relations, a strict ValueSet expander
//! - [`output::RowEncoder`] - row-at-a-time ndjson / csv / json for streamed
//!   exports
//!
//! # Cargo features
//!
//! The default build has no database or network dependency (parsing, SQL
//! generation for every dialect, in-memory evaluation). Optional backends are
//! opt-in:
//!
//! - `postgres` — the bundled sqlx `ViewRunner`
//! - `parquet` — the Parquet writer
//! - `tx-remote` — `terminology::HttpTerminologyProvider` for
//!   `memberOf()`/`subsumes()`/`translate()` against a remote terminology server
//!
//! # Terminology
//!
//! Terminology functions resolve through a [`TerminologyProvider`] passed in
//! [`CompileOptions`]; without one they fail with a clear error.
//!
//! # Example
//!
//! ```ignore
//! use octofhir_sof::{ViewDefinition, SqlGenerator, ViewRunner};
//!
//! // Parse a ViewDefinition
//! let view_def = ViewDefinition::from_json(&json)?;
//!
//! // Generate SQL
//! let generator = SqlGenerator::new();
//! let sql = generator.generate(&view_def)?;
//!
//! // Execute the view
//! let runner = ViewRunner::new(pool, generator);
//! let result = runner.run(&view_def).await?;
//! ```
//!
//! # SQL on FHIR Specification
//!
//! SQL on FHIR 3.0 (`hl7.fhir.uv.sql-on-fhir`, the HL7 continuous build of the
//! 3.0.0 ballot): <https://build.fhir.org/ig/HL7/sql-on-fhir/>. Views written for
//! 2.0 / 2.1 run unchanged.

pub mod catalog;
mod column;
#[cfg(feature = "duckdb")]
pub mod embedded;
mod eval;
pub mod expansion;
#[cfg(feature = "guard")]
pub mod guard;
pub mod library;
pub mod output;
pub mod plan;
pub mod relations;
mod runner;
mod sql_generator;
mod view_definition;

pub use column::{ColumnInfo, ColumnType};
pub use eval::{
    CompileOptions, CompiledView, execute, execute_blocking, execute_parallel,
    execute_parallel_with, execute_with,
};
#[cfg(feature = "postgres")]
pub use runner::ViewRunner;
pub use runner::{SqlExecutor, ViewResult, run_with};
pub use sql_generator::{Dialect, GeneratedColumn, GeneratedSql, SqlGenerator, create_table};
pub use view_definition::{Column, Constant, SelectColumn, ViewDefinition, WhereClause};

pub use octofhir_fhir_model::TerminologyProvider;

/// Terminology provider types, re-exported from `octofhir-fhir-model` so hosts
/// can implement or construct one without a direct dependency.
pub mod terminology {
    pub use octofhir_fhir_model::terminology::*;
}

use thiserror::Error;

/// Errors that can occur during SQL on FHIR operations.
#[derive(Debug, Error)]
pub enum Error {
    /// The ViewDefinition JSON is invalid or missing required fields.
    #[error("Invalid ViewDefinition: {0}")]
    InvalidViewDefinition(String),

    /// A FHIRPath expression is invalid or cannot be converted to SQL.
    #[error("Invalid path: {0}")]
    InvalidPath(String),

    /// An error occurred while executing SQL through the bundled sqlx runner.
    #[cfg(feature = "postgres")]
    #[error("SQL execution error: {0}")]
    Sql(#[from] sqlx_core::error::Error),

    /// An external [`SqlExecutor`] failed to run the generated SQL.
    #[error("SQL execution error: {0}")]
    Execution(String),

    /// An error occurred during FHIRPath evaluation or compilation.
    #[error("FHIRPath error: {0}")]
    FhirPath(String),

    /// An error occurred while generating output.
    #[error("Output error: {0}")]
    Output(String),

    /// A Library does not conform to the SQLQuery / SQLView profile.
    #[error("Invalid Library: {0}")]
    InvalidLibrary(String),

    /// A parameter value does not bind to the Library's declarations.
    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),

    /// JSON serialization/deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Result type alias using the crate's Error type.
pub type Result<T> = std::result::Result<T, Error>;
