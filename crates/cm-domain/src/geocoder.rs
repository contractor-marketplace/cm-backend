//! Address geocoding.
//!
//! Behind a trait so the provider is replaceable and so the worker can be
//! tested without a network. The default is the US Census Bureau's geocoder:
//! free, no key, authoritative for US street addresses, and — unlike the public
//! Nominatim instance — not forbidden for bulk use. Self-hosting Nominatim was
//! rejected on size: it needs tens of gigabytes, which is not what this VPS is.

use cm_core::AppError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coordinates {
    pub lat: f64,
    pub lon: f64,
}

impl Coordinates {
    /// Reject anything outside the possible range, and the (0,0) that broken
    /// geocoders return instead of an error.
    pub fn validate(self) -> Result<Self, AppError> {
        if !(-90.0..=90.0).contains(&self.lat) || !(-180.0..=180.0).contains(&self.lon) {
            return Err(AppError::invalid(
                "the geocoder returned an impossible point",
            ));
        }
        if self.lat == 0.0 && self.lon == 0.0 {
            return Err(AppError::invalid(
                "the geocoder returned a null island point",
            ));
        }
        Ok(self)
    }
}

/// What a geocoding attempt produced.
#[derive(Debug, Clone)]
pub enum Located {
    Found {
        coordinates: Coordinates,
        raw: serde_json::Value,
    },
    /// The provider answered, and had no match. Distinct from an error: there
    /// is nothing to retry.
    NotFound,
}

pub type GeocodeFuture = Pin<Box<dyn Future<Output = Result<Located, AppError>> + Send>>;

/// A geocoding provider.
pub trait Geocoder: Send + Sync {
    fn name(&self) -> &'static str;
    fn locate(&self, address: String) -> GeocodeFuture;
}

/// The US Census Bureau's one-line address geocoder.
///
/// **Not exercised against the live service in this environment.** The request
/// shape and response parsing are written from the documented contract and are
/// covered by unit tests over recorded payloads; the first real run should be a
/// small batch with `--limit` before the worker is left unattended.
pub struct CensusGeocoder {
    client: reqwest::Client,
    endpoint: String,
}

impl CensusGeocoder {
    pub const DEFAULT_ENDPOINT: &'static str =
        "https://geocoding.geo.census.gov/geocoder/locations/onelineaddress";

    pub fn new(endpoint: Option<String>) -> Result<Self, AppError> {
        let client = reqwest::Client::builder()
            // Bounded, so a hung provider cannot pin a worker slot forever.
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(AppError::internal)?;

        Ok(Self {
            client,
            endpoint: endpoint.unwrap_or_else(|| Self::DEFAULT_ENDPOINT.to_owned()),
        })
    }

    /// Pull coordinates out of a Census response.
    pub fn parse(body: &serde_json::Value) -> Result<Located, AppError> {
        let matches = body
            .get("result")
            .and_then(|result| result.get("addressMatches"))
            .and_then(|matches| matches.as_array());

        let Some(matches) = matches else {
            return Err(AppError::invalid(
                "the geocoder response had no result section",
            ));
        };
        let Some(first) = matches.first() else {
            return Ok(Located::NotFound);
        };

        let coordinates = first
            .get("coordinates")
            .ok_or_else(|| AppError::invalid("the geocoder match had no coordinates"))?;

        let lon = coordinates
            .get("x")
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(|| AppError::invalid("the geocoder match had no longitude"))?;
        let lat = coordinates
            .get("y")
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(|| AppError::invalid("the geocoder match had no latitude"))?;

        Ok(Located::Found {
            coordinates: Coordinates { lat, lon }.validate()?,
            raw: first.clone(),
        })
    }
}

impl Geocoder for CensusGeocoder {
    fn name(&self) -> &'static str {
        "us_census"
    }

    fn locate(&self, address: String) -> GeocodeFuture {
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();

        Box::pin(async move {
            let response = client
                .get(&endpoint)
                .query(&[
                    ("address", address.as_str()),
                    ("benchmark", "Public_AR_Current"),
                    ("format", "json"),
                ])
                .send()
                .await
                .map_err(|e| AppError::unavailable(format!("the geocoder is unreachable: {e}")))?;

            if !response.status().is_success() {
                return Err(AppError::unavailable(format!(
                    "the geocoder answered {}",
                    response.status()
                )));
            }

            let body: serde_json::Value = response.json().await.map_err(|e| {
                AppError::unavailable(format!("the geocoder reply was unreadable: {e}"))
            })?;

            Self::parse(&body)
        })
    }
}

/// A geocoder that answers from a fixed table. Used by the worker's tests, and
/// usable in development to avoid calling a public service in a loop.
pub struct StaticGeocoder {
    answers: std::collections::HashMap<String, Coordinates>,
}

impl StaticGeocoder {
    pub fn new(answers: std::collections::HashMap<String, Coordinates>) -> Self {
        Self { answers }
    }
}

impl Geocoder for StaticGeocoder {
    fn name(&self) -> &'static str {
        "static"
    }

    fn locate(&self, address: String) -> GeocodeFuture {
        let found = self.answers.get(&address).copied();
        Box::pin(async move {
            Ok(match found {
                Some(coordinates) => Located::Found {
                    coordinates,
                    raw: serde_json::json!({ "source": "static" }),
                },
                None => Located::NotFound,
            })
        })
    }
}

/// Build the configured provider.
pub fn build(name: &str, endpoint: Option<String>) -> Result<Arc<dyn Geocoder>, AppError> {
    match name {
        "us_census" => Ok(Arc::new(CensusGeocoder::new(endpoint)?)),
        other => Err(AppError::invalid(format!(
            "unknown geocoder \"{other}\"; expected us_census"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_census_match_is_parsed() {
        let body = serde_json::json!({
            "result": {
                "addressMatches": [{
                    "matchedAddress": "1600 PENNSYLVANIA AVE NW, WASHINGTON, DC, 20500",
                    "coordinates": { "x": -77.03654, "y": 38.89768 }
                }]
            }
        });

        match CensusGeocoder::parse(&body).expect("parse") {
            Located::Found { coordinates, .. } => {
                assert!((coordinates.lat - 38.89768).abs() < 1e-6);
                assert!((coordinates.lon + 77.03654).abs() < 1e-6);
            }
            Located::NotFound => panic!("should have matched"),
        }
    }

    #[test]
    fn an_empty_match_list_is_not_found_rather_than_an_error() {
        let body = serde_json::json!({ "result": { "addressMatches": [] } });
        assert!(matches!(
            CensusGeocoder::parse(&body).expect("parse"),
            Located::NotFound
        ));
    }

    #[test]
    fn a_malformed_response_is_an_error_not_a_silent_miss() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({ "result": {} }),
            serde_json::json!({ "result": { "addressMatches": [{}] } }),
        ] {
            assert!(CensusGeocoder::parse(&body).is_err(), "{body}");
        }
    }

    #[test]
    fn impossible_and_null_island_points_are_refused() {
        assert!(Coordinates {
            lat: 91.0,
            lon: 0.0
        }
        .validate()
        .is_err());
        assert!(Coordinates {
            lat: 0.0,
            lon: 181.0
        }
        .validate()
        .is_err());
        assert!(
            Coordinates { lat: 0.0, lon: 0.0 }.validate().is_err(),
            "(0,0) is what a broken geocoder returns instead of an error"
        );
        assert!(Coordinates {
            lat: 34.1,
            lon: -118.2
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn an_unknown_provider_name_is_refused_at_startup() {
        assert!(build("nominatim", None).is_err());
        assert!(build("us_census", None).is_ok());
    }
}
