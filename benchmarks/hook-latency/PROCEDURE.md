# Prompt-hook latency on a large memory store — procedure

Written 2026-10-01, before the first run. No result existed when this was committed.

## The question

How long does the `UserPromptSubmit` hook take on a memory store of 100,000 notes, against its
5-second timeout? The hook is `mushroomdb recall <db>` with the prompt on stdin, one fresh
process per prompt. Its cost is opening the store plus one `recall`.

This is a measurement, not a gate. Nothing fails on its result.

## The store

Built by `cargo run --release -p mushroomdb-cli --example hook_latency_store -- <dir> 100000`:

- the default memory schema (`memory_defaults()`);
- 1,000 `Person` nodes, `person-0000` … `person-0999`, each with a two-word `name`;
- 100,000 `Note` nodes, each with a `text` of twelve words drawn from a fixed 2,000-word
  vocabulary by a seeded generator (seed 7), a `kind`, a `ts`, and one `ABOUT` edge to a person;
- one `snapshot()` at the end, so the hook opens from a snapshot as a long-lived store does.

The generator is deterministic: the same command builds the same store.

The people's names are drawn from the same 2,000 words as the notes — person `p` is named
word `p` and word `p + 1000` — so every name word also occurs in about 600 notes.

## What is timed

`scripts/measure-hook-latency.py --binary target/release/mushroomdb --db <dir> --runs 20`

For each of three prompts, 20 runs, each a new process, wall clock from spawn to exit:

1. a name the store holds — `"wlomipuka wlomipulo"`, the two words of `person-0421`'s name;
2. two common words of the vocabulary — `"wkakakaka wlokakaka"`, each in about 600 notes;
3. a prompt that matches nothing — `"zzqx vvkw"`.

The prompt reaches the hook as it does from the host: a JSON object on stdin whose `prompt`
field carries the text.

And, as the baseline that separates open cost from recall cost, 20 runs of
`mushroomdb stats <db>`, which opens the store and does almost nothing else.

File cache warm (one untimed run of each first). Release build. No build or test of the
operator's own runs during the timing; the machine is a shared developer laptop, so other load
is possible, and the machine and its load average are recorded in the result.

## What is reported

For each of the four: median, p95 and max in milliseconds, and for the three prompts the size
of the digest in bytes. The result file also records the commit, the binary's version, the
machine, the store's size on disk, and whether any run exceeded 5,000 ms.

Everything is reported, whatever it shows. If a run exceeds the timeout that is the finding,
and it goes in the ledger (row 38) and the changelog's known limits, not into a retry.

The run happens once. A run that crashes is fixed in its own commit, the amendment is noted
here with its date, and the run is made again; a run that completes is not repeated.

## The hook must not write

The hook opens the store read-only. The store's files are checksummed and its `stats` output
taken before the timed runs and again after; both must be identical, and the result's commit
says whether they were.

## What this does not measure

A cold file cache; a store with the identity preset applied; a store on a network volume; the
`SessionStart` brief, which has its own 3-second internal budget.
