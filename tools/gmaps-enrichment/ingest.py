#!/usr/bin/env python3
"""Enrich CSLB contractor records with Google Maps places and reviews.

Staging load only. Nothing here writes to the chain or to SNIP, and nothing
here generates synthetic data — that is a separate job behind its own flag.

    APIFY_TOKEN=... APIFY_ACTOR_ID=... DATABASE_URL=... python3 ingest.py

Resumable by construction. A contractor is selected only when it has no
confirmed match and has not been attempted recently, so re-running after any
kind of stop — a spend cap, a crash, a failed batch — picks up where the last
one left off without redoing settled work.

    --limit N            stop after N contractors (default: the whole list)
    --max-spend USD      halt when Apify's reported spend passes this (default 50)
    --retry-after-days N re-attempt a previously tried contractor after N days
    --dry-run            do everything except call Apify
    --schema-only        create the staging schema and exit
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Optional

import apify_client
import matching
import store

BATCH_SIZE = 20
SLEEP_BETWEEN_BATCHES = 5

# From the spec, and fixed. maxConcurrency above 3 gets runs throttled, and
# maxReviews of 0 means "all", which on a contractor with 800 reviews burns
# credits for data the prototype does not read.
ACTOR_DEFAULTS = {
    "maxConcurrency": 3,
    "maxReviews": 50,
    "reviewsSort": "newest",
    "language": "en",
    "useProxy": True,
}

_stopping = False


def _handle_signal(signum, _frame):
    """Finish the batch in flight, then stop.

    Killing the process mid-batch would leave a run recorded as `running` with
    a dataset nobody fetches. One more batch is cheap; an orphaned dataset has
    to be reconciled by hand.
    """
    global _stopping
    _stopping = True
    print(f"\n  signal {signum} received — finishing this batch, then stopping", flush=True)


@dataclass
class Contractor:
    id: str
    display_name: str
    city: str
    license_no: str

    @property
    def query(self) -> str:
        """`"{business name}, {city}, CA"`, as the spec specifies."""
        return f"{self.display_name}, {self.city}, CA"


@dataclass
class Totals:
    attempted: int = 0
    confirmed: int = 0
    needs_review: int = 0
    rejected: int = 0
    no_result: int = 0
    places_written: int = 0
    reviews_written: int = 0
    batches_ok: int = 0
    batches_failed: int = 0
    spend_usd: float = 0.0


def require_env(name: str, default: Optional[str] = None) -> str:
    value = os.environ.get(name) or default
    if not value:
        print(
            f"error: {name} is not set.\n"
            f"       This job needs APIFY_TOKEN, APIFY_ACTOR_ID and DATABASE_URL.\n"
            f"       Export them and re-run; the token is read from the environment "
            f"and is never written to a log or to the database.",
            file=sys.stderr,
        )
        sys.exit(2)
    return value


def connect(database_url: str):
    try:
        import psycopg2
    except ImportError:
        print(
            "error: psycopg2 is not installed.\n"
            "       python3 -m venv .venv && .venv/bin/pip install psycopg2-binary",
            file=sys.stderr,
        )
        sys.exit(2)
    return psycopg2.connect(database_url)


def load_batch(conn, retry_after_days: int, limit: int) -> list[Contractor]:
    with conn.cursor() as cursor:
        cursor.execute(store.SELECT_CONTRACTORS, (str(retry_after_days), limit))
        return [Contractor(str(r[0]), r[1], r[2], r[3]) for r in cursor.fetchall()]


def split_dataset(rows, log) -> tuple[dict, dict, int]:
    """Turn the denormalised dataset into places and reviews.

    The actor returns one row per review with the place fields repeated on each
    one, so this groups by placeId. A row that cannot be read is logged and
    skipped: one bad review must not cost a batch of twenty places.
    """
    places: dict[str, dict] = {}
    reviews: dict[str, list[dict]] = {}
    skipped = 0

    for row in rows:
        if not isinstance(row, dict):
            skipped += 1
            continue
        try:
            place = store.place_from_row(row)
        except store.MalformedRow as error:
            log(f"    skipping a malformed row: {error}")
            skipped += 1
            continue

        places.setdefault(place["place_id"], place)

        try:
            review = store.review_from_row(row, place["place_id"])
        except store.MalformedRow as error:
            log(f"    skipping a malformed review: {error}")
            skipped += 1
            continue

        if review is not None:
            reviews.setdefault(place["place_id"], []).append(review)

    return places, reviews, skipped


def pair_places_to_contractors(batch: list[Contractor], places: dict) -> dict:
    """Decide which contractor each returned place belongs to.

    The actor does not tell us which query produced which place, so this is
    reconstructed by scoring every place against every contractor in the batch
    and keeping the best pairing per place. Scoring only against the
    same-indexed contractor would be wrong: the actor does not promise output
    order, and twenty queries do not reliably produce twenty places.

    A contractor can win at most one place — the highest scoring — because a
    licensed business has one Google listing, and attaching two would double
    count its reviews downstream.
    """
    scored = []
    for place_id, place in places.items():
        for contractor in batch:
            result = matching.score_match(
                contractor_name=contractor.display_name,
                contractor_city=contractor.city,
                place_name=place.get("place_name"),
                place_address=place.get("place_address"),
                place_category=place.get("place_category"),
            )
            scored.append((result.score, place_id, contractor, result))

    # Best first, so the strongest pairing claims its place and contractor.
    scored.sort(key=lambda item: item[0], reverse=True)

    taken_places: set[str] = set()
    taken_contractors: set[str] = set()
    pairing: dict[str, tuple[Contractor, matching.MatchResult]] = {}

    for _score, place_id, contractor, result in scored:
        if place_id in taken_places or contractor.id in taken_contractors:
            continue
        taken_places.add(place_id)
        taken_contractors.add(contractor.id)
        pairing[place_id] = (contractor, result)

    return pairing


def process_batch(
    conn,
    client: Optional[apify_client.ApifyClient],
    batch: list[Contractor],
    totals: Totals,
    actor_id: str,
    log,
    dry_run: bool,
) -> None:
    payload = {"locationNames": [c.query for c in batch], **ACTOR_DEFAULTS}

    if dry_run:
        log(f"  dry run: would send {len(batch)} queries, e.g. {batch[0].query!r}")
        totals.attempted += len(batch)
        return

    # Recorded BEFORE the run is started, so a process that dies between the
    # POST and the response still leaves a trace. The placeholder id is
    # replaced by the real one the moment Apify answers.
    provisional_id = f"pending:{batch[0].license_no}:{int(time.time())}"
    with conn.cursor() as cursor:
        store.start_run_row(
            cursor, run_id=provisional_id, actor_id=actor_id, payload=payload, status="starting"
        )
    conn.commit()

    try:
        handle = client.start_run(payload)
    except apify_client.ApifyError as error:
        with conn.cursor() as cursor:
            store.update_run_row(
                cursor, provisional_id, status="failed", error_message=str(error)[:2000]
            )
        conn.commit()
        totals.batches_failed += 1
        log(f"  batch failed to start: {error}")
        return

    # Swap the placeholder for the real run, and record the dataset id
    # immediately — if the process dies now, that id is how the results are
    # recovered.
    with conn.cursor() as cursor:
        cursor.execute(
            f"DELETE FROM {store.SCHEMA}.scrape_runs WHERE run_id = %s", (provisional_id,)
        )
        store.start_run_row(
            cursor, run_id=handle.run_id, actor_id=actor_id, payload=payload, status="running"
        )
        store.update_run_row(cursor, handle.run_id, dataset_id=handle.dataset_id)
    conn.commit()
    log(f"  run {handle.run_id} started (dataset {handle.dataset_id})")

    final = client.wait_for_run(handle.run_id)
    cost = apify_client.run_cost_usd(final)
    totals.spend_usd += cost
    log(f"  run finished: {final.status}  (${cost:.3f})")

    dataset_id = final.dataset_id or handle.dataset_id
    rows = []
    if dataset_id:
        try:
            # Anything other than SUCCEEDED is still worth fetching: partial
            # results are usable, and the run has already been paid for.
            rows = list(client.iter_dataset(dataset_id))
        except apify_client.ApifyError as error:
            log(f"  could not page the dataset: {error}")

    places, reviews_by_place, skipped = split_dataset(rows, log)
    if skipped:
        log(f"  skipped {skipped} unusable row(s)")

    pairing = pair_places_to_contractors(batch, places)

    written_places = 0
    written_reviews = 0
    matched_contractors: set[str] = set()

    try:
        with conn.cursor() as cursor:
            confirmed_places = []
            for place_id, (contractor, result) in pairing.items():
                matched_contractors.add(contractor.id)
                totals.attempted += 1

                store.record_attempt(
                    cursor,
                    {
                        "contractor_id": contractor.id,
                        "query_used": contractor.query,
                        "run_id": handle.run_id,
                        "outcome": result.status,
                        "place_id": place_id,
                        "place_name": places[place_id].get("place_name"),
                        "place_address": places[place_id].get("place_address"),
                        "match_score": result.score,
                        "score_components": result.components,
                    },
                )

                if result.status == "rejected":
                    totals.rejected += 1
                    continue

                confirmed_places.append((place_id, contractor, result))

            # Places before reviews and before matches: both foreign keys point
            # at gmaps_places.
            written_places = store.upsert_places(
                cursor, [places[pid] for pid, _c, _r in confirmed_places]
            )

            for place_id, contractor, result in confirmed_places:
                store.upsert_match(
                    cursor,
                    {
                        "contractor_id": contractor.id,
                        "place_id": place_id,
                        "match_status": result.status,
                        "match_score": result.score,
                        "score_components": result.components,
                        "query_used": contractor.query,
                    },
                )
                written_reviews += store.insert_reviews(
                    cursor, reviews_by_place.get(place_id, [])
                )
                if result.status == "confirmed":
                    totals.confirmed += 1
                else:
                    totals.needs_review += 1

            # Contractors this run returned nothing for. Recorded so the next
            # run does not spend its budget asking the same question — and as
            # `no_result` rather than `rejected`, because "Google returned
            # nothing" and "Google returned the wrong business" are different
            # facts and only one of them is about a place.
            for contractor in batch:
                if contractor.id in matched_contractors:
                    continue
                totals.attempted += 1
                totals.no_result += 1
                store.record_attempt(
                    cursor,
                    {
                        "contractor_id": contractor.id,
                        "query_used": contractor.query,
                        "run_id": handle.run_id,
                        "outcome": "no_result",
                    },
                )

            store.update_run_row(
                cursor,
                handle.run_id,
                status="succeeded" if final.status == "SUCCEEDED" else final.status.lower(),
                places_found=len(places),
                reviews_found=sum(len(v) for v in reviews_by_place.values()),
                cost_usd=cost,
                finished_at=datetime.now(timezone.utc),
            )
        conn.commit()
    except Exception as error:  # noqa: BLE001 — the whole run rolls back
        conn.rollback()
        with conn.cursor() as cursor:
            store.update_run_row(
                cursor,
                handle.run_id,
                status="failed",
                error_message=str(error)[:2000],
                cost_usd=cost,
                finished_at=datetime.now(timezone.utc),
            )
        conn.commit()
        totals.batches_failed += 1
        log(f"  batch failed while writing, rolled back: {error}")
        return

    totals.places_written += written_places
    totals.reviews_written += written_reviews
    totals.batches_ok += 1
    log(
        f"  wrote {written_places} place(s), {written_reviews} review(s); "
        f"{len(pairing)} paired, {len(batch) - len(matched_contractors)} with no result"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--limit", type=int, default=None)
    parser.add_argument("--max-spend", type=float, default=50.0)
    parser.add_argument("--retry-after-days", type=int, default=30)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--schema-only", action="store_true")
    args = parser.parse_args()

    def log(message: str) -> None:
        print(message, flush=True)

    database_url = require_env("DATABASE_URL")
    conn = connect(database_url)

    with conn.cursor() as cursor:
        cursor.execute(store.DDL)
    conn.commit()
    log(f"staging schema ready ({store.SCHEMA})")

    if args.schema_only:
        conn.close()
        return 0

    client = None
    actor_id = os.environ.get("APIFY_ACTOR_ID", "gT99sk2Z5BOn6jD7M")
    if not args.dry_run:
        token = require_env("APIFY_TOKEN")
        actor_id = require_env("APIFY_ACTOR_ID", actor_id)
        client = apify_client.ApifyClient(token, actor_id, log=log)

    signal.signal(signal.SIGINT, _handle_signal)
    signal.signal(signal.SIGTERM, _handle_signal)

    totals = Totals()
    with conn.cursor() as cursor:
        already_spent = store.total_spend_usd(cursor)
    if already_spent:
        log(f"previously recorded spend: ${already_spent:.2f}")

    remaining = args.limit if args.limit is not None else float("inf")
    batch_number = 0

    while remaining > 0 and not _stopping:
        spent = already_spent + totals.spend_usd
        if spent >= args.max_spend:
            log(f"\nspend cap reached: ${spent:.2f} of ${args.max_spend:.2f}. Stopping.")
            break

        take = int(min(BATCH_SIZE, remaining))
        batch = load_batch(conn, args.retry_after_days, take)
        if not batch:
            log("\nno contractors left to attempt")
            break

        batch_number += 1
        log(
            f"\nbatch {batch_number}: {len(batch)} contractor(s) "
            f"from licence {batch[0].license_no} "
            f"(spent ${spent:.2f} of ${args.max_spend:.2f})"
        )

        process_batch(conn, client, batch, totals, actor_id, log, args.dry_run)
        remaining -= len(batch)

        if args.dry_run:
            # Nothing was recorded, so the same batch would load forever.
            break

        if remaining > 0 and not _stopping:
            time.sleep(SLEEP_BETWEEN_BATCHES)

    log("\n" + "=" * 60)
    log("SUMMARY")
    log("=" * 60)
    log(f"  contractors attempted : {totals.attempted}")
    log(f"    confirmed           : {totals.confirmed}")
    log(f"    needs_review        : {totals.needs_review}")
    log(f"    rejected            : {totals.rejected}")
    log(f"    no result           : {totals.no_result}")
    log(f"  places written        : {totals.places_written}")
    log(f"  reviews written       : {totals.reviews_written}")
    log(f"  batches ok / failed   : {totals.batches_ok} / {totals.batches_failed}")
    log(f"  spend this session    : ${totals.spend_usd:.2f}")
    log(f"  spend recorded total  : ${already_spent + totals.spend_usd:.2f}")

    if totals.attempted:
        rate = (totals.confirmed + totals.needs_review) / totals.attempted * 100
        log(f"  match rate            : {rate:.1f}%")
        log("  (30-60% of contractors having no usable Google presence is expected)")

    with conn.cursor() as cursor:
        cursor.execute(
            f"""SELECT count(*) FROM public.contractors c
                 WHERE NOT EXISTS (SELECT 1 FROM {store.SCHEMA}.contractor_place_matches m
                                    WHERE m.contractor_id = c.id AND m.match_status = 'confirmed')"""
        )
        log(f"  contractors still unconfirmed: {cursor.fetchone()[0]}")

    conn.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
