//! How good a listing looks, as one number between 0 and 1.
//!
//! The directory has ordered alphabetically since it existed, which means the
//! first page is whoever is called "A...". Every signal needed to do better is
//! already stored and none of it has ever influenced an order.
//!
//! The score is deliberately explainable. Each term is a thing a person would
//! give as a reason — this one is rated well, this one has been rated by a lot
//! of people, this one proved who they are, this one bothered to fill the page
//! in — and the weights say how much each reason counts. When a listing ranks
//! somewhere surprising, the answer is arithmetic rather than a model.
//!
//! Nothing here writes SQL or reads the database: it takes the facts and
//! returns a number, which is what makes it testable without Postgres and what
//! keeps the rule in one place instead of inside an `ORDER BY`.

/// How much each reason counts. Sums to 1.0 at the defaults, so a score is
/// always in 0..=1 and can be read as a percentage of "as good as this
/// directory has seen".
#[derive(Debug, Clone, Copy)]
pub struct Weights {
    pub rating: f32,
    pub reviews: f32,
    pub verified: f32,
    pub claimed: f32,
    pub completeness: f32,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            // Rating leads, but not by so much that one five-star review beats
            // a long record — that is what the reviews term is for.
            rating: 0.40,
            reviews: 0.25,
            // Proving who you are is worth more than filling in a bio, and less
            // than being good at the job.
            verified: 0.20,
            claimed: 0.05,
            completeness: 0.10,
        }
    }
}

/// The configured weights are the same five numbers, validated at boot. The
/// conversion exists so `cm-core` does not have to know what a ranking is and
/// `cm-domain` does not have to read the environment.
impl From<cm_core::RankingConfig> for Weights {
    fn from(config: cm_core::RankingConfig) -> Self {
        Self {
            rating: config.rating,
            reviews: config.reviews,
            verified: config.verified,
            claimed: config.claimed,
            completeness: config.completeness,
        }
    }
}

impl Weights {
    fn total(&self) -> f32 {
        self.rating + self.reviews + self.verified + self.claimed + self.completeness
    }
}

/// How many reviews it takes before a listing's own rating outweighs the
/// directory's average.
///
/// This is the whole reason the rating term is Bayesian rather than raw. A
/// single five-star review is not evidence of a five-star contractor, and a
/// directory that ranked it above a 4.7 with three hundred reviews would be
/// wrong in the way most visible to anyone using it. At C = 10 a lone review
/// moves a listing about a tenth of the way from the average toward itself.
const REVIEW_PRIOR: f64 = 10.0;

/// Where the review-count term stops paying. Past this, more reviews say
/// nothing new about the business, and letting the term keep growing would let
/// volume alone dominate every other signal.
const REVIEW_SATURATION: f64 = 500.0;

/// What the directory knows about a listing, in the terms the score is built
/// from.
#[derive(Debug, Clone, Copy, Default)]
pub struct Signals {
    pub rating: Option<f64>,
    pub review_count: Option<i32>,
    pub verified: bool,
    pub claimed: bool,
    pub has_bio: bool,
    pub has_photo: bool,
    pub has_phone: bool,
    pub has_website: bool,
}

impl Signals {
    /// The fraction of the page a visitor would find filled in.
    ///
    /// Not a quality signal about the work, and weighted accordingly. It is a
    /// signal about the listing: a claimed page with a bio, a photo, a number
    /// and a site is more use to a homeowner than four blank fields, whatever
    /// the licence says.
    fn completeness(&self) -> f32 {
        let filled = [
            self.has_bio,
            self.has_photo,
            self.has_phone,
            self.has_website,
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        filled as f32 / 4.0
    }
}

/// Score one listing, given the directory's average rating.
///
/// `mean_rating` is the average across every listing that has one. It is the
/// value an unrated listing is assumed to have, which is the point: a business
/// nobody has reviewed is treated as ordinary rather than as bad, so the
/// absence of reviews is not itself a penalty.
pub fn score(signals: &Signals, mean_rating: f64, weights: &Weights) -> f32 {
    let count = signals.review_count.unwrap_or(0).max(0) as f64;

    // Bayesian average: the listing's own rating, pulled toward the directory
    // mean by however little evidence there is for it. With no reviews this is
    // exactly the mean; with many it is very nearly the listing's own rating.
    let observed = signals.rating.unwrap_or(mean_rating);
    let adjusted = (REVIEW_PRIOR * mean_rating + observed * count) / (REVIEW_PRIOR + count);

    // Ratings run 1..=5, so shift and divide to land in 0..=1.
    let rating_term = (((adjusted - 1.0) / 4.0).clamp(0.0, 1.0)) as f32;

    // Logarithmic, because the difference between 5 reviews and 50 is large and
    // the difference between 450 and 500 is not.
    let reviews_term = ((count + 1.0).ln() / (REVIEW_SATURATION + 1.0).ln()).clamp(0.0, 1.0) as f32;

    let blended = weights.rating * rating_term
        + weights.reviews * reviews_term
        + weights.verified * f32::from(signals.verified)
        + weights.claimed * f32::from(signals.claimed)
        + weights.completeness * signals.completeness();

    let total = weights.total();
    if total <= 0.0 {
        return 0.0;
    }

    (blended / total).clamp(0.0, 1.0)
}

/* ── Recomputing the whole directory ───────────────────────────────────────*/

use cm_core::AppError;
use cm_db::repo::contractors;
use cm_db::PgPool;

/// Contractors read and scored per round trip.
const BATCH: i64 = 1_000;

/// The rating an unrated listing is assumed to have when the directory has no
/// ratings at all — a fresh import, before any enrichment has run. The midpoint
/// of the scale, so nothing is advantaged or penalised by the absence.
const NEUTRAL_RATING: f64 = 3.0;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Recomputed {
    pub scanned: u64,
    pub changed: u64,
}

/// Rewrite `quality_score` for every contractor.
///
/// Runs on the nightly timer beside the badge recompute, from the same source
/// data, because both answer the same question — what does the register say
/// about this business today — and both go stale the same way.
pub async fn recompute_all(pool: &PgPool, weights: &Weights) -> Result<Recomputed, AppError> {
    let mean = {
        let mut conn = pool.acquire().await.map_err(AppError::internal)?;
        contractors::mean_rating(&mut conn)
            .await?
            .unwrap_or(NEUTRAL_RATING)
    };

    let mut stats = Recomputed::default();
    let mut after = None;

    loop {
        let batch = {
            let mut conn = pool.acquire().await.map_err(AppError::internal)?;
            contractors::ranking_signals_after(&mut conn, after, BATCH).await?
        };
        if batch.is_empty() {
            break;
        }
        after = batch.last().map(|row| row.id);
        stats.scanned += batch.len() as u64;

        let scored: Vec<(uuid::Uuid, f32)> = batch
            .iter()
            .map(|row| {
                let signals = Signals {
                    rating: row.rating,
                    review_count: row.review_count,
                    verified: row.verified,
                    claimed: row.claimed,
                    has_bio: row.has_bio,
                    has_photo: row.has_photo,
                    has_phone: row.has_phone,
                    has_website: row.has_website,
                };
                (row.id, score(&signals, mean, weights))
            })
            .collect();

        let mut conn = pool.acquire().await.map_err(AppError::internal)?;
        stats.changed += contractors::set_quality_scores(&mut conn, &scored).await?;
    }

    tracing::info!(
        scanned = stats.scanned,
        changed = stats.changed,
        mean_rating = mean,
        "recomputed quality scores"
    );

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEAN: f64 = 4.2;

    fn rated(rating: f64, count: i32) -> Signals {
        Signals {
            rating: Some(rating),
            review_count: Some(count),
            ..Signals::default()
        }
    }

    /// The property the whole rating term exists for. A perfect score from one
    /// person is not evidence; a near-perfect score from three hundred is.
    #[test]
    fn one_five_star_review_does_not_beat_a_long_good_record() {
        let weights = Weights::default();
        let newcomer = score(&rated(5.0, 1), MEAN, &weights);
        let established = score(&rated(4.7, 300), MEAN, &weights);

        assert!(
            established > newcomer,
            "4.7 across 300 reviews ({established:.3}) must outrank 5.0 across 1 ({newcomer:.3})"
        );
    }

    /// Having no reviews is not the same as being rated badly, and must not be
    /// scored as though it were.
    #[test]
    fn an_unrated_listing_sits_at_the_average_not_at_zero() {
        let weights = Weights::default();
        let unrated = score(&Signals::default(), MEAN, &weights);
        let average = score(&rated(MEAN, 40), MEAN, &weights);
        let poor = score(&rated(2.0, 40), MEAN, &weights);

        assert!(unrated > poor, "{unrated:.3} vs {poor:.3}");
        assert!(
            unrated < average,
            "an unrated listing should still rank below a proven average one: \
             {unrated:.3} vs {average:.3}"
        );
    }

    /// Trust is a signal, and it is one a listing cannot award itself: the
    /// badge comes from an approved claim and an active licence.
    #[test]
    fn proving_who_you_are_counts_for_something() {
        let weights = Weights::default();
        let plain = rated(4.5, 50);
        let proven = Signals {
            verified: true,
            claimed: true,
            ..plain
        };

        assert!(score(&proven, MEAN, &weights) > score(&plain, MEAN, &weights));
    }

    /// A filled-in page is worth more to a visitor than a blank one, and less
    /// than being good at the job.
    #[test]
    fn a_filled_in_page_helps_but_cannot_outweigh_the_work() {
        let weights = Weights::default();
        let complete_but_poor = Signals {
            has_bio: true,
            has_photo: true,
            has_phone: true,
            has_website: true,
            ..rated(2.5, 60)
        };
        let bare_but_good = rated(4.9, 60);

        assert!(
            score(&bare_but_good, MEAN, &weights) > score(&complete_but_poor, MEAN, &weights),
            "completeness must not rescue a badly rated listing"
        );
    }

    /// Every score is a fraction, whatever it is made of. The column has a
    /// CHECK saying so, and a score outside the range would fail the write
    /// rather than rank strangely.
    #[test]
    fn a_score_is_always_between_zero_and_one() {
        let weights = Weights::default();
        let extremes = [
            Signals::default(),
            rated(5.0, i32::MAX),
            rated(1.0, 0),
            Signals {
                rating: Some(5.0),
                review_count: Some(100_000),
                verified: true,
                claimed: true,
                has_bio: true,
                has_photo: true,
                has_phone: true,
                has_website: true,
            },
            // Values the database constrains but the function should survive.
            Signals {
                rating: Some(-3.0),
                review_count: Some(-10),
                ..Signals::default()
            },
        ];

        for signals in extremes {
            let value = score(&signals, MEAN, &weights);
            assert!(
                (0.0..=1.0).contains(&value),
                "{signals:?} scored {value}, outside 0..=1"
            );
        }
    }

    /// More reviews never hurt, and eventually stop helping.
    #[test]
    fn the_review_term_rises_and_then_flattens() {
        let weights = Weights::default();
        let at = |count| score(&rated(4.5, count), MEAN, &weights);

        assert!(at(50) > at(5));
        assert!(at(500) > at(50));

        let early = at(50) - at(5);
        let late = at(500) - at(450);
        assert!(
            early > late,
            "the first reviews must count for more than the five-hundredth: \
             {early:.4} vs {late:.4}"
        );
    }

    /// Weights that sum to nothing produce no ranking rather than a division by
    /// zero. Reachable only from configuration, which is exactly where a
    /// mistake like this comes from.
    #[test]
    fn zero_weights_score_zero_rather_than_panicking() {
        let none = Weights {
            rating: 0.0,
            reviews: 0.0,
            verified: 0.0,
            claimed: 0.0,
            completeness: 0.0,
        };
        assert_eq!(score(&rated(5.0, 100), MEAN, &none), 0.0);
    }
}
