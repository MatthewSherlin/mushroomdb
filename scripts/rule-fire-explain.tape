Output docs/assets/rule-fire-explain.gif

Set Shell "zsh"
Set FontSize 14
Set Width 920
Set Height 500
Set Theme "Dracula"
Set TypingSpeed 20ms
Set Padding 20

# ── hidden setup: add binary to PATH, seed fresh demo store ──
Hide
Type "export PATH=/Users/nilrehsttam/Repos/graph-db/.claude/worktrees/product-sprint/target/release:$PATH"
Enter
Type "export STORE=/tmp/mshroom-gif-demo"
Enter
Type "rm -rf $STORE && mushroomdb demo $STORE > /dev/null 2>&1"
Enter
Sleep 500ms
Type "clear"
Enter
Show

# ── 1. skill_fit edges before the property change ──
Type `mushroomdb query $STORE "MATCH (p:Person)-[r:FIT]->(proj:Project) WHERE p.id = 'person-01' RETURN p.id, proj.id, r.score ORDER BY r.score DESC LIMIT 3"`
Enter
Sleep 2.5s

# ── 2. SET new skills — rule retracts stale edges and fires new ones ──
Type `mushroomdb query $STORE "MATCH (p:Person) WHERE p.id = 'person-01' SET p.skills = ['s05','s06','s07'] RETURN p.id, p.skills"`
Enter
Sleep 2s

# ── 3. same query, different projects: proj-01 gone, proj-05 now top fit ──
Type `mushroomdb query $STORE "MATCH (p:Person)-[r:FIT]->(proj:Project) WHERE p.id = 'person-01' RETURN p.id, proj.id, r.score ORDER BY r.score DESC LIMIT 3"`
Enter
Sleep 2.5s

# ── hidden: start HTTP server for explain endpoint ──
Hide
Type `mushroomdb serve $STORE --addr 127.0.0.1:19333 --no-ui > /dev/null 2>&1 &`
Enter
Sleep 1.5s
Show

# ── 4. explain: skill_fit edge retracted, FK structural rule survives ──
Type `curl -s 'http://127.0.0.1:19333/explain?a=person-01&b=proj-01' | jq -r '.[] | "rule=" + .rule + "  type=" + .edge_type'`
Enter
Sleep 3s
