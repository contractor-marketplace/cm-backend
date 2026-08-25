"""End-to-end test of everything except the Apify HTTP calls.

Runs against a real database, using real contractors, and rolls back. That is
the point: the foreign keys, the upsert conflict targets, the JSONB casts and
the NOT NULL constraints are exactly the things a fixture-only test cannot
check, and exactly the things that fail at 2am on batch 200.

    DATABASE_URL=postgres://... .venv/bin/python -m unittest test_pipeline -v

Skipped when DATABASE_URL is unset, so the unit suite still runs anywhere.
"""

from __future__ import annotations

import json
import os
import unittest
from datetime import datetime, timezone

import ingest
import matching
import store

DATABASE_URL = os.environ.get("DATABASE_URL")


def actor_rows(place, reviews):
    """Build rows in the actor's real shape: denormalised, place repeated."""
    out = []
    for review in reviews:
        row = dict(place)
        row.update(review)
        out.append(row)
    return out


@unittest.skipUnless(DATABASE_URL, "DATABASE_URL is not set")
class Pipeline(unittest.TestCase):
    """Each test runs in a transaction that is rolled back."""

    @classmethod
    def setUpClass(cls):
        import psycopg2

        cls.conn = psycopg2.connect(DATABASE_URL)

    @classmethod
    def tearDownClass(cls):
        cls.conn.close()

    def setUp(self):
        self.conn.rollback()
        self.cursor = self.conn.cursor()
        # Two real contractors, so the foreign keys are exercised for real.
        self.cursor.execute(
            """SELECT c.id, c.display_name, l.city, l.license_no
                 FROM contractors c JOIN license_records l ON l.id = c.license_record_id
                WHERE l.city IS NOT NULL AND btrim(c.display_name) <> ''
                ORDER BY l.license_no LIMIT 2"""
        )
        rows = self.cursor.fetchall()
        self.assertEqual(len(rows), 2, "need two real contractors to test against")
        self.contractors = [ingest.Contractor(str(r[0]), r[1], r[2], r[3]) for r in rows]

    def tearDown(self):
        self.conn.rollback()
        self.cursor.close()

    # ── the shape of the data ────────────────────────────────────────────

    def test_a_denormalised_dataset_splits_into_places_and_reviews(self):
        place = {
            "placeId": "ChIJtest001",
            "placeName": self.contractors[0].display_name,
            "placeAddress": f"1 Main St, {self.contractors[0].city}, CA 90001, USA",
            "placeCategory": "Plumber",
            "placeOverallRating": 4.6,
            "placeTotalReviews": 3,
            "placeUrl": "https://maps.google.com/?cid=1",
        }
        reviews = [
            {"reviewId": f"rev-{n}", "rating": 5, "reviewText": f"Review {n}",
             "publishedAtDate": "2026-06-01T00:00:00.000Z", "publishedAt": "2 months ago"}
            for n in range(3)
        ]

        places, by_place, skipped = ingest.split_dataset(actor_rows(place, reviews), lambda _m: None)
        self.assertEqual(skipped, 0)
        self.assertEqual(len(places), 1, "three rows, one place")
        self.assertEqual(len(by_place["ChIJtest001"]), 3, "three reviews")

    def test_one_malformed_row_does_not_kill_the_batch(self):
        good = {
            "placeId": "ChIJtest002", "placeName": "Good Place",
            "reviewId": "rev-good", "rating": 5,
        }
        rows = [good, {"placeName": "no id at all"}, {"placeId": "x"}, "not even a dict"]
        places, _by_place, skipped = ingest.split_dataset(rows, lambda _m: None)
        self.assertIn("ChIJtest002", places)
        self.assertEqual(skipped, 3)

    # ── writing ──────────────────────────────────────────────────────────

    def test_a_confirmed_match_writes_place_reviews_and_match(self):
        contractor = self.contractors[0]
        place = store.place_from_row(
            {
                "placeId": "ChIJtest010",
                "placeName": contractor.display_name,
                "placeAddress": f"1 Main St, {contractor.city}, CA 90001, USA",
                "placeCategory": "Plumber",
                "placeOverallRating": 4.6,
                "placeTotalReviews": 2,
            }
        )
        reviews = [
            store.review_from_row(
                {"reviewId": "rev-a", "rating": 5, "reviewText": "",
                 "publishedAtDate": "2026-06-01T00:00:00.000Z",
                 "reviewPhotoUrls": ["https://example.test/1.jpg"]},
                place["place_id"],
            ),
            store.review_from_row(
                {"reviewId": "rev-b", "rating": 4, "publishedAt": "a year ago"},
                place["place_id"],
            ),
        ]

        result = matching.score_match(
            contractor_name=contractor.display_name,
            contractor_city=contractor.city,
            place_name=place["place_name"],
            place_address=place["place_address"],
            place_category=place["place_category"],
        )
        self.assertEqual(result.status, "confirmed", f"components: {result.components}")

        store.upsert_places(self.cursor, [place])
        store.upsert_match(
            self.cursor,
            {
                "contractor_id": contractor.id,
                "place_id": place["place_id"],
                "match_status": result.status,
                "match_score": result.score,
                "score_components": result.components,
                "query_used": contractor.query,
            },
        )
        written = store.insert_reviews(self.cursor, reviews)
        self.assertEqual(written, 2)

        # The empty review survived, and the relative date did not become one.
        self.cursor.execute(
            "SELECT review_text, published_at, published_at_raw, review_photo_urls, "
            "       review_photo_count "
            "  FROM staging.gmaps_reviews WHERE review_id = 'rev-a'"
        )
        text, published, raw, photos, count = self.cursor.fetchone()
        self.assertEqual(text, "", "a rating-only review is kept")
        self.assertIsNotNone(published)
        self.assertEqual(count, 1)
        self.assertEqual(photos, ["https://example.test/1.jpg"])

        self.cursor.execute(
            "SELECT published_at, published_at_raw FROM staging.gmaps_reviews "
            " WHERE review_id = 'rev-b'"
        )
        published, raw = self.cursor.fetchone()
        self.assertIsNone(published, "a relative date must never reach the timestamp")
        self.assertEqual(raw, "a year ago")

    def test_reviews_are_immutable_on_re_run(self):
        contractor = self.contractors[0]
        place = store.place_from_row(
            {"placeId": "ChIJtest020", "placeName": "Somewhere", "placeTotalReviews": 1}
        )
        store.upsert_places(self.cursor, [place])

        original = store.review_from_row(
            {"reviewId": "rev-fixed", "rating": 5, "reviewText": "As captured"},
            place["place_id"],
        )
        store.insert_reviews(self.cursor, [original])

        edited = store.review_from_row(
            {"reviewId": "rev-fixed", "rating": 1, "reviewText": "Edited later on Google"},
            place["place_id"],
        )
        store.insert_reviews(self.cursor, [edited])

        self.cursor.execute(
            "SELECT review_text, rating FROM staging.gmaps_reviews WHERE review_id = 'rev-fixed'"
        )
        text, rating = self.cursor.fetchone()
        self.assertEqual(text, "As captured", "an edit on Google must not rewrite our record")
        self.assertEqual(float(rating), 5.0)

    def test_a_place_is_updated_on_re_run_but_never_duplicated(self):
        place = store.place_from_row(
            {"placeId": "ChIJtest030", "placeName": "Before", "placeTotalReviews": 10,
             "placeOverallRating": 4.0}
        )
        store.upsert_places(self.cursor, [place])

        later = store.place_from_row(
            {"placeId": "ChIJtest030", "placeName": "After", "placeTotalReviews": 12,
             "placeOverallRating": 4.3}
        )
        store.upsert_places(self.cursor, [later])

        self.cursor.execute(
            "SELECT count(*), max(place_name), max(total_reviews) "
            "  FROM staging.gmaps_places WHERE place_id = 'ChIJtest030'"
        )
        count, name, total = self.cursor.fetchone()
        self.assertEqual(count, 1)
        self.assertEqual(name, "After", "the rating and count refresh")
        self.assertEqual(total, 12)

    def test_a_rejected_match_writes_an_attempt_and_no_reviews(self):
        contractor = self.contractors[0]
        result = matching.score_match(
            contractor_name=contractor.display_name,
            contractor_city=contractor.city,
            place_name="Completely Unrelated Business",
            place_address="1 Main St, Phoenix, AZ 85004, USA",
            place_category="Coffee shop",
        )
        self.assertEqual(result.status, "rejected")
        self.assertFalse(result.writes_reviews)

        store.record_attempt(
            self.cursor,
            {
                "contractor_id": contractor.id,
                "query_used": contractor.query,
                "run_id": "test-run",
                "outcome": "rejected",
                "place_id": None,
                "place_name": "Completely Unrelated Business",
                "match_score": result.score,
                "score_components": result.components,
            },
        )
        self.cursor.execute(
            "SELECT outcome, score_components FROM staging.place_match_attempts "
            " WHERE contractor_id = %s AND run_id = 'test-run'",
            (contractor.id,),
        )
        outcome, components = self.cursor.fetchone()
        self.assertEqual(outcome, "rejected")
        # The components are what tell you WHICH signal failed.
        self.assertIn("rejected_because", components)
        self.assertIn("name_similarity", components)

    def test_a_run_row_records_its_dataset_cost_and_finish(self):
        # The dataset id has to land BEFORE the run is polled: if the process
        # dies mid-run, that id is the only way back to the results.
        store.start_run_row(
            self.cursor,
            run_id="test-run-1",
            actor_id="actor-x",
            payload={"locationNames": ["a"]},
            status="running",
        )
        store.update_run_row(self.cursor, "test-run-1", dataset_id="ds-1")
        store.update_run_row(
            self.cursor,
            "test-run-1",
            status="succeeded",
            places_found=3,
            reviews_found=42,
            cost_usd=0.0412,
            finished_at=datetime.now(timezone.utc),
        )

        self.cursor.execute(
            "SELECT dataset_id, status, places_found, reviews_found, cost_usd, "
            "       finished_at, input_payload "
            "  FROM staging.scrape_runs WHERE run_id = 'test-run-1'"
        )
        dataset, status, places, reviews, cost, finished, payload = self.cursor.fetchone()
        self.assertEqual(dataset, "ds-1")
        self.assertEqual(status, "succeeded")
        self.assertEqual((places, reviews), (3, 42))
        self.assertAlmostEqual(float(cost), 0.0412)
        self.assertIsNotNone(finished)
        self.assertEqual(payload, {"locationNames": ["a"]})

    def test_spend_is_summed_across_runs(self):
        for n, cost in enumerate([0.10, 0.25, 0.05]):
            store.start_run_row(
                self.cursor, run_id=f"spend-{n}", actor_id="a", payload={}, status="succeeded"
            )
            store.update_run_row(self.cursor, f"spend-{n}", cost_usd=cost)
        self.assertAlmostEqual(store.total_spend_usd(self.cursor), 0.40)

    # ── pairing ──────────────────────────────────────────────────────────

    def test_places_are_paired_to_the_right_contractor_regardless_of_order(self):
        a, b = self.contractors
        places = {
            "p-b": {"place_name": b.display_name,
                    "place_address": f"2 Main St, {b.city}, CA 90002, USA",
                    "place_category": "Plumber"},
            "p-a": {"place_name": a.display_name,
                    "place_address": f"1 Main St, {a.city}, CA 90001, USA",
                    "place_category": "Plumber"},
        }
        pairing = ingest.pair_places_to_contractors([a, b], places)
        self.assertEqual(pairing["p-a"][0].id, a.id)
        self.assertEqual(pairing["p-b"][0].id, b.id)

    def test_a_contractor_wins_at_most_one_place(self):
        a, _b = self.contractors
        # Two plausible places for the same contractor. Attaching both would
        # double count the reviews downstream.
        places = {
            "p-1": {"place_name": a.display_name,
                    "place_address": f"1 Main St, {a.city}, CA 90001, USA",
                    "place_category": "Plumber"},
            "p-2": {"place_name": a.display_name,
                    "place_address": f"9 Other Rd, {a.city}, CA 90001, USA",
                    "place_category": "Plumber"},
        }
        pairing = ingest.pair_places_to_contractors([a], places)
        self.assertEqual(len(pairing), 1)

    # ── the work queue ───────────────────────────────────────────────────

    def test_a_confirmed_contractor_leaves_the_queue(self):
        contractor = self.contractors[0]
        before = ingest.load_batch(self.conn, 30, 5000)
        self.assertIn(contractor.id, [c.id for c in before])

        place = store.place_from_row({"placeId": "ChIJtest040", "placeName": "X"})
        store.upsert_places(self.cursor, [place])
        store.upsert_match(
            self.cursor,
            {
                "contractor_id": contractor.id,
                "place_id": place["place_id"],
                "match_status": "confirmed",
                "match_score": 0.9,
                "score_components": {},
                "query_used": contractor.query,
            },
        )

        # load_batch opens its own cursor on the same connection, so it sees
        # the uncommitted write — which is what makes this rollback-safe.
        after = ingest.load_batch(self.conn, 30, 5000)
        self.assertNotIn(contractor.id, [c.id for c in after])

    def test_a_recent_attempt_also_leaves_the_queue(self):
        # Without this the job spends every re-run re-querying the same
        # rejects and never advances down the list.
        contractor = self.contractors[0]
        store.record_attempt(
            self.cursor,
            {
                "contractor_id": contractor.id,
                "query_used": contractor.query,
                "run_id": "test-run",
                "outcome": "no_result",
            },
        )
        after = ingest.load_batch(self.conn, 30, 5000)
        self.assertNotIn(contractor.id, [c.id for c in after])

        # And returns once the window has passed.
        reopened = ingest.load_batch(self.conn, 0, 5000)
        self.assertIn(contractor.id, [c.id for c in reopened])

    def test_the_queue_is_ordered_by_licence_for_a_resumable_cursor(self):
        batch = ingest.load_batch(self.conn, 30, 50)
        licences = [c.license_no for c in batch]
        self.assertEqual(licences, sorted(licences))

    def test_the_query_is_business_city_ca(self):
        contractor = self.contractors[0]
        self.assertEqual(
            contractor.query,
            f"{contractor.display_name}, {contractor.city}, CA",
        )


@unittest.skipUnless(DATABASE_URL, "DATABASE_URL is not set")
class ProductSchema(unittest.TestCase):
    """The one change this job makes to product schema."""

    def test_contractors_carries_a_data_source(self):
        import psycopg2

        conn = psycopg2.connect(DATABASE_URL)
        try:
            with conn.cursor() as cursor:
                cursor.execute(
                    "SELECT column_name, column_default, is_nullable "
                    "  FROM information_schema.columns "
                    " WHERE table_schema='public' AND table_name='contractors' "
                    "   AND column_name='data_source'"
                )
                row = cursor.fetchone()
                self.assertIsNotNone(row, "0020 should have added data_source")
                self.assertEqual(row[2], "NO")
                self.assertIn("cslb", row[1])

                cursor.execute("SELECT DISTINCT data_source FROM contractors")
                self.assertEqual([r[0] for r in cursor.fetchall()], ["cslb"])
        finally:
            conn.close()


if __name__ == "__main__":
    unittest.main()
