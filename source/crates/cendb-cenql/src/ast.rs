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
