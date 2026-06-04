//! In-memory ViewDefinition execution.
//!
//! Evaluates a [`ViewDefinition`] directly against FHIR resources held as
//! `serde_json::Value`, with no database. It parses selectors with the real
//! `octofhir-fhirpath` parser and walks the AST over JSON, mirroring the
//! collection semantics of the SQL generator (every sub-expression yields a
//! FHIRPath collection — a `Vec<Value>` — so a singleton and a one-element
//! collection behave identically and navigation flattens arrays). The same view
//! therefore produces the same rows whether run in Postgres or here.

use std::collections::HashMap;

use octofhir_fhirpath::{BinaryOperator, ExpressionNode, LiteralValue, UnaryOperator, parse_ast};
use serde_json::{Map, Value};

use crate::column::{ColumnInfo, ColumnType};
use crate::runner::ViewResult;
use crate::sql_generator::{build_constants, substitute_constants};
use crate::view_definition::{Column, SelectColumn, ViewDefinition};
use crate::{Error, Result};

/// Execute a ViewDefinition against in-memory FHIR resources.
///
/// Resources whose `resourceType` differs from the view's `resource` are
/// skipped. Returns the tabular result with column metadata and rows.
///
/// # Errors
///
/// Returns an error if a selector cannot be parsed, a constant is undefined, the
/// column shape is inconsistent (duplicate names, mismatched `unionAll`
/// branches), or a non-collection column yields more than one value.
pub fn execute(view: &ViewDefinition, resources: &[Value]) -> Result<ViewResult> {
    if view.resource.trim().is_empty() {
        return Err(Error::InvalidViewDefinition(
            "ViewDefinition is missing the required `resource`".to_string(),
        ));
    }
    let constants = build_constants(view)?;
    let ev = Evaluator {
        resource_type: view.resource.clone(),
        constants,
    };

    let shape = ev.shape(&view.select)?;
    if shape.is_empty() {
        return Err(Error::InvalidViewDefinition(
            "ViewDefinition produces no columns".to_string(),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for (name, _) in &shape {
        if !seen.insert(name.clone()) {
            return Err(Error::InvalidViewDefinition(format!(
                "column `{name}` is defined more than once"
            )));
        }
    }

    let where_asts = view
        .where_
        .iter()
        .map(|w| ev.parse(&w.path))
        .collect::<Result<Vec<_>>>()?;

    let mut rows: Vec<Map<String, Value>> = Vec::new();
    for resource in resources {
        if resource.get("resourceType").and_then(Value::as_str) != Some(ev.resource_type.as_str()) {
            continue;
        }
        let mut keep = true;
        for ast in &where_asts {
            if !ev.eval_where(ast, resource)? {
                keep = false;
                break;
            }
        }
        if !keep {
            continue;
        }

        let mut combos = vec![Map::new()];
        for select in &view.select {
            let srows = ev.eval_select(select, resource, 0)?;
            combos = cartesian(&combos, &srows);
        }
        rows.extend(combos);
    }

    let columns: Vec<ColumnInfo> = shape
        .iter()
        .map(|(name, ty)| ColumnInfo::new(name.clone(), *ty))
        .collect();
    let data: Vec<Vec<Value>> = rows
        .iter()
        .map(|row| {
            shape
                .iter()
                .map(|(name, _)| row.get(name).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .collect();
    let row_count = data.len();
    Ok(ViewResult {
        columns,
        data,
        row_count,
    })
}

/// The cartesian product of two row sets, merging each pair of maps.
fn cartesian(a: &[Map<String, Value>], b: &[Map<String, Value>]) -> Vec<Map<String, Value>> {
    let mut out = Vec::with_capacity(a.len() * b.len());
    for x in a {
        for y in b {
            let mut merged = x.clone();
            for (k, v) in y {
                merged.insert(k.clone(), v.clone());
            }
            out.push(merged);
        }
    }
    out
}

struct Evaluator {
    resource_type: String,
    constants: HashMap<String, String>,
}

impl Evaluator {
    fn parse(&self, path: &str) -> Result<ExpressionNode> {
        let substituted = substitute_constants(path, &self.constants)?;
        parse_ast(&substituted).map_err(|e| Error::FhirPath(e.to_string()))
    }

    /// The ordered column shape of a select list, validating `unionAll` branch
    /// consistency.
    fn shape(&self, selects: &[SelectColumn]) -> Result<Vec<(String, ColumnType)>> {
        let mut cols = Vec::new();
        for select in selects {
            cols.extend(self.shape_of(select)?);
        }
        Ok(cols)
    }

    fn shape_of(&self, select: &SelectColumn) -> Result<Vec<(String, ColumnType)>> {
        let mut cols = Vec::new();
        if let Some(columns) = &select.column {
            for col in columns {
                cols.push((col.name.clone(), column_type(col)));
            }
        }
        for nested in &select.select {
            cols.extend(self.shape_of(nested)?);
        }
        if let Some(branches) = &select.union_all {
            let mut shapes = branches
                .iter()
                .map(|b| self.shape_of(b))
                .collect::<Result<Vec<_>>>()?;
            if let Some(first) = shapes.first() {
                let first_names: Vec<&str> = first.iter().map(|(n, _)| n.as_str()).collect();
                for other in &shapes[1..] {
                    let names: Vec<&str> = other.iter().map(|(n, _)| n.as_str()).collect();
                    if names != first_names {
                        return Err(Error::InvalidViewDefinition(
                            "unionAll branches have mismatched column shape".to_string(),
                        ));
                    }
                }
                cols.extend(shapes.swap_remove(0));
            }
        }
        Ok(cols)
    }

    /// Evaluate a select node against a focus value, yielding partial rows.
    fn eval_select(
        &self,
        select: &SelectColumn,
        ctx: &Value,
        rid: i64,
    ) -> Result<Vec<Map<String, Value>>> {
        let for_each = select.for_each.as_deref();
        let for_each_or_null = select.for_each_or_null.as_deref();

        // `repeat` recursively traverses the focus, collecting every node reached
        // by transitively re-applying the repeat path(s) (preorder, the focus
        // node itself excluded). %rowIndex tracks position in the flattened list.
        if !select.repeat.is_empty() {
            let asts = select
                .repeat
                .iter()
                .map(|p| self.parse(p))
                .collect::<Result<Vec<_>>>()?;
            let mut foci = Vec::new();
            self.repeat_collect(&asts, ctx, &mut foci)?;
            let mut rows = Vec::new();
            for (idx, focus) in foci.iter().enumerate() {
                rows.extend(self.eval_level(select, focus, idx as i64)?);
            }
            return Ok(rows);
        }

        if let Some(path) = for_each.or(for_each_or_null) {
            let ast = self.parse(path)?;
            let elements = self.eval_coll(&ast, ctx, rid)?;
            if elements.is_empty() {
                if for_each_or_null.is_some() {
                    return Ok(vec![self.null_row(select)]);
                }
                return Ok(Vec::new());
            }
            let mut rows = Vec::new();
            for (idx, elem) in elements.iter().enumerate() {
                rows.extend(self.eval_level(select, elem, idx as i64)?);
            }
            Ok(rows)
        } else {
            self.eval_level(select, ctx, rid)
        }
    }

    /// Evaluate this select's columns, nested selects and unionAll against an
    /// already-focused value (after any forEach has selected the element).
    fn eval_level(
        &self,
        select: &SelectColumn,
        ctx: &Value,
        rid: i64,
    ) -> Result<Vec<Map<String, Value>>> {
        let mut own = Map::new();
        if let Some(columns) = &select.column {
            for col in columns {
                own.insert(col.name.clone(), self.column_value(col, ctx, rid)?);
            }
        }
        let mut combos = vec![own];

        for nested in &select.select {
            let nrows = self.eval_select(nested, ctx, rid)?;
            combos = cartesian(&combos, &nrows);
        }

        if let Some(branches) = &select.union_all {
            let mut branch_rows = Vec::new();
            for branch in branches {
                branch_rows.extend(self.eval_select(branch, ctx, rid)?);
            }
            combos = cartesian(&combos, &branch_rows);
        }

        Ok(combos)
    }

    /// Preorder transitive closure of the `repeat` path(s): apply every path to
    /// `node`, push each result, then recurse into it. The starting node itself
    /// is not emitted.
    fn repeat_collect(
        &self,
        paths: &[ExpressionNode],
        node: &Value,
        out: &mut Vec<Value>,
    ) -> Result<()> {
        for ast in paths {
            for child in self.eval_coll(ast, node, 0)? {
                out.push(child.clone());
                self.repeat_collect(paths, &child, out)?;
            }
        }
        Ok(())
    }

    /// An all-null row for an empty `forEachOrNull`. Per spec, columns whose path
    /// is `%rowIndex` are bound to 0 rather than null.
    fn null_row(&self, select: &SelectColumn) -> Map<String, Value> {
        let mut row = Map::new();
        self.null_fill(select, &mut row);
        row
    }

    fn null_fill(&self, select: &SelectColumn, row: &mut Map<String, Value>) {
        if let Some(columns) = &select.column {
            for col in columns {
                let v = if col.path.trim() == "%rowIndex" {
                    Value::Number(0.into())
                } else {
                    Value::Null
                };
                row.insert(col.name.clone(), v);
            }
        }
        for nested in &select.select {
            self.null_fill(nested, row);
        }
        if let Some(first) = select.union_all.as_ref().and_then(|b| b.first()) {
            self.null_fill(first, row);
        }
    }

    fn column_value(&self, col: &Column, ctx: &Value, rid: i64) -> Result<Value> {
        let ast = self.parse(&col.path)?;
        let values = self.eval_coll(&ast, ctx, rid)?;
        if col.collection.unwrap_or(false) {
            return Ok(Value::Array(values));
        }
        match values.len() {
            0 => Ok(Value::Null),
            1 => Ok(values.into_iter().next().unwrap()),
            _ => Err(Error::InvalidPath(format!(
                "column `{}` yields multiple values but is not a collection",
                col.name
            ))),
        }
    }

    // --- Collection evaluation: every node yields a Vec<Value>. ---

    fn eval_coll(&self, node: &ExpressionNode, ctx: &Value, rid: i64) -> Result<Vec<Value>> {
        match node {
            ExpressionNode::Literal(l) => Ok(vec![literal_value(&l.value)]),
            ExpressionNode::Identifier(n) => {
                if n.name == self.resource_type {
                    Ok(vec![ctx.clone()])
                } else {
                    Ok(nav(std::slice::from_ref(ctx), &n.name))
                }
            }
            ExpressionNode::Variable(v) => match v.name.as_str() {
                "this" | "$this" => Ok(vec![ctx.clone()]),
                "rowIndex" => Ok(vec![Value::Number(rid.into())]),
                other => Err(Error::InvalidPath(format!("unsupported variable ${other}"))),
            },
            ExpressionNode::PropertyAccess(p) => {
                let base = self.eval_coll(&p.object, ctx, rid)?;
                Ok(nav(&base, &p.property))
            }
            ExpressionNode::IndexAccess(i) => {
                let base = self.eval_coll(&i.object, ctx, rid)?;
                let idx = self.int_literal(&i.index)?;
                Ok(index(&base, idx))
            }
            ExpressionNode::MethodCall(m) => {
                self.method(&m.object, &m.method, &m.arguments, ctx, rid)
            }
            ExpressionNode::FunctionCall(f) => {
                self.apply(&f.name, vec![ctx.clone()], &f.arguments, ctx, rid)
            }
            ExpressionNode::Filter(fl) => {
                let base = self.eval_coll(&fl.base, ctx, rid)?;
                self.filter(base, &fl.condition, rid)
            }
            ExpressionNode::Union(u) => {
                let mut left = self.eval_coll(&u.left, ctx, rid)?;
                let right = self.eval_coll(&u.right, ctx, rid)?;
                left.extend(right);
                Ok(left)
            }
            ExpressionNode::Parenthesized(e) => self.eval_coll(e, ctx, rid),
            ExpressionNode::TypeCast(c) => self.eval_coll(&c.expression, ctx, rid),
            ExpressionNode::BinaryOperation(_) | ExpressionNode::UnaryOperation(_) => {
                match self.eval_value(node, ctx, rid)? {
                    Some(v) => Ok(vec![v]),
                    None => Ok(Vec::new()),
                }
            }
            ExpressionNode::Collection(c) => {
                let mut out = Vec::new();
                for e in &c.elements {
                    out.extend(self.eval_coll(e, ctx, rid)?);
                }
                Ok(out)
            }
            other => Err(Error::InvalidPath(format!(
                "unsupported FHIRPath expression: {}",
                other.node_type()
            ))),
        }
    }

    fn method(
        &self,
        object: &ExpressionNode,
        name: &str,
        args: &[ExpressionNode],
        ctx: &Value,
        rid: i64,
    ) -> Result<Vec<Value>> {
        match name {
            "ofType" => {
                let arg = args
                    .first()
                    .ok_or_else(|| Error::InvalidPath("ofType() requires a type".into()))?;
                self.of_type(object, arg, ctx, rid)
            }
            "not" => {
                let truthy = self.eval_bool(object, ctx, rid);
                Ok(match truthy {
                    Some(b) => vec![Value::Bool(!b)],
                    None => Vec::new(),
                })
            }
            "lowBoundary" | "highBoundary" => {
                let hint = boundary_hint(object);
                let base = self.eval_coll(object, ctx, rid)?;
                let low = name == "lowBoundary";
                Ok(base.iter().map(|v| boundary(v, low, hint)).collect())
            }
            _ => {
                let base = self.eval_coll(object, ctx, rid)?;
                self.apply(name, base, args, ctx, rid)
            }
        }
    }

    /// Functions whose result depends on the object collection.
    fn apply(
        &self,
        name: &str,
        coll: Vec<Value>,
        args: &[ExpressionNode],
        _ctx: &Value,
        rid: i64,
    ) -> Result<Vec<Value>> {
        match name {
            "first" | "single" => Ok(index(&coll, 0)),
            "last" => Ok(index(&coll, -1)),
            "where" => {
                let cond = args
                    .first()
                    .ok_or_else(|| Error::InvalidPath("where() requires an argument".into()))?;
                self.filter(coll, cond, rid)
            }
            "exists" => {
                let base = match args.first() {
                    Some(cond) => self.filter(coll, cond, rid)?,
                    None => coll,
                };
                Ok(vec![Value::Bool(!base.is_empty())])
            }
            "empty" => Ok(vec![Value::Bool(coll.is_empty())]),
            "count" => Ok(vec![Value::Number((coll.len() as i64).into())]),
            "join" => {
                // join() over an empty collection yields the empty collection
                // (i.e. null), not an empty string. (FHIR/sql-on-fhir.js fn_join)
                if coll.is_empty() {
                    return Ok(vec![]);
                }
                let sep = match args.first() {
                    Some(a) => self.string_arg(a)?,
                    None => String::new(),
                };
                let parts: Vec<String> = coll.iter().map(value_to_string).collect();
                Ok(vec![Value::String(parts.join(&sep))])
            }
            "extension" => {
                let url = self
                    .string_arg(args.first().ok_or_else(|| {
                        Error::InvalidPath("extension() requires a url".into())
                    })?)?;
                let exts = nav(&coll, "extension");
                Ok(exts
                    .into_iter()
                    .filter(|e| e.get("url").and_then(Value::as_str) == Some(url.as_str()))
                    .collect())
            }
            "getReferenceKey" => {
                let want_type = match args.first() {
                    Some(a) => Some(self.type_name(a)?),
                    None => None,
                };
                Ok(coll
                    .iter()
                    .filter_map(|item| reference_key(item, want_type.as_deref()))
                    .map(Value::String)
                    .collect())
            }
            "getResourceKey" => Ok(coll
                .iter()
                .filter_map(|item| item.get("id").cloned())
                .collect()),
            "lowBoundary" => Ok(coll
                .iter()
                .map(|v| boundary(v, true, BoundaryType::Unknown))
                .collect()),
            "highBoundary" => Ok(coll
                .iter()
                .map(|v| boundary(v, false, BoundaryType::Unknown))
                .collect()),
            "toString" => Ok(coll
                .iter()
                .map(|v| Value::String(value_to_string(v)))
                .collect()),
            other => Err(Error::InvalidPath(format!(
                "unsupported function {other}()"
            ))),
        }
    }

    /// Evaluate a top-level `where` filter. The expression must yield a boolean
    /// (or empty); a non-boolean result is an error per the spec.
    fn eval_where(&self, ast: &ExpressionNode, resource: &Value) -> Result<bool> {
        let coll = self.eval_coll(ast, resource, 0)?;
        if coll.is_empty() {
            return Ok(false);
        }
        if coll.iter().any(|v| !v.is_boolean()) {
            return Err(Error::InvalidViewDefinition(
                "where path does not evaluate to a boolean".to_string(),
            ));
        }
        Ok(coll.iter().any(|v| v == &Value::Bool(true)))
    }

    fn filter(&self, coll: Vec<Value>, cond: &ExpressionNode, rid: i64) -> Result<Vec<Value>> {
        Ok(coll
            .into_iter()
            .filter(|item| self.eval_bool(cond, item, rid) == Some(true))
            .collect())
    }

    fn of_type(
        &self,
        object: &ExpressionNode,
        type_arg: &ExpressionNode,
        ctx: &Value,
        rid: i64,
    ) -> Result<Vec<Value>> {
        let tname = capitalize_first(&self.type_name(type_arg)?);
        match object {
            ExpressionNode::PropertyAccess(p) => {
                let base = self.eval_coll(&p.object, ctx, rid)?;
                Ok(nav(&base, &format!("{}{}", p.property, tname)))
            }
            ExpressionNode::Identifier(idn) if idn.name != self.resource_type => Ok(nav(
                std::slice::from_ref(ctx),
                &format!("{}{}", idn.name, tname),
            )),
            ExpressionNode::Parenthesized(e) => self.of_type(e, type_arg, ctx, rid),
            _ => self.eval_coll(object, ctx, rid),
        }
    }

    // --- Boolean evaluation (three-valued: None = empty). ---

    fn eval_bool(&self, node: &ExpressionNode, ctx: &Value, rid: i64) -> Option<bool> {
        match node {
            ExpressionNode::Parenthesized(e) => self.eval_bool(e, ctx, rid),
            ExpressionNode::UnaryOperation(u) if matches!(u.operator, UnaryOperator::Not) => {
                self.eval_bool(&u.operand, ctx, rid).map(|b| !b)
            }
            ExpressionNode::BinaryOperation(b) => self.eval_binop(b, ctx, rid),
            ExpressionNode::Literal(l) => match &l.value {
                LiteralValue::Boolean(b) => Some(*b),
                _ => None,
            },
            _ => match self.scalar(node, ctx, rid) {
                Some(Value::Bool(b)) => Some(b),
                Some(Value::Null) | None => None,
                Some(_) => Some(true),
            },
        }
    }

    fn eval_binop(
        &self,
        b: &octofhir_fhirpath::ast::BinaryOperationNode,
        ctx: &Value,
        rid: i64,
    ) -> Option<bool> {
        use BinaryOperator::*;
        match b.operator {
            And => match (
                self.eval_bool(&b.left, ctx, rid),
                self.eval_bool(&b.right, ctx, rid),
            ) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            },
            Or => match (
                self.eval_bool(&b.left, ctx, rid),
                self.eval_bool(&b.right, ctx, rid),
            ) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            },
            Xor => match (
                self.eval_bool(&b.left, ctx, rid),
                self.eval_bool(&b.right, ctx, rid),
            ) {
                (Some(l), Some(r)) => Some(l != r),
                _ => None,
            },
            Implies => match (
                self.eval_bool(&b.left, ctx, rid),
                self.eval_bool(&b.right, ctx, rid),
            ) {
                (Some(false), _) => Some(true),
                (_, Some(true)) => Some(true),
                (Some(true), Some(false)) => Some(false),
                _ => None,
            },
            Equal | Equivalent => {
                let l = self.scalar(&b.left, ctx, rid)?;
                let r = self.scalar(&b.right, ctx, rid)?;
                Some(json_eq(&l, &r))
            }
            NotEqual | NotEquivalent => {
                let l = self.scalar(&b.left, ctx, rid)?;
                let r = self.scalar(&b.right, ctx, rid)?;
                Some(!json_eq(&l, &r))
            }
            LessThan => self.compare(b, ctx, rid).map(|o| o.is_lt()),
            LessThanOrEqual => self.compare(b, ctx, rid).map(|o| o.is_le()),
            GreaterThan => self.compare(b, ctx, rid).map(|o| o.is_gt()),
            GreaterThanOrEqual => self.compare(b, ctx, rid).map(|o| o.is_ge()),
            _ => None,
        }
    }

    fn compare(
        &self,
        b: &octofhir_fhirpath::ast::BinaryOperationNode,
        ctx: &Value,
        rid: i64,
    ) -> Option<std::cmp::Ordering> {
        let l = self.scalar(&b.left, ctx, rid)?;
        let r = self.scalar(&b.right, ctx, rid)?;
        json_cmp(&l, &r)
    }

    /// The single value of an expression (FHIRPath singleton), or `None` if the
    /// collection is empty.
    fn scalar(&self, node: &ExpressionNode, ctx: &Value, rid: i64) -> Option<Value> {
        if let ExpressionNode::Literal(l) = node {
            return Some(literal_value(&l.value));
        }
        self.eval_coll(node, ctx, rid).ok()?.into_iter().next()
    }

    /// Evaluate a binary/unary expression to a single value (for column paths).
    fn eval_value(&self, node: &ExpressionNode, ctx: &Value, rid: i64) -> Result<Option<Value>> {
        match node {
            ExpressionNode::BinaryOperation(b) => {
                use BinaryOperator::*;
                if matches!(
                    b.operator,
                    Add | Subtract | Multiply | Divide | Modulo | IntegerDivide
                ) {
                    let (Some(l), Some(r)) = (
                        self.scalar(&b.left, ctx, rid).and_then(as_f64),
                        self.scalar(&b.right, ctx, rid).and_then(as_f64),
                    ) else {
                        return Ok(None);
                    };
                    let result = match b.operator {
                        Add => l + r,
                        Subtract => l - r,
                        Multiply => l * r,
                        Divide => l / r,
                        Modulo => l % r,
                        IntegerDivide => (l / r).trunc(),
                        _ => unreachable!(),
                    };
                    return Ok(Some(number_value(result)));
                }
                if matches!(b.operator, Concatenate) {
                    let l = self.scalar(&b.left, ctx, rid).map(|v| value_to_string(&v));
                    let r = self.scalar(&b.right, ctx, rid).map(|v| value_to_string(&v));
                    return Ok(Some(Value::String(format!(
                        "{}{}",
                        l.unwrap_or_default(),
                        r.unwrap_or_default()
                    ))));
                }
                Ok(self.eval_bool(node, ctx, rid).map(Value::Bool))
            }
            ExpressionNode::UnaryOperation(u) => match u.operator {
                UnaryOperator::Not => Ok(self.eval_bool(node, ctx, rid).map(Value::Bool)),
                UnaryOperator::Negate => Ok(self
                    .scalar(&u.operand, ctx, rid)
                    .and_then(as_f64)
                    .map(|v| number_value(-v))),
                UnaryOperator::Positive => Ok(self
                    .scalar(&u.operand, ctx, rid)
                    .and_then(as_f64)
                    .map(number_value)),
            },
            _ => Ok(self.eval_bool(node, ctx, rid).map(Value::Bool)),
        }
    }

    fn int_literal(&self, node: &ExpressionNode) -> Result<i64> {
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

    fn type_name(&self, node: &ExpressionNode) -> Result<String> {
        match node {
            ExpressionNode::Identifier(n) => Ok(n.name.clone()),
            ExpressionNode::TypeInfo(t) => Ok(t.name.clone()),
            other => Err(Error::InvalidPath(format!(
                "expected a type name, got {}",
                other.node_type()
            ))),
        }
    }

    fn string_arg(&self, node: &ExpressionNode) -> Result<String> {
        match node {
            ExpressionNode::Literal(l) => match &l.value {
                LiteralValue::String(s) => Ok(s.clone()),
                other => Ok(other.to_string()),
            },
            other => Err(Error::InvalidPath(format!(
                "expected a string literal, got {}",
                other.node_type()
            ))),
        }
    }
}

/// Flatten navigation `coll.prop` across a collection.
fn nav(coll: &[Value], prop: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for item in coll {
        match item.get(prop) {
            Some(Value::Array(a)) => {
                out.extend(a.iter().filter(|v| !v.is_null()).cloned());
            }
            Some(Value::Null) | None => {}
            Some(v) => out.push(v.clone()),
        }
    }
    out
}

/// FHIRPath indexer over a collection (negative indexes count from the end).
fn index(coll: &[Value], n: i64) -> Vec<Value> {
    let len = coll.len() as i64;
    let idx = if n < 0 { len + n } else { n };
    if idx >= 0 && (idx as usize) < coll.len() {
        vec![coll[idx as usize].clone()]
    } else {
        Vec::new()
    }
}

fn literal_value(v: &LiteralValue) -> Value {
    match v {
        LiteralValue::String(s) => Value::String(s.clone()),
        LiteralValue::Integer(i) | LiteralValue::Long(i) => Value::Number((*i).into()),
        LiteralValue::Decimal(d) => d
            .to_string()
            .parse::<serde_json::Number>()
            .map(Value::Number)
            .unwrap_or(Value::Null),
        LiteralValue::Boolean(b) => Value::Bool(*b),
        LiteralValue::Date(_) | LiteralValue::DateTime(_) | LiteralValue::Time(_) => {
            Value::String(v.to_string().trim_start_matches('@').to_string())
        }
        LiteralValue::Quantity { value, .. } => value
            .to_string()
            .parse::<serde_json::Number>()
            .map(Value::Number)
            .unwrap_or(Value::Null),
    }
}

/// The FHIR primitive string representation of a JSON value.
fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn as_f64(v: Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn number_value(f: f64) -> Value {
    if f.fract() == 0.0 && f.abs() < i64::MAX as f64 {
        Value::Number((f as i64).into())
    } else {
        serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => x == y,
            _ => x == y,
        },
        _ => a == b,
    }
}

fn json_cmp(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64()?.partial_cmp(&y.as_f64()?),
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

fn reference_key(item: &Value, want_type: Option<&str>) -> Option<String> {
    let reference = item.get("reference")?.as_str()?;
    if reference.starts_with('#') || reference.starts_with("urn:") || reference.contains("://") {
        return None;
    }
    let trimmed = reference.trim_start_matches('/');
    let mut parts = trimmed.split('/');
    let rtype = parts.next()?;
    let id = parts.next()?;
    if id.is_empty() {
        return None;
    }
    match want_type {
        Some(t) if t != rtype => None,
        _ => Some(id.to_string()),
    }
}

/// The FHIR temporal type a boundary operates on, inferred from a leading
/// `ofType(T)`.
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

/// FHIR precision boundary; numbers use the decimal half-ulp, temporal strings
/// are widened to their precision-filled bound.
fn boundary(v: &Value, low: bool, hint: BoundaryType) -> Value {
    match v {
        Value::Number(n) => {
            let text = n.to_string();
            let scale = text
                .split_once('.')
                .map(|(_, frac)| frac.len())
                .unwrap_or(0);
            let half = 0.5 / 10f64.powi(scale as i32);
            let base = n.as_f64().unwrap_or(0.0);
            number_value(if low { base - half } else { base + half })
        }
        Value::String(s) => Value::String(boundary_string(s, low, hint)),
        other => other.clone(),
    }
}

fn boundary_string(s: &str, low: bool, hint: BoundaryType) -> String {
    let is_time = matches!(hint, BoundaryType::Time)
        || (matches!(hint, BoundaryType::Unknown) && s.contains(':') && !s.contains('-'));
    if is_time {
        return match (s.len(), low) {
            (2, true) => format!("{s}:00:00.000"),
            (2, false) => format!("{s}:59:59.999"),
            (5, true) => format!("{s}:00.000"),
            (5, false) => format!("{s}:59.999"),
            (8, true) => format!("{s}.000"),
            (8, false) => format!("{s}.999"),
            _ => s.to_string(),
        };
    }
    if matches!(hint, BoundaryType::DateTime) {
        let time = if low {
            "T00:00:00.000+14:00"
        } else {
            "T23:59:59.999-12:00"
        };
        return match (s.len(), low) {
            (4, true) => format!("{s}-01-01{time}"),
            (4, false) => format!("{s}-12-31{time}"),
            (7, true) => format!("{s}-01{time}"),
            (7, false) => format!("{s}-{}{time}", last_day_of_month(s)),
            (10, _) => format!("{s}{time}"),
            _ => s.to_string(),
        };
    }
    match (s.len(), low) {
        (4, true) => format!("{s}-01-01"),
        (4, false) => format!("{s}-12-31"),
        (7, true) => format!("{s}-01"),
        (7, false) => format!("{s}-{}", last_day_of_month(s)),
        _ => s.to_string(),
    }
}

fn last_day_of_month(year_month: &str) -> String {
    let mut it = year_month.split('-');
    let year: i32 = it.next().and_then(|y| y.parse().ok()).unwrap_or(2000);
    let month: u32 = it.next().and_then(|m| m.parse().ok()).unwrap_or(1);
    let last = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 30,
    };
    format!("{last:02}")
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn column_type(col: &Column) -> ColumnType {
    if col.collection.unwrap_or(false) {
        return ColumnType::Json;
    }
    // An `ansi/type` tag explicitly overrides the inferred column type.
    if let Some(ansi) = ansi_type_tag(col) {
        return ColumnType::from_ansi_type(ansi);
    }
    col.col_type
        .as_deref()
        .map(ColumnType::from_fhir_type)
        .unwrap_or(ColumnType::String)
}

/// The value of a column's `ansi/type` tag, if present.
pub(crate) fn ansi_type_tag(col: &Column) -> Option<&str> {
    col.tag
        .iter()
        .find(|t| t.name == "ansi/type")
        .and_then(|t| t.value.as_deref())
}
