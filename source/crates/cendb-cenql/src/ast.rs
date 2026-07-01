//! CenQL AST: pipeline of stages with expressions.

use std::fmt;

// ============================================================================
// Pipeline.
// ============================================================================

/// A CenQL pipeline: a source followed by zero or more transformation stages.
#[derive(Clone, Debug)]
pub struct CenqlPipeline {
    /// The first stage is always `From`.
    pub stages: Vec<CenqlStage>,
}

impl CenqlPipeline {
    pub fn new(stages: Vec<CenqlStage>) -> Self {
        Self { stages }
    }

    /// The source name (from the `from` stage).
    pub fn source(&self) -> Option<&str> {
        self.stages
            .iter()
            .find_map(|s| if let CenqlStage::From { name } = s {
                Some(name.as_str())
            } else {
                None
            })
    }

    /// Number of stages in the pipeline.
    pub fn len(&self) -> usize {
        self.stages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }
}

impl fmt::Display for CenqlPipeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, stage) in self.stages.iter().enumerate() {
            if i > 0 {
                write!(f, "\n| ")?;
            }
            write!(f, "{}", stage)?;
        }
        Ok(())
    }
}

// ============================================================================
// Stages.
// ============================================================================

/// One stage in a CenQL pipeline.
#[derive(Clone, Debug)]
pub enum CenqlStage {
    /// `from <source>` — the data source.
    From { name: String },
    /// `from (subquery)` — a subquery in the FROM clause. The subquery is
    /// materialised first, then its rows feed the rest of the outer
    /// pipeline.
    FromSubquery { pipeline: Box<CenqlPipeline> },
    /// `filter <expr>` — keep rows where `expr` is true.
    Filter { expr: Expr },
    /// `select { col1, col2, ... }` — project to columns.
    Select { columns: Vec<String> },
    /// `sort <col> [asc|desc]` — sort by a column.
    Sort { column: String, dir: SortDir },
    /// `take <n>` — limit to n rows.
    Take { n: u64 },
    /// `join <src> on <expr>` — join with another source.
    Join {
        source: String,
        kind: JoinKind,
        on: Expr,
    },
    /// `group_by <col> { agg: expr, ... }` — aggregate.
    GroupBy {
        key: String,
        aggs: Vec<AggExpr>,
    },
    /// `window tumbling(<dur>) on <col> { ... }` — time-series windowing.
    Window {
        spec: WindowSpec,
        on: String,
        aggs: Vec<AggExpr>,
    },
    /// `match (n:Label)-[:TYPE*1..3]->(m:Label)` — graph traversal.
    Match { pattern: GraphMatchPattern },
    /// `return distinct <col>, ...` — final projection with dedup.
    Return { distinct: bool, columns: Vec<String> },
}

impl fmt::Display for CenqlStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CenqlStage::From { name } => write!(f, "from {}", name),
            CenqlStage::FromSubquery { pipeline } => write!(f, "from ({})", pipeline),
            CenqlStage::Filter { expr } => write!(f, "filter {}", expr),
            CenqlStage::Select { columns } => {
                write!(f, "select {{ ")?;
                for (i, c) in columns.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", c)?;
                }
                write!(f, " }}")
            }
            CenqlStage::Sort { column, dir } => write!(f, "sort {} {:?}", column, dir),
            CenqlStage::Take { n } => write!(f, "take {}", n),
            CenqlStage::Join { source, kind, on } => {
                write!(f, "join {:?} {} on {}", kind, source, on)
            }
            CenqlStage::GroupBy { key, aggs } => {
                write!(f, "group_by {} {{ ", key)?;
                for (i, a) in aggs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", a)?;
                }
                write!(f, " }}")
            }
            CenqlStage::Window { spec, on, aggs } => {
                write!(f, "window {:?} on {} {{ ", spec, on)?;
                for (i, a) in aggs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", a)?;
                }
                write!(f, " }}")
            }
            CenqlStage::Match { pattern } => write!(f, "match {}", pattern),
            CenqlStage::Return { distinct, columns } => {
                if *distinct {
                    write!(f, "return distinct ")?;
                } else {
                    write!(f, "return ")?;
                }
                for (i, c) in columns.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", c)?;
                }
                Ok(())
            }
        }
    }
}

// ============================================================================
// Expressions.
// ============================================================================

/// A boolean or value expression used in `filter` and `on` clauses.
#[derive(Clone, Debug)]
pub enum Expr {
    /// Column reference, e.g. `age` or `user.address.city`.
    Column(String),
    /// Integer literal.
    I64(i64),
    /// Float literal.
    F64(f64),
    /// String literal.
    Str(String),
    /// Boolean literal.
    Bool(bool),
    /// Binary operation: `lhs op rhs`.
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// Function call, e.g. `count()`, `sum(col)`, `mean(col)`.
    Call { name: String, args: Vec<Expr> },
    /// `value IN (subquery)` — membership test against a materialised
    /// subquery's single-column result.
    In {
        value: Box<Expr>,
        subquery: Box<CenqlPipeline>,
    },
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Column(c) => write!(f, "{}", c),
            Expr::I64(v) => write!(f, "{}", v),
            Expr::F64(v) => write!(f, "{}", v),
            Expr::Str(s) => write!(f, "\"{}\"", s),
            Expr::Bool(b) => write!(f, "{}", b),
            Expr::Binary { op, lhs, rhs } => write!(f, "({} {:?} {})", lhs, op, rhs),
            Expr::Call { name, args } => {
                write!(f, "{}(", name)?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", a)?;
                }
                write!(f, ")")
            }
            Expr::In { value, subquery } => {
                write!(f, "({} in ({}))", value, subquery)
            }
        }
    }
}

/// Binary operators supported in expressions.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BinaryOp {
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `and`
    And,
    /// `or`
    Or,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
}

// ============================================================================
// Aggregates.
// ============================================================================

/// An aggregate expression: `name: agg_fn(arg1, arg2, ...)`.
#[derive(Clone, Debug)]
pub struct AggExpr {
    /// Output column name.
    pub name: String,
    /// Aggregate function: `sum`, `count`, `mean`, `max`, `min`, etc.
    pub func: String,
    /// Argument expressions (e.g. the column to sum). Empty for zero-arg
    /// aggregates like `count()`.
    pub args: Vec<Expr>,
}

impl fmt::Display for AggExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}(", self.name, self.func)?;
        for (i, a) in self.args.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}", a)?;
        }
        write!(f, ")")
    }
}

// ============================================================================
// Sort direction.
// ============================================================================

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SortDir {
    Asc,
    Desc,
}

// ============================================================================
// Join kinds.
// ============================================================================

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
}

// ============================================================================
// Window spec.
// ============================================================================

#[derive(Clone, Debug, PartialEq)]
pub enum WindowSpec {
    /// `tumbling(<duration>)` — fixed-size, non-overlapping windows.
    Tumbling(String),
    /// `hopping(<duration>, <slide>)` — fixed-size, overlapping windows.
    Hopping { size: String, slide: String },
    /// `session(<gap>)` — windows delimited by gaps in the data.
    Session(String),
}

// ============================================================================
// Graph match pattern.
// ============================================================================

/// A graph match pattern like `(a:Person)-[:FOLLOWS*1..3]->(b:Person)`.
#[derive(Clone, Debug)]
pub struct GraphMatchPattern {
    /// Start node: `(var:Label)`.
    pub start_var: String,
    pub start_label: Option<String>,
    /// Edge: `-[:TYPE*min..max]->` or `<-[:TYPE]-`.
    pub edge_type: Option<String>,
    pub edge_min_hops: u32,
    pub edge_max_hops: u32,
    pub edge_direction: EdgeDirection,
    /// End node: `(var:Label)`.
    pub end_var: String,
    pub end_label: Option<String>,
}

impl fmt::Display for GraphMatchPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}", self.start_var)?;
        if let Some(l) = &self.start_label {
            write!(f, ":{}", l)?;
        }
        write!(f, ")")?;
        match self.edge_direction {
            EdgeDirection::Out => write!(f, "-")?,
            EdgeDirection::In => write!(f, "<-")?,
            EdgeDirection::Both => write!(f, "-")?,
        }
        write!(f, "[")?;
        if let Some(t) = &self.edge_type {
            write!(f, ":{}", t)?;
        }
        if self.edge_min_hops > 0 || self.edge_max_hops > 0 {
            write!(f, "*{}..{}", self.edge_min_hops, self.edge_max_hops)?;
        }
        write!(f, "]")?;
        match self.edge_direction {
            EdgeDirection::Out => write!(f, "->")?,
            EdgeDirection::In => write!(f, "-")?,
            EdgeDirection::Both => write!(f, "-")?,
        }
        write!(f, "({}", self.end_var)?;
        if let Some(l) = &self.end_label {
            write!(f, ":{}", l)?;
        }
        write!(f, ")")
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EdgeDirection {
    Out,
    In,
    Both,
}

// ============================================================================
// Top-level statements (DDL, DML, queries, transactions).
// ============================================================================

/// A CenQL statement. This is the top-level parse result — a single
/// statement can be a query (pipeline), DDL, DML, or transaction control.
#[derive(Clone, Debug)]
pub enum CenqlStatement {
    /// A query pipeline: `from users | filter age > 18 | ...`
    Query(CenqlPipeline),
    /// `create table <name> { col1: type, col2: type, ... }`
    CreateTable {
        name: String,
        columns: Vec<ColumnDef>,
        primary_key: Option<String>,
    },
    /// `drop table <name>`
    DropTable { name: String },
    /// `create index <name> on <table>(<column>)`
    CreateIndex {
        name: String,
        table: String,
        column: String,
    },
    /// `drop index <name>`
    DropIndex { name: String },
    /// `insert into <table> { col1: val1, col2: val2, ... }`
    Insert {
        table: String,
        values: Vec<(String, Expr)>,
    },
    /// `update <table> set col1 = val1, col2 = val2 where <expr>`
    Update {
        table: String,
        assignments: Vec<(String, Expr)>,
        where_clause: Option<Expr>,
    },
    /// `delete from <table> where <expr>`
    Delete {
        table: String,
        where_clause: Option<Expr>,
    },
    /// `upsert into <table> { col1: val1, ... }` — insert or update
    Upsert {
        table: String,
        values: Vec<(String, Expr)>,
    },
    /// `create view <name> as <pipeline>`
    CreateView {
        name: String,
        pipeline: Box<CenqlPipeline>,
    },
    /// `drop view <name>`
    DropView { name: String },
    /// `begin` — start a transaction
    Begin,
    /// `commit` — commit the current transaction
    Commit,
    /// `rollback` — abort the current transaction
    Rollback,
    /// `with <name> as (<pipeline>) <pipeline>` — CTE
    WithCte {
        cte_name: String,
        cte_pipeline: Box<CenqlPipeline>,
        main_pipeline: Box<CenqlPipeline>,
    },
    /// `<pipeline> union <pipeline>` — set union
    Union {
        left: Box<CenqlPipeline>,
        right: Box<CenqlPipeline>,
        all: bool,
    },
    /// `<pipeline> intersect <pipeline>` — set intersection
    Intersect {
        left: Box<CenqlPipeline>,
        right: Box<CenqlPipeline>,
    },
    /// `<pipeline> except <pipeline>` — set difference
    Except {
        left: Box<CenqlPipeline>,
        right: Box<CenqlPipeline>,
    },
    /// `distinct <pipeline>` — deduplicate results
    Distinct {
        pipeline: Box<CenqlPipeline>,
    },
}

impl fmt::Display for CenqlStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CenqlStatement::Query(p) => write!(f, "{}", p),
            CenqlStatement::CreateTable { name, columns, primary_key } => {
                write!(f, "create table {} {{ ", name)?;
                for (i, c) in columns.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "{}", c)?;
                }
                if let Some(pk) = primary_key {
                    write!(f, ", primary_key: {}", pk)?;
                }
                write!(f, " }}")
            }
            CenqlStatement::DropTable { name } => write!(f, "drop table {}", name),
            CenqlStatement::CreateIndex { name, table, column } => {
                write!(f, "create index {} on {}({})", name, table, column)
            }
            CenqlStatement::DropIndex { name } => write!(f, "drop index {}", name),
            CenqlStatement::Insert { table, values } => {
                write!(f, "insert into {} {{ ", table)?;
                for (i, (k, v)) in values.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "{}: {}", k, v)?;
                }
                write!(f, " }}")
            }
            CenqlStatement::Update { table, assignments, where_clause } => {
                write!(f, "update {} set ", table)?;
                for (i, (k, v)) in assignments.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "{} = {}", k, v)?;
                }
                if let Some(w) = where_clause {
                    write!(f, " where {}", w)?;
                }
                Ok(())
            }
            CenqlStatement::Delete { table, where_clause } => {
                write!(f, "delete from {}", table)?;
                if let Some(w) = where_clause {
                    write!(f, " where {}", w)?;
                }
                Ok(())
            }
            CenqlStatement::Upsert { table, values } => {
                write!(f, "upsert into {} {{ ", table)?;
                for (i, (k, v)) in values.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "{}: {}", k, v)?;
                }
                write!(f, " }}")
            }
            CenqlStatement::CreateView { name, pipeline } => {
                write!(f, "create view {} as {}", name, pipeline)
            }
            CenqlStatement::DropView { name } => write!(f, "drop view {}", name),
            CenqlStatement::Begin => write!(f, "begin"),
            CenqlStatement::Commit => write!(f, "commit"),
            CenqlStatement::Rollback => write!(f, "rollback"),
            CenqlStatement::WithCte { cte_name, cte_pipeline, main_pipeline } => {
                write!(f, "with {} as ({}) {}", cte_name, cte_pipeline, main_pipeline)
            }
            CenqlStatement::Union { left, right, all } => {
                if *all {
                    write!(f, "{} union all {}", left, right)
                } else {
                    write!(f, "{} union {}", left, right)
                }
            }
            CenqlStatement::Intersect { left, right } => {
                write!(f, "{} intersect {}", left, right)
            }
            CenqlStatement::Except { left, right } => {
                write!(f, "{} except {}", left, right)
            }
            CenqlStatement::Distinct { pipeline } => {
                write!(f, "distinct {}", pipeline)
            }
        }
    }
}

/// A column definition for `create table`.
#[derive(Clone, Debug)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: ColumnType,
    pub nullable: bool,
}

impl fmt::Display for ColumnDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let type_str = match self.data_type {
            ColumnType::I64 => "i64",
            ColumnType::F64 => "f64",
            ColumnType::Str => "str",
            ColumnType::Bool => "bool",
            ColumnType::Bytes => "bytes",
            ColumnType::Timestamp => "timestamp",
            ColumnType::Json => "json",
        };
        write!(f, "{}: {}", self.name, type_str)?;
        if !self.nullable {
            write!(f, " not null")?;
        }
        Ok(())
    }
}

/// Supported column types.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ColumnType {
    I64,
    F64,
    Str,
    Bool,
    Bytes,
    Timestamp,
    Json,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_display_roundtrips() {
        let p = CenqlPipeline::new(vec![
            CenqlStage::From { name: "users".to_string() },
            CenqlStage::Filter {
                expr: Expr::Binary {
                    op: BinaryOp::Ge,
                    lhs: Box::new(Expr::Column("age".to_string())),
                    rhs: Box::new(Expr::I64(18)),
                },
            },
            CenqlStage::Select {
                columns: vec!["name".to_string(), "age".to_string()],
            },
            CenqlStage::Take { n: 100 },
        ]);
        let s = format!("{}", p);
        assert!(s.contains("from users"));
        assert!(s.contains("filter"));
        assert!(s.contains("select"));
        assert!(s.contains("take 100"));
    }
}
