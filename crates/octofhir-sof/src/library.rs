//! SQLQuery and SQLView Libraries (SQL on FHIR v3, `Library` profiles).
//!
//! A SQLQuery holds one query over its dependencies; a SQLView is a reusable
//! query other Libraries read as a table, and declares no parameters. Both
//! carry the SQL as `content` attachments (`application/sql`, optionally per
//! `dialect`), name their dependencies (ViewDefinitions, SQLViews, ValueSets
//! or ConceptMaps) in `relatedArtifact` (`depends-on`, `label` = the table
//! name in the SQL), and a SQLQuery declares its inputs in `parameter`, which
//! [`bind_parameters`] binds from a request's `Parameters`.

use std::collections::HashMap;

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;

use crate::{Error, Result};

/// `Library.type` system of the SQL on FHIR Library profiles.
pub const LIBRARY_TYPES: &str = "http://hl7.org/fhir/uv/sql-on-fhir/CodeSystem/LibraryTypesCodes";

/// Which SQL Library profile a Library conforms to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqlLibraryKind {
    Query,
    View,
}

/// A table the SQL reads: a ViewDefinition or a SQLView.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dependency {
    /// The table name in the SQL (`relatedArtifact.label`).
    pub label: String,
    /// Canonical URL of the ViewDefinition or SQLView, optionally `|version`.
    pub canonical: String,
}

/// An input of a SQLQuery (`Library.parameter`, `use = in`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredParameter {
    pub name: String,
    /// FHIR type of the value (`string`, `date`, ...).
    pub fhir_type: String,
    /// The parameter must be supplied (`min` ≥ 1).
    pub required: bool,
}

impl DeclaredParameter {
    /// The `value[x]` field carrying a value of this type in `Parameters`.
    pub fn value_field(&self) -> String {
        let mut chars = self.fhir_type.chars();
        match chars.next() {
            Some(first) => format!("value{}{}", first.to_ascii_uppercase(), chars.as_str()),
            None => "value".into(),
        }
    }

    /// The SQL type (PostgreSQL spelling) a value of this type binds as, or
    /// `None` for a type that has no scalar SQL counterpart.
    pub fn sql_type(&self) -> Option<&'static str> {
        Some(match self.fhir_type.as_str() {
            "string" | "code" | "id" | "uri" | "url" | "canonical" | "oid" | "uuid"
            | "markdown" | "base64Binary" => "text",
            "integer" | "positiveInt" | "unsignedInt" => "integer",
            "integer64" => "bigint",
            "decimal" => "numeric",
            "boolean" => "boolean",
            "date" => "date",
            "dateTime" | "instant" => "timestamptz",
            "time" => "time",
            _ => return None,
        })
    }
}

/// A SQLQuery or SQLView, with the SQL for one dialect.
#[derive(Clone, Debug)]
pub struct SqlLibrary {
    pub kind: SqlLibraryKind,
    /// `url|version`, or the resource id, for messages.
    pub display: String,
    /// The SQL of the requested dialect, else the portable one.
    pub sql: String,
    pub dependencies: Vec<Dependency>,
    pub parameters: Vec<DeclaredParameter>,
}

/// The profile `library` claims by its type, if it is a SQL Library.
pub fn kind_of(library: &Value) -> Option<SqlLibraryKind> {
    library["type"]["coding"]
        .as_array()?
        .iter()
        .filter(|coding| coding["system"] == LIBRARY_TYPES)
        .find_map(|coding| match coding["code"].as_str() {
            Some("sql-query") => Some(SqlLibraryKind::Query),
            Some("sql-view") => Some(SqlLibraryKind::View),
            _ => None,
        })
}

impl SqlLibrary {
    /// Read a SQLQuery or SQLView, taking the SQL of `dialect` (e.g.
    /// `postgresql`, `duckdb`) else the portable `application/sql` content,
    /// never a translation of another dialect. [`Error::InvalidLibrary`]
    /// names what does not conform to the profile.
    pub fn from_json(library: &Value, dialect: &str) -> Result<Self> {
        Self::read(library, dialect).map_err(Error::InvalidLibrary)
    }

    fn read(library: &Value, dialect: &str) -> std::result::Result<Self, String> {
        if library["resourceType"] != "Library" {
            return Err("The subject is not a Library".into());
        }
        let kind = kind_of(library)
            .ok_or_else(|| format!("Library type is not {LIBRARY_TYPES}#sql-query or #sql-view"))?;
        let display = match (library["url"].as_str(), library["version"].as_str()) {
            (Some(url), Some(version)) => format!("{url}|{version}"),
            (Some(url), None) => url.to_owned(),
            _ => format!("Library/{}", library["id"].as_str().unwrap_or("(inline)")),
        };
        let sql = select_sql(library, dialect)?;
        let dependencies = dependencies(library)?;
        let parameters = parameters(library)?;
        if kind == SqlLibraryKind::View && !parameters.is_empty() {
            return Err("A SQLView declares no parameters".into());
        }
        Ok(Self {
            kind,
            display,
            sql,
            dependencies,
            parameters,
        })
    }
}

/// The SQL to run: the `dialect` attachment, else the portable
/// `application/sql` one; never a translation of another dialect.
fn select_sql(library: &Value, dialect: &str) -> std::result::Result<String, String> {
    let contents = library["content"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if contents.is_empty() {
        return Err("The Library has no SQL content".into());
    }
    let mut portable = None;
    let mut native = None;
    for content in contents {
        let content_type = content["contentType"].as_str().unwrap_or_default();
        let (media_type, content_dialect) = media_type_and_dialect(content_type);
        if media_type != "application/sql" {
            return Err(format!(
                "Every content.contentType must be application/sql, found {content_type:?}"
            ));
        }
        match content_dialect.as_deref() {
            None => portable = Some(content),
            Some(named) if named.eq_ignore_ascii_case(dialect) => native = Some(content),
            Some(_) => {}
        }
    }
    let content = native.or(portable).ok_or_else(|| {
        format!("The Library has no SQL for the {dialect} dialect nor portable application/sql")
    })?;
    let data = content["data"]
        .as_str()
        .ok_or("SQL content requires data (base64)")?;
    let bytes = STANDARD
        .decode(data)
        .map_err(|_| "SQL content data is not base64".to_owned())?;
    String::from_utf8(bytes).map_err(|_| "SQL content is not UTF-8 text".to_owned())
}

/// `application/sql;dialect=postgresql` → (`application/sql`, `postgresql`).
fn media_type_and_dialect(content_type: &str) -> (String, Option<String>) {
    let mut parts = content_type.split(';');
    let media_type = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
    let dialect = parts.find_map(|part| {
        let (key, value) = part.split_once('=')?;
        (key.trim().eq_ignore_ascii_case("dialect"))
            .then(|| value.trim().trim_matches('"').to_ascii_lowercase())
    });
    (media_type, dialect)
}

fn dependencies(library: &Value) -> std::result::Result<Vec<Dependency>, String> {
    let mut dependencies: Vec<Dependency> = Vec::new();
    for artifact in library["relatedArtifact"].as_array().into_iter().flatten() {
        if artifact["type"] != "depends-on" {
            continue;
        }
        let label = artifact["label"]
            .as_str()
            .ok_or("A depends-on relatedArtifact requires a label (its table name)")?;
        if !is_sql_name(label) {
            return Err(format!(
                "relatedArtifact.label {label:?} is not a valid SQL identifier ([A-Za-z][A-Za-z0-9_]*)"
            ));
        }
        let canonical = artifact["resource"]
            .as_str()
            .ok_or_else(|| format!("relatedArtifact {label} requires a resource canonical"))?;
        let label = label.to_ascii_lowercase();
        if dependencies.iter().any(|known| known.label == label) {
            return Err(format!("relatedArtifact.label {label} is not unique"));
        }
        dependencies.push(Dependency {
            label,
            canonical: canonical.to_owned(),
        });
    }
    Ok(dependencies)
}

fn parameters(library: &Value) -> std::result::Result<Vec<DeclaredParameter>, String> {
    let mut parameters: Vec<DeclaredParameter> = Vec::new();
    for parameter in library["parameter"].as_array().into_iter().flatten() {
        let name = parameter["name"]
            .as_str()
            .ok_or("Library.parameter requires a name")?;
        if parameter["use"] != "in" {
            return Err(format!("Parameter {name} must have use = in"));
        }
        if !is_sql_name(name) {
            return Err(format!(
                "Parameter name {name:?} is not a valid SQL identifier"
            ));
        }
        let fhir_type = parameter["type"]
            .as_str()
            .ok_or_else(|| format!("Parameter {name} requires a type"))?;
        if parameter["max"].as_str().is_some_and(|max| max != "1") {
            return Err(format!("Parameter {name} binds a single value (max = 1)"));
        }
        let declared = DeclaredParameter {
            name: name.to_owned(),
            fhir_type: fhir_type.to_owned(),
            required: parameter["min"].as_u64().is_some_and(|min| min >= 1),
        };
        if declared.sql_type().is_none() {
            return Err(format!(
                "Parameter {name} has type {fhir_type}, which has no SQL value"
            ));
        }
        if parameters.iter().any(|known| known.name == name) {
            return Err(format!("Parameter {name} is declared twice"));
        }
        parameters.push(declared);
    }
    Ok(parameters)
}

/// A bound parameter value.
#[derive(Clone, Debug, PartialEq)]
pub enum ParameterValue {
    Text(String),
    Integer(i64),
    Boolean(bool),
    /// A decimal, as written (bind it as numeric text).
    Decimal(String),
    Null,
}

/// The values of `supplied` (a `Parameters` resource, or none) for
/// `library`'s declared parameters, checked by name and declared type; a
/// declared parameter not supplied is [`ParameterValue::Null`] unless it is
/// required. [`Error::InvalidParameter`] names the problem (the operations
/// answer 400).
pub fn bind_parameters(
    library: &SqlLibrary,
    supplied: Option<&Value>,
) -> Result<HashMap<String, ParameterValue>> {
    let invalid = |message: String| Error::InvalidParameter(format!("parameters: {message}"));
    let mut bound = HashMap::new();
    let values = supplied
        .and_then(|supplied| supplied["parameter"].as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();
    for value in values {
        let name = value["name"]
            .as_str()
            .ok_or_else(|| invalid("every parameter needs a name".into()))?;
        let declared = library
            .parameters
            .iter()
            .find(|declared| declared.name == name)
            .ok_or_else(|| invalid(format!("{} declares no parameter {name}", library.display)))?;
        let field = declared.value_field();
        let raw = value.get(&field).ok_or_else(|| {
            invalid(format!(
                "{name} is {}, so it takes {field}",
                declared.fhir_type
            ))
        })?;
        let bound_value = match (declared.sql_type(), raw) {
            (Some("integer" | "bigint"), Value::Number(number)) => number
                .as_i64()
                .map(ParameterValue::Integer)
                .ok_or_else(|| invalid(format!("{name} is not an integer")))?,
            (Some("boolean"), Value::Bool(flag)) => ParameterValue::Boolean(*flag),
            (Some("numeric"), Value::Number(number)) => ParameterValue::Decimal(number.to_string()),
            (Some(_), Value::String(text)) => ParameterValue::Text(text.clone()),
            _ => return Err(invalid(format!("{name} has a value of the wrong type"))),
        };
        if bound.insert(name.to_owned(), bound_value).is_some() {
            return Err(invalid(format!("{name} is supplied twice")));
        }
    }
    for declared in &library.parameters {
        if !bound.contains_key(&declared.name) {
            if declared.required {
                return Err(invalid(format!("{} is required", declared.name)));
            }
            bound.insert(declared.name.clone(), ParameterValue::Null);
        }
    }
    Ok(bound)
}

/// Canonical of the `sql-text` extension (the SQL in plain text on an
/// `Attachment`).
pub const SQL_TEXT_EXTENSION: &str =
    "http://hl7.org/fhir/uv/sql-on-fhir/StructureDefinition/sql-text";

/// Profiles a built Library claims.
const SQL_QUERY_PROFILE: &str = "http://hl7.org/fhir/uv/sql-on-fhir/StructureDefinition/SQLQuery";
const SQL_VIEW_PROFILE: &str = "http://hl7.org/fhir/uv/sql-on-fhir/StructureDefinition/SQLView";

/// How [`build_annotated`] builds a Library.
#[derive(Clone, Debug, Default)]
pub struct AnnotatedOptions {
    /// Build a SQLView rather than a SQLQuery.
    pub view: bool,
    /// The name to use when the SQL has no `@name` (e.g. the file name).
    pub name: Option<String>,
    /// `Library.url`, if any.
    pub url: Option<String>,
    /// The dialect of the SQL (`application/sql;dialect=…`); portable if
    /// none.
    pub dialect: Option<String>,
}

/// A Library built from annotated SQL, with what was not understood.
#[derive(Clone, Debug)]
pub struct AnnotatedLibrary {
    pub library: Value,
    /// Unrecognized annotations and similar notes.
    pub warnings: Vec<String>,
}

/// Build a SQLQuery (or SQLView) Library from SQL carrying annotations in its
/// comments (SQL on FHIR, SQLQuery "SQL Annotations"): `@name`, `@title`,
/// `@description`, `@version`, `@status`, `@author`, `@publisher`,
/// `@param: name type [description]` and
/// `@relatedDependency: URL [as label]`, in block (`/* */`) and line (`--`)
/// comments. The SQL goes into `content.data` (base64) and the `sql-text`
/// extension; the name falls back to `options.name`, the status to `draft`;
/// parameter types and labels are checked, and the result is read back as a
/// [`SqlLibrary`] so it conforms.
pub fn build_annotated(sql: &str, options: &AnnotatedOptions) -> Result<AnnotatedLibrary> {
    let invalid = |message: String| Error::InvalidLibrary(message);
    let mut warnings = Vec::new();
    let mut library = serde_json::Map::new();
    library.insert("resourceType".into(), Value::from("Library"));
    library.insert(
        "meta".into(),
        serde_json::json!({"profile": [if options.view { SQL_VIEW_PROFILE } else { SQL_QUERY_PROFILE }]}),
    );
    if let Some(url) = &options.url {
        library.insert("url".into(), Value::from(url.as_str()));
    }
    let mut authors = Vec::new();
    let mut parameters = Vec::new();
    let mut dependencies = Vec::new();
    for comment in comments(sql) {
        for line in comment.lines() {
            let line = line.trim().trim_start_matches('*').trim();
            let Some(annotation) = line.strip_prefix('@') else {
                continue;
            };
            let Some((key, value)) = annotation.split_once(':') else {
                warnings.push(format!("annotation @{annotation} has no value"));
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "name" | "title" | "description" | "version" | "status" | "publisher" => {
                    library.insert(key.into(), Value::from(value));
                }
                "author" => authors.push(serde_json::json!({"name": value})),
                "param" => {
                    let mut words = value.splitn(3, char::is_whitespace);
                    let (Some(name), Some(fhir_type)) = (words.next(), words.next()) else {
                        return Err(invalid(format!("@param {value:?} needs a name and a type")));
                    };
                    let declared = DeclaredParameter {
                        name: name.to_owned(),
                        fhir_type: fhir_type.to_owned(),
                        required: false,
                    };
                    if !is_sql_name(name) {
                        return Err(invalid(format!(
                            "@param name {name:?} is not a SQL identifier"
                        )));
                    }
                    if declared.sql_type().is_none() {
                        return Err(invalid(format!(
                            "@param {name} has type {fhir_type}, which has no SQL value"
                        )));
                    }
                    let mut parameter =
                        serde_json::json!({"name": name, "use": "in", "type": fhir_type});
                    if let Some(documentation) =
                        words.next().map(str::trim).filter(|text| !text.is_empty())
                    {
                        parameter["documentation"] = Value::from(documentation);
                    }
                    parameters.push(parameter);
                }
                "relatedDependency" => {
                    let (canonical, label) = match value.split_once(" as ") {
                        Some((canonical, label)) => (canonical.trim(), label.trim().to_owned()),
                        None => (value, default_label(value)),
                    };
                    if !is_sql_name(&label) {
                        return Err(invalid(format!(
                            "@relatedDependency label {label:?} is not a SQL identifier"
                        )));
                    }
                    dependencies.push(serde_json::json!({
                        "type": "depends-on", "resource": canonical, "label": label
                    }));
                }
                other => warnings.push(format!("unrecognized annotation @{other}")),
            }
        }
    }
    if !library.contains_key("name")
        && let Some(name) = &options.name
    {
        library.insert("name".into(), Value::from(name.as_str()));
    }
    library
        .entry("status")
        .or_insert_with(|| Value::from("draft"));
    if !authors.is_empty() {
        library.insert("author".into(), Value::Array(authors));
    }
    library.insert(
        "type".into(),
        serde_json::json!({"coding": [{
            "system": LIBRARY_TYPES,
            "code": if options.view { "sql-view" } else { "sql-query" }
        }]}),
    );
    if !parameters.is_empty() {
        library.insert("parameter".into(), Value::Array(parameters));
    }
    if !dependencies.is_empty() {
        library.insert("relatedArtifact".into(), Value::Array(dependencies));
    }
    let content_type = match &options.dialect {
        Some(dialect) => format!("application/sql;dialect={dialect}"),
        None => "application/sql".to_owned(),
    };
    library.insert(
        "content".into(),
        serde_json::json!([{
            "contentType": content_type,
            "data": STANDARD.encode(sql),
            "extension": [{"url": SQL_TEXT_EXTENSION, "valueString": sql}]
        }]),
    );
    let library = Value::Object(library);
    SqlLibrary::from_json(&library, options.dialect.as_deref().unwrap_or("postgresql"))?;
    Ok(AnnotatedLibrary { library, warnings })
}

/// A dependency label from its canonical: the last path segment, as a SQL
/// identifier.
fn default_label(canonical: &str) -> String {
    let url = canonical.split('|').next().unwrap_or(canonical);
    let segment = url.trim_end_matches('/').rsplit('/').next().unwrap_or(url);
    let mut label: String = segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if !label.starts_with(|c: char| c.is_ascii_alphabetic()) {
        label.insert(0, 't');
    }
    label
}

/// The texts of the comments of `sql` (block and line), outside string
/// literals and quoted identifiers.
fn comments(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut found = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            quote @ ('\'' | '"') => {
                i += 1;
                while i < chars.len() {
                    if chars[i] == quote {
                        if chars.get(i + 1) == Some(&quote) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            '-' if chars.get(i + 1) == Some(&'-') => {
                let start = i + 2;
                let end = (start..chars.len())
                    .find(|&j| chars[j] == '\n')
                    .unwrap_or(chars.len());
                found.push(chars[start..end].iter().collect());
                i = end;
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                let start = i + 2;
                let end = (start..chars.len().saturating_sub(1))
                    .find(|&j| chars[j] == '*' && chars[j + 1] == '/')
                    .unwrap_or(chars.len());
                found.push(chars[start..end.min(chars.len())].iter().collect());
                i = end + 2;
            }
            _ => i += 1,
        }
    }
    found
}

/// `sql-name`: `^[A-Za-z][A-Za-z0-9_]*$`.
fn is_sql_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn library(content: Value) -> Value {
        json!({
            "resourceType": "Library",
            "url": "http://example.org/Library/BpByPatient",
            "version": "1.0",
            "type": {"coding": [{"system": LIBRARY_TYPES, "code": "sql-query"}]},
            "status": "active",
            "relatedArtifact": [
                {"type": "depends-on", "label": "patient",
                 "resource": "http://example.org/ViewDefinition/patient"},
                {"type": "documentation", "url": "http://example.org/doc"}
            ],
            "parameter": [
                {"name": "from_date", "use": "in", "type": "date", "min": 1}
            ],
            "content": content
        })
    }

    impl SqlLibrary {
        fn from_json_pg(library: &Value) -> std::result::Result<Self, String> {
            Self::from_json(library, "postgresql").map_err(|error| error.to_string())
        }
    }

    fn sql(text: &str, content_type: &str) -> Value {
        json!({"contentType": content_type, "data": STANDARD.encode(text)})
    }

    #[test]
    fn a_sql_query_reads_with_its_dependencies_and_parameters() {
        let parsed =
            SqlLibrary::from_json_pg(&library(json!([sql("SELECT 1", "application/sql")])))
                .unwrap();
        assert_eq!(parsed.kind, SqlLibraryKind::Query);
        assert_eq!(parsed.display, "http://example.org/Library/BpByPatient|1.0");
        assert_eq!(parsed.sql, "SELECT 1");
        assert_eq!(
            parsed.dependencies,
            [Dependency {
                label: "patient".into(),
                canonical: "http://example.org/ViewDefinition/patient".into()
            }]
        );
        assert_eq!(parsed.parameters[0].value_field(), "valueDate");
        assert!(parsed.parameters[0].required);
    }

    #[test]
    fn the_postgresql_variant_wins_and_other_dialects_are_never_used() {
        let parsed = SqlLibrary::from_json_pg(&library(json!([
            sql("SELECT 'portable'", "application/sql"),
            sql("SELECT 'duck'", "application/sql;dialect=duckdb"),
            sql("SELECT 'pg'", "application/sql; dialect=postgresql")
        ])))
        .unwrap();
        assert_eq!(parsed.sql, "SELECT 'pg'");
        let error = SqlLibrary::from_json_pg(&library(json!([sql(
            "SELECT 'duck'",
            "application/sql;dialect=duckdb"
        )])))
        .unwrap_err();
        assert!(error.contains("postgresql"), "{error}");
    }

    #[test]
    fn non_conforming_libraries_are_described() {
        let mut not_sql = library(json!([sql("SELECT 1", "application/sql")]));
        not_sql["type"] = json!({"coding": [{"code": "logic-library"}]});
        assert!(SqlLibrary::from_json_pg(&not_sql).is_err());

        let cql = library(json!([sql("define X: 1", "text/cql")]));
        assert!(
            SqlLibrary::from_json_pg(&cql)
                .unwrap_err()
                .contains("application/sql")
        );

        let mut bad_label = library(json!([sql("SELECT 1", "application/sql")]));
        bad_label["relatedArtifact"][0]["label"] = json!("patient; DROP");
        assert!(
            SqlLibrary::from_json_pg(&bad_label)
                .unwrap_err()
                .contains("SQL identifier")
        );

        let mut view_with_parameters = library(json!([sql("SELECT 1", "application/sql")]));
        view_with_parameters["type"]["coding"][0]["code"] = json!("sql-view");
        assert!(
            SqlLibrary::from_json_pg(&view_with_parameters)
                .unwrap_err()
                .contains("no parameters")
        );
    }

    fn query_library() -> SqlLibrary {
        SqlLibrary::from_json(
            &json!({
                "resourceType": "Library",
                "url": "http://example.org/Library/q",
                "type": {"coding": [{"system": LIBRARY_TYPES, "code": "sql-query"}]},
                "parameter": [
                    {"name": "from_date", "use": "in", "type": "date", "min": 1},
                    {"name": "max_count", "use": "in", "type": "integer"}
                ],
                "content": [{"contentType": "application/sql", "data": STANDARD.encode("SELECT 1")}]
            }),
            "postgresql",
        )
        .unwrap()
    }

    fn supplied(values: Value) -> Value {
        json!({"resourceType": "Parameters", "parameter": values})
    }

    #[test]
    fn parameters_bind_by_name_and_declared_type() {
        let bound = bind_parameters(
            &query_library(),
            Some(&supplied(
                json!([{"name": "from_date", "valueDate": "2026-01-01"}]),
            )),
        )
        .unwrap();
        assert_eq!(
            bound["from_date"],
            ParameterValue::Text("2026-01-01".into())
        );
        assert_eq!(bound["max_count"], ParameterValue::Null);
        let bound = bind_parameters(
            &query_library(),
            Some(&supplied(json!([
                {"name": "from_date", "valueDate": "2026-01-01"},
                {"name": "max_count", "valueInteger": 5}
            ]))),
        )
        .unwrap();
        assert_eq!(bound["max_count"], ParameterValue::Integer(5));
    }

    #[test]
    fn unknown_mistyped_and_missing_parameters_are_rejected() {
        for values in [
            json!([{"name": "from_date", "valueDate": "2026-01-01"},
                   {"name": "other", "valueString": "x"}]),
            json!([{"name": "from_date", "valueString": "2026-01-01"}]),
            json!([{"name": "max_count", "valueInteger": 1}]),
        ] {
            assert!(
                matches!(
                    bind_parameters(&query_library(), Some(&supplied(values.clone()))),
                    Err(Error::InvalidParameter(_))
                ),
                "{values}"
            );
        }
    }

    #[test]
    fn annotated_sql_builds_a_conforming_query() {
        let sql = "/*\n@name: PatientBloodPressure\n@title: Patient Blood Pressure Report\n@version: 1.0.0\n@status: active\n@colour: blue\n*/\n-- @param: patient_id string Patient identifier\n-- @param: from_date date Start date\n-- @relatedDependency: https://example.org/ViewDefinition/patient_view as patient\n-- @relatedDependency: https://example.org/ViewDefinition/bp_view|2\nSELECT patient.id, bp_view.systolic, '-- @name: not a comment'\nFROM patient JOIN bp_view ON patient.id = bp_view.patient_id\nWHERE patient.id = :patient_id AND bp_view.effective_date >= :from_date";
        let built = build_annotated(sql, &AnnotatedOptions::default()).unwrap();
        let library = &built.library;
        assert_eq!(library["name"], "PatientBloodPressure");
        assert_eq!(library["status"], "active");
        assert_eq!(library["type"]["coding"][0]["code"], "sql-query");
        assert_eq!(library["parameter"][1]["type"], "date");
        assert_eq!(
            library["parameter"][0]["documentation"],
            "Patient identifier"
        );
        assert_eq!(library["relatedArtifact"][1]["label"], "bp_view");
        assert_eq!(library["content"][0]["extension"][0]["valueString"], sql);
        assert_eq!(built.warnings, ["unrecognized annotation @colour"]);
        let parsed = SqlLibrary::from_json(library, "postgresql").unwrap();
        assert_eq!(parsed.dependencies.len(), 2);

        let view = build_annotated(
            "-- @relatedDependency: http://x/ViewDefinition/p as p\nSELECT * FROM p",
            &AnnotatedOptions {
                view: true,
                name: Some("ActivePatients".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(view.library["name"], "ActivePatients");
        assert_eq!(view.library["status"], "draft");
        assert_eq!(view.library["type"]["coding"][0]["code"], "sql-view");

        for bad in [
            "-- @param: x widget\nSELECT 1",
            "-- @relatedDependency: http://x/v as 1st\nSELECT 1",
            "-- @param: x\nSELECT 1",
        ] {
            assert!(
                build_annotated(bad, &AnnotatedOptions::default()).is_err(),
                "{bad}"
            );
        }
        // A SQLView declares no parameters.
        assert!(
            build_annotated(
                "-- @param: x string\nSELECT 1",
                &AnnotatedOptions {
                    view: true,
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
}
