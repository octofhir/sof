//! SQL generation from ViewDefinitions.
//!
//! Converts a [`ViewDefinition`] into a PostgreSQL query over FHIR resources
//! stored as JSONB. FHIRPath selectors, `where` filters and `constant`
//! references are parsed with the real `octofhir-fhirpath` parser and the AST is
//! lowered to SQL with a collection-aware model: every FHIRPath sub-expression
//! evaluates to a JSONB *array* (a FHIRPath collection), so a singleton and a
//! one-element collection behave identically and array navigation flattens the
//! way the spec requires.

use std::cell::Cell;
use std::collections::HashMap;

use octofhir_fhirpath::{BinaryOperator, ExpressionNode, LiteralValue, UnaryOperator, parse_ast};
use serde_json::Value;

use crate::Error;
use crate::column::ColumnType;
use crate::view_definition::{Column, Constant, SelectColumn, ViewDefinition};

/// Generates SQL queries from ViewDefinitions.
pub struct SqlGenerator {
    /// Alias used for the base table (e.g. `base` in `FROM patient base`).
    table_pattern: String,
    /// Optional row-status predicate template; `{base}` is replaced with the
    /// base alias. `None` disables row filtering entirely.
    row_filter: Option<String>,
}

impl Default for SqlGenerator {
    fn default() -> Self {
        Self::new()
    }
}

impl SqlGenerator {
    /// Create a new SQL generator with default settings.
    pub fn new() -> Self {
        Self {
            table_pattern: "base".to_string(),
            row_filter: Some("{base}.status <> 'deleted'".to_string()),
        }
    }

    /// Create a new SQL generator with a custom base-table alias.
    pub fn with_table_pattern(table_pattern: impl Into<String>) -> Self {
        Self {
            table_pattern: table_pattern.into(),
            ..Self::new()
        }
    }

    /// Set the row-status predicate. `{base}` is replaced with the base alias.
    /// Pass `None` to emit no row filter (useful for plain tables that have no
    /// `status` column).
    pub fn with_row_filter(mut self, filter: Option<String>) -> Self {
        self.row_filter = filter;
        self
    }

    /// Generate SQL from a ViewDefinition.
    ///
    /// # Errors
    ///
    /// Returns an error if a FHIRPath selector cannot be parsed or lowered, if a
    /// referenced constant is undefined, or if the view's column shape is
    /// inconsistent across `unionAll` branches.
    pub fn generate(&self, view: &ViewDefinition) -> Result<GeneratedSql, Error> {
        if view.resource.trim().is_empty() {
            return Err(Error::InvalidViewDefinition(
                "ViewDefinition is missing the required `resource`".to_string(),
            ));
        }

        let constants = build_constants(view)?;
        let lower = Lower {
            resource_type: view.resource.clone(),
            constants,
            seq: Cell::new(0),
        };

        let table = view.resource.to_lowercase();
        let ctx0 = format!("{}.resource", self.table_pattern);

        // Top-level selects cross-join, exactly like nested selects.
        let mut plans = vec![Plan::empty()];
        for select in &view.select {
            let mut next = Vec::new();
            for p in &plans {
                for child in lower.build_select(select, &p.joins, &ctx0)? {
                    next.push(p.cross(&child));
                }
            }
            plans = next;
        }

        if plans.is_empty() || plans.iter().all(|p| p.columns.is_empty()) {
            return Err(Error::InvalidViewDefinition(
                "ViewDefinition produces no columns".to_string(),
            ));
        }

        // Every UNION ALL branch must expose the same ordered column shape.
        let shape: Vec<&str> = plans[0].columns.iter().map(|c| c.name.as_str()).collect();
        for p in &plans[1..] {
            let other: Vec<&str> = p.columns.iter().map(|c| c.name.as_str()).collect();
            if other != shape {
                return Err(Error::InvalidViewDefinition(
                    "unionAll branches have mismatched column shape".to_string(),
                ));
            }
        }

        // Lower top-level `where` filters to booleans over the base resource.
        let mut where_sql = Vec::new();
        for clause in &view.where_ {
            let path = lower.substitute(&clause.path)?;
            let ast = parse_ast(&path).map_err(|e| Error::FhirPath(e.to_string()))?;
            where_sql.push(lower.bool(&ast, &ctx0)?);
        }

        let select_sqls: Vec<String> = plans
            .iter()
            .map(|p| self.render_plan(&table, p, &where_sql))
            .collect();
        let sql = select_sqls.join(" UNION ALL ");

        let columns = plans[0]
            .columns
            .iter()
            .map(|c| GeneratedColumn {
                name: c.name.clone(),
                expression: c.expr.clone(),
                alias: c.name.clone(),
                col_type: c.col_type,
            })
            .collect();

        Ok(GeneratedSql {
            sql,
            columns,
            ctes: Vec::new(),
        })
    }

    fn render_plan(&self, table: &str, plan: &Plan, where_sql: &[String]) -> String {
        let cols: Vec<String> = plan
            .columns
            .iter()
            .map(|c| format!("{} AS \"{}\"", c.expr, c.name.replace('"', "\"\"")))
            .collect();
        let mut sql = format!(
            "SELECT {} FROM {} {}",
            cols.join(", "),
            table,
            self.table_pattern
        );
        for j in &plan.joins {
            sql.push(' ');
            sql.push_str(j);
        }
        let mut conds = Vec::new();
        if let Some(f) = &self.row_filter {
            conds.push(f.replace("{base}", &self.table_pattern));
        }
        for w in where_sql {
            conds.push(format!("({w})"));
        }
        if !conds.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conds.join(" AND "));
        }
        sql
    }
}

/// A column produced by a plan, with the SQL expression that yields it.
#[derive(Debug, Clone)]
struct PlanColumn {
    name: String,
    expr: String,
    col_type: ColumnType,
}

/// One UNION ALL branch: a chain of lateral joins plus its output columns.
#[derive(Debug, Clone)]
struct Plan {
    joins: Vec<String>,
    columns: Vec<PlanColumn>,
}

impl Plan {
    fn empty() -> Self {
        Self {
            joins: Vec::new(),
            columns: Vec::new(),
        }
    }

    /// Cross join two plans: the child's joins already include this plan's joins
    /// as a prefix, so its join list wins; columns concatenate.
    fn cross(&self, child: &Plan) -> Plan {
        let mut columns = self.columns.clone();
        columns.extend(child.columns.iter().cloned());
        Plan {
            joins: child.joins.clone(),
            columns,
        }
    }
}

/// The AST→SQL lowering context for a single `generate` call.
struct Lower {
    resource_type: String,
    /// Constant name → FHIRPath literal text, pre-rendered for substitution.
    constants: HashMap<String, String>,
    seq: Cell<usize>,
}

impl Lower {
    fn fresh(&self) -> usize {
        let v = self.seq.get();
        self.seq.set(v + 1);
        v
    }

    /// Build the UNION ALL branches for a select node, given the join prefix and
    /// the current focus item (`ctx`, a JSONB scalar SQL expression).
    fn build_select(
        &self,
        select: &SelectColumn,
        prefix: &[String],
        ctx: &str,
    ) -> Result<Vec<Plan>, Error> {
        // forEach / forEachOrNull establish a new focus for this level.
        let (joins, ctx2): (Vec<String>, String) = if let Some(p) = &select.for_each {
            self.for_each_join(p, prefix, ctx, false)?
        } else if let Some(p) = &select.for_each_or_null {
            self.for_each_join(p, prefix, ctx, true)?
        } else {
            (prefix.to_vec(), ctx.to_string())
        };

        // This node's own columns, evaluated against the (possibly new) focus.
        let mut own = Vec::new();
        if let Some(cols) = &select.column {
            for col in cols {
                own.push(self.lower_column(col, &ctx2)?);
            }
        }

        let mut plans = vec![Plan {
            joins,
            columns: own,
        }];

        // Nested selects cross-join with the accumulated plans.
        for child in &select.select {
            let mut next = Vec::new();
            for p in &plans {
                for cp in self.build_select(child, &p.joins, &ctx2)? {
                    next.push(p.cross(&cp));
                }
            }
            plans = next;
        }

        // unionAll branches are row alternatives, each cross-joined with the
        // sibling columns/selects of this level.
        if let Some(branches) = &select.union_all {
            let mut unioned = Vec::new();
            for p in &plans {
                for branch in branches {
                    for bp in self.build_select(branch, &p.joins, &ctx2)? {
                        unioned.push(p.cross(&bp));
                    }
                }
            }
            plans = unioned;
        }

        Ok(plans)
    }

    fn for_each_join(
        &self,
        path: &str,
        prefix: &[String],
        ctx: &str,
        or_null: bool,
    ) -> Result<(Vec<String>, String), Error> {
        let path = self.substitute(path)?;
        let ast = parse_ast(&path).map_err(|e| Error::FhirPath(e.to_string()))?;
        let coll = self.coll(&ast, ctx)?;
        let alias = format!("fe{}", self.fresh());
        let join = if or_null {
            format!("LEFT JOIN LATERAL jsonb_array_elements({coll}) AS {alias}(value) ON true")
        } else {
            format!("CROSS JOIN LATERAL jsonb_array_elements({coll}) AS {alias}(value)")
        };
        let mut joins = prefix.to_vec();
        joins.push(join);
        Ok((joins, format!("{alias}.value")))
    }

    fn lower_column(&self, col: &Column, ctx: &str) -> Result<PlanColumn, Error> {
        let path = self.substitute(&col.path)?;
        let ast = parse_ast(&path).map_err(|e| Error::FhirPath(e.to_string()))?;
        let coll = self.coll(&ast, ctx)?;
        let collection = col.collection.unwrap_or(false);
        let ty = col
            .col_type
            .as_deref()
            .map(ColumnType::from_fhir_type)
            .unwrap_or(ColumnType::String);

        let (expr, col_type) = if collection {
            (coll, ColumnType::Json)
        } else {
            (self.scalar_col(&coll, ty), ty)
        };
        Ok(PlanColumn {
            name: col.name.clone(),
            expr,
            col_type,
        })
    }

    /// Extract a single value from a collection, applying the column's type cast.
    /// A collection of more than one element raises an error (the spec's
    /// "expected a single value" case for non-collection columns).
    fn scalar_col(&self, coll: &str, ty: ColumnType) -> String {
        let inner = match ty {
            ColumnType::Integer => "(_e #>> '{}')::bigint",
            ColumnType::Decimal => "(_e #>> '{}')::numeric",
            ColumnType::Boolean => "(_e #>> '{}')::boolean",
            ColumnType::Json => "_e",
            _ => "_e #>> '{}'",
        };
        let n = self.fresh();
        format!("(SELECT {inner} FROM jsonb_array_elements({coll}) AS _c{n}(_e))")
    }

    // --- Collection lowering: every node evaluates to a JSONB array. ---

    fn coll(&self, node: &ExpressionNode, ctx: &str) -> Result<String, Error> {
        match node {
            ExpressionNode::Literal(l) => Ok(format!(
                "jsonb_build_array({})",
                self.literal_jsonb(&l.value)?
            )),
            ExpressionNode::Identifier(n) => {
                if n.name == self.resource_type {
                    Ok(format!("jsonb_build_array({ctx})"))
                } else {
                    Ok(self.nav(&format!("jsonb_build_array({ctx})"), &n.name))
                }
            }
            ExpressionNode::Variable(v) => {
                if v.name == "this" || v.name == "$this" {
                    Ok(format!("jsonb_build_array({ctx})"))
                } else {
                    Err(Error::InvalidPath(format!(
                        "unsupported variable ${}",
                        v.name
                    )))
                }
            }
            ExpressionNode::PropertyAccess(p) => {
                let base = self.coll(&p.object, ctx)?;
                Ok(self.nav(&base, &p.property))
            }
            ExpressionNode::IndexAccess(i) => {
                let base = self.coll(&i.object, ctx)?;
                let idx = self.int_literal(&i.index)?;
                Ok(self.index(&base, idx))
            }
            ExpressionNode::MethodCall(m) => self.method(&m.object, &m.method, &m.arguments, ctx),
            ExpressionNode::FunctionCall(f) => self.func_root(&f.name, &f.arguments, ctx),
            ExpressionNode::Filter(fl) => {
                let base = self.coll(&fl.base, ctx)?;
                self.where_fn(&base, &fl.condition)
            }
            ExpressionNode::Union(u) => {
                let l = self.coll(&u.left, ctx)?;
                let r = self.coll(&u.right, ctx)?;
                let n = self.fresh();
                Ok(format!(
                    "(SELECT coalesce(jsonb_agg(_v),'[]'::jsonb) FROM (SELECT _v FROM jsonb_array_elements({l}) AS _ua{n}(_v) UNION ALL SELECT _v FROM jsonb_array_elements({r}) AS _ub{n}(_v)) AS _uu{n})"
                ))
            }
            ExpressionNode::Parenthesized(e) => self.coll(e, ctx),
            ExpressionNode::TypeCast(c) => self.coll(&c.expression, ctx),
            ExpressionNode::BinaryOperation(_) | ExpressionNode::UnaryOperation(_) => Ok(format!(
                "jsonb_build_array({})",
                self.value_jsonb(node, ctx)?
            )),
            ExpressionNode::Collection(c) => {
                let mut parts = Vec::new();
                for e in &c.elements {
                    parts.push(self.coll(e, ctx)?);
                }
                if parts.is_empty() {
                    return Ok("'[]'::jsonb".to_string());
                }
                let n = self.fresh();
                let froms: Vec<String> = parts
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        format!("SELECT _v FROM jsonb_array_elements({p}) AS _cl{n}_{i}(_v)")
                    })
                    .collect();
                Ok(format!(
                    "(SELECT coalesce(jsonb_agg(_v),'[]'::jsonb) FROM ({}) AS _clu{n})",
                    froms.join(" UNION ALL ")
                ))
            }
            other => Err(Error::InvalidPath(format!(
                "unsupported FHIRPath expression: {}",
                other.node_type()
            ))),
        }
    }

    /// JSONB navigation that flattens collections: for every item in `coll`,
    /// take `item->prop`, unwrapping arrays into the result.
    fn nav(&self, coll: &str, prop: &str) -> String {
        let p = prop.replace('\'', "''");
        format!(
            "(SELECT coalesce(jsonb_agg(_v),'[]'::jsonb) \
             FROM jsonb_array_elements({coll}) AS _i(_item) \
             CROSS JOIN LATERAL jsonb_array_elements(\
               CASE \
                 WHEN jsonb_typeof(_item -> '{p}') = 'array' THEN _item -> '{p}' \
                 WHEN _item -> '{p}' IS NULL THEN '[]'::jsonb \
                 WHEN jsonb_typeof(_item -> '{p}') = 'null' THEN '[]'::jsonb \
                 ELSE jsonb_build_array(_item -> '{p}') \
               END) AS _j(_v))"
        )
    }

    fn index(&self, coll: &str, n: i64) -> String {
        format!(
            "(CASE WHEN ({coll}) -> {n} IS NULL THEN '[]'::jsonb ELSE jsonb_build_array(({coll}) -> {n}) END)"
        )
    }

    /// A function applied to the current context (`name(...)` with no object).
    fn func_root(&self, name: &str, args: &[ExpressionNode], ctx: &str) -> Result<String, Error> {
        let seed = format!("jsonb_build_array({ctx})");
        self.apply_on_coll(name, &seed, args, ctx)
    }

    /// A method call `object.name(args)`.
    fn method(
        &self,
        object: &ExpressionNode,
        name: &str,
        args: &[ExpressionNode],
        ctx: &str,
    ) -> Result<String, Error> {
        match name {
            "ofType" => self.of_type(object, args.first(), ctx),
            "not" => Ok(format!(
                "jsonb_build_array(to_jsonb(NOT ({})))",
                self.bool(object, ctx)?
            )),
            "lowBoundary" | "highBoundary" => {
                let hint = boundary_hint(object);
                let oc = self.coll(object, ctx)?;
                Ok(self.boundary(&oc, name == "lowBoundary", hint))
            }
            _ => {
                let oc = self.coll(object, ctx)?;
                self.apply_on_coll(name, &oc, args, ctx)
            }
        }
    }

    /// Functions whose result depends only on the object collection.
    fn apply_on_coll(
        &self,
        name: &str,
        coll: &str,
        args: &[ExpressionNode],
        _ctx: &str,
    ) -> Result<String, Error> {
        match name {
            "first" | "single" => Ok(self.index(coll, 0)),
            "last" => Ok(self.index(coll, -1)),
            "where" => {
                let cond = args
                    .first()
                    .ok_or_else(|| Error::InvalidPath("where() requires an argument".into()))?;
                self.where_fn(coll, cond)
            }
            "exists" => {
                let base = match args.first() {
                    Some(cond) => self.where_fn(coll, cond)?,
                    None => coll.to_string(),
                };
                Ok(format!(
                    "jsonb_build_array(to_jsonb(jsonb_array_length({base}) > 0))"
                ))
            }
            "empty" => Ok(format!(
                "jsonb_build_array(to_jsonb(jsonb_array_length({coll}) = 0))"
            )),
            "count" => Ok(format!(
                "jsonb_build_array(to_jsonb(jsonb_array_length({coll})))"
            )),
            "join" => self.join_fn(coll, args),
            "extension" => {
                let url = self.string_text(args.first())?;
                self.extension_fn(coll, &url)
            }
            "getReferenceKey" => self.reference_key(coll, args.first()),
            "getResourceKey" => self.resource_key(coll),
            "lowBoundary" => Ok(self.boundary(coll, true, BoundaryType::Unknown)),
            "highBoundary" => Ok(self.boundary(coll, false, BoundaryType::Unknown)),
            "toString" => Ok(coll.to_string()),
            other => Err(Error::InvalidPath(format!(
                "unsupported function {other}()"
            ))),
        }
    }

    /// `object.where(cond)` — keep the items for which `cond` is true.
    fn where_fn(&self, coll: &str, cond: &ExpressionNode) -> Result<String, Error> {
        let n = self.fresh();
        let item = format!("_w{n}._it");
        let pred = self.bool(cond, &item)?;
        Ok(format!(
            "(SELECT coalesce(jsonb_agg(_w{n}._it),'[]'::jsonb) FROM jsonb_array_elements({coll}) AS _w{n}(_it) WHERE {pred})"
        ))
    }

    fn join_fn(&self, coll: &str, args: &[ExpressionNode]) -> Result<String, Error> {
        let sep = match args.first() {
            Some(a) => self.string_text(Some(a))?,
            None => String::new(),
        };
        let n = self.fresh();
        // join() over an empty collection yields the empty collection (null),
        // not an empty string. (FHIR/sql-on-fhir.js fn_join)
        Ok(format!(
            "(SELECT CASE WHEN count(*) = 0 THEN '[]'::jsonb \
             ELSE jsonb_build_array(to_jsonb(string_agg(_e #>> '{{}}', '{sep}'))) END \
             FROM jsonb_array_elements({coll}) AS _jn{n}(_e))"
        ))
    }

    fn extension_fn(&self, coll: &str, url: &str) -> Result<String, Error> {
        let nav_ext = self.nav(coll, "extension");
        let n = self.fresh();
        let url = url.replace('\'', "''");
        Ok(format!(
            "(SELECT coalesce(jsonb_agg(_x{n}._e),'[]'::jsonb) FROM jsonb_array_elements({nav_ext}) AS _x{n}(_e) WHERE _x{n}._e ->> 'url' = '{url}')"
        ))
    }

    fn reference_key(
        &self,
        coll: &str,
        type_arg: Option<&ExpressionNode>,
    ) -> Result<String, Error> {
        let n = self.fresh();
        let guard = match type_arg {
            Some(t) => {
                let ty = self.type_name(t)?.replace('\'', "''");
                format!(" AND split_part(_r{n}._e ->> 'reference', '/', 1) = '{ty}'")
            }
            None => String::new(),
        };
        Ok(format!(
            "(SELECT coalesce(jsonb_agg(to_jsonb(split_part(_r{n}._e ->> 'reference', '/', 2))),'[]'::jsonb) FROM jsonb_array_elements({coll}) AS _r{n}(_e) WHERE _r{n}._e ->> 'reference' IS NOT NULL{guard})"
        ))
    }

    fn resource_key(&self, coll: &str) -> Result<String, Error> {
        let n = self.fresh();
        Ok(format!(
            "(SELECT coalesce(jsonb_agg(_k{n}._e -> 'id'),'[]'::jsonb) FROM jsonb_array_elements({coll}) AS _k{n}(_e))"
        ))
    }

    /// FHIR precision boundary. Numbers use the decimal half-ulp boundary;
    /// date/dateTime/time strings are widened to their precision-filled bounds
    /// (`lowBoundary` to the earliest instant, `highBoundary` to the latest).
    fn boundary(&self, coll: &str, low: bool, hint: BoundaryType) -> String {
        let sign = if low { "-" } else { "+" };
        let n = self.fresh();
        let e = format!("_b{n}._e");
        let t = format!("({e} #>> '{{}}')");
        let numeric = format!(
            "(({t})::numeric {sign} (0.5 / power(10, coalesce(length(nullif(split_part({t}, '.', 2), '')), 0))::numeric))"
        );
        let string_bound = match hint {
            BoundaryType::Date => date_bound(&t, low),
            BoundaryType::DateTime => datetime_bound(&t, low),
            BoundaryType::Time => time_bound(&t, low),
            BoundaryType::Unknown => format!(
                "CASE WHEN position(':' in {t}) > 0 THEN {} ELSE {} END",
                time_bound(&t, low),
                date_bound(&t, low)
            ),
        };
        format!(
            "(SELECT coalesce(jsonb_agg(\
               CASE jsonb_typeof({e}) \
                 WHEN 'number' THEN to_jsonb({numeric}) \
                 WHEN 'string' THEN to_jsonb({string_bound}) \
                 ELSE {e} END),'[]'::jsonb) \
             FROM jsonb_array_elements({coll}) AS _b{n}(_e))"
        )
    }

    /// `object.ofType(T)` — FHIR choice elements are stored as `<base><Type>`.
    fn of_type(
        &self,
        object: &ExpressionNode,
        type_arg: Option<&ExpressionNode>,
        ctx: &str,
    ) -> Result<String, Error> {
        let arg = type_arg
            .ok_or_else(|| Error::InvalidPath("ofType() requires a type argument".into()))?;
        let tname = capitalize_first(&self.type_name(arg)?);
        match object {
            ExpressionNode::PropertyAccess(p) => {
                let base = self.coll(&p.object, ctx)?;
                Ok(self.nav(&base, &format!("{}{}", p.property, tname)))
            }
            ExpressionNode::Identifier(idn) if idn.name != self.resource_type => Ok(self.nav(
                &format!("jsonb_build_array({ctx})"),
                &format!("{}{}", idn.name, tname),
            )),
            ExpressionNode::Parenthesized(e) => self.of_type(e, type_arg, ctx),
            _ => self.coll(object, ctx),
        }
    }

    // --- Boolean lowering for `where` filters and comparisons. ---

    fn bool(&self, node: &ExpressionNode, item: &str) -> Result<String, Error> {
        match node {
            ExpressionNode::Parenthesized(e) => self.bool(e, item),
            ExpressionNode::UnaryOperation(u) if matches!(u.operator, UnaryOperator::Not) => {
                Ok(format!("(NOT {})", self.bool(&u.operand, item)?))
            }
            ExpressionNode::BinaryOperation(b) => self.bool_binop(b, item),
            ExpressionNode::Literal(l) => match &l.value {
                LiteralValue::Boolean(v) => Ok(v.to_string()),
                _ => Ok(format!(
                    "({} #>> '{{}}')::boolean",
                    self.scalar_jsonb(node, item)?
                )),
            },
            ExpressionNode::MethodCall(m) => match m.method.as_str() {
                "exists" | "empty" | "not" | "first" | "last" | "where" | "count" | "ofType"
                | "extension" | "getReferenceKey" | "getResourceKey" => Ok(format!(
                    "({} #>> '{{}}')::boolean",
                    self.scalar_jsonb(node, item)?
                )),
                _ => Ok(format!(
                    "({} #>> '{{}}')::boolean",
                    self.scalar_jsonb(node, item)?
                )),
            },
            _ => Ok(format!(
                "({} #>> '{{}}')::boolean",
                self.scalar_jsonb(node, item)?
            )),
        }
    }

    fn bool_binop(
        &self,
        b: &octofhir_fhirpath::ast::BinaryOperationNode,
        item: &str,
    ) -> Result<String, Error> {
        use BinaryOperator::*;
        let logical = |op: &str, l: &str, r: &str| format!("({l} {op} {r})");
        match b.operator {
            And => Ok(logical(
                "AND",
                &self.bool(&b.left, item)?,
                &self.bool(&b.right, item)?,
            )),
            Or => Ok(logical(
                "OR",
                &self.bool(&b.left, item)?,
                &self.bool(&b.right, item)?,
            )),
            Xor => Ok(logical(
                "<>",
                &self.bool(&b.left, item)?,
                &self.bool(&b.right, item)?,
            )),
            Implies => Ok(format!(
                "((NOT {}) OR {})",
                self.bool(&b.left, item)?,
                self.bool(&b.right, item)?
            )),
            Equal | Equivalent => Ok(format!(
                "({} = {})",
                self.scalar_jsonb(&b.left, item)?,
                self.scalar_jsonb(&b.right, item)?
            )),
            NotEqual | NotEquivalent => Ok(format!(
                "({} <> {})",
                self.scalar_jsonb(&b.left, item)?,
                self.scalar_jsonb(&b.right, item)?
            )),
            LessThan => self.cmp("<", b, item),
            LessThanOrEqual => self.cmp("<=", b, item),
            GreaterThan => self.cmp(">", b, item),
            GreaterThanOrEqual => self.cmp(">=", b, item),
            other => Err(Error::InvalidPath(format!(
                "unsupported boolean operator {other:?}"
            ))),
        }
    }

    fn cmp(
        &self,
        op: &str,
        b: &octofhir_fhirpath::ast::BinaryOperationNode,
        item: &str,
    ) -> Result<String, Error> {
        Ok(format!(
            "({} {op} {})",
            self.scalar_jsonb(&b.left, item)?,
            self.scalar_jsonb(&b.right, item)?
        ))
    }

    /// A single JSONB value (the FHIRPath singleton) for use in comparisons.
    fn scalar_jsonb(&self, node: &ExpressionNode, item: &str) -> Result<String, Error> {
        if let ExpressionNode::Literal(l) = node {
            return self.literal_jsonb(&l.value);
        }
        let coll = self.coll(node, item)?;
        let n = self.fresh();
        Ok(format!(
            "(SELECT _s FROM jsonb_array_elements({coll}) AS _sq{n}(_s))"
        ))
    }

    /// A JSONB value for a binary/unary expression used as a column value.
    fn value_jsonb(&self, node: &ExpressionNode, item: &str) -> Result<String, Error> {
        match node {
            ExpressionNode::BinaryOperation(b) => {
                use BinaryOperator::*;
                let arith = |g: &Self, op: &str| -> Result<String, Error> {
                    Ok(format!(
                        "to_jsonb(({} #>> '{{}}')::numeric {op} ({} #>> '{{}}')::numeric)",
                        g.scalar_jsonb(&b.left, item)?,
                        g.scalar_jsonb(&b.right, item)?
                    ))
                };
                match b.operator {
                    Add => arith(self, "+"),
                    Subtract => arith(self, "-"),
                    Multiply => arith(self, "*"),
                    Divide => arith(self, "/"),
                    Modulo => arith(self, "%"),
                    IntegerDivide => Ok(format!(
                        "to_jsonb(div(({} #>> '{{}}')::numeric, ({} #>> '{{}}')::numeric))",
                        self.scalar_jsonb(&b.left, item)?,
                        self.scalar_jsonb(&b.right, item)?
                    )),
                    Concatenate => Ok(format!(
                        "to_jsonb(({} #>> '{{}}') || ({} #>> '{{}}'))",
                        self.scalar_jsonb(&b.left, item)?,
                        self.scalar_jsonb(&b.right, item)?
                    )),
                    _ => Ok(format!("to_jsonb({})", self.bool(node, item)?)),
                }
            }
            ExpressionNode::UnaryOperation(u) => match u.operator {
                UnaryOperator::Not => Ok(format!("to_jsonb({})", self.bool(node, item)?)),
                UnaryOperator::Negate => Ok(format!(
                    "to_jsonb(- ({} #>> '{{}}')::numeric)",
                    self.scalar_jsonb(&u.operand, item)?
                )),
                UnaryOperator::Positive => Ok(format!(
                    "to_jsonb(({} #>> '{{}}')::numeric)",
                    self.scalar_jsonb(&u.operand, item)?
                )),
            },
            _ => Ok(format!("to_jsonb({})", self.bool(node, item)?)),
        }
    }

    fn literal_jsonb(&self, v: &LiteralValue) -> Result<String, Error> {
        Ok(match v {
            LiteralValue::String(s) => format!("to_jsonb('{}'::text)", s.replace('\'', "''")),
            LiteralValue::Integer(i) | LiteralValue::Long(i) => {
                format!("to_jsonb({i}::bigint)")
            }
            LiteralValue::Decimal(d) => format!("to_jsonb({d}::numeric)"),
            LiteralValue::Boolean(b) => format!("to_jsonb({b})"),
            LiteralValue::Date(_) | LiteralValue::DateTime(_) | LiteralValue::Time(_) => {
                let s = v.to_string();
                let s = s.trim_start_matches('@');
                format!("to_jsonb('{}'::text)", s.replace('\'', "''"))
            }
            LiteralValue::Quantity { value, .. } => format!("to_jsonb({value}::numeric)"),
        })
    }

    fn int_literal(&self, node: &ExpressionNode) -> Result<i64, Error> {
        match node {
            ExpressionNode::Literal(l) => match &l.value {
                LiteralValue::Integer(i) | LiteralValue::Long(i) => Ok(*i),
                other => Err(Error::InvalidPath(format!(
                    "expected integer index, got {other}"
                ))),
            },
            ExpressionNode::Parenthesized(e) => self.int_literal(e),
            other => Err(Error::InvalidPath(format!(
                "index must be an integer literal, got {}",
                other.node_type()
            ))),
        }
    }

    fn type_name(&self, node: &ExpressionNode) -> Result<String, Error> {
        match node {
            ExpressionNode::Identifier(n) => Ok(n.name.clone()),
            ExpressionNode::TypeInfo(t) => Ok(t.name.clone()),
            other => Err(Error::InvalidPath(format!(
                "expected a type name, got {}",
                other.node_type()
            ))),
        }
    }

    fn string_text(&self, node: Option<&ExpressionNode>) -> Result<String, Error> {
        match node {
            Some(ExpressionNode::Literal(l)) => match &l.value {
                LiteralValue::String(s) => Ok(s.clone()),
                other => Ok(other.to_string()),
            },
            Some(other) => Err(Error::InvalidPath(format!(
                "expected a string literal, got {}",
                other.node_type()
            ))),
            None => Ok(String::new()),
        }
    }

    /// Replace `%name` constant references with their FHIRPath literal text.
    /// Errors on a reference to an undefined constant.
    fn substitute(&self, path: &str) -> Result<String, Error> {
        substitute_constants(path, &self.constants)
    }
}

/// Replace `%name` constant references with their FHIRPath literal text.
/// `%rowIndex` is preserved for the evaluator to resolve. Errors on a reference
/// to an undefined constant.
pub(crate) fn substitute_constants(
    path: &str,
    constants: &HashMap<String, String>,
) -> Result<String, Error> {
    let mut out = String::with_capacity(path.len());
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let mut name = String::new();
        while let Some(&nc) = chars.peek() {
            if nc.is_ascii_alphanumeric() || nc == '_' {
                name.push(nc);
                chars.next();
            } else {
                break;
            }
        }
        if name.is_empty() {
            out.push('%');
            continue;
        }
        if name == "rowIndex" {
            out.push('%');
            out.push_str(&name);
            continue;
        }
        match constants.get(&name) {
            Some(lit) => out.push_str(lit),
            None => {
                return Err(Error::InvalidViewDefinition(format!(
                    "undefined constant %{name}"
                )));
            }
        }
    }
    Ok(out)
}

/// Render each constant as a FHIRPath literal for substitution into selectors.
pub(crate) fn build_constants(view: &ViewDefinition) -> Result<HashMap<String, String>, Error> {
    let mut map = HashMap::new();
    for c in &view.constant {
        map.insert(c.name.clone(), constant_literal(c)?);
    }
    Ok(map)
}

fn constant_literal(c: &Constant) -> Result<String, Error> {
    if let Some(s) = &c.value_string {
        return Ok(fhirpath_string(s));
    }
    if let Some(i) = c.value_integer {
        return Ok(i.to_string());
    }
    if let Some(b) = c.value_boolean {
        return Ok(b.to_string());
    }
    if let Some(d) = c.value_decimal {
        return Ok(d.to_string());
    }
    // Polymorphic value[x] captured via flatten.
    for (k, v) in &c.values {
        if !k.starts_with("value") {
            continue;
        }
        return match v {
            Value::String(s) => Ok(fhirpath_string(s)),
            Value::Bool(b) => Ok(b.to_string()),
            Value::Number(n) => Ok(n.to_string()),
            _ => Err(Error::InvalidViewDefinition(format!(
                "unsupported constant value type for {}",
                c.name
            ))),
        };
    }
    Err(Error::InvalidViewDefinition(format!(
        "constant {} has no value",
        c.name
    )))
}

/// A FHIRPath single-quoted string literal (backslash-escaped).
fn fhirpath_string(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The FHIR temporal type a boundary function operates on, inferred from a
/// leading `ofType(T)` where present.
#[derive(Debug, Clone, Copy)]
enum BoundaryType {
    Date,
    DateTime,
    Time,
    Unknown,
}

fn boundary_hint(object: &ExpressionNode) -> BoundaryType {
    match object {
        ExpressionNode::MethodCall(m) if m.method == "ofType" => {
            let name = match m.arguments.first() {
                Some(ExpressionNode::Identifier(n)) => n.name.to_lowercase(),
                Some(ExpressionNode::TypeInfo(t)) => t.name.to_lowercase(),
                _ => return BoundaryType::Unknown,
            };
            match name.as_str() {
                "datetime" | "instant" => BoundaryType::DateTime,
                "date" => BoundaryType::Date,
                "time" => BoundaryType::Time,
                _ => BoundaryType::Unknown,
            }
        }
        ExpressionNode::Parenthesized(e) => boundary_hint(e),
        _ => BoundaryType::Unknown,
    }
}

/// Widen a partial date string `t` to its low/high full-date bound.
fn date_bound(t: &str, low: bool) -> String {
    if low {
        format!(
            "CASE length({t}) WHEN 4 THEN {t} || '-01-01' WHEN 7 THEN {t} || '-01' ELSE {t} END"
        )
    } else {
        format!(
            "CASE length({t}) \
               WHEN 4 THEN {t} || '-12-31' \
               WHEN 7 THEN to_char(to_date({t}, 'YYYY-MM') + interval '1 month' - interval '1 day', 'YYYY-MM-DD') \
               ELSE {t} END"
        )
    }
}

/// Widen a partial dateTime string `t`, using the timezone extremes (+14:00 for
/// the earliest instant, -12:00 for the latest) the spec mandates.
fn datetime_bound(t: &str, low: bool) -> String {
    if low {
        format!(
            "CASE length({t}) \
               WHEN 4 THEN {t} || '-01-01T00:00:00.000+14:00' \
               WHEN 7 THEN {t} || '-01T00:00:00.000+14:00' \
               WHEN 10 THEN {t} || 'T00:00:00.000+14:00' \
               ELSE {t} END"
        )
    } else {
        format!(
            "CASE length({t}) \
               WHEN 4 THEN {t} || '-12-31T23:59:59.999-12:00' \
               WHEN 7 THEN to_char(to_date({t}, 'YYYY-MM') + interval '1 month' - interval '1 day', 'YYYY-MM-DD') || 'T23:59:59.999-12:00' \
               WHEN 10 THEN {t} || 'T23:59:59.999-12:00' \
               ELSE {t} END"
        )
    }
}

/// Widen a partial time string `t` to its low/high bound.
fn time_bound(t: &str, low: bool) -> String {
    if low {
        format!(
            "CASE length({t}) WHEN 2 THEN {t} || ':00:00.000' WHEN 5 THEN {t} || ':00.000' WHEN 8 THEN {t} || '.000' ELSE {t} END"
        )
    } else {
        format!(
            "CASE length({t}) WHEN 2 THEN {t} || ':59:59.999' WHEN 5 THEN {t} || ':59.999' WHEN 8 THEN {t} || '.999' ELSE {t} END"
        )
    }
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Generated SQL with column metadata.
#[derive(Debug, Clone)]
pub struct GeneratedSql {
    /// The generated SQL query.
    pub sql: String,

    /// Column information for the result set.
    pub columns: Vec<GeneratedColumn>,

    /// Common Table Expressions (CTEs) to prepend to the query.
    pub ctes: Vec<String>,
}

/// A generated column with its SQL expression and metadata.
#[derive(Debug, Clone)]
pub struct GeneratedColumn {
    /// Original column name from the ViewDefinition.
    pub name: String,

    /// SQL expression that produces this column's value.
    pub expression: String,

    /// Alias used in the SQL SELECT clause.
    pub alias: String,

    /// Data type of the column.
    pub col_type: ColumnType,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn build_sql(view: serde_json::Value) -> GeneratedSql {
        let v = ViewDefinition::from_json(&view).unwrap();
        SqlGenerator::new().generate(&v).unwrap()
    }

    #[test]
    fn simple_columns() {
        let g = build_sql(json!({
            "resource": "Patient",
            "select": [{ "column": [
                { "name": "id", "path": "id", "type": "id" },
                { "name": "gender", "path": "gender", "type": "code" }
            ] }]
        }));
        assert!(g.sql.contains("FROM patient base"));
        assert_eq!(g.columns.len(), 2);
        assert_eq!(g.columns[0].name, "id");
    }

    #[test]
    fn collection_column_is_json() {
        let g = build_sql(json!({
            "resource": "Patient",
            "select": [{ "column": [
                { "name": "fam", "path": "name.family", "type": "string", "collection": true }
            ] }]
        }));
        assert_eq!(g.columns[0].col_type, ColumnType::Json);
    }

    #[test]
    fn union_shape_mismatch_errors() {
        let v = ViewDefinition::from_json(&json!({
            "resource": "Patient",
            "select": [{ "unionAll": [
                { "column": [{ "name": "a", "path": "id" }, { "name": "b", "path": "id" }] },
                { "column": [{ "name": "a", "path": "id" }, { "name": "c", "path": "id" }] }
            ] }]
        }))
        .unwrap();
        assert!(SqlGenerator::new().generate(&v).is_err());
    }

    #[test]
    fn undefined_constant_errors() {
        let v = ViewDefinition::from_json(&json!({
            "resource": "Patient",
            "select": [{ "forEach": "name.where(use = %missing)",
                "column": [{ "name": "f", "path": "family" }] }]
        }))
        .unwrap();
        assert!(SqlGenerator::new().generate(&v).is_err());
    }

    #[test]
    fn missing_resource_errors() {
        let v = ViewDefinition::from_json(&json!({
            "resource": "",
            "select": [{ "column": [{ "name": "id", "path": "id" }] }]
        }))
        .unwrap();
        assert!(SqlGenerator::new().generate(&v).is_err());
    }
}
