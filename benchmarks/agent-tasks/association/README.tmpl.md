# The matching dataset

{{N_NODES}} entities — Talent, Company and Job — plus a {{N_CHANGES}}-change history
covering the {{N_DAYS}} days from {{DAY_FIRST}} to {{DAY_LAST}}. The base state below is
day 0 ({{DAY_FIRST}}); every later day is the base state with that day's changes, and
every change before it, applied in order.

Relationships between entities are **not stored**. They are *derived*: a pair of
entities is related exactly when one of the rules below holds for it. Working out
which pairs those are is the job.

## Fields

Every entity has:

| Field | Meaning |
|---|---|
| `key` | the identifier — `talent-000123`, `company-000045`, `job-000007` |
| `name` | a human label; carries no facts the rules use |
| `user_id` | the account behind the entity; carries no facts the rules use |
| `status` | `published`, `draft` or `archived` |
| `industry` | `architecture`, `interior-design` or `both` |
| `specialties` | a list of 1–4 of: {{SPECIALTIES}} |
| `design_styles` | a list of 0–5 of: {{DESIGN_STYLES}} |
| `size_bucket` | an integer 1–5. For a Talent it brackets experience; for a Company and a Job it brackets headcount |
| `location` | `[latitude, longitude]` in degrees. **This is the only geographic fact the rules use.** |
| `address` | the metro the entity was created in. A relocation changes `location` and leaves `address` as it was, so `location` — never `address` — decides a location rule |

Talent also has `email` and `years_of_experience` (an integer). Company also has
`email`, `company_size` (a headcount range as text) and `founded_year`. Job also
has `company_name`, `company_id` (the `key` of the Company that posted it) and
`company_size`.

There is one more generated field, and it is deliberately not in the glossary:
`embedding`, a 1536-number vector that **only the graph form has**. No rule below
uses it and no question asks about it.

## How a rule decides a pair

A rule names a source label, a destination label, a predicate and a relationship
type. It holds for an ordered pair (source, destination) when the predicate holds.
Four kinds of predicate appear:

- **`field_equal(f)`** — both entities have field `f` and the two values are equal.
- **`overlap(f, min)`** — both entities have list field `f`. Take each list as a
  **set** (duplicates collapse; values are compared exactly, no case folding). The
  score is the **Jaccard index**: the number of values in **both** lists divided by
  the number of values in **either** list —
  `|A ∩ B| / |A ∪ B|`. **The denominator is the union, not the smaller list.**
  The rule holds when the intersection is non-empty *and* that ratio is `>= min`.
- **`geo_radius(f, km)`** — both entities have `[lat, lon]` in field `f` and the
  great-circle distance between them is `<= km`, by the haversine formula with an
  Earth radius of 6371.0088 km.
- **`numeric_within(f, tolerance)`** — both entities have a number in field `f` and
  `|a - b| <= tolerance`. A tolerance of `0` means the two numbers must be equal.

A pair with a missing field on either side never matches.

## The rules

{{RULES}}

Rules apply to the state of the day being asked about. A change that breaks a
predicate retracts the relationship on that day; a change that satisfies one
creates it.

## The history

{{N_CHANGES}} changes, in order, over days 1–{{LAST_DAY}} (day 0 is the base state).
No entity is changed twice on the same day. Each change is one of:

| `op` | Meaning |
|---|---|
| `set_prop` | `key`'s field `field` becomes `value` (one of `status`, `industry`, `specialties`, `location`, `size_bucket`) |
| `insert_node` | a new entity appears; `node` carries its whole record |
| `delete_node` | `key` stops existing from that day onward, along with every relationship it was in |

No later change refers to a deleted entity. A Job's `company_id`, however, is a
plain field and not a change: a Job may name a Company that a later change
deleted, and after that day no Company with that key exists.

## Roles

Two roles restrict what an entity may be told about:

| Role | Sees |
|---|---|
{{ROLES}}

A role sees only entities carrying one of its labels, and only relationships whose
**both** ends it can see.

{{FORM_NOTE}}
