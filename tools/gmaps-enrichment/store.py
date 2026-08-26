"""Schema and writes for the staging load.

WHY THE TABLES LIVE IN A `staging` SCHEMA
-----------------------------------------
`public` is the product schema, and cm-db's migration tests enforce four
invariants on it: every table carries created_at and updated_at, every foreign
key is single-column and indexed, UUID primary keys have no database default,
and only four extensions are installed. Those tests scope themselves to
`public` (see `crates/cm-db/tests/migrations.rs`).

The tables in this spec do not satisfy them — `gmaps_places` has
first/last_scraped_at rather than created/updated_at, and
`contractor_place_matches` has a composite key whose second column is an
unindexed foreign key. Creating them in `public` would fail the product test
suite, and reshaping them would mean not building what was asked for.

Putting them in `staging` resolves it honestly rather than by exception. It also
says the true thing about them: these are ETL artefacts with their own
lifecycle, not part of the product's data model. Cross-schema foreign keys to
`public.contractors(id)` work exactly as within-schema ones do, so the
referential integrity the spec asks for is intact.

The one change to the product schema — `contractors.data_source` — goes through
a normal cm-backend migration, because that IS product schema.
"""

from __future__ import annotations

import hashlib
import json
import re
import urllib.parse
from typing import Any, Optional, Sequence

SCHEMA = "staging"

# ── DDL ───────────────────────────────────────────────────────────────────
# Every statement is IF NOT EXISTS: this job is re-runnable by design, and
# that has to include its own schema.

DDL = f"""
CREATE SCHEMA IF NOT EXISTS {SCHEMA};

CREATE TABLE IF NOT EXISTS {SCHEMA}.gmaps_places (
  place_id            TEXT PRIMARY KEY,
  place_name          TEXT NOT NULL,
  place_address       TEXT,
  place_category      TEXT,
  overall_rating      NUMERIC(2,1),
  total_reviews       INTEGER,
  place_url           TEXT,
  first_scraped_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
  last_scraped_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS {SCHEMA}.gmaps_reviews (
  review_id               TEXT PRIMARY KEY,
  place_id                TEXT NOT NULL REFERENCES {SCHEMA}.gmaps_places(place_id),
  review_number           INTEGER,
  reviewer_name           TEXT,
  reviewer_profile_url    TEXT,
  reviewer_photo_url      TEXT,
  reviewer_total_reviews  INTEGER,
  reviewer_is_local_guide BOOLEAN,
  rating                  NUMERIC(2,1) NOT NULL,
  published_at_raw        TEXT,
  published_at            TIMESTAMPTZ,
  review_text             TEXT,
  review_text_original    TEXT,
  review_photo_urls       JSONB DEFAULT '[]'::jsonb,
  review_photo_count      INTEGER DEFAULT 0,
  owner_reply             TEXT,
  owner_reply_date        TEXT,
  likes_count             INTEGER DEFAULT 0,
  visit_type              TEXT,
  scraped_at              TIMESTAMPTZ,
  ingested_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_reviews_place ON {SCHEMA}.gmaps_reviews(place_id);
CREATE INDEX IF NOT EXISTS idx_reviews_published ON {SCHEMA}.gmaps_reviews(published_at DESC);

CREATE TABLE IF NOT EXISTS {SCHEMA}.contractor_place_matches (
  contractor_id     UUID NOT NULL REFERENCES public.contractors(id),
  place_id          TEXT NOT NULL REFERENCES {SCHEMA}.gmaps_places(place_id),
  match_status      TEXT NOT NULL CHECK (match_status IN ('confirmed','needs_review','rejected')),
  match_score       NUMERIC(3,2) NOT NULL,
  score_components  JSONB NOT NULL,
  query_used        TEXT NOT NULL,
  matched_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (contractor_id, place_id)
);

-- The composite key leads on contractor_id, so the place_id foreign key has no
-- supporting index of its own. Added explicitly: without it, deleting a place
-- sequentially scans this table, and "which contractors matched this place" is
-- the obvious audit query.
CREATE INDEX IF NOT EXISTS idx_matches_place ON {SCHEMA}.contractor_place_matches(place_id);
CREATE INDEX IF NOT EXISTS idx_matches_status ON {SCHEMA}.contractor_place_matches(match_status);

-- Referenced by Part 3 of the spec but never defined in Part 4. It is needed
-- for two things: auditing rejects, and knowing which contractors have already
-- been tried. Without the second, every re-run spends its whole budget
-- re-querying the same rejects and the job can never advance down the list.
CREATE TABLE IF NOT EXISTS {SCHEMA}.place_match_attempts (
  id                BIGSERIAL PRIMARY KEY,
  contractor_id     UUID NOT NULL REFERENCES public.contractors(id),
  query_used        TEXT NOT NULL,
  run_id            TEXT,
  outcome           TEXT NOT NULL
      CHECK (outcome IN ('confirmed','needs_review','rejected','no_result')),
  place_id          TEXT,
  place_name        TEXT,
  place_address     TEXT,
  match_score       NUMERIC(3,2),
  score_components  JSONB,
  attempted_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_attempts_contractor
    ON {SCHEMA}.place_match_attempts(contractor_id, attempted_at DESC);

CREATE TABLE IF NOT EXISTS {SCHEMA}.scrape_runs (
  run_id          TEXT PRIMARY KEY,
  dataset_id      TEXT,
  actor_id        TEXT NOT NULL,
  input_payload   JSONB NOT NULL,
  status          TEXT NOT NULL,
  places_found    INTEGER DEFAULT 0,
  reviews_found   INTEGER DEFAULT 0,
  error_message   TEXT,
  -- Not in the spec's DDL, added because the $50 ceiling has to be counted
  -- against something. This holds Apify's own `usageTotalUsd` for the run
  -- rather than a locally modelled estimate.
  cost_usd        NUMERIC(10,4),
  started_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  finished_at     TIMESTAMPTZ
);
"""


# ── Reading the dataset ───────────────────────────────────────────────────
# The actor returns one row per review with the place fields repeated on every
# row. Splitting that is the caller's job, and these helpers are how a row is
# read: defensively, because a malformed row must be skipped rather than kill a
# batch of twenty places.


def _first(row: dict, *names: str) -> Any:
    for name in names:
        if name in row and row[name] is not None:
            return row[name]
    return None


def _as_int(value: Any) -> Optional[int]:
    if value is None or isinstance(value, bool):
        return None
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def _as_float(value: Any) -> Optional[float]:
    if value is None or isinstance(value, bool):
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def _as_bool(value: Any) -> Optional[bool]:
    if isinstance(value, bool):
        return value
    if isinstance(value, str):
        if value.lower() in ("true", "yes", "1"):
            return True
        if value.lower() in ("false", "no", "0"):
            return False
    return None


def _as_text(value: Any) -> Optional[str]:
    if value is None:
        return None
    if isinstance(value, str):
        return value
    return str(value)


class MalformedRow(ValueError):
    """A dataset row that cannot be used. Logged and skipped, never fatal."""


# ── Reconciling the spec with what the actor actually returns ─────────────
#
# The spec documents `placeId`, `reviewId`, `placeCategory`, `publishedAtDate`
# and several reviewer fields. Measured against the real actor
# (datablow/google-reviews-scraper, run sdL5McXqMvLoaaHoN), NONE of those are
# present. What arrives is:
#
#   placeName, placeAddress, placeOverallRating, placeTotalReviews, placeUrl,
#   scrapedAt, reviewNumber, reviewerName, reviewerProfileUrl, rating,
#   publishedAt, reviewText, reviewPhotoUrls, reviewPhotoCount, ownerReply,
#   ownerReplyDate
#
# Three consequences, handled here rather than by relaxing the schema:
#
#  1. No placeId. Google's own feature id is embedded in placeUrl as
#     `!1s0x<hex>:0x<hex>`, which is stable for a place across runs, so it is
#     extracted and used as the primary key. That is a real identifier, not a
#     hash of mutable fields.
#
#  2. No reviewId. Derived as a digest of the things about a review that do not
#     drift. Deliberately NOT included: `publishedAt`, which is relative text
#     and changes from "4 months ago" to "5 months ago" on its own, and
#     `reviewNumber`, which shifts as newer reviews arrive. Including either
#     would mint a new id on every run and duplicate the whole table.
#
#  3. No placeCategory, ever. The category signal is therefore always 0, and
#     the maximum achievable score is 0.9 rather than 1.0. Confirming at 0.75
#     now requires a name similarity of at least 0.625 alongside a matching
#     city and state, which is a higher bar than the spec intended — the right
#     direction to err, but worth knowing when reading the match rate.

# Google Maps embeds the feature id as `!1s0x8cba451fd5df596d:0xb34214a87b15e528`.
_FEATURE_ID = re.compile(r"!1s(0x[0-9a-f]+:0x[0-9a-f]+)", re.IGNORECASE)
# And the Knowledge Graph id as `!16s%2Fg%2F11yhpk4xxp` -> /g/11yhpk4xxp.
_KG_ID = re.compile(r"!16s([^!?&]+)")

# Google prefixes the address with a private-use glyph (U+E0C8, a Material
# icon). It is not part of the address and must not reach the database or the
# comma parser that recovers the city.
_PRIVATE_USE = re.compile(r"[\ue000-\uf8ff]")


def clean_text(value: Any) -> Optional[str]:
    if value is None:
        return None
    text = _PRIVATE_USE.sub("", str(value)).strip()
    return text or None


def place_key(row: dict) -> Optional[str]:
    """A stable identifier for a place, from its Google Maps URL.

    Prefers Google's feature id, then its Knowledge Graph id. Returns None when
    neither is present — the caller decides whether to fall back, so that a
    silent hash-of-the-name never masquerades as a real place id.
    """
    url = _first(row, "placeUrl", "place_url")
    if not url:
        return None

    found = _FEATURE_ID.search(url)
    if found:
        return found.group(1).lower()

    found = _KG_ID.search(url)
    if found:
        return urllib.parse.unquote(found.group(1))

    return None


def derive_review_id(place_id: str, row: dict) -> str:
    """A stable synthetic review id.

    Built only from fields that do not drift between runs. The reviewer's
    profile URL is included when present because it identifies a person, which
    keeps two rating-only reviews by different people with the same display
    name from colliding.
    """
    parts = [
        place_id,
        _as_text(_first(row, "reviewerName", "reviewer_name")) or "",
        _as_text(_first(row, "reviewerProfileUrl", "reviewer_profile_url")) or "",
        _as_text(_first(row, "reviewText", "review_text")) or "",
    ]
    digest = hashlib.sha256("\u0000".join(parts).encode("utf-8")).hexdigest()
    return f"d:{digest[:40]}"


def place_from_row(row: dict) -> dict:
    """The place-level fields, identical across every row for a given place."""
    # The documented field first, then Google's own id out of the URL. A row
    # with neither cannot be keyed and is genuinely unusable.
    place_id = _as_text(_first(row, "placeId", "place_id")) or place_key(row)
    if not place_id:
        raise MalformedRow("row carries neither a placeId nor a usable placeUrl")

    name = clean_text(_first(row, "placeName", "place_name"))
    if not name:
        raise MalformedRow(f"place {place_id} carries no placeName")

    return {
        "place_id": place_id,
        "place_name": name,
        "place_address": clean_text(_first(row, "placeAddress", "place_address")),
        "place_category": clean_text(_first(row, "placeCategory", "place_category")),
        "overall_rating": _as_float(_first(row, "placeOverallRating", "place_overall_rating")),
        "total_reviews": _as_int(_first(row, "placeTotalReviews", "place_total_reviews")),
        "place_url": _as_text(_first(row, "placeUrl", "place_url")),
    }


def review_from_row(row: dict, place_id: str) -> Optional[dict]:
    """The review-level fields, or None when the row carries no review.

    A place with reviews turned off still produces a row on some actor
    versions, with the place fields populated and every review field null.
    That is a place, not a malformed row, so it returns None rather than
    raising.
    """
    review_id = _as_text(_first(row, "reviewId", "review_id"))
    if not review_id:
        # The actor sends no review id. A row that carries an actual review is
        # still a review, so one is derived rather than the row discarded.
        # A row with no reviewer AND no text is a place with no reviews.
        has_content = any(
            _first(row, key) is not None
            for key in ("reviewerName", "reviewText", "rating", "reviewNumber")
        )
        if not has_content:
            return None
        review_id = derive_review_id(place_id, row)

    rating = _as_float(_first(row, "rating", "reviewRating", "stars"))
    if rating is None:
        # Rating is NOT NULL, and a review without one is not usable. This is
        # the one review-level field worth rejecting a row over.
        raise MalformedRow(f"review {review_id} carries no rating")

    photos = _first(row, "reviewPhotoUrls", "review_photo_urls")
    if not isinstance(photos, list):
        photos = [] if photos is None else [photos]

    # `publishedAt` is relative text — "2 months ago" — and is never parsed.
    # The ISO field is the only thing that reaches a timestamp column; when it
    # is missing the raw string is kept and the timestamp stays null, so
    # nothing downstream can mistake a guess for a date.
    published_iso = _as_text(_first(row, "publishedAtDate", "published_at_date"))
    published_raw = _as_text(_first(row, "publishedAt", "published_at"))

    photo_count = _as_int(_first(row, "reviewPhotoCount", "review_photo_count"))

    return {
        "review_id": review_id,
        "place_id": place_id,
        "review_number": _as_int(_first(row, "reviewNumber", "review_number")),
        "reviewer_name": _as_text(_first(row, "reviewerName", "reviewer_name")),
        "reviewer_profile_url": _as_text(_first(row, "reviewerProfileUrl", "reviewer_profile_url")),
        "reviewer_photo_url": _as_text(
            _first(row, "reviewerProfilePhotoUrl", "reviewer_profile_photo_url")
        ),
        "reviewer_total_reviews": _as_int(
            _first(row, "reviewerTotalReviews", "reviewer_total_reviews")
        ),
        "reviewer_is_local_guide": _as_bool(
            _first(row, "reviewerIsLocalGuide", "reviewer_is_local_guide")
        ),
        "rating": rating,
        "published_at_raw": published_raw,
        "published_at": published_iso,
        # An empty review is valid and common: a rating with no words still
        # counts toward the average, so it is kept rather than dropped.
        "review_text": clean_text(_first(row, "reviewText", "review_text")),
        "review_text_original": _as_text(_first(row, "reviewTextOriginal", "review_text_original")),
        "review_photo_urls": json.dumps(photos),
        "review_photo_count": photo_count if photo_count is not None else len(photos),
        # Frequently null, which is normal rather than an error.
        "owner_reply": _as_text(_first(row, "ownerReply", "owner_reply")),
        "owner_reply_date": _as_text(_first(row, "ownerReplyDate", "owner_reply_date")),
        "likes_count": _as_int(_first(row, "likesCount", "likes_count")) or 0,
        "visit_type": _as_text(_first(row, "visitType", "visit_type")),
        "scraped_at": _as_text(_first(row, "scrapedAt", "scraped_at")),
    }


# ── Writes ────────────────────────────────────────────────────────────────

PLACE_COLUMNS = (
    "place_id",
    "place_name",
    "place_address",
    "place_category",
    "overall_rating",
    "total_reviews",
    "place_url",
)

REVIEW_COLUMNS = (
    "review_id",
    "place_id",
    "review_number",
    "reviewer_name",
    "reviewer_profile_url",
    "reviewer_photo_url",
    "reviewer_total_reviews",
    "reviewer_is_local_guide",
    "rating",
    "published_at_raw",
    "published_at",
    "review_text",
    "review_text_original",
    "review_photo_urls",
    "review_photo_count",
    "owner_reply",
    "owner_reply_date",
    "likes_count",
    "visit_type",
    "scraped_at",
)

REVIEW_BATCH = 500


def upsert_places(cursor, places: Sequence[dict]) -> int:
    """Places first — the review foreign key requires it."""
    if not places:
        return 0

    columns = ", ".join(PLACE_COLUMNS)
    placeholders = ", ".join(["%s"] * len(PLACE_COLUMNS))
    sql = f"""
        INSERT INTO {SCHEMA}.gmaps_places ({columns})
        VALUES ({placeholders})
        ON CONFLICT (place_id) DO UPDATE SET
          overall_rating  = EXCLUDED.overall_rating,
          total_reviews   = EXCLUDED.total_reviews,
          place_name      = EXCLUDED.place_name,
          place_address   = EXCLUDED.place_address,
          last_scraped_at = now()
    """
    cursor.executemany(sql, [tuple(p[c] for c in PLACE_COLUMNS) for p in places])
    return len(places)


def insert_reviews(cursor, reviews: Sequence[dict]) -> int:
    """Reviews are immutable once captured.

    DO NOTHING rather than DO UPDATE: if a review was edited on Google after we
    captured it, our original is the correct thing to keep. This is an audit
    trail, and an audit trail that silently rewrites itself is not one.
    """
    if not reviews:
        return 0

    columns = ", ".join(REVIEW_COLUMNS)
    placeholders = ", ".join(["%s"] * len(REVIEW_COLUMNS))
    sql = f"""
        INSERT INTO {SCHEMA}.gmaps_reviews ({columns})
        VALUES ({placeholders})
        ON CONFLICT (review_id) DO NOTHING
    """

    written = 0
    for start in range(0, len(reviews), REVIEW_BATCH):
        chunk = reviews[start : start + REVIEW_BATCH]
        cursor.executemany(sql, [tuple(r[c] for c in REVIEW_COLUMNS) for r in chunk])
        written += len(chunk)
    return written


def upsert_match(cursor, match: dict) -> None:
    cursor.execute(
        f"""
        INSERT INTO {SCHEMA}.contractor_place_matches
            (contractor_id, place_id, match_status, match_score, score_components, query_used)
        VALUES (%s, %s, %s, %s, %s, %s)
        ON CONFLICT (contractor_id, place_id) DO UPDATE SET
          match_status     = EXCLUDED.match_status,
          match_score      = EXCLUDED.match_score,
          score_components = EXCLUDED.score_components,
          query_used       = EXCLUDED.query_used,
          matched_at       = now()
        """,
        (
            match["contractor_id"],
            match["place_id"],
            match["match_status"],
            match["match_score"],
            json.dumps(match["score_components"]),
            match["query_used"],
        ),
    )


def record_attempt(cursor, attempt: dict) -> None:
    cursor.execute(
        f"""
        INSERT INTO {SCHEMA}.place_match_attempts
            (contractor_id, query_used, run_id, outcome, place_id, place_name,
             place_address, match_score, score_components)
        VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s)
        """,
        (
            attempt["contractor_id"],
            attempt["query_used"],
            attempt.get("run_id"),
            attempt["outcome"],
            attempt.get("place_id"),
            attempt.get("place_name"),
            attempt.get("place_address"),
            attempt.get("match_score"),
            json.dumps(attempt["score_components"]) if attempt.get("score_components") else None,
        ),
    )


def start_run_row(cursor, *, run_id: str, actor_id: str, payload: dict, status: str) -> None:
    cursor.execute(
        f"""
        INSERT INTO {SCHEMA}.scrape_runs (run_id, actor_id, input_payload, status)
        VALUES (%s, %s, %s, %s)
        ON CONFLICT (run_id) DO UPDATE SET status = EXCLUDED.status
        """,
        (run_id, actor_id, json.dumps(payload), status),
    )


def update_run_row(cursor, run_id: str, **fields: Any) -> None:
    if not fields:
        return
    assignments = ", ".join(f"{k} = %s" for k in fields)
    cursor.execute(
        f"UPDATE {SCHEMA}.scrape_runs SET {assignments} WHERE run_id = %s",
        (*fields.values(), run_id),
    )


def total_spend_usd(cursor) -> float:
    cursor.execute(f"SELECT COALESCE(SUM(cost_usd), 0) FROM {SCHEMA}.scrape_runs")
    return float(cursor.fetchone()[0] or 0)


# ── Selecting work ────────────────────────────────────────────────────────

SELECT_CONTRACTORS = f"""
SELECT c.id, c.display_name, l.city, l.license_no
  FROM public.contractors c
  JOIN public.license_records l ON l.id = c.license_record_id
 WHERE NOT EXISTS (
         SELECT 1 FROM {SCHEMA}.contractor_place_matches m
          WHERE m.contractor_id = c.id AND m.match_status = 'confirmed')
   AND NOT EXISTS (
         SELECT 1 FROM {SCHEMA}.place_match_attempts a
          WHERE a.contractor_id = c.id
            AND a.attempted_at > now() - (%s || ' days')::interval)
   AND l.city IS NOT NULL
   AND btrim(c.display_name) <> ''
 ORDER BY l.license_no
 LIMIT %s
"""
