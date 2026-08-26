"""Deciding whether a Google place is actually the contractor we asked about.

This is the part of the job that matters. `locationNames` is a fuzzy search:
Google returns *something* for almost any query, and that something is routinely
the wrong business, a national chain, or a different shop in the same strip
mall. An unverified match is worse than no match at all, because it welds a
stranger's reviews onto a real licensed contractor and there is nothing in the
downstream data to say it happened.

So every returned place is scored against the contractor record that produced
the query, and every component of that score is stored. When the match rate
comes out low — and it will, 30-60% of contractors having no usable Google
presence is the expected outcome — the components are what tell you whether it
is the name comparison failing or the city parse.

Nothing here has a dependency. The comparisons are short strings and the
algorithms are twenty lines each, which is cheaper than pinning rapidfuzz and
makes the scoring reproducible from the source alone.
"""

from __future__ import annotations

import re
import unicodedata
from dataclasses import dataclass, field
from typing import Optional

# ── Thresholds ────────────────────────────────────────────────────────────
# From the spec, and deliberately not tunable from the command line. The one
# thing that would quietly ruin this dataset is somebody lowering these to make
# the match rate look better.
CONFIRM_AT = 0.75
NEEDS_REVIEW_AT = 0.50

# A name floor, on the same "forces a reject regardless of total" pattern the
# spec establishes for state. Not in the spec, and here for a reason worth
# stating plainly:
#
#   city (0.3) + state (0.2) = 0.50, which is exactly NEEDS_REVIEW_AT.
#
# So under the weights as given, ANY place in the right city and state clears
# the needs_review bar with a name score of zero and an implausible category —
# and needs_review writes the reviews. A nail salon two doors down from the
# plumber would have its reviews attached to that plumber, flagged but written.
# That is precisely the outcome Part 3 of the spec opens by calling worse than
# no match at all.
#
# Measured against real pairs, the separation is clean and not near this line:
# unrelated businesses score 0.105-0.263, the weakest plausible match ("ABC
# Plumbing" vs "ABC Electric", possibly the same owner) scores 0.417, and
# genuine matches score 0.727-1.000. 0.35 sits in empty space between them.
#
# This tightens rather than loosens. Nothing here raises the match rate.
NAME_FLOOR = 0.35

WEIGHT_NAME = 0.4
WEIGHT_CITY = 0.3
WEIGHT_STATE = 0.2
WEIGHT_CATEGORY = 0.1

# ── Name normalisation ────────────────────────────────────────────────────
# Legal suffixes carry no identifying information and appear inconsistently:
# the CSLB register says "ABC PLUMBING INC" where Google says "ABC Plumbing".
# Comparing those raw costs a fifth of the similarity score for no reason.
_LEGAL_SUFFIXES = {
    "inc",
    "incorporated",
    "llc",
    "l.l.c",
    "llp",
    "lp",
    "corp",
    "corporation",
    "co",
    "company",
    "ltd",
    "limited",
    "pc",
    "pllc",
    "dba",
    "the",
}

# Trade words are NOT stripped. "ABC Plumbing" and "ABC Electric" are different
# businesses, and dropping the trade word would score them identical.

_PUNCT = re.compile(r"[^a-z0-9\s]")
_SPACE = re.compile(r"\s+")


def normalise_name(value: Optional[str]) -> str:
    """Lowercase, strip accents, drop punctuation and legal suffixes."""
    if not value:
        return ""

    # NFKD so "Ñ" compares equal to "N" — CSLB and Google disagree about
    # accents on the same business more often than you would think.
    folded = unicodedata.normalize("NFKD", value)
    folded = "".join(c for c in folded if not unicodedata.combining(c))

    lowered = folded.lower().replace("&", " and ")
    stripped = _PUNCT.sub(" ", lowered)
    tokens = [t for t in _SPACE.sub(" ", stripped).strip().split(" ") if t]
    kept = [t for t in tokens if t not in _LEGAL_SUFFIXES]

    # A name that is *entirely* legal suffixes keeps its tokens rather than
    # becoming the empty string, which would score 0 against everything.
    return " ".join(kept or tokens)


def _levenshtein(a: str, b: str) -> int:
    """Standard DP edit distance. Business names are short; this is fine."""
    if a == b:
        return 0
    if not a:
        return len(b)
    if not b:
        return len(a)

    previous = list(range(len(b) + 1))
    for i, ca in enumerate(a, start=1):
        current = [i]
        for j, cb in enumerate(b, start=1):
            current.append(
                min(
                    previous[j] + 1,  # deletion
                    current[j - 1] + 1,  # insertion
                    previous[j - 1] + (ca != cb),  # substitution
                )
            )
        previous = current
    return previous[-1]


def levenshtein_ratio(a: str, b: str) -> float:
    if not a and not b:
        return 0.0
    longest = max(len(a), len(b))
    return 1.0 - (_levenshtein(a, b) / longest) if longest else 0.0


def token_set_ratio(a: str, b: str) -> float:
    """Order-insensitive similarity that tolerates extra words.

    "Stillwater Plumbing" against "Stillwater Plumbing and Rooter" should score
    high — it is the same business with a service line appended — but plain edit
    distance punishes the extra tokens heavily. This compares the shared tokens
    against each full name, which is the rapidfuzz token_set_ratio idea.
    """
    ta, tb = set(a.split()), set(b.split())
    if not ta or not tb:
        return 0.0

    shared = " ".join(sorted(ta & tb))
    only_a = " ".join(sorted(ta - tb))
    only_b = " ".join(sorted(tb - ta))

    combined_a = f"{shared} {only_a}".strip()
    combined_b = f"{shared} {only_b}".strip()

    if not shared:
        return levenshtein_ratio(combined_a, combined_b)

    return max(
        levenshtein_ratio(shared, combined_a),
        levenshtein_ratio(shared, combined_b),
        levenshtein_ratio(combined_a, combined_b),
    )


def name_similarity(contractor_name: str, place_name: str) -> float:
    """The better of the two measures.

    They fail in different directions — edit distance punishes extra words,
    token overlap punishes spelling drift — so taking the max means a name has
    to look wrong under BOTH to score low.
    """
    a, b = normalise_name(contractor_name), normalise_name(place_name)
    if not a or not b:
        return 0.0
    return max(levenshtein_ratio(a, b), token_set_ratio(a, b))


# ── Address parsing ───────────────────────────────────────────────────────
# Google writes "5530 Berkshire Dr, Los Angeles, CA 90032, USA". The state and
# ZIP travel together in one comma component, which is what makes the city
# recoverable: it is the component immediately before that one.
_STATE_ZIP = re.compile(r"^([A-Z]{2})(?:\s+(\d{5})(?:-\d{4})?)?$")


@dataclass
class ParsedAddress:
    city: Optional[str] = None
    state: Optional[str] = None
    postal_code: Optional[str] = None


def parse_address(place_address: Optional[str]) -> ParsedAddress:
    if not place_address:
        return ParsedAddress()

    parts = [p.strip() for p in place_address.split(",") if p.strip()]
    for index, part in enumerate(parts):
        match = _STATE_ZIP.match(part)
        if match:
            return ParsedAddress(
                city=parts[index - 1] if index > 0 else None,
                state=match.group(1),
                postal_code=match.group(2),
            )

    # No "CA 90032" component. Fall back to a bare two-letter token so an
    # address written "Los Angeles CA" is not thrown away, but do not invent a
    # city — a wrong city is worse than an absent one, because it scores.
    tail = re.search(r"\b([A-Z]{2})\b\s*(\d{5})?\s*(?:,\s*USA)?\s*$", place_address)
    if tail:
        return ParsedAddress(state=tail.group(1), postal_code=tail.group(2))

    return ParsedAddress()


def _normalise_city(value: Optional[str]) -> str:
    if not value:
        return ""
    folded = unicodedata.normalize("NFKD", value)
    folded = "".join(c for c in folded if not unicodedata.combining(c))
    return _SPACE.sub(" ", _PUNCT.sub(" ", folded.lower())).strip()


# ── Category plausibility ─────────────────────────────────────────────────
# Substrings rather than exact values, because Google's category vocabulary is
# large, changes without notice, and is not ours. "Roofing contractor",
# "Commercial roofing contractor" and "Roofer" should all pass on "roof".
TRADE_KEYWORDS = frozenset(
    {
        "plumb",
        "electric",
        "roof",
        "paint",
        "landscap",
        "contractor",
        "construction",
        "builder",
        "hvac",
        "heating",
        "air conditioning",
        "remodel",
        "renovat",
        "carpent",
        "concrete",
        "mason",
        "handyman",
        "flooring",
        "drywall",
        "fence",
        "solar",
        "pool",
        "tile",
        "window",
        "door",
        "garage door",
        "septic",
        "well drilling",
        "insulation",
        "gutter",
        "sheet metal",
        "welding",
        "excavat",
        "pav",
        "demolition",
        "restoration",
        "waterproof",
        "cabinet",
        "glass",
        "sign",
        "elevator",
        "fire protection",
        "lock",
        "tree service",
        "swimming pool",
    }
)


def category_plausible(place_category: Optional[str]) -> bool:
    if not place_category:
        return False
    lowered = place_category.lower()
    return any(keyword in lowered for keyword in TRADE_KEYWORDS)


# ── Scoring ───────────────────────────────────────────────────────────────


@dataclass
class MatchResult:
    status: str  # confirmed | needs_review | rejected
    score: float
    components: dict = field(default_factory=dict)

    @property
    def writes_reviews(self) -> bool:
        """Reviews are written for confirmed and needs_review, never rejected."""
        return self.status in ("confirmed", "needs_review")


def score_match(
    *,
    contractor_name: str,
    contractor_city: Optional[str],
    place_name: Optional[str],
    place_address: Optional[str],
    place_category: Optional[str],
) -> MatchResult:
    """Score one candidate place against the contractor that produced the query.

    Every component is returned alongside the total, because a bare score tells
    you the match rate is low and nothing about why.
    """
    parsed = parse_address(place_address)

    name = name_similarity(contractor_name, place_name or "")

    contractor_city_n = _normalise_city(contractor_city)
    place_city_n = _normalise_city(parsed.city)
    city = 1.0 if contractor_city_n and contractor_city_n == place_city_n else 0.0

    state_ok = parsed.state == "CA"
    state = 1.0 if state_ok else 0.0

    category = 1.0 if category_plausible(place_category) else 0.0

    total = (
        name * WEIGHT_NAME
        + city * WEIGHT_CITY
        + state * WEIGHT_STATE
        + category * WEIGHT_CATEGORY
    )
    total = round(total, 4)

    components = {
        "name_similarity": round(name, 4),
        "city_match": city,
        "state_match": state,
        "category_plausible": category,
        "parsed_city": parsed.city,
        "parsed_state": parsed.state,
        "contractor_city": contractor_city,
        "normalised_contractor_name": normalise_name(contractor_name),
        "normalised_place_name": normalise_name(place_name or ""),
        "weights": {
            "name": WEIGHT_NAME,
            "city": WEIGHT_CITY,
            "state": WEIGHT_STATE,
            "category": WEIGHT_CATEGORY,
        },
    }

    # A place outside California is not this contractor, whatever else lines up.
    # Chains make this bite: "ABC Plumbing" in Phoenix can score well on name
    # and category and still be a different company entirely.
    if not state_ok:
        components["rejected_because"] = "state is not CA (parsed: {})".format(
            parsed.state or "none"
        )
        return MatchResult("rejected", total, components)

    # See NAME_FLOOR. Being in the right city is not evidence of being the
    # right business, and on its own it should not be enough to attach
    # somebody else's reviews to a licensed contractor.
    if name < NAME_FLOOR:
        components["rejected_because"] = (
            f"name similarity {round(name, 3)} below the {NAME_FLOOR} floor — "
            "right area, wrong business"
        )
        return MatchResult("rejected", total, components)

    if total >= CONFIRM_AT:
        return MatchResult("confirmed", total, components)
    if total >= NEEDS_REVIEW_AT:
        return MatchResult("needs_review", total, components)

    components["rejected_because"] = f"score {total} below {NEEDS_REVIEW_AT}"
    return MatchResult("rejected", total, components)
