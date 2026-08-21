"""ListingsHub listing JSON → flat mushroomdb node dicts.

Field semantics follow the representative workload schema (talent / company /
opportunity flattening). Specialty merge, geo `[lat, lon]`, and
size-bucketization are the fixture adaptations for the matching workload.

Embeddings are synthetic (fixtures carry none): a deterministic 1536-dim
unit vector per Talent/Company, hashed from a fixed seed + industry with
small per-key jitter, so `semantic_match` fires on same-industry pairs.
"""

from __future__ import annotations

import hashlib
import json
import math
import re
from pathlib import Path
from typing import Any, Iterable, Optional

FIXTURE_FILES = (
    "listings_talent.json",
    "listings_company.json",
    "listings_opportunity.json",
    "users.json",
    "applications.json",
)

EMBED_DIM = 1536
EMBED_SEED = b"marketplace-dogfood-t1-v1"
_JITTER = 0.05

_HEADCOUNT_CODES = {
    "110": 1,
    "1020": 10,
    "2030": 20,
    "3040": 30,
    "4050": 40,
    "50": 50,
}


def load_fixtures(directory: str | Path) -> dict[str, list[dict]]:
    """Load the five fixture JSONs into `{label: [node_dict, ...]}`.

    node_dict = `{key, label, props}`. Labels: Talent, Company, Job, User.
    """
    root = Path(directory)
    talent = [_talent_node(raw) for raw in _read_json(root / "listings_talent.json")]
    company = [_company_node(raw) for raw in _read_json(root / "listings_company.json")]
    jobs = [_job_node(raw) for raw in _read_json(root / "listings_opportunity.json")]
    users = [_user_node(raw) for raw in _read_json(root / "users.json")]
    return {
        "Talent": talent,
        "Company": company,
        "Job": jobs,
        "User": users,
    }


def load_user_edges(directory: str | Path) -> list[tuple[str, str, str]]:
    """User-typed edges from the copied fixtures: INQUIRED, CONNECTED.

    INQUIRED: application customer (user id) → job listing id.
    CONNECTED: none — the five JSON fixtures do not include user_connections
    (ListingsHub transactions live in a separate CSV we do not copy).
    """
    root = Path(directory)
    users = {u["id"]: u for u in _read_json(root / "users.json") if isinstance(u, dict) and "id" in u}
    jobs = {j["id"] for j in _read_json(root / "listings_opportunity.json") if isinstance(j, dict) and "id" in j}
    edges: list[tuple[str, str, str]] = []
    for app in _read_json(root / "applications.json"):
        customer = _rel_id(app, "customer")
        listing = _rel_id(app, "listing")
        if customer in users and listing in jobs:
            edges.append(("INQUIRED", customer, listing))
    return edges


def synthetic_embedding(node_key: str, industry: Optional[str]) -> list[float]:
    """Deterministic 1536-dim unit vector. Same industry → cosine ≫ 0.85."""
    base = _hash_vec(EMBED_SEED + b"|ind|" + (industry or "").encode("utf-8"))
    jitter = _hash_vec(EMBED_SEED + b"|key|" + node_key.encode("utf-8"))
    mixed = [b + _JITTER * j for b, j in zip(base, jitter)]
    return _normalize(mixed)


def cosine(a: Iterable[float], b: Iterable[float]) -> Optional[float]:
    """Engine-equivalent cosine (finite, both norms > 0), clamped to 1.0."""
    av = list(a)
    bv = list(b)
    if len(av) != len(bv) or not av:
        return None
    dot = na2 = nb2 = 0.0
    for x, y in zip(av, bv):
        dot += x * y
        na2 += x * x
        nb2 += y * y
    na = math.sqrt(na2)
    nb = math.sqrt(nb2)
    if not (na > 0.0 and nb > 0.0):
        return None
    cos = dot / (na * nb)
    if not math.isfinite(cos):
        return None
    return min(1.0, cos)


def _talent_node(raw: dict) -> dict:
    listing_id, attrs, pd, author = _listing_parts(raw)
    industry = _clean_str(pd.get("industry"))
    years = _clean_experience(pd.get("experience"))
    props: dict[str, Any] = {
        "name": attrs.get("title") or "",
        "status": attrs.get("state") or "",
        "specialties": _merge_specialties(pd),
        "design_styles": _as_str_list(pd.get("style")),
        "embedding": synthetic_embedding(listing_id, industry),
    }
    _put(props, "industry", industry)
    _put(props, "email", _clean_str(pd.get("email")))
    _put(props, "user_id", author)
    _put(props, "years_of_experience", years)
    _put(props, "size_bucket", experience_bucket(years))
    _put(props, "location", _geo(attrs, pd))
    _put(props, "address", _address(pd))
    return {"key": listing_id, "label": "Talent", "props": props}


def _company_node(raw: dict) -> dict:
    listing_id, attrs, pd, author = _listing_parts(raw)
    industry = _normalize_industry(_clean_str(pd.get("industry")))
    size_raw = _clean_str(pd.get("companySize"))
    props: dict[str, Any] = {
        "name": attrs.get("title") or "",
        "status": attrs.get("state") or "",
        "specialties": _merge_specialties(pd),
        "design_styles": _as_str_list(pd.get("style")),
        "embedding": synthetic_embedding(listing_id, industry),
    }
    _put(props, "industry", industry)
    _put(props, "email", _clean_str(pd.get("email")))
    _put(props, "user_id", author)
    _put(props, "company_size", size_raw)
    _put(props, "size_bucket", company_size_bucket(size_raw))
    _put(props, "location", _geo(attrs, pd))
    _put(props, "address", _address(pd))
    return {"key": listing_id, "label": "Company", "props": props}


def _job_node(raw: dict) -> dict:
    listing_id, attrs, pd, author = _listing_parts(raw)
    size_raw = _clean_str(pd.get("companySize"))
    raw_company = _clean_str(pd.get("company") or pd.get("companyName"))
    company_name = raw_company.split(" | ")[0].strip() if raw_company else None
    props: dict[str, Any] = {
        "name": attrs.get("title") or "",
        "status": attrs.get("state") or "",
        "specialties": _merge_specialties(pd),
        "design_styles": _as_str_list(pd.get("style")),
    }
    _put(props, "industry", _clean_str(pd.get("industry")))
    _put(props, "user_id", author)
    _put(props, "company_name", company_name)
    _put(props, "company_size", size_raw)
    _put(props, "size_bucket", company_size_bucket(size_raw))
    _put(props, "location", _geo(attrs, pd))
    _put(props, "address", _address(pd))
    return {"key": listing_id, "label": "Job", "props": props}


def _user_node(raw: dict) -> dict:
    attrs = raw.get("attributes") or {}
    profile = attrs.get("profile") or {}
    props: dict[str, Any] = {}
    _put(props, "email", attrs.get("email"))
    _put(props, "first_name", profile.get("firstName"))
    _put(props, "last_name", profile.get("lastName"))
    _put(props, "state", attrs.get("state"))
    return {"key": raw["id"], "label": "User", "props": props}


def experience_bucket(years: Optional[int]) -> Optional[int]:
    """Experience tier 1–5 (T2's experience-tier scale)."""
    if years is None:
        return None
    if years <= 2:
        return 1
    if years <= 5:
        return 2
    if years <= 9:
        return 3
    if years <= 14:
        return 4
    return 5


def company_size_bucket(raw: Optional[str]) -> Optional[int]:
    """Company-size → 1–5 via headcount low-bound.

    Enum codes (`110`, `1020`, …, `50`) and range strings (`10-50`, `50-200`)
    both land on: <10→1, <50→2, <200→3, <500→4, else 5.
    """
    if not raw:
        return None
    s = str(raw).strip().lower()
    if s in _HEADCOUNT_CODES:
        return _headcount_bucket(_HEADCOUNT_CODES[s])
    if s.endswith("+"):
        try:
            return _headcount_bucket(int(s[:-1]))
        except ValueError:
            return None
    m = re.match(r"^(\d+)\s*-\s*(\d+)$", s)
    if m:
        return _headcount_bucket(int(m.group(1)))
    try:
        return _headcount_bucket(int(float(s)))
    except ValueError:
        return None


def _headcount_bucket(n: int) -> int:
    if n < 10:
        return 1
    if n < 50:
        return 2
    if n < 200:
        return 3
    if n < 500:
        return 4
    return 5


def _merge_specialties(pd: dict) -> list[str]:
    """primarySpecialty + secondarySpecialty → one `specialties` list."""
    parts: list[str] = []
    primary = _clean_str(pd.get("primarySpecialty"))
    if primary:
        parts.append(primary)
    parts.extend(_as_str_list(pd.get("secondarySpecialty")))
    seen: set[str] = set()
    out: list[str] = []
    for s in parts:
        if s not in seen:
            seen.add(s)
            out.append(s)
    return out


def _geo(attrs: dict, pd: dict) -> Optional[list[float]]:
    """`[lat, lon]` from latitude/longitude, location JSON, or geolocation."""
    for src in (pd, attrs):
        pair = _lat_lon_fields(src)
        if pair is not None:
            return pair
    loc = pd.get("location")
    parsed = _parse_geo(loc)
    if parsed is not None:
        return parsed
    geo = attrs.get("geolocation") or pd.get("geolocation")
    return _parse_geo(geo)


def _lat_lon_fields(src: dict) -> Optional[list[float]]:
    lat = src.get("latitude")
    lon = src.get("longitude")
    if lon is None:
        lon = src.get("lng")
    if lat is None or lon is None:
        return None
    try:
        pair = [float(lat), float(lon)]
    except (TypeError, ValueError):
        return None
    return pair if _valid_latlon(pair[0], pair[1]) else None


def _parse_geo(raw: Any) -> Optional[list[float]]:
    if raw is None:
        return None
    if isinstance(raw, str):
        try:
            raw = json.loads(raw)
        except json.JSONDecodeError:
            return None
    if not isinstance(raw, dict):
        return None
    lat = raw.get("lat")
    lon = raw.get("lon", raw.get("lng"))
    if lat is None or lon is None:
        return None
    try:
        pair = [float(lat), float(lon)]
    except (TypeError, ValueError):
        return None
    return pair if _valid_latlon(pair[0], pair[1]) else None


def _valid_latlon(lat: float, lon: float) -> bool:
    return math.isfinite(lat) and math.isfinite(lon) and -90.0 <= lat <= 90.0 and -180.0 <= lon <= 180.0


def _address(pd: dict) -> Optional[str]:
    location = pd.get("location")
    if isinstance(location, dict):
        selected = location.get("selectedPlace")
        if isinstance(selected, dict):
            addr = selected.get("address")
            if isinstance(addr, str) and addr.strip():
                return addr.strip()
        search = location.get("search")
        if isinstance(search, str) and search.strip():
            return search.strip()
    address = pd.get("address")
    if isinstance(address, dict):
        parts = [
            str(address[k]).strip()
            for k in ("addressLine1", "city", "state", "zip")
            if address.get(k) and str(address[k]).strip()
        ]
        if parts:
            return ", ".join(parts)
    if isinstance(address, str) and address.strip():
        return address.strip()
    return None


def _listing_parts(raw: dict) -> tuple[str, dict, dict, Optional[str]]:
    attrs = raw.get("attributes") or {}
    pd = attrs.get("publicData") or {}
    if not isinstance(pd, dict):
        pd = {}
    return raw["id"], attrs, pd, _rel_id(raw, "author")


def _rel_id(raw: dict, name: str) -> Optional[str]:
    rels = raw.get("relationships") or {}
    data = (rels.get(name) or {}).get("data") or {}
    ident = data.get("id")
    return ident if isinstance(ident, str) and ident else None


def _as_str_list(value: Any) -> list[str]:
    if value is None:
        return []
    if isinstance(value, list):
        return [str(v).strip() for v in value if v is not None and str(v).strip()]
    if isinstance(value, str) and value.strip():
        return [value.strip()]
    return []


def _clean_str(value: Any) -> Optional[str]:
    if value is None:
        return None
    if isinstance(value, list):
        for item in value:
            s = _clean_str(item)
            if s:
                return s
        return None
    s = str(value).strip().strip("\"'").strip()
    if not s or s.lower() in ("nan", "none", "null", "n/a"):
        return None
    return s


def _clean_experience(value: Any) -> Optional[int]:
    if value is None or value == "":
        return None
    if isinstance(value, list):
        value = value[0] if value else None
        if value is None:
            return None
    s = str(value).strip().strip("[]")
    if re.match(r"\d{4}-\d{2}-\d{2}", s):
        return None
    range_match = re.match(r"^(\d+)\s*-\s*(\d+)$", s)
    if range_match:
        low, high = int(range_match.group(1)), int(range_match.group(2))
        return (low + high) // 2
    try:
        val = int(float(s))
    except (TypeError, ValueError):
        return None
    return val if 0 <= val <= 60 else None


def _normalize_industry(raw: Optional[str]) -> Optional[str]:
    if not raw:
        return None
    valid = {"architecture", "interior-design", "both"}
    if raw in valid:
        return raw
    for part in raw.split(","):
        part = part.strip()
        if part in valid:
            return part
    return raw


def _hash_vec(parts: bytes) -> list[float]:
    out: list[float] = []
    i = 0
    while len(out) < EMBED_DIM:
        digest = hashlib.sha256(parts + i.to_bytes(4, "little")).digest()
        for j in range(0, 32, 4):
            u = int.from_bytes(digest[j : j + 4], "little")
            out.append((u / 2**32) * 2.0 - 1.0)
            if len(out) == EMBED_DIM:
                break
        i += 1
    return out


def _normalize(v: list[float]) -> list[float]:
    n = math.sqrt(sum(x * x for x in v))
    if n == 0.0:
        raise ValueError("zero vector")
    return [x / n for x in v]


def _put(props: dict[str, Any], key: str, value: Any) -> None:
    if value is not None:
        props[key] = value


def _read_json(path: Path) -> list:
    with path.open() as f:
        data = json.load(f)
    if not isinstance(data, list):
        raise ValueError(f"{path} is not a JSON list")
    return data
