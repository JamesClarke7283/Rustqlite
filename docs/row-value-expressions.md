# Row-Value Expressions (SQLite `TK_VECTOR`)

## Grammar (from `parse.y`)

A row value is produced by this single rule in upstream `parse.y`:

```yacc
expr(A) ::= LP nexprlist(X) COMMA expr(Y) RP. {
  ExprList *pList = sqlite3ExprListAppend(pParse, X, Y);
  A = sqlite3PExpr(pParse, TK_VECTOR, 0, 0);
  ...
}
```

Key points:
- A row value **requires at least one comma**. The form is `LP nexprlist COMMA expr RP`, where
  `nexprlist` is `expr (COMMA expr)*`. So `(a)` is *not* a row value — it is just the parenthesised
  expression `a` (handled by the `expr ::= LP expr RP` rule). A row value has ≥2 entries.
- The result is a single `TK_VECTOR` Expr node whose `x.pList` carries the element expressions.
- `nexprlist` is non-empty, so the minimal row value is `(e0, e1)` (two entries).

## Where row values are accepted

Row values are only meaningful in specific contexts (enforced at resolve time in
`resolve.c`/`expr.c`, not by the grammar):

- **Comparisons** with `=`, `<`, `<=`, `>`, `>=`, `<>`, `!=`: both sides must be row values of the
  same arity. Comparison is element-wise lexicographic (NULL propagates per-element).
- **`IN` against a row set**: `(a, b) IN ((1,2),(3,4))` or `(a, b) IN (SELECT x, y FROM t)` or
  `(a, b) IN (VALUES (1,2),(3,4))`. The LHS arity must match the RHS row width.
- **`IN` against a subquery**: `X IN (SELECT …)` works for scalar `X` too — the subquery must
  return a single column. The grammar rule is `expr ::= expr in_op LP exprlist RP` and
  `expr ::= expr in_op LP select RP`.

A bare row value used outside these contexts (e.g. `SELECT (1,2)`) raises
`row value misused` at resolve time — it is a *parser-accepted* but *runtime-rejected* form.

## `IN (SELECT …)` and `IN (VALUES …)`

Upstream handles the subquery form via a separate rule:
```yacc
expr(A) ::= expr(A) in_op(N) LP select(Y) RP.  [IN]
expr(A) ::= expr(A) in_op(N) LP exprlist(Y) RP. [IN]
```

`exprlist` can be empty (`IN ()` simplifies to constant false/true). `select` covers both
`SELECT …` and `VALUES …` (a VALUES select body is a kind of `select_core`).

## Rustqlite mapping (M2.60)

- AST: `Expr::Row(Vec<Expr>)` (≥2 entries) for row-value literals.
- AST: `Expr::InSubquery { expr, subquery, negated }` for `X [NOT] IN (SELECT …)`. Kept separate
  from `Expr::In` (inline value list) so codegen can distinguish a row-set membership test
  (materialise subquery into an ephemeral index — M8.9) from a small inline value list.
- Grammar: `in_suffix = { K_NOT? ~ K_IN ~ "(" ~ in_rhs ~ ")" }` where
  `in_rhs = _{ select_stmt | (expr ~ ("," ~ expr)*)? }`. The `select_stmt` alternative is tried
  first so `IN (SELECT …)` / `IN (VALUES …)` are recognised as subqueries; the value-list
  alternative is the fallback (and `(expr ~ ("," ~ expr)*)?` admits the empty `IN ()` form).
- Grammar: `row_value = { "(" ~ expr ~ "," ~ expr ~ ("," ~ expr)* ~ ")" }` is a `primary`
  alternative, tried before the generic parenthesised-expression `"(" ~ expr ~ ")"` so `(a, b)`
  parses as a row value rather than backtracking.
- Execution is deferred (codegen raises a "not supported by the executor yet" error).