# Data safety and evidence

A store is recovered from its snapshot and committed WAL records. The tests
below exercise specific failures and compare recovered data. They do not
simulate every filesystem, device or hardware failure. Choose a sync policy
and backup schedule for the data you can afford to lose; see
[Durability and crash recovery](durability.md) for operation and error handling.

## Crash recovery

<!-- ci-evidence: crash-recovery .github/workflows/ci.yml#test -->

The `test` job in the [CI workflow](https://github.com/MatthewSherlin/mushroomdb/actions/workflows/ci.yml)
runs the workspace tests, including
`recovery_is_consistent_at_every_crash_offset` and `recovery_byte_sweep_rules`
in [the deterministic recovery suite](https://github.com/MatthewSherlin/mushroomdb/blob/release/0.10/crates/sim-harness/tests/crash_recovery.rs).
They interrupt modeled writes at each byte offset of their workloads, reopen
the surviving state and check node/property consistency and edge invariants.
The rule-aware sweep also checks that rebuilding recovered rules is a no-op,
with its documented fire-counter normalization.

A recovery error or a mismatched invariant fails that evidence. These are
deterministic modeled faults, rather than a claim about every possible crash
or a test of physical power loss. The separate recall job measures search
recall and is not the crash-recovery proof.

## Old formats and V11 refusal

<!-- ci-evidence: format-compatibility .github/workflows/ci.yml#format-compat -->
<!-- ci-evidence: format-compatibility .github/workflows/ci.yml#test -->

The `format-compat` job opens historical fixtures and runs the
[V11 compatibility suite](https://github.com/MatthewSherlin/mushroomdb/blob/release/0.10/crates/core-api/tests/snapshot_v11.rs).
`test_v11_golden_fixture_opens` checks a real typed V11 fixture, including its
date and declarations. `test_v11_refused_by_09x_binary` invokes the released
0.9.2 binary and checks that refusing V11 leaves snapshot and WAL bytes
unchanged. `test_v9_store_still_opens_under_09x` is the old-reader positive
control. V10 behavior is also covered by the workspace `snapshot_v10` tests.

Writing a new property type, unique constraint or range-index declaration
opts a store into sticky V11; simply opening it does not. A store that uses
no V11 feature stays old-readable. See [Property value types](types.md) and
[Format stability](https://github.com/MatthewSherlin/mushroomdb/blob/release/0.10/docs/format-stability.md).

An unexpected old-reader success on V11, a failed old-readable control, or
changed files after refusal invalidates that evidence. The V9 byte comparison
uses a controlled fixture, normalizes only HashMap pair ordering in the
`LAST_CHANGE` and Mixed sections, and clears their derived checksums. Metadata
values and floating-point bits are preserved. It does not promise that every
rebuilt snapshot is byte-identical.

## Acknowledged writes and sync policy

<!-- ci-evidence: acknowledged-writes .github/workflows/ci.yml#format-compat -->
<!-- ci-evidence: acknowledged-writes .github/workflows/ci.yml#test -->

The `format-compat` job explicitly runs
[`write_survives_process_abort`](https://github.com/MatthewSherlin/mushroomdb/blob/release/0.10/crates/core-api/tests/process_abort_durability.rs),
including its ignored release test. A child aborts immediately after a write
returns successfully, without running destructors or a closing snapshot; its
parent reopens the store and checks the node and properties. This covers
single and batch writes under Strict, Batched and Relaxed policies.

That test proves the acknowledged data was not held only in process memory.
The kernel survives the abort, so it does not prove power-loss durability or
distinguish the policies' sync behavior.

Strict is the default engine policy. The workspace `test` job separately runs
[`strict_policy_syncs_each_single_op`](https://github.com/MatthewSherlin/mushroomdb/blob/release/0.10/crates/core-api/tests/batches.rs)
and [`write_batch_strict_always_fsyncs`](https://github.com/MatthewSherlin/mushroomdb/blob/release/0.10/crates/core-api/tests/group_commit.rs):
the counting filesystem observes two syncs for two direct writes, five for
five single-operation batches, and one for a five-operation batch. Explicit
Batched and Relaxed policies have different sync guarantees; Relaxed skips
WAL sync. Hardware durability still depends on the filesystem and device
honoring sync requests.

Lost acknowledged data or wrong sync counts fail these tests. A returned
error is not a blanket promise that nothing committed: follow the
[unknown-outcome guidance](durability.md) before retrying a write.

## Backup, restore and recovery points

<!-- ci-evidence: backup-restore .github/workflows/hosting-templates.yml#s3-backup -->

The unconditional `s3-backup` job in the
[hosting workflow](https://github.com/MatthewSherlin/mushroomdb/actions/workflows/hosting-templates.yml)
runs against a local object-storage emulator. The job checks its gated-test
log so that an absent emulator cannot turn the required tests into silent
skips.

[`s3_round_trip_with_a_multipart_snapshot` and `s3_restore_latest_and_a_point_in_time`](https://github.com/MatthewSherlin/mushroomdb/blob/release/0.10/crates/backup/tests/minio.rs)
exercise multipart snapshots, WAL segments, latest restoration and a selected
historical recovery point. Their assertions compare node/edge and commit
counts and selected node presence; CLI smoke checks also restore a ten-node
store explicitly and during service boot. A missing expected record or
incorrect compared count fails that evidence. This does not assert universal
all-property byte identity for every possible backup.

The available recovery point follows the last shipped snapshot and WAL
segments, so it can lag live writes. Emulator CI is separate from validating
a real deployment and its object-storage credentials. Follow
[the self-hosting guide](self-host.md) and verify a restore with your own
configuration.

## Interrupted snapshots and imports

<!-- ci-evidence: snapshot-interruption .github/workflows/ci.yml#test -->
<!-- ci-evidence: snapshot-interruption .github/workflows/ci.yml#format-compat -->

The workspace and format jobs run
[`a_snapshot_interrupted_before_its_wal_is_replaced_leaves_a_store_that_opens` and `an_interrupted_second_snapshot_leaves_a_store_that_opens`](https://github.com/MatthewSherlin/mushroomdb/blob/release/0.10/crates/core-api/tests/snapshot.rs).
The tests stop truncating and archiving snapshots at their defined
interruption points and compare reopened state, including the interrupted
replacement sequence. Snapshot intent and atomic replacement identify the
recoverable state; this does not mean every partially written file is itself
a valid snapshot.

Bulk import commits in batches. Completed batches survive a process crash; an
in-flight atomic batch does not leave half its operations. Rule-derived edges
are rebuilt on open from the recovered committed base data, as described by
[the importer](https://github.com/MatthewSherlin/mushroomdb/blob/release/0.10/crates/cli/src/import.rs).
The whole import file is not one transaction. A changed recovered model
invalidates the corresponding interruption test.

## Keeping the evidence current

The docs check validates the explicit claim-to-workflow/job references on this
page. A missing or renamed referenced job fails it; comments or similarly
named steps cannot stand in for a job definition. This is an existence check,
not proof that a job ran, passed, or avoided a conditional skip. Release
approval separately checks successful execution of the required evidence on
the release candidate. A failed evidence job leaves its associated claim
unverified for that candidate.
