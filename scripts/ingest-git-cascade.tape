Output docs/assets/ingest-git-cascade.gif

Set Shell "zsh"
Set FontSize 13
Set Width 1360
Set Height 460
Set Theme "Dracula"
Set TypingSpeed 20ms
Set Padding 20

# ── hidden setup: release binary on PATH, throwaway store ──
# Run from the repo root, after `cargo build --release -p mushroomdb-cli`:
#   vhs scripts/ingest-git-cascade.tape
Hide
Type "export PATH=./target/release:$PATH"
Enter
Type "export S=/tmp/mushroom-git-demo"
Enter
Type "rm -rf $S"
Enter
Sleep 500ms
Type "clear"
Enter
Show

# ── 1. ingest this repository: authors, commits, files, and two rules ──
Type "mushroomdb ingest-git $S . --exclude 'target/' --exclude 'ui/node_modules/'"
Enter
Sleep 3s

# ── 2. co_changed rule: the ingest code, its tests, and its docs move together ──
Type `mushroomdb query $S "MATCH (a:File {id:'crates/cli/src/ingest_git.rs'})-[r:CO_CHANGED]->(b:File) RETURN b.id, r.score"`
Enter
Sleep 3s

# ── 3. a new owner joins (placeholder identity, RFC 2606 example.com) ──
Type `mushroomdb query $S "CREATE (a:Author {id:'new-maintainer@example.com', name:'New Maintainer'}) RETURN a.name"`
Enter
Sleep 2s

# ── 4. they know nothing yet ──
Type `mushroomdb query $S "MATCH (au:Author {name:'New Maintainer'})-[k:KNOWS]->(f:File) RETURN f.id, k.score"`
Enter
Sleep 2.5s

# ── 5. hand them one file: TOP_AUTHOR moves and knows re-derives in the same write ──
Type `mushroomdb query $S "MATCH (f:File {id:'crates/cli/src/ingest_git.rs'}) SET f.top_author_id = 'new-maintainer@example.com' RETURN f.top_author_id"`
Enter
Sleep 2.5s

# ── 6. same query: they now know the file, its tests, and its docs ──
Type `mushroomdb query $S "MATCH (au:Author {name:'New Maintainer'})-[k:KNOWS]->(f:File) RETURN f.id, k.score"`
Enter
Sleep 4s

# ── hidden teardown ──
Hide
Type "rm -rf $S"
Enter
Show
