//! Search input handling: validation, defaults and the page cursor.
//!
//! One deliberate asymmetry, inherited from how the front end already behaves:
//! a junk *optional filter* is dropped rather than 400-ing the page, because a
//! visitor who followed a shared link cannot act on a validation error they
//! never typed. A junk *structural* parameter — the cursor, the page size — is
//! a 400, because silently ignoring it would return the wrong page and look
//! like data loss.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use cm_core::AppError;
use cm_db::repo::search::{BoundingBox, Cursor, Filters, Near, Sort, MAX_PAGE};
use uuid::Uuid;

/// Raw query parameters, all optional and all strings, as they arrive.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct RawQuery {
    pub q: Option<String>,
    pub trade: Option<String>,
    pub verified: Option<String>,
    pub zip: Option<String>,
    pub lat: Option<String>,
    pub lon: Option<String>,
    pub radius_m: Option<String>,
    pub bbox: Option<String>,
    pub sort: Option<String>,
    pub limit: Option<String>,
    pub cursor: Option<String>,
}

/// The validated request, plus a note of anything that was dropped.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub filters: Filters,
    pub sort: Sort,
    pub limit: i64,
    pub cursor: Option<Cursor>,
    /// Filters that were ignored because they could not be parsed. Returned to
    /// the caller so "why did my filter do nothing" is answerable.
    pub ignored: Vec<String>,
}

/// The largest radius a single query may ask for: about 125 miles, comfortably
/// more than the launch county and small enough that the index stays useful.
pub const MAX_RADIUS_M: f64 = 200_000.0;
const DEFAULT_LIMIT: i64 = 20;

pub fn parse(raw: &RawQuery, trade_ids: Vec<Uuid>) -> Result<SearchRequest, AppError> {
    let mut ignored = Vec::new();
    let mut filters = Filters {
        trade_ids,
        ..Filters::default()
    };

    filters.query = raw
        .q
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        // Bounded so a pathological query string cannot become a pathological
        // tsquery.
        .map(|q| q.chars().take(200).collect());

    filters.verified_only = matches!(raw.verified.as_deref(), Some("1" | "true" | "yes"));

    // A trade nobody offers is a dropped filter and has to say so. The caller
    // resolves slugs to ids; slugs that match no trade simply do not come back,
    // and an empty set reads downstream as "no trade filter" — so `?trade=banana`
    // widened the search to every contractor in the county and reported nothing,
    // the one filter with no such reporting. Only a wholly unresolved parameter
    // is reported: `?trade=plumber,banana` still filters by plumber, and saying
    // "trade" there would make the client clear a control that is working.
    if raw
        .trade
        .as_deref()
        .map(str::trim)
        .is_some_and(|trade| !trade.is_empty())
        && filters.trade_ids.is_empty()
    {
        ignored.push("trade".to_owned());
    }

    match raw.zip.as_deref().map(str::trim).filter(|z| !z.is_empty()) {
        Some(zip) if zip.len() == 5 && zip.chars().all(|c| c.is_ascii_digit()) => {
            filters.postal_code = Some(zip.to_owned());
        }
        Some(_) => ignored.push("zip".to_owned()),
        None => {}
    }

    // Latitude, longitude and radius are one filter in three parts. A partial
    // set is a half-filled form, not an instruction to hide the county.
    match (
        parse_f64(raw.lat.as_deref()),
        parse_f64(raw.lon.as_deref()),
        parse_f64(raw.radius_m.as_deref()),
    ) {
        (Some(lat), Some(lon), radius)
            if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) =>
        {
            // No radius means no narrowing, not a default one. A location on
            // its own asks "who travels here", which coverage answers; a radius
            // is the separate, rarer request to see only the closest of them.
            // Defaulting it would silently hide every contractor who covers the
            // address from further away, which is most of the point.
            filters.near = Some(Near {
                lat,
                lon,
                radius_m: radius.map(|r| r.clamp(1.0, MAX_RADIUS_M)),
            });
        }
        (None, None, None) => {}
        _ => ignored.push("lat/lon/radius_m".to_owned()),
    }

    match raw.bbox.as_deref().map(parse_bbox) {
        Some(Some(bbox)) => filters.bbox = Some(bbox),
        Some(None) => ignored.push("bbox".to_owned()),
        None => {}
    }

    let sort = match raw.sort.as_deref() {
        // "relevance" is the name the previous release used for the default and
        // is still accepted, so a shared or bookmarked link keeps working.
        None | Some("best") | Some("relevance") => Sort::Best,
        Some("rating") => Sort::Rating,
        Some("distance") => {
            if filters.near.is_none() {
                // Sorting by distance from nowhere is meaningless; say so
                // rather than silently returning alphabetical order.
                return Err(AppError::invalid(
                    "sort=distance needs lat and lon to measure from",
                ));
            }
            Sort::Distance
        }
        Some("name") => Sort::Name,
        Some(other) => {
            return Err(AppError::invalid(format!(
                "unknown sort \"{other}\"; expected best, rating, distance or name"
            )))
        }
    };

    // Structural: a bad value here would return the wrong page.
    let limit = match raw.limit.as_deref() {
        None => DEFAULT_LIMIT,
        Some(value) => value
            .parse::<i64>()
            .map_err(|_| AppError::invalid("limit must be a whole number"))?
            .clamp(1, MAX_PAGE),
    };

    let cursor = raw
        .cursor
        .as_deref()
        .filter(|c| !c.is_empty())
        .map(decode_cursor)
        .transpose()?;

    Ok(SearchRequest {
        filters,
        sort,
        limit,
        cursor,
        ignored,
    })
}

fn parse_f64(value: Option<&str>) -> Option<f64> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

/// `min_lon,min_lat,max_lon,max_lat`, the order every mapping library uses.
fn parse_bbox(value: &str) -> Option<BoundingBox> {
    let parts: Vec<f64> = value
        .split(',')
        .map(str::trim)
        .filter_map(|part| part.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .collect();

    let [min_lon, min_lat, max_lon, max_lat] = parts[..] else {
        return None;
    };

    if !(-180.0..=180.0).contains(&min_lon)
        || !(-180.0..=180.0).contains(&max_lon)
        || !(-90.0..=90.0).contains(&min_lat)
        || !(-90.0..=90.0).contains(&max_lat)
        || min_lon >= max_lon
        || min_lat >= max_lat
    {
        return None;
    }

    Some(BoundingBox {
        min_lat,
        min_lon,
        max_lat,
        max_lon,
    })
}

/// Cursors are opaque: the client should not construct one, and encoding the
/// sort key in the clear invites exactly that.
/// Opaque by encoding, and versioned by shape.
///
/// The payload gained a third field when the cursor learned to carry the key
/// its ordering leads with. A cursor from the previous release has two, fails
/// the shape check in `decode_cursor`, and comes back as a 400 that says the
/// page cursor is not valid — which is the right answer. The alternative was to
/// accept it and resume a scored ordering from a name, which is the bug this
/// replaced.
pub fn encode_cursor(cursor: &Cursor) -> String {
    let key = cursor
        .sort_key
        .map(|value| value.to_string())
        .unwrap_or_default();
    URL_SAFE_NO_PAD.encode(format!("{}\u{0}{}\u{0}{}", cursor.id, key, cursor.name))
}

fn decode_cursor(value: &str) -> Result<Cursor, AppError> {
    let invalid = || AppError::invalid("that page cursor is not valid");

    let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|_| invalid())?;
    let text = String::from_utf8(bytes).map_err(|_| invalid())?;

    // id \0 sort key \0 name. The name is last because it is the only field
    // that may itself contain anything, and splitting from the left twice
    // leaves the remainder intact however it is spelled.
    let (id, rest) = text.split_once('\u{0}').ok_or_else(invalid)?;
    let (key, name) = rest.split_once('\u{0}').ok_or_else(invalid)?;

    let sort_key = if key.is_empty() {
        None
    } else {
        Some(key.parse::<f64>().map_err(|_| invalid())?)
    };
    if sort_key.is_some_and(|value| !value.is_finite()) {
        return Err(invalid());
    }

    Ok(Cursor {
        id: Uuid::parse_str(id).map_err(|_| invalid())?,
        name: name.to_owned(),
        sort_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw() -> RawQuery {
        RawQuery::default()
    }

    #[test]
    fn an_empty_query_searches_everything() {
        let request = parse(&raw(), vec![]).expect("parse");

        assert!(request.filters.query.is_none());
        assert!(!request.filters.verified_only);
        assert!(request.filters.near.is_none());
        assert_eq!(request.limit, DEFAULT_LIMIT);
        assert!(request.ignored.is_empty());
    }

    #[test]
    fn a_junk_optional_filter_is_dropped_and_reported_not_fatal() {
        let request = parse(
            &RawQuery {
                zip: Some("banana".into()),
                bbox: Some("nonsense".into()),
                ..raw()
            },
            vec![],
        )
        .expect("a browsing visitor must still get results");

        assert!(request.filters.postal_code.is_none());
        assert!(request.filters.bbox.is_none());
        assert_eq!(request.ignored, vec!["zip", "bbox"]);
    }

    #[test]
    fn a_junk_structural_parameter_is_a_400() {
        assert!(parse(
            &RawQuery {
                limit: Some("banana".into()),
                ..raw()
            },
            vec![]
        )
        .is_err());

        assert!(parse(
            &RawQuery {
                cursor: Some("!!!not-base64!!!".into()),
                ..raw()
            },
            vec![]
        )
        .is_err());
    }

    #[test]
    fn a_partial_location_filter_is_ignored_rather_than_half_applied() {
        let request = parse(
            &RawQuery {
                lat: Some("34.1".into()),
                ..raw()
            },
            vec![],
        )
        .expect("parse");

        assert!(request.filters.near.is_none());
        assert_eq!(request.ignored, vec!["lat/lon/radius_m"]);
    }

    #[test]
    fn a_radius_is_clamped_rather_than_refused() {
        let request = parse(
            &RawQuery {
                lat: Some("34.1".into()),
                lon: Some("-118.2".into()),
                radius_m: Some("99999999".into()),
                ..raw()
            },
            vec![],
        )
        .expect("parse");

        assert_eq!(
            request.filters.near.expect("near").radius_m,
            Some(MAX_RADIUS_M)
        );
    }

    /// A location with no radius asks who covers it, which is the ordinary
    /// case. Filling in a default here would quietly narrow the search to a
    /// circle nobody asked for, hiding every contractor who travels further —
    /// and travelling further is what the coverage model exists to express.
    #[test]
    fn a_location_without_a_radius_does_not_acquire_one() {
        let request = parse(
            &RawQuery {
                lat: Some("34.1".into()),
                lon: Some("-118.2".into()),
                radius_m: None,
                ..raw()
            },
            vec![],
        )
        .expect("parse");

        let near = request.filters.near.expect("near");
        assert_eq!(
            near.radius_m, None,
            "a centre alone must not imply a radius"
        );
        assert!(request.ignored.is_empty(), "{:?}", request.ignored);
    }

    /// The wire names the API accepts, including the one it used to use.
    #[test]
    fn the_sort_vocabulary_is_stable_and_named() {
        let sorted = |value: Option<&str>| {
            parse(
                &RawQuery {
                    sort: value.map(str::to_owned),
                    ..raw()
                },
                vec![],
            )
            .map(|request| request.sort)
        };

        assert_eq!(sorted(None).expect("default"), Sort::Best);
        assert_eq!(sorted(Some("best")).expect("best"), Sort::Best);
        // Still accepted, so a link shared before the rename keeps working.
        assert_eq!(sorted(Some("relevance")).expect("relevance"), Sort::Best);
        assert_eq!(sorted(Some("rating")).expect("rating"), Sort::Rating);
        assert_eq!(sorted(Some("name")).expect("name"), Sort::Name);

        let refused = sorted(Some("cheapest")).expect_err("unknown sort");
        assert!(
            refused.to_string().contains("best"),
            "the error should name what is accepted: {refused}"
        );
    }

    #[test]
    fn sorting_by_distance_without_a_centre_is_refused() {
        assert!(parse(
            &RawQuery {
                sort: Some("distance".into()),
                ..raw()
            },
            vec![]
        )
        .is_err());
    }

    #[test]
    fn a_page_size_is_clamped_to_the_ceiling() {
        let request = parse(
            &RawQuery {
                limit: Some("100000".into()),
                ..raw()
            },
            vec![],
        )
        .expect("parse");
        assert_eq!(request.limit, MAX_PAGE);
    }

    #[test]
    fn a_bounding_box_is_validated_not_merely_parsed() {
        assert!(parse_bbox("-118.5,33.9,-118.1,34.2").is_some());
        assert!(
            parse_bbox("-118.1,33.9,-118.5,34.2").is_none(),
            "inverted longitude"
        );
        assert!(
            parse_bbox("-118.5,34.2,-118.1,33.9").is_none(),
            "inverted latitude"
        );
        assert!(
            parse_bbox("-181,33.9,-118.1,34.2").is_none(),
            "out of range"
        );
        assert!(parse_bbox("-118.5,33.9,-118.1").is_none(), "too few parts");
        assert!(parse_bbox("").is_none());
    }

    #[test]
    fn cursors_round_trip_and_reject_tampering() {
        let cursor = Cursor {
            name: "Ibarra & Daughters".to_owned(),
            id: Uuid::now_v7(),
            sort_key: Some(0.8125),
        };
        let encoded = encode_cursor(&cursor);
        let decoded = decode_cursor(&encoded).expect("round trip");

        assert_eq!(decoded.name, cursor.name);
        assert_eq!(decoded.id, cursor.id);
        assert_eq!(decoded.sort_key, cursor.sort_key);

        // The alphabetical sort leads with the name and carries no other key.
        let keyless = Cursor {
            name: "Stillwater Plumbing".to_owned(),
            id: Uuid::now_v7(),
            sort_key: None,
        };
        assert_eq!(
            decode_cursor(&encode_cursor(&keyless))
                .expect("round trip")
                .sort_key,
            None
        );

        // A cursor from the previous release carried two fields, not three.
        // Resuming a scored ordering from a name is the defect this replaced,
        // so the old shape is refused rather than half-understood.
        let previous = URL_SAFE_NO_PAD.encode(format!("{}\u{0}Ibarra", Uuid::now_v7()));
        assert!(decode_cursor(&previous).is_err());

        // A key that is not a number, and one that is not finite.
        let bad_key = URL_SAFE_NO_PAD.encode(format!("{}\u{0}banana\u{0}Ibarra", Uuid::now_v7()));
        assert!(decode_cursor(&bad_key).is_err());
        let infinite = URL_SAFE_NO_PAD.encode(format!("{}\u{0}inf\u{0}Ibarra", Uuid::now_v7()));
        assert!(decode_cursor(&infinite).is_err());

        assert!(decode_cursor("not base64!").is_err());
        assert!(
            decode_cursor(&URL_SAFE_NO_PAD.encode("no separator here")).is_err(),
            "a well-formed base64 payload of the wrong shape is still refused"
        );
    }

    #[test]
    fn a_very_long_query_string_is_bounded() {
        let request = parse(
            &RawQuery {
                q: Some("x".repeat(10_000)),
                ..raw()
            },
            vec![],
        )
        .expect("parse");

        assert_eq!(request.filters.query.expect("query").chars().count(), 200);
    }
}
