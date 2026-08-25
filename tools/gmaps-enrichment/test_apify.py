"""Tests for the Apify client that need no network.

Two things here are constraints rather than behaviour, and both are the kind
that get broken by a well-meaning edit six months from now: the token must
never reach a log, and the actor limits must not drift.
"""

import unittest

import apify_client
import ingest


class Redaction(unittest.TestCase):
    """The token travels in the query string, so every printable URL is scrubbed."""

    def test_the_token_is_replaced_in_a_url(self):
        url = "https://api.apify.com/v2/acts/abc/runs?token=secret-value-here"
        redacted = apify_client._redact(url)
        self.assertNotIn("secret-value-here", redacted)
        self.assertIn("token=%2A%2A%2A", redacted)

    def test_other_parameters_survive(self):
        url = "https://api.apify.com/v2/datasets/d1/items?token=s3cr3t&offset=1000&limit=1000"
        redacted = apify_client._redact(url)
        self.assertNotIn("s3cr3t", redacted)
        self.assertIn("offset=1000", redacted)
        self.assertIn("limit=1000", redacted)

    def test_a_url_with_no_token_is_unchanged_in_substance(self):
        redacted = apify_client._redact("https://api.apify.com/v2/acts/abc?limit=5")
        self.assertIn("limit=5", redacted)

    def test_a_client_refuses_an_empty_token(self):
        with self.assertRaises(ValueError):
            apify_client.ApifyClient("", "actor")


class ActorInput(unittest.TestCase):
    """The spec fixes these. Drifting from them costs money or gets throttled."""

    def test_concurrency_never_exceeds_three(self):
        self.assertLessEqual(ingest.ACTOR_DEFAULTS["maxConcurrency"], 3)

    def test_reviews_are_capped_and_never_unlimited(self):
        max_reviews = ingest.ACTOR_DEFAULTS["maxReviews"]
        self.assertNotEqual(max_reviews, 0, "0 means ALL reviews and burns credits")
        self.assertLessEqual(max_reviews, 50)

    def test_the_proxy_is_always_on(self):
        self.assertIs(ingest.ACTOR_DEFAULTS["useProxy"], True)

    def test_sorting_and_language(self):
        self.assertEqual(ingest.ACTOR_DEFAULTS["reviewsSort"], "newest")
        self.assertEqual(ingest.ACTOR_DEFAULTS["language"], "en")

    def test_batches_are_twenty(self):
        self.assertEqual(ingest.BATCH_SIZE, 20)

    def test_the_pause_between_batches_is_kept(self):
        self.assertGreaterEqual(ingest.SLEEP_BETWEEN_BATCHES, 5)


class Pagination(unittest.TestCase):
    def test_a_full_page_is_one_thousand(self):
        # The loop continues while a page is full, so this constant and the
        # `limit` sent to Apify have to be the same number.
        self.assertEqual(apify_client.DATASET_PAGE, 1000)

    def test_the_run_timeout_is_thirty_minutes(self):
        self.assertEqual(apify_client.RUN_TIMEOUT_SECONDS, 30 * 60)

    def test_polling_is_every_fifteen_seconds(self):
        self.assertEqual(apify_client.POLL_SECONDS, 15)

    def test_backoff_is_three_retries_from_five_seconds(self):
        self.assertEqual(apify_client.MAX_RETRIES, 3)
        self.assertEqual(apify_client.BACKOFF_BASE_SECONDS, 5)

    def test_every_terminal_status_is_recognised(self):
        for status in ("SUCCEEDED", "FAILED", "TIMED-OUT", "ABORTED"):
            self.assertIn(status, apify_client.TERMINAL_STATUSES)
        self.assertNotIn("RUNNING", apify_client.TERMINAL_STATUSES)


class Cost(unittest.TestCase):
    def test_the_run_reports_its_own_cost(self):
        handle = apify_client.RunHandle("r1", "d1", "SUCCEEDED", {"usageTotalUsd": 0.0412})
        self.assertAlmostEqual(apify_client.run_cost_usd(handle), 0.0412)

    def test_a_run_with_no_reported_cost_is_zero_rather_than_a_guess(self):
        handle = apify_client.RunHandle("r1", "d1", "SUCCEEDED", {})
        self.assertEqual(apify_client.run_cost_usd(handle), 0.0)


if __name__ == "__main__":
    unittest.main()
