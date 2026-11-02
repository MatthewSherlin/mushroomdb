"""Match people to projects with one rule, and keep the matches true.

No AI in this one, just the graph:

1. Declare once that a person fits a project when they share enough skills.
2. Insert people and projects. The rule writes the FIT edges.
3. Ask why a match exists, and preview a change without writing it.
4. Change someone's skills. The same write adds and removes their matches.

    pip install mushroomdb
    python team_matching.py
"""

from __future__ import annotations

import tempfile

from mushroomdb import GraphDb

PEOPLE = {
    "ana": ["rust", "storage"],
    "ben": ["python", "ml"],
    "chloe": ["python", "frontend"],
}

PROJECTS = {
    "search-rewrite": ["rust", "storage", "python"],
    "dashboard": ["frontend", "python"],
}


def build(db: GraphDb) -> None:
    db.create_rule(
        {
            "name": "skill_fit",
            "src_label": "Person",
            "dst_label": "Project",
            "predicate": {"kind": "overlap", "fields": ["skills"], "min": 0.5},
            "edge_type": "FIT",
            "weight_prop": "score",
            "max_edges": 5,
        }
    )
    for person, skills in PEOPLE.items():
        db.insert_node("Person", person, {"skills": skills})
    for project, skills in PROJECTS.items():
        db.insert_node("Project", project, {"skills": skills})


def matches(db: GraphDb) -> list[tuple[str, str]]:
    rows = db.query(
        "MATCH (p:Person)-[:FIT]->(o:Project) RETURN key(p) AS person, key(o) AS project "
        "ORDER BY person, project"
    )
    return [(row["person"], row["project"]) for row in rows]


def show(db: GraphDb) -> None:
    for person, project in matches(db):
        print(f"  {person} -> {project}")
    print()


def main() -> None:
    with tempfile.TemporaryDirectory() as directory, GraphDb.open(f"{directory}/store") as db:
        build(db)
        print("Matches, written by the rule:")
        show(db)

        print("Why does ana fit search-rewrite?")
        row = db.explain("ana", "search-rewrite")[0]
        print(f"  rule {row['rule']}: overlap on skills, score {row['weight']:.2f}\n")

        new_skills = ["python", "ml", "frontend"]
        preview = db.what_if_set_prop("ana", "skills", new_skills)
        print("If ana's skills changed, nothing written yet:")
        print(f"  would gain {sorted(edge['dst'] for edge in preview['gained'])}")
        print(f"  would lose {sorted(edge['dst'] for edge in preview['lost'])}\n")

        db.set_prop("ana", "skills", new_skills)
        print("After the write:")
        show(db)


if __name__ == "__main__":
    main()
