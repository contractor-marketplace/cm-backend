"""Tests for the match scoring.

The negative cases carry the weight here. A match that should have been
rejected and was not is the failure that actually costs something: it welds a
stranger's reviews onto a licensed contractor, and nothing downstream can tell.

    python3 -m unittest discover -s tools/gmaps-enrichment -v
"""

import unittest

import matching
import store


class NameNormalisation(unittest.TestCase):
    def test_legal_suffixes_are_dropped(self):
        self.assertEqual(matching.normalise_name("ABC Plumbing, Inc."), "abc plumbing")
        self.assertEqual(matching.normalise_name("ABC PLUMBING LLC"), "abc plumbing")
        self.assertEqual(matching.normalise_name("The ABC Plumbing Co."), "abc plumbing")

    def test_ampersand_becomes_a_word_so_it_survives_punctuation_stripping(self):
        # "Ibarra & Daughters" against "Ibarra and Daughters" should be the
        # same business, and it is only the same if & normalises to "and".
        self.assertEqual(
            matching.normalise_name("Ibarra & Daughters"),
            matching.normalise_name("Ibarra and Daughters"),
        )

    def test_accents_fold(self):
        self.assertEqual(matching.normalise_name("Núñez Roofing"), "nunez roofing")

    def test_a_name_that_is_all_suffixes_keeps_its_tokens(self):
        # Stripping to "" would score 0 against everything, which reads as a
        # rejected match rather than as an unusable name.
        self.assertNotEqual(matching.normalise_name("The Co"), "")

    def test_trade_words_are_not_stripped(self):
        # "ABC Plumbing" and "ABC Electric" are different businesses.
        self.assertLess(matching.name_similarity("ABC Plumbing", "ABC Electric"), 0.8)


class NameSimilarity(unittest.TestCase):
    def test_identical_after_normalisation_scores_one(self):
        self.assertEqual(matching.name_similarity("ABC Plumbing Inc", "ABC Plumbing"), 1.0)

    def test_an_appended_service_line_still_scores_high(self):
        score = matching.name_similarity("Stillwater Plumbing", "Stillwater Plumbing & Rooter")
        self.assertGreater(score, 0.75, f"token overlap should carry this: {score}")

    def test_word_order_does_not_matter(self):
        score = matching.name_similarity("Meridian Electric Co", "Electric Meridian")
        self.assertGreater(score, 0.7, f"got {score}")

    def test_an_unrelated_business_scores_low(self):
        self.assertLess(matching.name_similarity("ABC Plumbing", "Starbucks"), 0.4)


class AddressParsing(unittest.TestCase):
    def test_the_usual_google_shape(self):
        parsed = matching.parse_address("5530 Berkshire Dr, Los Angeles, CA 90032, USA")
        self.assertEqual(parsed.city, "Los Angeles")
        self.assertEqual(parsed.state, "CA")
        self.assertEqual(parsed.postal_code, "90032")

    def test_a_suite_number_does_not_confuse_it(self):
        parsed = matching.parse_address("227 W Valley Blvd Ste 288B, San Gabriel, CA 91776, USA")
        self.assertEqual(parsed.city, "San Gabriel")
        self.assertEqual(parsed.state, "CA")

    def test_no_zip_still_yields_a_city(self):
        parsed = matching.parse_address("1 Main St, Burbank, CA, USA")
        self.assertEqual(parsed.city, "Burbank")
        self.assertEqual(parsed.state, "CA")

    def test_an_unparseable_address_invents_nothing(self):
        # A wrong city is worse than an absent one, because it scores.
        parsed = matching.parse_address("somewhere near the freeway")
        self.assertIsNone(parsed.city)
        self.assertIsNone(parsed.state)

    def test_an_out_of_state_address_reports_its_state(self):
        parsed = matching.parse_address("100 E Washington St, Phoenix, AZ 85004, USA")
        self.assertEqual(parsed.state, "AZ")


class Categories(unittest.TestCase):
    def test_the_trades_we_carry_are_plausible(self):
        for category in [
            "Plumber",
            "Electrician",
            "Roofing contractor",
            "General contractor",
            "HVAC contractor",
            "Painter",
            "Landscaper",
            "Commercial roofing contractor",
        ]:
            self.assertTrue(matching.category_plausible(category), category)

    def test_a_coffee_shop_is_not(self):
        for category in ["Coffee shop", "Restaurant", "Nail salon", "Bank"]:
            self.assertFalse(matching.category_plausible(category), category)

    def test_a_missing_category_is_not_plausible(self):
        self.assertFalse(matching.category_plausible(None))
        self.assertFalse(matching.category_plausible(""))


class Scoring(unittest.TestCase):
    def test_a_clean_match_is_confirmed(self):
        result = matching.score_match(
            contractor_name="STILLWATER PLUMBING INC",
            contractor_city="BURBANK",
            place_name="Stillwater Plumbing",
            place_address="1000 N Hollywood Way, Burbank, CA 91505, USA",
            place_category="Plumber",
        )
        self.assertEqual(result.status, "confirmed")
        self.assertGreaterEqual(result.score, matching.CONFIRM_AT)
        self.assertTrue(result.writes_reviews)

    def test_out_of_state_is_rejected_however_well_everything_else_scores(self):
        # The chain case: same name, right category, wrong state.
        result = matching.score_match(
            contractor_name="Stillwater Plumbing",
            contractor_city="Burbank",
            place_name="Stillwater Plumbing",
            place_address="100 E Washington St, Phoenix, AZ 85004, USA",
            place_category="Plumber",
        )
        self.assertEqual(result.status, "rejected")
        self.assertFalse(result.writes_reviews)
        self.assertIn("rejected_because", result.components)

    def test_an_unparseable_state_is_rejected_rather_than_assumed(self):
        result = matching.score_match(
            contractor_name="Stillwater Plumbing",
            contractor_city="Burbank",
            place_name="Stillwater Plumbing",
            place_address="somewhere near the freeway",
            place_category="Plumber",
        )
        self.assertEqual(result.status, "rejected")

    def test_the_strip_mall_neighbour_is_rejected(self):
        # The case the name floor exists for. City and state alone are worth
        # exactly the needs_review threshold, so without the floor this nail
        # salon would have its reviews written against a plumber.
        result = matching.score_match(
            contractor_name="Stillwater Plumbing",
            contractor_city="Burbank",
            place_name="Sunrise Nail Salon",
            place_address="1002 N Hollywood Way, Burbank, CA 91505, USA",
            place_category="Nail salon",
        )
        self.assertEqual(result.status, "rejected")

    def test_right_name_wrong_city_lands_in_needs_review(self):
        # Worth a human look rather than an automatic yes or no: a contractor
        # licensed in one city genuinely may list a Google address in the next
        # one over.
        result = matching.score_match(
            contractor_name="Stillwater Plumbing",
            contractor_city="Burbank",
            place_name="Stillwater Plumbing",
            place_address="1 Main St, Glendale, CA 91203, USA",
            place_category="Plumber",
        )
        self.assertEqual(result.status, "needs_review")
        self.assertTrue(result.writes_reviews, "needs_review still writes, but flagged")

    def test_every_component_is_recorded(self):
        result = matching.score_match(
            contractor_name="ABC Plumbing Inc",
            contractor_city="Burbank",
            place_name="ABC Plumbing",
            place_address="1 Main St, Burbank, CA 91505, USA",
            place_category="Plumber",
        )
        for key in ("name_similarity", "city_match", "state_match", "category_plausible"):
            self.assertIn(key, result.components)
        # And enough context to debug a low match rate without re-running.
        self.assertIn("parsed_city", result.components)
        self.assertIn("normalised_place_name", result.components)

    def test_city_and_state_alone_cannot_reach_needs_review(self):
        # The arithmetic that makes NAME_FLOOR necessary, pinned so that
        # changing a weight surfaces the interaction rather than hiding it.
        self.assertAlmostEqual(matching.WEIGHT_CITY + matching.WEIGHT_STATE, 0.5)
        self.assertAlmostEqual(matching.NEEDS_REVIEW_AT, 0.5)

        result = matching.score_match(
            contractor_name="Stillwater Plumbing",
            contractor_city="Burbank",
            place_name="Completely Different Business",
            place_address="1 Main St, Burbank, CA 91505, USA",
            place_category="Bank",
        )
        self.assertEqual(result.status, "rejected")
        self.assertIn("floor", result.components["rejected_because"])

    def test_the_name_floor_lets_a_weak_but_plausible_match_through(self):
        # "ABC Plumbing" vs "ABC Electric" scores 0.417 — possibly the same
        # owner under a second trade name. That is a human's call, not an
        # automatic reject.
        self.assertGreater(matching.name_similarity("ABC Plumbing", "ABC Electric"), matching.NAME_FLOOR)

    def test_the_weights_sum_to_one(self):
        total = (
            matching.WEIGHT_NAME
            + matching.WEIGHT_CITY
            + matching.WEIGHT_STATE
            + matching.WEIGHT_CATEGORY
        )
        self.assertAlmostEqual(total, 1.0)

    def test_a_perfect_match_scores_exactly_one(self):
        result = matching.score_match(
            contractor_name="ABC Plumbing",
            contractor_city="Burbank",
            place_name="ABC Plumbing",
            place_address="1 Main St, Burbank, CA 91505, USA",
            place_category="Plumber",
        )
        self.assertAlmostEqual(result.score, 1.0)

    def test_name_alone_cannot_confirm(self):
        # 0.4 for a perfect name plus 0.2 for the state is 0.6 — deliberately
        # short of the 0.75 bar. Loosening this is the thing that would quietly
        # ruin the dataset.
        result = matching.score_match(
            contractor_name="ABC Plumbing",
            contractor_city="Burbank",
            place_name="ABC Plumbing",
            place_address="1 Main St, Glendale, CA 91203, USA",
            place_category="Coffee shop",
        )
        self.assertLess(result.score, matching.CONFIRM_AT)


class RowSplitting(unittest.TestCase):
    """The dataset is one row per review with the place repeated on each."""

    def _row(self, **overrides):
        row = {
            "placeId": "ChIJabc123",
            "placeName": "Stillwater Plumbing",
            "placeAddress": "1000 N Hollywood Way, Burbank, CA 91505, USA",
            "placeCategory": "Plumber",
            "placeOverallRating": 4.7,
            "placeTotalReviews": 210,
            "placeUrl": "https://maps.google.com/?cid=1",
            "reviewId": "rev-1",
            "reviewNumber": 1,
            "reviewerName": "A Person",
            "rating": 5,
            "publishedAt": "2 months ago",
            "publishedAtDate": "2026-06-01T10:00:00.000Z",
            "reviewText": "Fixed the leak same day.",
            "reviewPhotoUrls": ["https://example.test/a.jpg"],
            "likesCount": 3,
        }
        row.update(overrides)
        return row

    def test_place_fields_are_lifted_off_a_review_row(self):
        place = store.place_from_row(self._row())
        self.assertEqual(place["place_id"], "ChIJabc123")
        self.assertEqual(place["total_reviews"], 210)
        self.assertEqual(place["overall_rating"], 4.7)

    def test_a_row_without_a_place_id_is_malformed(self):
        with self.assertRaises(store.MalformedRow):
            store.place_from_row(self._row(placeId=None))

    def test_the_relative_date_never_reaches_the_timestamp(self):
        review = store.review_from_row(self._row(), "ChIJabc123")
        self.assertEqual(review["published_at"], "2026-06-01T10:00:00.000Z")
        self.assertEqual(review["published_at_raw"], "2 months ago")

    def test_a_missing_iso_date_leaves_the_timestamp_null(self):
        review = store.review_from_row(self._row(publishedAtDate=None), "ChIJabc123")
        self.assertIsNone(review["published_at"])
        self.assertEqual(review["published_at_raw"], "2 months ago")

    def test_an_empty_review_is_kept(self):
        # A rating with no words still counts toward the average.
        review = store.review_from_row(self._row(reviewText=""), "ChIJabc123")
        self.assertIsNotNone(review)
        self.assertEqual(review["rating"], 5.0)

    def test_photo_urls_become_json_and_are_counted(self):
        review = store.review_from_row(self._row(), "ChIJabc123")
        self.assertEqual(review["review_photo_urls"], '["https://example.test/a.jpg"]')
        self.assertEqual(review["review_photo_count"], 1)

    def test_a_null_owner_reply_is_normal(self):
        review = store.review_from_row(self._row(ownerReply=None), "ChIJabc123")
        self.assertIsNone(review["owner_reply"])

    def test_a_row_with_no_review_id_is_a_place_with_no_review(self):
        self.assertIsNone(store.review_from_row(self._row(reviewId=None), "ChIJabc123"))

    def test_a_review_without_a_rating_is_malformed(self):
        with self.assertRaises(store.MalformedRow):
            store.review_from_row(self._row(rating=None), "ChIJabc123")


if __name__ == "__main__":
    unittest.main()
