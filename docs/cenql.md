# CenQL — the CenDB Query Language

CenQL (pronounced *"sen-cue-el"*) is CenDB's pipeline-oriented,
model-polymorphic query language. It is designed to be:

- **Readable left-to-right** — data flows from the source through a series
  of transforms, like a Unix pipeline.
- **Composable** — every stage's output is the next stage's input; you
  can chain, refactor, and reuse sub-pipelines.
- **Multi-model** — the same operators work over relations, documents,
  graphs, and time-series, with model-specific operators where needed.
- **Transpilable** — the AST is a stable contract; future work can lower
  it to SQL, PRQL, or native code.

## Why not SQL?

SQL fails at:
- **Nested data** — `JSON_EXTRACT` is awkward and non-composable.
- **Graph traversal** — recursive CTEs are unreadable for anything
  beyond trivial patterns.
- **Time windows** — vendor-specific, inconsistent syntax.
- **Composability** — subquery nesting makes pipelines hard to read.

CenQL addresses all four by adopting a pipeline-first syntax inspired by
PRQL and Kusto, then adding model-specific operators (window, match)
that integrate naturally.

## Lexical structure

### Keywords

```
from  filter  select  sort  asc  desc  take
join  on  inner  left  right  full
group_by  window  tumbling  hopping  session  on
match  return  distinct
last  and  or  not
```

### Identifiers

Identifiers start with a letter or `_`, followed by letters, digits, or
`_`. Dotted paths (`user.address.city`) are supported in column
references and select clauses.

### Literals

| Type | Example |
|---|---|
| Integer | `42`, `-7` |
| Float | `19.99`, `3.14` |
| String | `"alice"`, `"hello, world"` |
| Boolean | `true`, `false` |
| Duration | `5m`, `1h`, `30s`, `7d` |

### Operators

| Operator | Meaning |
|---|---|
| `==` `!=` | equality / inequality |
| `<` `<=` `>` `>=` | comparison |
| `and` `or` `not` | boolean |
| `+` `-` `*` `/` | arithmetic |
| `\|` | pipeline separator |
| `->` `<-` | graph edge direction |
| `*` | variable-length edge (in `match`) |
| `..` | range (in `*min..max`) |

### Comments

Line comments start with `--` and continue to end of line:

```cenql
from users  -- this is a comment
| take 5
```

## Pipeline structure

A CenQL pipeline is a source followed by zero or more transformation
stages, separated by `|`:

```
<source> | <stage> | <stage> | ... | <sink>
```

The first stage must be `from`. Every subsequent stage transforms the
stream of rows produced by the previous stage.

### Example

```cenql
from users
| filter age >= 18 and country == "DE"
| select { name, email, age }
| sort age desc
| take 100
```

## Stage reference

### `from <source>`

Specifies the data source. For the graph model, a two-word name is
allowed: `from graph social`.

```cenql
from users
from graph social
from metrics
```

### `filter <predicate>`

Keep rows where the predicate evaluates to true. Predicates support
boolean operators (`and`, `or`, `not`), comparisons, and dotted paths.

```cenql
from users
| filter age >= 18 and country == "DE"
| filter not (status == "banned" or status == "pending")
```

Dotted paths into nested documents:

```cenql
from events
| filter payload.user.tier == "premium"
| filter payload.address.city == "Berlin"
```

### `select { col1, col2, ... }`

Project to a subset of columns. Dotted paths are supported:

```cenql
from events
| select {
    user_id,
    payload.user.address.city,
    payload.cart.items[*].sku
  }
```

### `sort <col> [asc|desc]`

Sort by a column. Default direction is `asc`.

```cenql
from users
| sort age desc
| sort name asc
```

### `take <n>`

Limit to `n` rows.

```cenql
from users
| take 100
```

### `join [inner|left|right|full] <source> on <predicate>`

Join with another source. The join kind defaults to `inner`.

```cenql
from orders
| join customers on orders.customer_id == customers.id
| take 10
```

### `group_by <key> { name: agg_fn(args), ... }`

Aggregate by a key column. Each aggregate has the form
`output_name: function(args)`.

Supported functions:
- `count()` — count of rows in the group.
- `sum(expr)` — sum of `expr`.
- `mean(expr)` — arithmetic mean.
- `max(expr)`, `min(expr)` — extremes.
- `percentile(expr, p)` — p-th percentile (p in [0, 100]).

```cenql
from orders
| group_by region {
    revenue: sum(total),
    count:   count(),
    avg:     mean(total)
  }
| sort revenue desc
```

### `window tumbling(<dur>)|hopping(<dur>, <slide>)|session(<gap>) on <col> { ... }`

Time-series windowing. The `on` clause specifies the timestamp column.

```cenql
from metrics
| filter sensor_id == 42 and ts in last(7d)
| window tumbling(5m) on ts {
    avg_temp:  mean(temperature),
    p99_temp:  percentile(temperature, 99),
    max_temp:  max(temperature)
  }
| fill gaps with linear
```

Three window kinds:
- **tumbling** — fixed-size, non-overlapping.
- **hopping** — fixed-size, overlapping (slide < size).
- **session** — windows delimited by gaps of inactivity.

### `match (n:Label)-[:TYPE*min..max]->(m:Label)`

Graph traversal pattern. Variable-length edges are specified with
`*min..max`.

```cenql
from graph social
| match (a:Person {name: "Ada"})
        -[:FOLLOWS*1..3]->
        (b:Person)
| filter b.active == true
| return distinct b.name, path_length() as hops
| sort hops
```

Edge direction:
- `-[:T]->` — outgoing.
- `<-[:T]-` — incoming.
- `-[:T]-` — either direction (undirected traversal).

Edge modifiers:
- `[:T]` — exactly one hop.
- `[:T*]` — variable-length, any number of hops.
- `[:T*1..3]` — between 1 and 3 hops.
- `[:T*2..]` — at least 2 hops.

### `return [distinct] <col>, ...`

Final projection, optionally with deduplication.

```cenql
from users
| filter active == true
| return distinct name, email
```

## Expressions

Expressions appear in `filter`, `on`, and aggregate arguments. They
support:

| Form | Example |
|---|---|
| Column ref | `age`, `user.address.city` |
| Integer | `42` |
| Float | `19.99` |
| String | `"alice"` |
| Boolean | `true` |
| Comparison | `age >= 18` |
| Boolean | `a and b`, `a or b`, `not a` |
| Arithmetic | `price * quantity`, `total - discount` |
| Function call | `count()`, `sum(price)`, `mean(score)` |
| Parenthesised | `(price + tax) * 1.2` |

### Operator precedence (lowest to highest)

1. `or`
2. `and`
3. comparisons (`==`, `!=`, `<`, `<=`, `>`, `>=`)
4. additive (`+`, `-`)
5. multiplicative (`*`, `/`)
6. unary `not`
7. primary (literals, columns, function calls, parenthesised)

## Cross-model pipeline

CenQL's killer feature: a single pipeline can touch multiple models.

```cenql
from graph social
| match (u:Person)-[:PURCHASED]->(p:Product)
| join metrics on metrics.product_id == p.id
| filter metrics.ts in last(30d)
| window tumbling(1d) on metrics.ts {
    daily_buyers: count_distinct(u.id),
    revenue:       sum(p.price)
  }
| filter u.profile.preferences.notify == true
| sort revenue desc
```

This pipeline:
1. Traverses the **graph** (Person → Product via PURCHASED edges).
2. **Joins** the result with a **relational** table (metrics).
3. Filters on a **time-series** predicate (last 30 days).
4. Aggregates into **time windows**.
5. Filters on a **document** path (user preferences).

One pipeline, four models, no intermediate serialisation.

## Parser API (Rust)

```rust
use cendb_cenql::{parse, CenqlPipeline};

let pipeline: CenqlPipeline = parse(r#"
    from users
    | filter age >= 18
    | select { name, email }
    | take 100
"#)?;
println!("{}", pipeline);  // displays the (normalised) pipeline
```

The parser produces an AST defined in
[`crates/cendb-cenql/src/ast.rs`](../crates/cendb-cenql/src/ast.rs). The
AST is the contract between the parser and a future executor/planner.

## Grammar (EBNF summary)

```
pipeline  = stage { "|" stage } ;
stage     = "from" ident [ ident ]
          | "filter" expr
          | "select" "{" ident_or_path { "," ident_or_path } "}"
          | "sort" ident_or_path [ "asc" | "desc" ]
          | "take" int
          | "join" [ "inner" | "left" | "right" | "full" ] ident "on" expr
          | "group_by" ident_or_path "{" agg { "," agg } "}"
          | "window" window_kind "on" ident_or_path "{" agg { "," agg } "}"
          | "match" graph_pattern
          | "return" [ "distinct" ] ident_or_path { "," ident_or_path } ;
agg       = ident ":" ident "(" [ expr { "," expr } ] ")" ;
window_kind = "tumbling" "(" duration ")"
            | "hopping" "(" duration "," duration ")"
            | "session" "(" duration ")" ;
graph_pattern = "(" ident [ ":" ident ] ")"
                  edge_pattern
                  "(" ident [ ":" ident ] ")" ;
edge_pattern = [ "<-" | "-" ] "[" [ ":" ident ] [ "*" [ int ] [ ".." [ int ] ] ] "]"
                  [ "->" | "-" ] ;
expr      = or_expr ;
or_expr   = and_expr { "or" and_expr } ;
and_expr  = cmp_expr { "and" cmp_expr } ;
cmp_expr  = add_expr [ ( "==" | "!=" | "<" | "<=" | ">" | ">=" ) add_expr ] ;
add_expr  = mul_expr { ( "+" | "-" ) mul_expr } ;
mul_expr  = primary { ( "*" | "/" ) primary } ;
primary   = int | float | string | "true" | "false"
          | ident [ "(" [ expr { "," expr } ] ")" ]
          | "(" expr ")"
          | ident_or_path ;
ident_or_path = ident { "." ident } ;
```

## Comparison to SQL

**Task**: Find the top 5 products co-purchased with product X (2 hops in
the purchase graph), but only counting purchases in the last 30 days,
returning daily revenue.

### SQL (recursive CTE + window — barely readable)

```sql
WITH RECURSIVE copurchased AS (
  SELECT p2.product_id, 1 AS depth
  FROM purchases p1
  JOIN purchases p2 ON p1.basket_id = p2.basket_id
  WHERE p1.product_id = :x AND p2.product_id <> :x
  UNION ALL
  SELECT p3.product_id, c.depth + 1
  FROM copurchased c
  JOIN purchases p1 ON p1.product_id = c.product_id
  JOIN purchases p3 ON p1.basket_id = p3.basket_id
  WHERE c.depth < 2
)
SELECT cp.product_id,
       date_trunc('day', pu.purchased_at) AS day,
       SUM(pr.price) AS revenue
FROM copurchased cp
JOIN purchases pu ON pu.product_id = cp.product_id
JOIN products  pr ON pr.id = cp.product_id
WHERE pu.purchased_at >= NOW() - INTERVAL '30 days'
GROUP BY cp.product_id, date_trunc('day', pu.purchased_at)
ORDER BY revenue DESC
LIMIT 5;
```

### CenQL (intent-revealing)

```cenql
from graph purchases
| match (start:Product {id: $x})
        -[:CO_PURCHASED*1..2]->
        (co:Product)
| join purchases pu on pu.product_id == co.id
| filter pu.purchased_at in last(30d)
| window tumbling(1d) on pu.purchased_at
    group_by co.id {
      revenue: sum(co.price)
    }
| sort revenue desc
| take 5
```

The CenQL version is ~40% shorter, the data flow is left-to-right
readable, and the graph pattern is a first-class operator instead of a
recursive CTE.

## Limitations of the prototype

The current CenQL implementation ships:
- ✅ Lexer (zero-alloc tokenizer).
- ✅ Recursive-descent parser producing an AST.
- ✅ AST with `Pipeline`, `Stage`, `Expr` types.
- ✅ `Display` impl that round-trips.

Not yet implemented:
- ❌ Binder / resolver (resolving names against the catalog).
- ❌ Logical/physical planner.
- ❌ Vectorized executor.
- ❌ JIT compilation of expression trees (via Cranelift).
- ❌ `fill gaps with linear` (gap interpolation).
- ❌ `path_length()` and other graph-traversal introspection functions.

These are tracked as future work; the AST is the stable contract between
the parser and the future executor.
