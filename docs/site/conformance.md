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
| Passed | 712 |
| Failed | 2612 |
| Skipped | 573 |
| Total | 3897 |
| Pass rate | 21.4% (`0.2142`) |

## By area

| Area | Passed | Failed | Skipped | Pass rate |
|---|---|---|---|---|
| `clauses/call` | 2 | 0 | 50 | 100.0% |
| `clauses/create` | 14 | 62 | 2 | 18.4% |
| `clauses/delete` | 3 | 38 | 0 | 7.3% |
| `clauses/match` | 101 | 275 | 5 | 26.9% |
| `clauses/match-where` | 0 | 33 | 1 | 0.0% |
| `clauses/merge` | 13 | 61 | 1 | 17.6% |
| `clauses/remove` | 0 | 31 | 2 | 0.0% |
| `clauses/return` | 8 | 55 | 0 | 12.7% |
| `clauses/return-orderby` | 12 | 23 | 0 | 34.3% |
| `clauses/return-skip-limit` | 9 | 22 | 0 | 29.0% |
| `clauses/set` | 2 | 46 | 5 | 4.2% |
| `clauses/union` | 8 | 4 | 0 | 66.7% |
| `clauses/unwind` | 11 | 3 | 0 | 78.6% |
| `clauses/with` | 2 | 27 | 0 | 6.9% |
| `clauses/with-orderBy` | 63 | 203 | 26 | 23.7% |
| `clauses/with-skip-limit` | 0 | 9 | 0 | 0.0% |
| `clauses/with-where` | 0 | 18 | 1 | 0.0% |
| `expressions/aggregation` | 2 | 33 | 0 | 5.7% |
| `expressions/boolean` | 39 | 111 | 0 | 26.0% |
| `expressions/comparison` | 35 | 37 | 0 | 48.6% |
| `expressions/conditional` | 0 | 13 | 0 | 0.0% |
| `expressions/existentialSubqueries` | 0 | 0 | 10 | — |
| `expressions/graph` | 3 | 45 | 13 | 6.2% |
| `expressions/list` | 99 | 78 | 8 | 55.9% |
| `expressions/literals` | 83 | 48 | 0 | 63.4% |
| `expressions/map` | 33 | 11 | 0 | 75.0% |
| `expressions/mathematical` | 1 | 5 | 0 | 16.7% |
| `expressions/null` | 36 | 8 | 0 | 81.8% |
| `expressions/path` | 0 | 7 | 0 | 0.0% |
| `expressions/pattern` | 2 | 12 | 36 | 14.3% |
| `expressions/precedence` | 51 | 70 | 0 | 42.1% |
| `expressions/quantifier` | 12 | 592 | 0 | 2.0% |
| `expressions/string` | 2 | 30 | 0 | 6.2% |
| `expressions/temporal` | 45 | 546 | 413 | 7.6% |
| `expressions/typeConversion` | 21 | 26 | 0 | 44.7% |
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
| parser gap | 1088 |
| setup: parser gap | 761 |
| semantics difference | 284 |
| error not raised (missing validation) | 239 |
| ruled deviation | 110 |
| setup: runtime error | 96 |
| runtime error (semantics difference) | 31 |
| named unsupported feature | 2 |
| setup: named unsupported feature | 1 |

| Cause | Kind | Scenarios | First scenario |
|---|---|---|---|
| unexpected error: parse: expected … to close function call (found Where) | parser gap | 584 | expressions/quantifier/Quantifier1.feature: [1] None quantifier is always true on empty list |
| setup failed: parse: unexpected tokens after CREATE (found Comma) | setup: parser gap | 339 | clauses/delete/Delete3.feature: [1] Detach deleting paths |
| unexpected error: parse: unexpected tokens after UNION query (found LParen) | parser gap | 260 | expressions/temporal/Temporal1.feature: [11] Should construct date time from epoch |
| wrong result: values differ | semantics difference | 250 | clauses/with-orderBy/WithOrderBy1.feature: [9] Sort lists in ascending order |
| expected SyntaxError not raised | error not raised (missing validation) | 184 | clauses/create/Create1.feature: [13] Fail when creating a node that is already bound |
| setup failed: parse: CREATE node requires a label (found LBrace) | setup: parser gap | 174 | clauses/create/Create3.feature: [5] WITH-CREATE: Nodes are not created when aliases are applied to variable names |
| setup failed: parse: CREATE node requires a label (found RParen) | setup: parser gap | 149 | clauses/create/Create3.feature: [1] MATCH-CREATE |
| ruled deviation: 0.10 keeps boolean coercion (AND/OR/XOR/NOT accept non-booleans) | ruled deviation | 110 | expressions/boolean/Boolean1.feature: [8] Fail on conjunction of at least one non-booleans (example 1) |
| setup failed: CREATE/MERGE node requires a string `id` property (the node key) | setup: runtime error | 96 | clauses/create/Create2.feature: [10] Create a single self loop on an existing node |
| unexpected error: parse: expected variable or … in aggregate argument (found LParen) | parser gap | 59 | expressions/precedence/Precedence1.feature: [14] Exclusive disjunction takes precedence over inclusive disjunction in every combination of truth values |
| setup failed: parse: unexpected tokens after CREATE (found Create) | setup: parser gap | 46 | clauses/create/Create2.feature: [5] Create a single relationship between two existing nodes |
| expected TypeError not raised | error not raised (missing validation) | 33 | expressions/graph/Graph6.feature: [9] Fail when performing property access on a non-graph element (example 1) |
| wrong result: column names differ | semantics difference | 30 | clauses/return/Return6.feature: [17] Handle constants and parameters inside an expression which contains an aggregation expression |
| unexpected error: parse: unexpected tokens after UNION query (found Ident(…)) | parser gap | 27 | expressions/literals/Literals3.feature: [1] Return a short positive hexadecimal integer |
| unexpected error: parse: CREATE node requires a label (found LBrace) | parser gap | 24 | clauses/create/Create1.feature: [7] Create a single node with a property |
| expected ArgumentError not raised | error not raised (missing validation) | 22 | expressions/list/List11.feature: [5] Fail on invalid argument types for `range()` (example 1) |
| unexpected error: parse: CREATE node requires a label (found RParen) | parser gap | 22 | clauses/create/Create1.feature: [1] Create a single node |
| setup failed: lex: illegal character … at position # | setup: parser gap | 19 | useCases/triadicSelection/TriadicSelection1.feature: [1] Handling triadic friend of a friend |
| setup failed: parse: property … evaluated to null (a CREATE or MERGE property value cannot be null) (found RBrace) | setup: parser gap | 17 | clauses/with-orderBy/WithOrderBy1.feature: [41] Sort by a date time variable projected from a node property in ascending order (example 1) |
| unexpected error: parse: expected … to close a list subscript (found Dot) | parser gap | 13 | expressions/list/List2.feature: [1] List slice |
| setup failed: parse: expected … (found LBrace) | setup: parser gap | 12 | clauses/match/Match2.feature: [5] Match relationship with inline property value |
| unexpected error: parse: expected … in a relationship pattern (found Dash) | parser gap | 12 | clauses/delete/Delete3.feature: [2] Delete on null path |
| unexpected error: CREATE/MERGE node requires a string `id` property (the node key) | runtime error (semantics difference) | 11 | clauses/create/Create1.feature: [3] Create a single node with a label |
| unexpected error: parse: expected operand (property, literal, or parameter) (found Dot) | parser gap | 9 | expressions/list/List2.feature: [3] List slice with implicit start |
| unexpected error: execute: division by zero | runtime error (semantics difference) | 8 | expressions/comparison/Comparison1.feature: [8] Equality and inequality of NaN (example 1) |
| unexpected error: parse: CASE requires at least one WHEN ... THEN branch (found Int(#)) | parser gap | 7 | expressions/conditional/Conditional2.feature: [1] Simple cases over integers (example 2) |
| unexpected error: parse: unary minus only applies to numeric literals (found Dot) | parser gap | 7 | expressions/literals/Literals5.feature: [10] Return a negative zero float without integer digits |
| unexpected error: execute: duplicate RETURN column … | runtime error (semantics difference) | 6 | expressions/temporal/Temporal7.feature: [1] Should compare dates (example 1) |
| unexpected error: parse: CREATE node requires a label (found Dash) | parser gap | 5 | clauses/create/Create6.feature: [10] Skipping and limiting to a few results after creating relationships does not affect the result set nor the side effects |
| setup failed: parse: CREATE node requires a label (found end of input) | setup: parser gap | 4 | clauses/return/Return4.feature: [8] Support column renaming for aggregations |
| unexpected error: parse: expected RETURN (found Ident(…)) | parser gap | 4 | clauses/remove/Remove1.feature: [5] Ignore null when removing property from a node |
| unexpected error: parse: expected RETURN (found LParen) | parser gap | 4 | expressions/temporal/Temporal10.feature: [1] Should split between boundaries correctly (example 1) |
| unexpected error: parse: expected RETURN (found Set) | parser gap | 4 | clauses/set/Set1.feature: [8] Ignore null when setting property |
| unexpected error: parse: expected … or … in a list literal (found Ident(…)) | parser gap | 4 | expressions/literals/Literals7.feature: [5] Return a list containing a hexadecimal integer |
| unexpected error: execute: unbound variable … | runtime error (semantics difference) | 3 | expressions/graph/Graph9.feature: [3] `properties()` on null |
| unexpected error: parse: MERGE requires a property map with exactly one key (found RParen) | parser gap | 3 | clauses/merge/Merge1.feature: [2] Merge node with label |
| unexpected error: parse: collect() is only supported as a top-level RETURN/WITH aggregate | parser gap | 3 | clauses/return/Return6.feature: [6] Handle aggregates inside non-aggregate expressions |
| unexpected error: parse: expected RETURN (found Delete) | parser gap | 3 | clauses/delete/Delete1.feature: [4] Delete on null node |
| unexpected error: parse: expected field name after … (found Dot) | parser gap | 3 | expressions/list/List2.feature: [9] List slice with null range (example 1) |
| unexpected error: parse: unary minus only applies to numeric literals (found LParen) | parser gap | 3 | expressions/precedence/Precedence2.feature: [4] Numeric unary negative takes precedence over exponentiation |

### Scenarios that ran and failed one check

A scenario whose query ran is checked to the end even after a check fails. These failed exactly one check (a failed setup or a query that raised an error leaves the rest unchecked, so those are not counted here).

| Cause | Scenarios |
|---|---|
| wrong result: values differ | 250 |
| expected SyntaxError not raised | 184 |
| ruled deviation: 0.10 keeps boolean coercion (AND/OR/XOR/NOT accept non-booleans) | 110 |
| expected TypeError not raised | 33 |
| expected ArgumentError not raised | 22 |
| wrong result: column names differ | 15 |
| wrong result: row order differs | 2 |
| a write without RETURN returns its counter row (the TCK expects no rows) | 1 |
| wrong result: row count differs | 1 |

## Expected errors

A scenario that expects an error passes on any error. This table shows, for the ones that passed that way, the stage the engine's error came from: a TCK `SyntaxError` met by an `execute:` error, or a runtime error met by a `parse:` error, passed for a different reason than the TCK names.

| TCK error | Phase | Passed | Engine `lex:` / `parse:` | Engine `plan:` / `execute:` | Other |
|---|---|---|---|---|---|
| `ArgumentError` | runtime | 4 | 0 | 4 | 0 |
| `ProcedureError` | compile time | 2 | 0 | 2 | 0 |
| `SemanticError` | runtime | 2 | 2 | 0 | 0 |
| `SyntaxError` | compile time | 232 | 100 | 132 | 0 |
| `TypeError` | runtime | 1 | 1 | 0 | 0 |

## Functions

Every function a TCK query calls. Supported means the engine knows the name (a call of it does not fail as an unknown function); the counts are the scenarios whose queries call it.

| Function | Supported | TCK scenarios passed / run | Skipped |
|---|---|---|---|
| `abs` | yes | 0 / 4 | 0 |
| `avg` | yes | 1 / 6 | 0 |
| `ceil` | yes | 0 / 1 | 0 |
| `coalesce` | yes | 2 / 30 | 0 |
| `collect` | yes | 7 / 96 | 2 |
| `count` | yes | 13 / 183 | 3 |
| `date` | yes | 10 / 177 | 45 |
| `date.realtime` | no | 0 / 1 | 0 |
| `date.statement` | no | 0 / 1 | 0 |
| `date.transaction` | no | 0 / 1 | 0 |
| `date.truncate` | no | 0 / 51 | 0 |
| `datetime` | yes | 8 / 251 | 169 |
| `datetime.fromepoch` | no | 0 / 1 | 0 |
| `datetime.fromepochmillis` | no | 0 / 1 | 0 |
| `datetime.realtime` | no | 0 / 1 | 0 |
| `datetime.statement` | no | 0 / 1 | 0 |
| `datetime.transaction` | no | 0 / 1 | 0 |
| `datetime.truncate` | no | 0 / 74 | 31 |
| `duration` | yes | 20 / 74 | 18 |
| `duration.between` | no | 0 / 15 | 18 |
| `duration.indays` | no | 0 / 10 | 12 |
| `duration.inmonths` | no | 0 / 15 | 12 |
| `duration.inseconds` | no | 0 / 15 | 34 |
| `endnode` | no | 0 / 1 | 0 |
| `foo` | no | 0 / 1 | 0 |
| `head` | yes | 1 / 5 | 0 |
| `keys` | yes | 10 / 28 | 0 |
| `labels` | yes | 0 / 12 | 10 |
| `last` | yes | 0 / 1 | 0 |
| `length` | no | 0 / 8 | 0 |
| `localdatetime` | yes | 11 / 234 | 96 |
| `localdatetime.realtime` | no | 0 / 1 | 0 |
| `localdatetime.statement` | no | 0 / 1 | 0 |
| `localdatetime.transaction` | no | 0 / 1 | 0 |
| `localdatetime.truncate` | no | 0 / 74 | 0 |
| `localtime` | yes | 0 / 0 | 144 |
| `localtime.realtime` | no | 0 / 0 | 1 |
| `localtime.statement` | no | 0 / 0 | 1 |
| `localtime.transaction` | no | 0 / 0 | 1 |
| `localtime.truncate` | no | 0 / 0 | 44 |
| `max` | yes | 4 / 29 | 0 |
| `min` | yes | 0 / 9 | 0 |
| `nodes` | no | 1 / 12 | 0 |
| `percentilecont` | yes | 0 / 6 | 0 |
| `percentiledisc` | yes | 0 / 7 | 0 |
| `properties` | yes | 1 / 7 | 0 |
| `rand` | yes | 1 / 65 | 0 |
| `range` | yes | 50 / 93 | 2 |
| `relationships` | no | 0 / 7 | 0 |
| `reverse` | yes | 0 / 64 | 0 |
| `sign` | yes | 0 / 1 | 0 |
| `size` | yes | 6 / 151 | 10 |
| `split` | yes | 1 / 2 | 0 |
| `sqrt` | yes | 0 / 1 | 0 |
| `startnode` | no | 0 / 1 | 0 |
| `substring` | yes | 1 / 1 | 0 |
| `sum` | yes | 0 / 31 | 0 |
| `tail` | yes | 0 / 9 | 0 |
| `time` | yes | 0 / 0 | 149 |
| `time.realtime` | no | 0 / 0 | 1 |
| `time.statement` | no | 0 / 0 | 1 |
| `time.transaction` | no | 0 / 0 | 1 |
| `time.truncate` | no | 0 / 0 | 48 |
| `toboolean` | yes | 4 / 10 | 0 |
| `tofloat` | yes | 4 / 11 | 0 |
| `tointeger` | yes | 7 / 18 | 0 |
| `tolower` | yes | 0 / 1 | 0 |
| `tostring` | yes | 20 / 29 | 3 |
| `type` | yes | 0 / 16 | 1 |

## By feature file

| Feature | Passed | Failed | Skipped |
|---|---|---|---|
| `clauses/call/Call1.feature` | 2 | 0 | 14 |
| `clauses/call/Call2.feature` | 0 | 0 | 6 |
| `clauses/call/Call3.feature` | 0 | 0 | 6 |
| `clauses/call/Call4.feature` | 0 | 0 | 2 |
| `clauses/call/Call5.feature` | 0 | 0 | 19 |
| `clauses/call/Call6.feature` | 0 | 0 | 3 |
| `clauses/create/Create1.feature` | 7 | 11 | 2 |
| `clauses/create/Create2.feature` | 7 | 17 | 0 |
| `clauses/create/Create3.feature` | 0 | 13 | 0 |
| `clauses/create/Create4.feature` | 0 | 2 | 0 |
| `clauses/create/Create5.feature` | 0 | 5 | 0 |
| `clauses/create/Create6.feature` | 0 | 14 | 0 |
| `clauses/delete/Delete1.feature` | 1 | 7 | 0 |
| `clauses/delete/Delete2.feature` | 0 | 5 | 0 |
| `clauses/delete/Delete3.feature` | 0 | 2 | 0 |
| `clauses/delete/Delete4.feature` | 0 | 3 | 0 |
| `clauses/delete/Delete5.feature` | 2 | 7 | 0 |
| `clauses/delete/Delete6.feature` | 0 | 14 | 0 |
| `clauses/match-where/MatchWhere1.feature` | 0 | 15 | 0 |
| `clauses/match-where/MatchWhere2.feature` | 0 | 2 | 0 |
| `clauses/match-where/MatchWhere3.feature` | 0 | 3 | 0 |
| `clauses/match-where/MatchWhere4.feature` | 0 | 1 | 1 |
| `clauses/match-where/MatchWhere5.feature` | 0 | 4 | 0 |
| `clauses/match-where/MatchWhere6.feature` | 0 | 8 | 0 |
| `clauses/match/Match1.feature` | 12 | 73 | 1 |
| `clauses/match/Match2.feature` | 9 | 77 | 0 |
| `clauses/match/Match3.feature` | 2 | 26 | 2 |
| `clauses/match/Match4.feature` | 0 | 9 | 1 |
| `clauses/match/Match5.feature` | 0 | 29 | 0 |
| `clauses/match/Match6.feature` | 77 | 20 | 0 |
| `clauses/match/Match7.feature` | 1 | 29 | 1 |
| `clauses/match/Match8.feature` | 0 | 3 | 0 |
| `clauses/match/Match9.feature` | 0 | 9 | 0 |
| `clauses/merge/Merge1.feature` | 3 | 13 | 1 |
| `clauses/merge/Merge2.feature` | 1 | 5 | 0 |
| `clauses/merge/Merge3.feature` | 1 | 4 | 0 |
| `clauses/merge/Merge4.feature` | 0 | 2 | 0 |
| `clauses/merge/Merge5.feature` | 8 | 21 | 0 |
| `clauses/merge/Merge6.feature` | 0 | 6 | 0 |
| `clauses/merge/Merge7.feature` | 0 | 5 | 0 |
| `clauses/merge/Merge8.feature` | 0 | 1 | 0 |
| `clauses/merge/Merge9.feature` | 0 | 4 | 0 |
| `clauses/remove/Remove1.feature` | 0 | 7 | 0 |
| `clauses/remove/Remove2.feature` | 0 | 3 | 2 |
| `clauses/remove/Remove3.feature` | 0 | 21 | 0 |
| `clauses/return-orderby/ReturnOrderBy1.feature` | 8 | 4 | 0 |
| `clauses/return-orderby/ReturnOrderBy2.feature` | 0 | 14 | 0 |
| `clauses/return-orderby/ReturnOrderBy3.feature` | 0 | 1 | 0 |
| `clauses/return-orderby/ReturnOrderBy4.feature` | 1 | 1 | 0 |
| `clauses/return-orderby/ReturnOrderBy5.feature` | 0 | 1 | 0 |
| `clauses/return-orderby/ReturnOrderBy6.feature` | 3 | 2 | 0 |
| `clauses/return-skip-limit/ReturnSkipLimit1.feature` | 4 | 7 | 0 |
| `clauses/return-skip-limit/ReturnSkipLimit2.feature` | 5 | 12 | 0 |
| `clauses/return-skip-limit/ReturnSkipLimit3.feature` | 0 | 3 | 0 |
| `clauses/return/Return1.feature` | 1 | 1 | 0 |
| `clauses/return/Return2.feature` | 1 | 17 | 0 |
| `clauses/return/Return3.feature` | 0 | 3 | 0 |
| `clauses/return/Return4.feature` | 1 | 10 | 0 |
| `clauses/return/Return5.feature` | 0 | 5 | 0 |
| `clauses/return/Return6.feature` | 4 | 17 | 0 |
| `clauses/return/Return7.feature` | 1 | 1 | 0 |
| `clauses/return/Return8.feature` | 0 | 1 | 0 |
| `clauses/set/Set1.feature` | 2 | 9 | 0 |
| `clauses/set/Set2.feature` | 0 | 3 | 0 |
| `clauses/set/Set3.feature` | 0 | 3 | 5 |
| `clauses/set/Set4.feature` | 0 | 5 | 0 |
| `clauses/set/Set5.feature` | 0 | 5 | 0 |
| `clauses/set/Set6.feature` | 0 | 21 | 0 |
| `clauses/union/Union1.feature` | 4 | 1 | 0 |
| `clauses/union/Union2.feature` | 4 | 1 | 0 |
| `clauses/union/Union3.feature` | 0 | 2 | 0 |
| `clauses/unwind/Unwind1.feature` | 11 | 3 | 0 |
| `clauses/with-orderBy/WithOrderBy1.feature` | 17 | 63 | 16 |
| `clauses/with-orderBy/WithOrderBy2.feature` | 4 | 69 | 10 |
| `clauses/with-orderBy/WithOrderBy3.feature` | 40 | 53 | 0 |
| `clauses/with-orderBy/WithOrderBy4.feature` | 2 | 18 | 0 |
| `clauses/with-skip-limit/WithSkipLimit1.feature` | 0 | 2 | 0 |
| `clauses/with-skip-limit/WithSkipLimit2.feature` | 0 | 4 | 0 |
| `clauses/with-skip-limit/WithSkipLimit3.feature` | 0 | 3 | 0 |
| `clauses/with-where/WithWhere1.feature` | 0 | 4 | 0 |
| `clauses/with-where/WithWhere2.feature` | 0 | 2 | 0 |
| `clauses/with-where/WithWhere3.feature` | 0 | 3 | 0 |
| `clauses/with-where/WithWhere4.feature` | 0 | 1 | 1 |
| `clauses/with-where/WithWhere5.feature` | 0 | 4 | 0 |
| `clauses/with-where/WithWhere6.feature` | 0 | 1 | 0 |
| `clauses/with-where/WithWhere7.feature` | 0 | 3 | 0 |
| `clauses/with/With1.feature` | 0 | 6 | 0 |
| `clauses/with/With2.feature` | 0 | 2 | 0 |
| `clauses/with/With3.feature` | 0 | 1 | 0 |
| `clauses/with/With4.feature` | 0 | 7 | 0 |
| `clauses/with/With5.feature` | 0 | 2 | 0 |
| `clauses/with/With6.feature` | 2 | 7 | 0 |
| `clauses/with/With7.feature` | 0 | 2 | 0 |
| `expressions/aggregation/Aggregation1.feature` | 0 | 2 | 0 |
| `expressions/aggregation/Aggregation2.feature` | 0 | 12 | 0 |
| `expressions/aggregation/Aggregation3.feature` | 0 | 2 | 0 |
| `expressions/aggregation/Aggregation5.feature` | 0 | 2 | 0 |
| `expressions/aggregation/Aggregation6.feature` | 0 | 13 | 0 |
| `expressions/aggregation/Aggregation8.feature` | 2 | 2 | 0 |
| `expressions/boolean/Boolean1.feature` | 7 | 23 | 0 |
| `expressions/boolean/Boolean2.feature` | 7 | 23 | 0 |
| `expressions/boolean/Boolean3.feature` | 7 | 23 | 0 |
| `expressions/boolean/Boolean4.feature` | 10 | 42 | 0 |
| `expressions/boolean/Boolean5.feature` | 8 | 0 | 0 |
| `expressions/comparison/Comparison1.feature` | 29 | 14 | 0 |
| `expressions/comparison/Comparison2.feature` | 6 | 13 | 0 |
| `expressions/comparison/Comparison3.feature` | 0 | 9 | 0 |
| `expressions/comparison/Comparison4.feature` | 0 | 1 | 0 |
| `expressions/conditional/Conditional1.feature` | 0 | 1 | 0 |
| `expressions/conditional/Conditional2.feature` | 0 | 12 | 0 |
| `expressions/existentialSubqueries/ExistentialSubquery1.feature` | 0 | 0 | 4 |
| `expressions/existentialSubqueries/ExistentialSubquery2.feature` | 0 | 0 | 3 |
| `expressions/existentialSubqueries/ExistentialSubquery3.feature` | 0 | 0 | 3 |
| `expressions/graph/Graph3.feature` | 0 | 4 | 5 |
| `expressions/graph/Graph4.feature` | 0 | 11 | 0 |
| `expressions/graph/Graph5.feature` | 0 | 1 | 8 |
| `expressions/graph/Graph6.feature` | 2 | 12 | 0 |
| `expressions/graph/Graph7.feature` | 0 | 3 | 0 |
| `expressions/graph/Graph8.feature` | 0 | 8 | 0 |
| `expressions/graph/Graph9.feature` | 1 | 6 | 0 |
| `expressions/list/List1.feature` | 4 | 19 | 0 |
| `expressions/list/List11.feature` | 44 | 23 | 0 |
| `expressions/list/List12.feature` | 1 | 6 | 0 |
| `expressions/list/List2.feature` | 0 | 15 | 0 |
| `expressions/list/List3.feature` | 7 | 0 | 0 |
| `expressions/list/List4.feature` | 2 | 0 | 0 |
| `expressions/list/List5.feature` | 39 | 7 | 0 |
| `expressions/list/List6.feature` | 2 | 7 | 8 |
| `expressions/list/List9.feature` | 0 | 1 | 0 |
| `expressions/literals/Literals1.feature` | 6 | 0 | 0 |
| `expressions/literals/Literals2.feature` | 11 | 1 | 0 |
| `expressions/literals/Literals3.feature` | 5 | 11 | 0 |
| `expressions/literals/Literals4.feature` | 2 | 8 | 0 |
| `expressions/literals/Literals5.feature` | 7 | 20 | 0 |
| `expressions/literals/Literals6.feature` | 13 | 0 | 0 |
| `expressions/literals/Literals7.feature` | 15 | 5 | 0 |
| `expressions/literals/Literals8.feature` | 24 | 3 | 0 |
| `expressions/map/Map1.feature` | 12 | 7 | 0 |
| `expressions/map/Map2.feature` | 11 | 3 | 0 |
| `expressions/map/Map3.feature` | 10 | 1 | 0 |
| `expressions/mathematical/Mathematical11.feature` | 0 | 1 | 0 |
| `expressions/mathematical/Mathematical13.feature` | 0 | 1 | 0 |
| `expressions/mathematical/Mathematical2.feature` | 0 | 1 | 0 |
| `expressions/mathematical/Mathematical3.feature` | 1 | 0 | 0 |
| `expressions/mathematical/Mathematical8.feature` | 0 | 2 | 0 |
| `expressions/null/Null1.feature` | 13 | 4 | 0 |
| `expressions/null/Null2.feature` | 13 | 4 | 0 |
| `expressions/null/Null3.feature` | 10 | 0 | 0 |
| `expressions/path/Path1.feature` | 0 | 1 | 0 |
| `expressions/path/Path2.feature` | 0 | 3 | 0 |
| `expressions/path/Path3.feature` | 0 | 3 | 0 |
| `expressions/pattern/Pattern1.feature` | 2 | 1 | 36 |
| `expressions/pattern/Pattern2.feature` | 0 | 11 | 0 |
| `expressions/precedence/Precedence1.feature` | 12 | 60 | 0 |
| `expressions/precedence/Precedence2.feature` | 23 | 3 | 0 |
| `expressions/precedence/Precedence3.feature` | 4 | 7 | 0 |
| `expressions/precedence/Precedence4.feature` | 12 | 0 | 0 |
| `expressions/quantifier/Quantifier1.feature` | 3 | 102 | 0 |
| `expressions/quantifier/Quantifier10.feature` | 0 | 8 | 0 |
| `expressions/quantifier/Quantifier11.feature` | 0 | 22 | 0 |
| `expressions/quantifier/Quantifier12.feature` | 0 | 17 | 0 |
| `expressions/quantifier/Quantifier2.feature` | 3 | 103 | 0 |
| `expressions/quantifier/Quantifier3.feature` | 3 | 102 | 0 |
| `expressions/quantifier/Quantifier4.feature` | 3 | 102 | 0 |
| `expressions/quantifier/Quantifier5.feature` | 0 | 31 | 0 |
| `expressions/quantifier/Quantifier6.feature` | 0 | 21 | 0 |
| `expressions/quantifier/Quantifier7.feature` | 0 | 36 | 0 |
| `expressions/quantifier/Quantifier8.feature` | 0 | 31 | 0 |
| `expressions/quantifier/Quantifier9.feature` | 0 | 17 | 0 |
| `expressions/string/String1.feature` | 1 | 0 | 0 |
| `expressions/string/String10.feature` | 0 | 9 | 0 |
| `expressions/string/String11.feature` | 0 | 2 | 0 |
| `expressions/string/String3.feature` | 0 | 1 | 0 |
| `expressions/string/String4.feature` | 1 | 0 | 0 |
| `expressions/string/String8.feature` | 0 | 9 | 0 |
| `expressions/string/String9.feature` | 0 | 9 | 0 |
| `expressions/temporal/Temporal1.feature` | 24 | 130 | 53 |
| `expressions/temporal/Temporal10.feature` | 0 | 55 | 76 |
| `expressions/temporal/Temporal2.feature` | 4 | 29 | 20 |
| `expressions/temporal/Temporal3.feature` | 1 | 73 | 109 |
| `expressions/temporal/Temporal4.feature` | 4 | 21 | 14 |
| `expressions/temporal/Temporal5.feature` | 0 | 4 | 3 |
| `expressions/temporal/Temporal6.feature` | 12 | 2 | 3 |
| `expressions/temporal/Temporal7.feature` | 0 | 12 | 6 |
| `expressions/temporal/Temporal8.feature` | 0 | 21 | 6 |
| `expressions/temporal/Temporal9.feature` | 0 | 199 | 123 |
| `expressions/typeConversion/TypeConversion1.feature` | 4 | 6 | 0 |
| `expressions/typeConversion/TypeConversion2.feature` | 6 | 6 | 0 |
| `expressions/typeConversion/TypeConversion3.feature` | 4 | 7 | 0 |
| `expressions/typeConversion/TypeConversion4.feature` | 7 | 7 | 0 |
| `useCases/countingSubgraphMatches/CountingSubgraphMatches1.feature` | 0 | 11 | 0 |
| `useCases/triadicSelection/TriadicSelection1.feature` | 0 | 19 | 0 |
