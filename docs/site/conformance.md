# openCypher conformance

mushroomdb runs the openCypher Technology Compatibility Kit (TCK) against every change. The TCK is a suite of scenarios, each a query with the result, error or graph change it must give. This page is generated from one run: every number on it is a count of scenarios.

- TCK version: `2024.3`, vendored unmodified in `vendor/opencypher-tck/`.
- Generated: 2026-10-23 by `cargo run --release -p tck-runner -- --report docs/site/conformance.md`.
- The pass rate is passed / (passed + failed). Skipped scenarios are in neither: a scenario is skipped only when it uses a feature this release deliberately does not have (listed under [Skipped scenarios](#skipped-scenarios)).
- CI fails when the pass rate falls below `tck-baseline.txt`; the baseline is raised by an ordinary change when the rate rises.
- The Cypher this release supports is documented in [Cypher Query Reference](query.md).

## Summary

| | Scenarios |
|---|---|
| Passed | 2400 |
| Failed | 924 |
| Skipped | 573 |
| Total | 3897 |
| Pass rate | 72.2% (`0.7220`) |

A scenario that expects an error passes on any error, so the pass rate counts two kinds of pass. The strict rate counts only the passes that checked a result, plus the error passes whose error came from the phase the TCK names (compile time: the lexer, parser or planner; runtime: execution) and, when the engine's error names its openCypher kind (`VariableTypeConflict: …`), is that kind. CI holds both rates to `tck-baseline.txt`.

| Pass | Scenarios |
|---|---|
| Checked a result or side effects | 1883 |
| Raised an error the TCK expects | 517 |
| … in the phase (and of the kind) the TCK names | 497 |
| … in another phase or of another kind (any error accepted) | 20 |
| Strict pass rate | 71.6% (`0.7160`) |

## By area

| Area | Passed | Failed | Skipped | Pass rate |
|---|---|---|---|---|
| `clauses/call` | 2 | 0 | 50 | 100.0% |
| `clauses/create` | 32 | 44 | 2 | 42.1% |
| `clauses/delete` | 5 | 36 | 0 | 12.2% |
| `clauses/match` | 306 | 70 | 5 | 81.4% |
| `clauses/match-where` | 20 | 13 | 1 | 60.6% |
| `clauses/merge` | 29 | 45 | 1 | 39.2% |
| `clauses/remove` | 8 | 23 | 2 | 25.8% |
| `clauses/return` | 28 | 35 | 0 | 44.4% |
| `clauses/return-orderby` | 21 | 14 | 0 | 60.0% |
| `clauses/return-skip-limit` | 28 | 3 | 0 | 90.3% |
| `clauses/set` | 18 | 30 | 5 | 37.5% |
| `clauses/union` | 12 | 0 | 0 | 100.0% |
| `clauses/unwind` | 12 | 2 | 0 | 85.7% |
| `clauses/with` | 15 | 14 | 0 | 51.7% |
| `clauses/with-orderBy` | 187 | 79 | 26 | 70.3% |
| `clauses/with-skip-limit` | 4 | 5 | 0 | 44.4% |
| `clauses/with-where` | 6 | 12 | 1 | 33.3% |
| `expressions/aggregation` | 15 | 20 | 0 | 42.9% |
| `expressions/boolean` | 40 | 110 | 0 | 26.7% |
| `expressions/comparison` | 39 | 33 | 0 | 54.2% |
| `expressions/conditional` | 1 | 12 | 0 | 7.7% |
| `expressions/existentialSubqueries` | 0 | 0 | 10 | — |
| `expressions/graph` | 38 | 10 | 13 | 79.2% |
| `expressions/list` | 146 | 31 | 8 | 82.5% |
| `expressions/literals` | 83 | 48 | 0 | 63.4% |
| `expressions/map` | 42 | 2 | 0 | 95.5% |
| `expressions/mathematical` | 1 | 5 | 0 | 16.7% |
| `expressions/null` | 38 | 6 | 0 | 86.4% |
| `expressions/path` | 2 | 5 | 0 | 28.6% |
| `expressions/pattern` | 2 | 12 | 36 | 14.3% |
| `expressions/precedence` | 52 | 69 | 0 | 43.0% |
| `expressions/quantifier` | 596 | 8 | 0 | 98.7% |
| `expressions/string` | 28 | 4 | 0 | 87.5% |
| `expressions/temporal` | 519 | 72 | 413 | 87.8% |
| `expressions/typeConversion` | 25 | 22 | 0 | 53.2% |
| `useCases/countingSubgraphMatches` | 0 | 11 | 0 | 0.0% |
| `useCases/triadicSelection` | 0 | 19 | 0 | 0.0% |

## Skipped scenarios

| Rule | Scenarios | Reason |
|---|---|---|
| `time-types` | 439 | Time and LocalTime values, and DateTime with a named time zone, are not in 0.10 (Date, LocalDateTime, offset DateTime and Duration are, and run) |
| `user-procedures` | 50 | registers a test procedure (`there exists a procedure`); 0.10 has built-in procedures only and no way to declare one |
| `pattern-predicate` | 46 | pattern predicates (a relationship pattern used as an expression) are not in 0.10 |
| `multi-label` | 27 | a node has one label in 0.10; more than one label (n:A:B, n:A\|B, n:A&B) is not supported |
| `subquery-expression` | 10 | EXISTS { … }, COUNT { … } and COLLECT { … } subqueries are not in 0.10 |
| `ignore` | 1 | tagged @ignore by the TCK itself |

## Failures by cause

Each failing scenario is counted once, under the first step that failed.

| Kind | Scenarios |
|---|---|
| parser gap | 425 |
| semantics difference | 178 |
| ruled deviation | 111 |
| runtime error (semantics difference) | 90 |
| named unsupported feature | 58 |
| setup: parser gap | 33 |
| error not raised (missing validation) | 23 |
| setup: named unsupported feature | 4 |
| setup: runtime error | 2 |

| Cause | Kind | Scenarios | First scenario |
|---|---|---|---|
| ruled deviation: 0.10 keeps boolean coercion (AND/OR/XOR/NOT accept non-booleans) | ruled deviation | 110 | expressions/boolean/Boolean1.feature: [8] Fail on conjunction of at least one non-booleans (example 1) |
| unexpected error: parse: expected … in a relationship pattern (found Dash) | parser gap | 89 | clauses/delete/Delete3.feature: [1] Detach deleting paths |
| wrong result: column names differ | semantics difference | 67 | clauses/match/Match9.feature: [5] Variable length pattern with label predicate on both sides |
| unexpected error: parse: expected variable or … in aggregate argument (found LParen) | parser gap | 60 | clauses/return/Return6.feature: [16] Aggregation on complex expressions |
| a write without RETURN returns its counter row (the TCK expects no rows) | semantics difference | 58 | clauses/create/Create1.feature: [1] Create a single node |
| unexpected error: parse: expected RETURN (found Ident(…)) | parser gap | 56 | clauses/remove/Remove1.feature: [5] Ignore null when removing property from a node |
| wrong result: values differ | semantics difference | 41 | clauses/create/Create6.feature: [6] Aggregating in `RETURN` after creating nodes affects the result set but not the side effects |
| unexpected error: parse: unexpected tokens after UNION query (found Ident(…)) | parser gap | 27 | expressions/literals/Literals3.feature: [1] Return a short positive hexadecimal integer |
| expected TypeError not raised | error not raised (missing validation) | 23 | clauses/set/Set1.feature: [10] Failing when setting a list of maps as a property |
| unexpected error: refused as not supported: a MATCH, UNWIND or WITH clause after a write clause in the same statement | named unsupported feature | 21 | clauses/create/Create3.feature: [2] WITH-CREATE |
| setup failed: lex: illegal character … at position # | setup: parser gap | 19 | useCases/triadicSelection/TriadicSelection1.feature: [1] Handling triadic friend of a friend |
| unexpected error: parse: zero-length variable-length paths are not supported; minimum hop count is # (found RBracket) | parser gap | 18 | clauses/match/Match4.feature: [3] Zero-length variable length pattern in the middle of the pattern |
| unexpected error: parse: expected SET, REMOVE or DELETE after MATCH [WHERE]; combined MATCH…RETURN is a read query, not a write … | parser gap | 16 | clauses/delete/Delete2.feature: [2] Delete optionally matched relationship |
| setup failed: parse: expected … in CREATE node pattern (found Slash) | setup: parser gap | 14 | clauses/with-orderBy/WithOrderBy4.feature: [1] Sort by a projected expression |
| unexpected error: parse: expected … to close a list subscript (found Dot) | parser gap | 13 | expressions/list/List2.feature: [1] List slice |
| unexpected error: refused as not supported: SET n:Label | named unsupported feature | 13 | clauses/merge/Merge2.feature: [1] Merge node with label add label on create |
| unexpected error: execute: division by zero | runtime error (semantics difference) | 12 | clauses/return-orderby/ReturnOrderBy1.feature: [11] ORDER BY should order distinct types in the expected order |
| unexpected error: execute: date(): cannot read … as a Date | runtime error (semantics difference) | 11 | expressions/temporal/Temporal10.feature: [9] Should handle large durations |
| unexpected error: plan: duplicate RETURN column … | runtime error (semantics difference) | 10 | expressions/null/Null1.feature: [1] Property null check on non-null node |
| wrong result: row count differs | semantics difference | 10 | clauses/match/Match6.feature: [14] Named path with undirected fixed variable length pattern |
| unexpected error: parse: expected operand (property, literal, or parameter) (found Dot) | parser gap | 9 | expressions/list/List2.feature: [3] List slice with implicit start |
| unexpected error: refused as not supported: REMOVE n:Label | named unsupported feature | 9 | clauses/remove/Remove2.feature: [1] Remove a single label from a node with a single label |
| unexpected error: parse: unexpected tokens after DELETE (found Return) | parser gap | 8 | clauses/delete/Delete6.feature: [1] Limiting to zero results after deleting nodes affects the result set but not the side effects |
| unexpected error: execute: datetime(): cannot read … as a DateTime | runtime error (semantics difference) | 7 | expressions/temporal/Temporal2.feature: [5] Should parse date time from string (example 2) |
| unexpected error: execute: localdatetime(): cannot read … as a LocalDateTime | runtime error (semantics difference) | 7 | expressions/temporal/Temporal10.feature: [10] Should handle large durations in seconds |
| unexpected error: parse: CASE requires at least one WHEN ... THEN branch (found Int(#)) | parser gap | 7 | expressions/conditional/Conditional2.feature: [1] Simple cases over integers (example 2) |
| unexpected error: parse: unary minus only applies to numeric literals (found Dot) | parser gap | 7 | expressions/literals/Literals5.feature: [10] Return a negative zero float without integer digits |
| unexpected error: plan: cannot return variable-length relationship variable … bare; return r.length, or name the path (p = …) an… | runtime error (semantics difference) | 6 | clauses/match/Match4.feature: [1] Handling fixed-length variable length pattern |
| unexpected error: refused as not supported: pattern comprehension [ | named unsupported feature | 6 | expressions/list/List6.feature: [7] Using size of pattern comprehension to test existence |
| unexpected error: refused as not supported: pattern predicate | named unsupported feature | 6 | expressions/pattern/Pattern2.feature: [1] Return a pattern comprehension |
| unexpected error: execute: duration(): cannot read … as a Duration | runtime error (semantics difference) | 5 | expressions/temporal/Temporal2.feature: [7] Should parse duration from string (example 2) |
| unexpected error: parse: UnknownFunction: unknown function …; supported: toLower, toUpper, size, coalesce, type, abs, round, tex… | parser gap | 5 | clauses/with/With6.feature: [4] Implicit grouping with single path variable as grouping key and single aggregation |
| unexpected error: parse: expected RETURN (found Le) | parser gap | 5 | expressions/comparison/Comparison3.feature: [2] Handling numerical ranges 2 |
| unexpected error: parse: variable-length paths are capped at # hops | parser gap | 5 | clauses/match/Match5.feature: [16] Handling lower bounded variable length match 1 |
| unexpected error: plan: unbound variable … in ORDER BY in aggregate WITH | runtime error (semantics difference) | 5 | clauses/with-orderBy/WithOrderBy2.feature: [22] Sort by an expression that is only partially orderable on a non-distinct binding table, but used as a grouping key (example 1) |
| unexpected error: execute: unbound variable … | runtime error (semantics difference) | 4 | clauses/match/Match7.feature: [15] Variable length patterns and nulls |
| unexpected error: parse: DELETE …: variable is not bound as a relationship or node in any MATCH pattern | parser gap | 4 | clauses/delete/Delete2.feature: [1] Delete relationships |
| unexpected error: parse: collect() is only supported as a top-level RETURN/WITH aggregate | parser gap | 4 | clauses/return/Return6.feature: [5] Aggregates inside normal functions |
| unexpected error: parse: expected RETURN (found Lt) | parser gap | 4 | expressions/comparison/Comparison3.feature: [1] Handling numerical ranges 1 |
| unexpected error: parse: expected RETURN (found Set) | parser gap | 4 | clauses/set/Set1.feature: [8] Ignore null when setting property |

### Scenarios that ran and failed one check

A scenario whose query ran is checked to the end even after a check fails. These failed exactly one check (a failed setup or a query that raised an error leaves the rest unchecked, so those are not counted here).

| Cause | Scenarios |
|---|---|
| ruled deviation: 0.10 keeps boolean coercion (AND/OR/XOR/NOT accept non-booleans) | 110 |
| a write without RETURN returns its counter row (the TCK expects no rows) | 54 |
| wrong result: column names differ | 51 |
| wrong result: values differ | 41 |
| expected TypeError not raised | 23 |
| wrong result: row count differs | 10 |
| wrong result: row order differs | 2 |
| ruled deviation: 0.10 keeps boolean coercion (a node as a WHERE predicate reads as true) | 1 |

## Expected errors

A scenario that expects an error passes on any error. This table shows, for the ones that passed that way, the stage the engine's error came from: a TCK `SyntaxError` met by an `execute:` error, or a runtime error met by a `parse:` error, passed for a different reason than the TCK names.

| TCK error | Phase | Passed | Engine `lex:` / `parse:` | Engine `plan:` / `execute:` | Other |
|---|---|---|---|---|---|
| `ArgumentError` | runtime | 32 | 0 | 32 | 0 |
| `ConstraintVerificationFailed` | runtime | 1 | 0 | 0 | 1 |
| `EntityNotFound` | runtime | 3 | 3 | 0 | 0 |
| `ProcedureError` | compile time | 2 | 0 | 2 | 0 |
| `SemanticError` | runtime | 2 | 1 | 0 | 1 |
| `SyntaxError` | compile time | 433 | 376 | 57 | 0 |
| `SyntaxError` | runtime | 6 | 0 | 6 | 0 |
| `TypeError` | any time | 18 | 0 | 18 | 0 |
| `TypeError` | compile time | 12 | 12 | 0 | 0 |
| `TypeError` | runtime | 8 | 0 | 8 | 0 |

## Functions

Every function a TCK query calls. Supported means the engine knows the name (a call of it does not fail as an unknown function); the counts are the scenarios whose queries call it.

| Function | Supported | TCK scenarios passed / run | Skipped |
|---|---|---|---|
| `abs` | yes | 2 / 4 | 0 |
| `avg` | yes | 1 / 6 | 0 |
| `ceil` | yes | 0 / 1 | 0 |
| `coalesce` | yes | 28 / 30 | 0 |
| `collect` | yes | 11 / 96 | 2 |
| `count` | yes | 94 / 183 | 3 |
| `date` | yes | 153 / 177 | 45 |
| `date.realtime` | yes | 1 / 1 | 0 |
| `date.statement` | yes | 1 / 1 | 0 |
| `date.transaction` | yes | 1 / 1 | 0 |
| `date.truncate` | yes | 51 / 51 | 0 |
| `datetime` | yes | 231 / 251 | 169 |
| `datetime.fromepoch` | yes | 1 / 1 | 0 |
| `datetime.fromepochmillis` | yes | 1 / 1 | 0 |
| `datetime.realtime` | yes | 1 / 1 | 0 |
| `datetime.statement` | yes | 1 / 1 | 0 |
| `datetime.transaction` | yes | 1 / 1 | 0 |
| `datetime.truncate` | yes | 74 / 74 | 31 |
| `duration` | yes | 46 / 74 | 18 |
| `duration.between` | yes | 10 / 15 | 18 |
| `duration.indays` | yes | 10 / 10 | 12 |
| `duration.inmonths` | yes | 15 / 15 | 12 |
| `duration.inseconds` | yes | 13 / 15 | 34 |
| `endnode` | no | 0 / 1 | 0 |
| `foo` | no | 1 / 1 | 0 |
| `head` | yes | 1 / 5 | 0 |
| `keys` | yes | 19 / 28 | 0 |
| `labels` | yes | 5 / 12 | 10 |
| `last` | yes | 0 / 1 | 0 |
| `length` | no | 2 / 8 | 0 |
| `localdatetime` | yes | 208 / 234 | 96 |
| `localdatetime.realtime` | yes | 1 / 1 | 0 |
| `localdatetime.statement` | yes | 1 / 1 | 0 |
| `localdatetime.transaction` | yes | 1 / 1 | 0 |
| `localdatetime.truncate` | yes | 74 / 74 | 0 |
| `localtime` | yes | 0 / 0 | 144 |
| `localtime.realtime` | yes | 0 / 0 | 1 |
| `localtime.statement` | yes | 0 / 0 | 1 |
| `localtime.transaction` | yes | 0 / 0 | 1 |
| `localtime.truncate` | yes | 0 / 0 | 44 |
| `max` | yes | 22 / 29 | 0 |
| `min` | yes | 0 / 9 | 0 |
| `nodes` | no | 1 / 12 | 0 |
| `percentilecont` | yes | 6 / 6 | 0 |
| `percentiledisc` | yes | 6 / 7 | 0 |
| `properties` | yes | 6 / 7 | 0 |
| `rand` | yes | 64 / 65 | 0 |
| `range` | yes | 77 / 93 | 2 |
| `relationships` | no | 0 / 7 | 0 |
| `reverse` | yes | 63 / 64 | 0 |
| `sign` | yes | 0 / 1 | 0 |
| `size` | yes | 132 / 151 | 10 |
| `split` | yes | 1 / 2 | 0 |
| `sqrt` | yes | 0 / 1 | 0 |
| `startnode` | no | 0 / 1 | 0 |
| `substring` | yes | 1 / 1 | 0 |
| `sum` | yes | 0 / 31 | 0 |
| `tail` | yes | 0 / 9 | 0 |
| `time` | yes | 0 / 0 | 149 |
| `time.realtime` | yes | 0 / 0 | 1 |
| `time.statement` | yes | 0 / 0 | 1 |
| `time.transaction` | yes | 0 / 0 | 1 |
| `time.truncate` | yes | 0 / 0 | 48 |
| `toboolean` | yes | 4 / 10 | 0 |
| `tofloat` | yes | 5 / 11 | 0 |
| `tointeger` | yes | 8 / 18 | 0 |
| `tolower` | yes | 0 / 1 | 0 |
| `tostring` | yes | 22 / 29 | 3 |
| `type` | yes | 13 / 16 | 1 |

## By feature file

| Feature | Passed | Failed | Skipped |
|---|---|---|---|
| `clauses/call/Call1.feature` | 2 | 0 | 14 |
| `clauses/call/Call2.feature` | 0 | 0 | 6 |
| `clauses/call/Call3.feature` | 0 | 0 | 6 |
| `clauses/call/Call4.feature` | 0 | 0 | 2 |
| `clauses/call/Call5.feature` | 0 | 0 | 19 |
| `clauses/call/Call6.feature` | 0 | 0 | 3 |
| `clauses/create/Create1.feature` | 12 | 6 | 2 |
| `clauses/create/Create2.feature` | 10 | 14 | 0 |
| `clauses/create/Create3.feature` | 2 | 11 | 0 |
| `clauses/create/Create4.feature` | 0 | 2 | 0 |
| `clauses/create/Create5.feature` | 0 | 5 | 0 |
| `clauses/create/Create6.feature` | 8 | 6 | 0 |
| `clauses/delete/Delete1.feature` | 2 | 6 | 0 |
| `clauses/delete/Delete2.feature` | 1 | 4 | 0 |
| `clauses/delete/Delete3.feature` | 0 | 2 | 0 |
| `clauses/delete/Delete4.feature` | 0 | 3 | 0 |
| `clauses/delete/Delete5.feature` | 2 | 7 | 0 |
| `clauses/delete/Delete6.feature` | 0 | 14 | 0 |
| `clauses/match-where/MatchWhere1.feature` | 11 | 4 | 0 |
| `clauses/match-where/MatchWhere2.feature` | 0 | 2 | 0 |
| `clauses/match-where/MatchWhere3.feature` | 3 | 0 | 0 |
| `clauses/match-where/MatchWhere4.feature` | 1 | 0 | 1 |
| `clauses/match-where/MatchWhere5.feature` | 0 | 4 | 0 |
| `clauses/match-where/MatchWhere6.feature` | 5 | 3 | 0 |
| `clauses/match/Match1.feature` | 85 | 0 | 1 |
| `clauses/match/Match2.feature` | 85 | 1 | 0 |
| `clauses/match/Match3.feature` | 17 | 11 | 2 |
| `clauses/match/Match4.feature` | 3 | 6 | 1 |
| `clauses/match/Match5.feature` | 15 | 14 | 0 |
| `clauses/match/Match6.feature` | 81 | 16 | 0 |
| `clauses/match/Match7.feature` | 18 | 12 | 1 |
| `clauses/match/Match8.feature` | 1 | 2 | 0 |
| `clauses/match/Match9.feature` | 1 | 8 | 0 |
| `clauses/merge/Merge1.feature` | 11 | 5 | 1 |
| `clauses/merge/Merge2.feature` | 4 | 2 | 0 |
| `clauses/merge/Merge3.feature` | 3 | 2 | 0 |
| `clauses/merge/Merge4.feature` | 1 | 1 | 0 |
| `clauses/merge/Merge5.feature` | 10 | 19 | 0 |
| `clauses/merge/Merge6.feature` | 0 | 6 | 0 |
| `clauses/merge/Merge7.feature` | 0 | 5 | 0 |
| `clauses/merge/Merge8.feature` | 0 | 1 | 0 |
| `clauses/merge/Merge9.feature` | 0 | 4 | 0 |
| `clauses/remove/Remove1.feature` | 4 | 3 | 0 |
| `clauses/remove/Remove2.feature` | 0 | 3 | 2 |
| `clauses/remove/Remove3.feature` | 4 | 17 | 0 |
| `clauses/return-orderby/ReturnOrderBy1.feature` | 8 | 4 | 0 |
| `clauses/return-orderby/ReturnOrderBy2.feature` | 7 | 7 | 0 |
| `clauses/return-orderby/ReturnOrderBy3.feature` | 0 | 1 | 0 |
| `clauses/return-orderby/ReturnOrderBy4.feature` | 2 | 0 | 0 |
| `clauses/return-orderby/ReturnOrderBy5.feature` | 1 | 0 | 0 |
| `clauses/return-orderby/ReturnOrderBy6.feature` | 3 | 2 | 0 |
| `clauses/return-skip-limit/ReturnSkipLimit1.feature` | 10 | 1 | 0 |
| `clauses/return-skip-limit/ReturnSkipLimit2.feature` | 15 | 2 | 0 |
| `clauses/return-skip-limit/ReturnSkipLimit3.feature` | 3 | 0 | 0 |
| `clauses/return/Return1.feature` | 2 | 0 | 0 |
| `clauses/return/Return2.feature` | 12 | 6 | 0 |
| `clauses/return/Return3.feature` | 2 | 1 | 0 |
| `clauses/return/Return4.feature` | 5 | 6 | 0 |
| `clauses/return/Return5.feature` | 1 | 4 | 0 |
| `clauses/return/Return6.feature` | 5 | 16 | 0 |
| `clauses/return/Return7.feature` | 1 | 1 | 0 |
| `clauses/return/Return8.feature` | 0 | 1 | 0 |
| `clauses/set/Set1.feature` | 6 | 5 | 0 |
| `clauses/set/Set2.feature` | 0 | 3 | 0 |
| `clauses/set/Set3.feature` | 0 | 3 | 5 |
| `clauses/set/Set4.feature` | 4 | 1 | 0 |
| `clauses/set/Set5.feature` | 4 | 1 | 0 |
| `clauses/set/Set6.feature` | 4 | 17 | 0 |
| `clauses/union/Union1.feature` | 5 | 0 | 0 |
| `clauses/union/Union2.feature` | 5 | 0 | 0 |
| `clauses/union/Union3.feature` | 2 | 0 | 0 |
| `clauses/unwind/Unwind1.feature` | 12 | 2 | 0 |
| `clauses/with-orderBy/WithOrderBy1.feature` | 57 | 23 | 16 |
| `clauses/with-orderBy/WithOrderBy2.feature` | 51 | 22 | 10 |
| `clauses/with-orderBy/WithOrderBy3.feature` | 77 | 16 | 0 |
| `clauses/with-orderBy/WithOrderBy4.feature` | 2 | 18 | 0 |
| `clauses/with-skip-limit/WithSkipLimit1.feature` | 1 | 1 | 0 |
| `clauses/with-skip-limit/WithSkipLimit2.feature` | 1 | 3 | 0 |
| `clauses/with-skip-limit/WithSkipLimit3.feature` | 2 | 1 | 0 |
| `clauses/with-where/WithWhere1.feature` | 1 | 3 | 0 |
| `clauses/with-where/WithWhere2.feature` | 0 | 2 | 0 |
| `clauses/with-where/WithWhere3.feature` | 3 | 0 | 0 |
| `clauses/with-where/WithWhere4.feature` | 1 | 0 | 1 |
| `clauses/with-where/WithWhere5.feature` | 0 | 4 | 0 |
| `clauses/with-where/WithWhere6.feature` | 0 | 1 | 0 |
| `clauses/with-where/WithWhere7.feature` | 1 | 2 | 0 |
| `clauses/with/With1.feature` | 3 | 3 | 0 |
| `clauses/with/With2.feature` | 1 | 1 | 0 |
| `clauses/with/With3.feature` | 0 | 1 | 0 |
| `clauses/with/With4.feature` | 5 | 2 | 0 |
| `clauses/with/With5.feature` | 0 | 2 | 0 |
| `clauses/with/With6.feature` | 5 | 4 | 0 |
| `clauses/with/With7.feature` | 1 | 1 | 0 |
| `expressions/aggregation/Aggregation1.feature` | 0 | 2 | 0 |
| `expressions/aggregation/Aggregation2.feature` | 0 | 12 | 0 |
| `expressions/aggregation/Aggregation3.feature` | 0 | 2 | 0 |
| `expressions/aggregation/Aggregation5.feature` | 1 | 1 | 0 |
| `expressions/aggregation/Aggregation6.feature` | 12 | 1 | 0 |
| `expressions/aggregation/Aggregation8.feature` | 2 | 2 | 0 |
| `expressions/boolean/Boolean1.feature` | 7 | 23 | 0 |
| `expressions/boolean/Boolean2.feature` | 7 | 23 | 0 |
| `expressions/boolean/Boolean3.feature` | 7 | 23 | 0 |
| `expressions/boolean/Boolean4.feature` | 11 | 41 | 0 |
| `expressions/boolean/Boolean5.feature` | 8 | 0 | 0 |
| `expressions/comparison/Comparison1.feature` | 33 | 10 | 0 |
| `expressions/comparison/Comparison2.feature` | 6 | 13 | 0 |
| `expressions/comparison/Comparison3.feature` | 0 | 9 | 0 |
| `expressions/comparison/Comparison4.feature` | 0 | 1 | 0 |
| `expressions/conditional/Conditional1.feature` | 1 | 0 | 0 |
| `expressions/conditional/Conditional2.feature` | 0 | 12 | 0 |
| `expressions/existentialSubqueries/ExistentialSubquery1.feature` | 0 | 0 | 4 |
| `expressions/existentialSubqueries/ExistentialSubquery2.feature` | 0 | 0 | 3 |
| `expressions/existentialSubqueries/ExistentialSubquery3.feature` | 0 | 0 | 3 |
| `expressions/graph/Graph3.feature` | 3 | 1 | 5 |
| `expressions/graph/Graph4.feature` | 9 | 2 | 0 |
| `expressions/graph/Graph5.feature` | 0 | 1 | 8 |
| `expressions/graph/Graph6.feature` | 12 | 2 | 0 |
| `expressions/graph/Graph7.feature` | 0 | 3 | 0 |
| `expressions/graph/Graph8.feature` | 8 | 0 | 0 |
| `expressions/graph/Graph9.feature` | 6 | 1 | 0 |
| `expressions/list/List1.feature` | 22 | 1 | 0 |
| `expressions/list/List11.feature` | 66 | 1 | 0 |
| `expressions/list/List12.feature` | 1 | 6 | 0 |
| `expressions/list/List2.feature` | 0 | 15 | 0 |
| `expressions/list/List3.feature` | 7 | 0 | 0 |
| `expressions/list/List4.feature` | 2 | 0 | 0 |
| `expressions/list/List5.feature` | 44 | 2 | 0 |
| `expressions/list/List6.feature` | 4 | 5 | 8 |
| `expressions/list/List9.feature` | 0 | 1 | 0 |
| `expressions/literals/Literals1.feature` | 6 | 0 | 0 |
| `expressions/literals/Literals2.feature` | 11 | 1 | 0 |
| `expressions/literals/Literals3.feature` | 5 | 11 | 0 |
| `expressions/literals/Literals4.feature` | 2 | 8 | 0 |
| `expressions/literals/Literals5.feature` | 7 | 20 | 0 |
| `expressions/literals/Literals6.feature` | 13 | 0 | 0 |
| `expressions/literals/Literals7.feature` | 15 | 5 | 0 |
| `expressions/literals/Literals8.feature` | 24 | 3 | 0 |
| `expressions/map/Map1.feature` | 18 | 1 | 0 |
| `expressions/map/Map2.feature` | 14 | 0 | 0 |
| `expressions/map/Map3.feature` | 10 | 1 | 0 |
| `expressions/mathematical/Mathematical11.feature` | 0 | 1 | 0 |
| `expressions/mathematical/Mathematical13.feature` | 0 | 1 | 0 |
| `expressions/mathematical/Mathematical2.feature` | 0 | 1 | 0 |
| `expressions/mathematical/Mathematical3.feature` | 1 | 0 | 0 |
| `expressions/mathematical/Mathematical8.feature` | 0 | 2 | 0 |
| `expressions/null/Null1.feature` | 14 | 3 | 0 |
| `expressions/null/Null2.feature` | 14 | 3 | 0 |
| `expressions/null/Null3.feature` | 10 | 0 | 0 |
| `expressions/path/Path1.feature` | 0 | 1 | 0 |
| `expressions/path/Path2.feature` | 0 | 3 | 0 |
| `expressions/path/Path3.feature` | 2 | 1 | 0 |
| `expressions/pattern/Pattern1.feature` | 2 | 1 | 36 |
| `expressions/pattern/Pattern2.feature` | 0 | 11 | 0 |
| `expressions/precedence/Precedence1.feature` | 13 | 59 | 0 |
| `expressions/precedence/Precedence2.feature` | 23 | 3 | 0 |
| `expressions/precedence/Precedence3.feature` | 4 | 7 | 0 |
| `expressions/precedence/Precedence4.feature` | 12 | 0 | 0 |
| `expressions/quantifier/Quantifier1.feature` | 103 | 2 | 0 |
| `expressions/quantifier/Quantifier10.feature` | 8 | 0 | 0 |
| `expressions/quantifier/Quantifier11.feature` | 22 | 0 | 0 |
| `expressions/quantifier/Quantifier12.feature` | 17 | 0 | 0 |
| `expressions/quantifier/Quantifier2.feature` | 104 | 2 | 0 |
| `expressions/quantifier/Quantifier3.feature` | 103 | 2 | 0 |
| `expressions/quantifier/Quantifier4.feature` | 103 | 2 | 0 |
| `expressions/quantifier/Quantifier5.feature` | 31 | 0 | 0 |
| `expressions/quantifier/Quantifier6.feature` | 21 | 0 | 0 |
| `expressions/quantifier/Quantifier7.feature` | 36 | 0 | 0 |
| `expressions/quantifier/Quantifier8.feature` | 31 | 0 | 0 |
| `expressions/quantifier/Quantifier9.feature` | 17 | 0 | 0 |
| `expressions/string/String1.feature` | 1 | 0 | 0 |
| `expressions/string/String10.feature` | 8 | 1 | 0 |
| `expressions/string/String11.feature` | 2 | 0 | 0 |
| `expressions/string/String3.feature` | 0 | 1 | 0 |
| `expressions/string/String4.feature` | 1 | 0 | 0 |
| `expressions/string/String8.feature` | 8 | 1 | 0 |
| `expressions/string/String9.feature` | 8 | 1 | 0 |
| `expressions/temporal/Temporal1.feature` | 154 | 0 | 53 |
| `expressions/temporal/Temporal10.feature` | 48 | 7 | 76 |
| `expressions/temporal/Temporal2.feature` | 5 | 28 | 20 |
| `expressions/temporal/Temporal3.feature` | 74 | 0 | 109 |
| `expressions/temporal/Temporal4.feature` | 13 | 12 | 14 |
| `expressions/temporal/Temporal5.feature` | 0 | 4 | 3 |
| `expressions/temporal/Temporal6.feature` | 12 | 2 | 3 |
| `expressions/temporal/Temporal7.feature` | 0 | 12 | 6 |
| `expressions/temporal/Temporal8.feature` | 14 | 7 | 6 |
| `expressions/temporal/Temporal9.feature` | 199 | 0 | 123 |
| `expressions/typeConversion/TypeConversion1.feature` | 4 | 6 | 0 |
| `expressions/typeConversion/TypeConversion2.feature` | 7 | 5 | 0 |
| `expressions/typeConversion/TypeConversion3.feature` | 5 | 6 | 0 |
| `expressions/typeConversion/TypeConversion4.feature` | 9 | 5 | 0 |
| `useCases/countingSubgraphMatches/CountingSubgraphMatches1.feature` | 0 | 11 | 0 |
| `useCases/triadicSelection/TriadicSelection1.feature` | 0 | 19 | 0 |
