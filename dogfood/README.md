# mushroomdb dogfood — marketplace fixtures

Real-workload validation of the shipped product (Python bindings). Zero
engine / UI / bindings source changes.

## Fixtures

Copied verbatim from Matthew's marketplace-app test fixtures
(`talentco/talentcoai-backend/tests/migration/fixtures/`). Test data, no
production PII.

| file | nodes |
|---|---|
| `listings_talent.json` | 5 Talent |
| `listings_company.json` | 2 Company |
| `listings_opportunity.json` | 2 Job |
| `users.json` | 7 User |
| `applications.json` | 1 `INQUIRED` user→job |

Plan 10 Task 1 cited 10 talent / 4 companies / 4 jobs / ~13 inquiries —
those counts do not match the copied files. Tests assert the verbatim
counts. No `user_connections` JSON is in that fixture set (`CONNECTED` = 0).

No listing in this set carries lat/lon or design styles, so `LOCATION_FIT`
and `MATCHES_DESIGN_STYLE` produce empty edge sets (honest).

## Flattening

`transform.py` extracts the same fields as marketplace
`listingshub_transformers.py` (`transform_talent_listing` /
`transform_company_listing` / `transform_opportunity_listing`):

- `specialties` = primarySpecialty + secondarySpecialty (one list)
- `location` = `[lat, lon]` from latitude/longitude, location JSON, or geolocation
- `size_bucket` int 1–5: experience years (0–2/3–5/6–9/10–14/15+) ↔ company
  headcount (`<10/<50/<200/<500/else`; ListingsHub codes `110`…`50` and range
  strings like `10-50` / `50-200`)
- `embedding` (Talent/Company only): synthetic 1536-dim unit vector, seeded
  hash of industry + small per-key jitter. Documented as synthetic — fixtures
  have no Meili/OpenAI vectors. Same-industry pairs cosine ≫ 0.85.

## Rules (`rules.SIX_RULES`, 10 instances)

mushroomdb binds `(src_label, dst_label)`. The six kinds become:

| instance | predicate | pair |
|---|---|---|
| `industry_alignment_tc` / `_tj` | `FieldEqual(industry)` | Talent→Company, Talent→Job |
| `specialty_match_tc` / `_tj` | `Overlap(specialties, 0.15)` | Talent→Company, Talent→Job |
| `location_fit_tc` / `_tj` | `GeoRadius(location, 160.9)` | Talent→Company, Talent→Job |
| `similar_size_tc` | `NumericWithin(size_bucket, 1)` | Talent→Company |
| `matches_design_style_tc` | `Overlap(design_styles, 0.2)` | Talent→Company |
| `semantic_match_tc` | `VectorSimilar(embedding, 0.85)` | Talent→Company |

Composition gap (accepted): marketplace scores industry `both` at 0.8; our
`field_equal` is binary.

## Run

```
cd dogfood && ../bindings/python/.venv/bin/python -m pytest
```
