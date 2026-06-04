//! The AST→SQL lowering engine: turns a parsed FHIRPath expression into a
//! JSONB-collection SQL expression, and builds the per-level row plans.

use std::cell::Cell;
use std::collections::HashMap;

use octofhir_fhirpath::{BinaryOperator, ExpressionNode, LiteralValue, UnaryOperator, parse_ast};

use crate::Error;
use crate::column::ColumnType;
use crate::view_definition::{Column, SelectColumn};

use super::boundary::{
    BoundaryType, boundary_hint, capitalize_first, date_bound, datetime_bound, time_bound,
};
use super::constants::substitute_constants;
use super::{Plan, PlanColumn};

/// The AST→SQL lowering context for a single `generate` call.
pub(super) struct Lower {
    resource_type: String,
    /// Constant name → FHIRPath literal text, pre-rendered for substitution.
    constants: HashMap<String, String>,
    seq: Cell<usize>,
    /// SQL expression yielding the current `%rowIndex` (0-based). "0" at the top
    /// level; a `forEach`/`forEachOrNull` sets it to `<alias>.ord - 1` for the
    /// duration of that level (saved/restored around each `build_select`).
    row_idx: std::cell::RefCell<String>,
    /// SQL expression yielding the root resource JSONB (e.g. `base.resource`).
    /// Fragment (`#id`) references resolve against its `contained[]`.
    root: String,
}

impl Lower {
    pub(super) fn new(
        resource_type: String,
        constants: HashMap<String, String>,
        root: String,
    ) -> Self {
        Self {
            resource_type,
            constants,
            seq: Cell::new(0),
            row_idx: std::cell::RefCell::new("0".to_string()),
            root,
        }
    }

    fn fresh(&self) -> usize {
        let v = self.seq.get();
        self.seq.set(v + 1);
        v
    }

    /// Build the UNION ALL branches for a select node, given the join prefix and
    /// the current focus item (`ctx`, a JSONB scalar SQL expression).
    pub(super) fn build_select(
        &self,
        select: &SelectColumn,
        prefix: &[String],
        ctx: &str,
    ) -> Result<Vec<Plan>, Error> {
        // `%rowIndex` is scoped to this level: save the enclosing value, restore
        // it before returning so siblings/unionAll branches inherit correctly.
        let saved_ri = self.row_idx.borrow().clone();

        // forEach / forEachOrNull / repeat establish a new focus (and a new
        // %rowIndex) for this level; they are mutually exclusive (sql-expressions).
        let (joins, ctx2): (Vec<String>, String) = if !select.repeat.is_empty() {
            self.repeat_join(&select.repeat, prefix, ctx)?
        } else if let Some(p) = &select.for_each {
            self.for_each_join(p, prefix, ctx, false)?
        } else if let Some(p) = &select.for_each_or_null {
            self.for_each_join(p, prefix, ctx, true)?
        } else {
            (prefix.to_vec(), ctx.to_string())
        };

        let plans = self.build_level(select, &joins, &ctx2);

        *self.row_idx.borrow_mut() = saved_ri;
        plans
    }

    /// Lower a select's own columns, nested selects and unionAll branches against
    /// an already-established focus and `%rowIndex` (set by the caller).
    fn build_level(
        &self,
        select: &SelectColumn,
        joins: &[String],
        ctx2: &str,
    ) -> Result<Vec<Plan>, Error> {
        // This node's own columns, evaluated against the focus.
        let mut own = Vec::new();
        if let Some(cols) = &select.column {
            for col in cols {
                own.push(self.lower_column(col, ctx2)?);
            }
        }

        let mut plans = vec![Plan {
            joins: joins.to_vec(),
            columns: own,
        }];

        // Nested selects cross-join with the accumulated plans.
        for child in &select.select {
            let mut next = Vec::new();
            for p in &plans {
                for cp in self.build_select(child, &p.joins, ctx2)? {
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
                    for bp in self.build_select(branch, &p.joins, ctx2)? {
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
        // WITH ORDINALITY exposes a 1-based position; %rowIndex is 0-based.
        let join = if or_null {
            format!(
                "LEFT JOIN LATERAL jsonb_array_elements({coll}) WITH ORDINALITY AS {alias}(value, ord) ON true"
            )
        } else {
            format!(
                "CROSS JOIN LATERAL jsonb_array_elements({coll}) WITH ORDINALITY AS {alias}(value, ord)"
            )
        };
        let mut joins = prefix.to_vec();
        joins.push(join);
        // For forEachOrNull, the null row carries %rowIndex = 0 (per spec).
        let ri = if or_null {
            format!("coalesce(({alias}.ord - 1), 0)")
        } else {
            format!("({alias}.ord - 1)")
        };
        *self.row_idx.borrow_mut() = ri;
        Ok((joins, format!("{alias}.value")))
    }

    /// Lower a `repeat` directive to a lateral, recursive traversal. The recursive
    /// CTE re-applies the repeat path(s) to every reached node (the focus itself
    /// excluded). A `pathkey` array of per-level ordinals orders the result in
    /// preorder, and `%rowIndex` is its 0-based position in that flattened list.
    fn repeat_join(
        &self,
        paths: &[String],
        prefix: &[String],
        ctx: &str,
    ) -> Result<(Vec<String>, String), Error> {
        let asts = paths
            .iter()
            .map(|p| {
                let s = self.substitute(p)?;
                parse_ast(&s).map_err(|e| Error::FhirPath(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let n = self.fresh();
        let cte = format!("_rep{n}");
        let alias = format!("rep{n}");
        let base = self.repeat_expand(&asts, ctx)?;
        let step = self.repeat_expand(&asts, &format!("{cte}.value"))?;

        // The recursive CTE lives inside a LATERAL subquery so its base case can
        // reference the enclosing focus `ctx`.
        let lateral = format!(
            "CROSS JOIN LATERAL (\
             WITH RECURSIVE {cte}(value, pathkey) AS (\
             SELECT _b{n}.value, ARRAY[_b{n}.key]::bigint[] FROM ({base}) AS _b{n} \
             UNION ALL \
             SELECT _s{n}.value, {cte}.pathkey || _s{n}.key \
             FROM {cte}, LATERAL ({step}) AS _s{n}) \
             SELECT value, (row_number() OVER (ORDER BY pathkey) - 1)::bigint AS ridx \
             FROM {cte}) AS {alias}(value, ridx)"
        );
        let mut joins = prefix.to_vec();
        joins.push(lateral);
        *self.row_idx.borrow_mut() = format!("{alias}.ridx");
        Ok((joins, format!("{alias}.value")))
    }

    /// One traversal step: every node reached by applying each repeat path to
    /// `ctx`, tagged with an ordering `key` of `path_rank * 1e9 + element_ord` so
    /// that path order then element order is preserved.
    fn repeat_expand(&self, asts: &[ExpressionNode], ctx: &str) -> Result<String, Error> {
        let mut parts = Vec::new();
        for (i, ast) in asts.iter().enumerate() {
            let coll = self.coll(ast, ctx)?;
            let m = self.fresh();
            parts.push(format!(
                "SELECT _e{m}.value AS value, ({i}::bigint * 1000000000 + _e{m}.ord) AS key \
                 FROM jsonb_array_elements({coll}) WITH ORDINALITY AS _e{m}(value, ord)"
            ));
        }
        Ok(parts.join(" UNION ALL "))
    }

    fn lower_column(&self, col: &Column, ctx: &str) -> Result<PlanColumn, Error> {
        let path = self.substitute(&col.path)?;
        let ast = parse_ast(&path).map_err(|e| Error::FhirPath(e.to_string()))?;
        let coll = self.coll(&ast, ctx)?;
        let collection = col.collection.unwrap_or(false);
        // An `ansi/type` tag explicitly overrides the inferred column type.
        let ty = if let Some(ansi) = crate::eval::ansi_type_tag(col) {
            ColumnType::from_ansi_type(ansi)
        } else {
            col.col_type
                .as_deref()
                .map(ColumnType::from_fhir_type)
                .unwrap_or(ColumnType::String)
        };

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
            ColumnType::Integer | ColumnType::Integer64 => "(_e #>> '{}')::bigint",
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
                } else if v.name == "rowIndex" {
                    let ri = self.row_idx.borrow();
                    Ok(format!("jsonb_build_array(to_jsonb(({ri})::bigint))"))
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
        let refexpr = format!("_r{n}._e ->> 'reference'");
        let is_frag = format!("{refexpr} LIKE '#%'");
        // Fragment (`#id`) references key on the local id (sans `#`), resolved
        // into the resource's contained[]; relative/absolute references key on
        // the `Type/id` id segment. Both equal getResourceKey() (the id).
        let key = format!(
            "CASE WHEN {is_frag} THEN substring({refexpr} from 2) \
             ELSE split_part({refexpr}, '/', 2) END"
        );
        let guard = match type_arg {
            Some(t) => {
                let ty = self.type_name(t)?.replace('\'', "''");
                // A contained resource's type is its declared `resourceType`,
                // falling back to the Reference.type element; a normal
                // reference's type is its leading path segment.
                let contained_ty = format!(
                    "(SELECT _ct._ce ->> 'resourceType' \
                     FROM jsonb_array_elements(coalesce({root} -> 'contained', '[]'::jsonb)) AS _ct(_ce) \
                     WHERE _ct._ce ->> 'id' = substring({refexpr} from 2) LIMIT 1)",
                    root = self.root
                );
                format!(
                    " AND (CASE WHEN {is_frag} \
                       THEN coalesce({contained_ty}, _r{n}._e ->> 'type') = '{ty}' \
                       ELSE split_part({refexpr}, '/', 1) = '{ty}' END)"
                )
            }
            None => String::new(),
        };
        Ok(format!(
            "(SELECT coalesce(jsonb_agg(to_jsonb({key})),'[]'::jsonb) FROM jsonb_array_elements({coll}) AS _r{n}(_e) WHERE {refexpr} IS NOT NULL{guard})"
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

    pub(super) fn bool(&self, node: &ExpressionNode, item: &str) -> Result<String, Error> {
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
    pub(super) fn substitute(&self, path: &str) -> Result<String, Error> {
        substitute_constants(path, &self.constants)
    }
}
