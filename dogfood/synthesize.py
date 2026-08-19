"""Seeded marketplace-faithful node synthesizer (stdlib only).

`generate(n_talent, n_companies, n_jobs, seed)` yields node_dicts of shape
`{key, label, props}` — the same shape as `transform.load_fixtures`.

Embeddings reuse transform's SHA-256 hash chain (`synthetic_embedding`) with
the industry argument set to `{industry}|{primary_specialty}`, so the base
vector is per (industry, primary specialty) and the per-key jitter is
unchanged. Similar profiles therefore share a base; this is structured, not
noise.

Geo: 12 real metro centers (the brief names 10; Denver and Atlanta complete
the dozen). Jitter is a uniform disk of radius 30 km so LOCATION_FIT
clusters form inside a metro and distinct metros stay > 160.9 km apart.
"""

from __future__ import annotations

import math
import random
from typing import Iterator

from transform import synthetic_embedding

DEFAULT_N_TALENT = 70_000
DEFAULT_N_COMPANIES = 20_000
DEFAULT_N_JOBS = 10_000

INDUSTRIES: tuple[str, ...] = ("architecture", "interior-design", "both")
INDUSTRY_WEIGHTS: tuple[float, ...] = (0.45, 0.45, 0.10)

# marketplace ProjectSpecialty (13).
SPECIALTIES: tuple[str, ...] = (
    "single-family",
    "multi-family",
    "residential",
    "hospitality",
    "retail",
    "healthcare",
    "large-scale",
    "civic",
    "industrial",
    "landscape",
    "commercial",
    "educational",
    "institutional",
)

# marketplace DesignStyle (23).
DESIGN_STYLES: tuple[str, ...] = (
    "bohemian",
    "bright",
    "classic",
    "coastal",
    "colorful",
    "cottage",
    "contemporary",
    "country",
    "eclectic",
    "farmhouse",
    "glamorous",
    "industrial",
    "mediterranean",
    "mid-century",
    "minimal",
    "modern",
    "rustic",
    "scandinavian",
    "southern",
    "sustainable",
    "transitional",
    "traditional",
    "preppy",
)

# (name, lat, lon). 12 centers; all inter-metro haversine >> 160.9 km.
METROS: tuple[tuple[str, float, float], ...] = (
    ("NYC", 40.7128, -74.0060),
    ("LA", 34.0522, -118.2437),
    ("SF", 37.7749, -122.4194),
    ("Chicago", 41.8781, -87.6298),
    ("Miami", 25.7617, -80.1918),
    ("Austin", 30.2672, -97.7431),
    ("Seattle", 47.6062, -122.3321),
    ("Boston", 42.3601, -71.0589),
    ("London", 51.5074, -0.1278),
    ("Paris", 48.8566, 2.3522),
    ("Denver", 39.7392, -104.9903),
    ("Atlanta", 33.7490, -84.3880),
)

JITTER_KM = 30.0
_KM_PER_DEG = 111.32

_EXPERIENCE_YEARS = {
    1: (0, 2),
    2: (3, 5),
    3: (6, 9),
    4: (10, 14),
    5: (15, 40),
}

_COMPANY_SIZE = {
    1: "1-9",
    2: "10-50",
    3: "50-200",
    4: "200-499",
    5: "500+",
}


def generate(
    n_talent: int,
    n_companies: int,
    n_jobs: int,
    seed: int,
) -> Iterator[dict]:
    """Yield Talent, then Company, then Job node_dicts. Fully seeded."""
    rng = random.Random(seed)
    talent_ind = _industry_bag(rng, n_talent)
    company_ind = _industry_bag(rng, n_companies)
    job_ind = _industry_bag(rng, n_jobs)
    for i in range(n_talent):
        yield _talent(rng, i, talent_ind[i])
    for i in range(n_companies):
        yield _company(rng, i, company_ind[i])
    for i in range(n_jobs):
        yield _job(rng, i, job_ind[i], n_companies)


def _talent(rng: random.Random, i: int, industry: str) -> dict:
    key = f"talent-{i:06d}"
    cat = _categoricals(rng, i, industry)
    lo, hi = _EXPERIENCE_YEARS[cat["size_bucket"]]
    years = rng.randint(lo, hi)
    props = {
        "name": f"Talent {i}",
        "status": "published",
        "specialties": cat["specialties"],
        "design_styles": cat["design_styles"],
        "embedding": synthetic_embedding(key, f"{industry}|{cat['primary']}"),
        "industry": industry,
        "email": f"{key}@example.com",
        "user_id": f"user-{i:06d}",
        "years_of_experience": years,
        "size_bucket": cat["size_bucket"],
        "location": cat["location"],
        "address": cat["address"],
    }
    return {"key": key, "label": "Talent", "props": props}


def _company(rng: random.Random, i: int, industry: str) -> dict:
    key = f"company-{i:06d}"
    cat = _categoricals(rng, i, industry)
    props = {
        "name": f"Company {i}",
        "status": "published",
        "specialties": cat["specialties"],
        "design_styles": cat["design_styles"],
        "embedding": synthetic_embedding(key, f"{industry}|{cat['primary']}"),
        "industry": industry,
        "email": f"{key}@example.com",
        "user_id": f"user-c-{i:06d}",
        "company_size": _COMPANY_SIZE[cat["size_bucket"]],
        "size_bucket": cat["size_bucket"],
        "location": cat["location"],
        "address": cat["address"],
        "founded_year": rng.randint(1950, 2024),
    }
    return {"key": key, "label": "Company", "props": props}


def _job(rng: random.Random, i: int, industry: str, n_companies: int) -> dict:
    key = f"job-{i:06d}"
    cat = _categoricals(rng, i, industry)
    company_i = i % n_companies if n_companies else 0
    props = {
        "name": f"Job {i}",
        "status": "published",
        "specialties": cat["specialties"],
        "design_styles": cat["design_styles"],
        "industry": industry,
        "user_id": f"user-j-{i:06d}",
        "company_name": f"Company {company_i}",
        "company_id": f"company-{company_i:06d}",
        "company_size": _COMPANY_SIZE[cat["size_bucket"]],
        "size_bucket": cat["size_bucket"],
        "location": cat["location"],
        "address": cat["address"],
    }
    return {"key": key, "label": "Job", "props": props}


def _categoricals(rng: random.Random, i: int, industry: str) -> dict:
    metro_name, mlat, mlon = METROS[i % len(METROS)]
    bucket = (i % 5) + 1
    primary = SPECIALTIES[i % len(SPECIALTIES)]
    n_sec = 0 if i == 0 else (3 if i == 1 else rng.randint(0, 3))
    pool = [s for s in SPECIALTIES if s != primary]
    secondary = rng.sample(pool, n_sec)
    if i == 0:
        styles: list[str] = []
    elif i == 1:
        styles = rng.sample(list(DESIGN_STYLES), 5)
    else:
        k = rng.randint(0, 5)
        styles = rng.sample(list(DESIGN_STYLES), k) if k else []
    return {
        "primary": primary,
        "specialties": [primary, *secondary],
        "design_styles": styles,
        "size_bucket": bucket,
        "location": _jitter(rng, mlat, mlon),
        "address": metro_name,
        "industry": industry,
    }


def _industry_bag(rng: random.Random, n: int) -> list[str]:
    if n <= 0:
        return []
    counts = [int(round(w * n)) for w in INDUSTRY_WEIGHTS]
    counts[-1] = n - sum(counts[:-1])
    if counts[-1] < 0:
        overflow = -counts[-1]
        counts[-1] = 0
        for i in range(len(counts) - 1):
            take = min(counts[i], overflow)
            counts[i] -= take
            overflow -= take
            if overflow == 0:
                break
        counts[-1] = n - sum(counts[:-1])
    bag: list[str] = []
    for label, c in zip(INDUSTRIES, counts):
        bag.extend([label] * max(0, c))
    if len(bag) < n:
        bag.extend([INDUSTRIES[0]] * (n - len(bag)))
    del bag[n:]
    rng.shuffle(bag)
    return bag


def _jitter(rng: random.Random, lat: float, lon: float, radius_km: float = JITTER_KM) -> list[float]:
    r = radius_km * math.sqrt(rng.random())
    theta = 2.0 * math.pi * rng.random()
    north = r * math.cos(theta)
    east = r * math.sin(theta)
    dlat = north / _KM_PER_DEG
    coslat = math.cos(math.radians(lat))
    dlon = east / (_KM_PER_DEG * coslat) if abs(coslat) > 1e-12 else 0.0
    nlat = min(90.0, max(-90.0, lat + dlat))
    nlon = ((lon + dlon + 180.0) % 360.0) - 180.0
    return [nlat, nlon]
