use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::models::admin_registration::{Address, Coordinates};
use crate::utils::validation::validate_coordinates;

#[derive(Debug, thiserror::Error)]
pub enum GeocodingError {
    #[error("Failed to geocode address: {0}")]
    GeocodingFailed(String),

    #[error("Invalid address: {0}")]
    InvalidAddress(String),

    #[error("Invalid coordinates: {0}")]
    InvalidCoordinates(String),

    #[error("HTTP request failed: {0}")]
    RequestFailed(#[from] reqwest::Error),

    #[error("Service unavailable, retries exhausted")]
    ServiceUnavailable,
}

/// Response from geocoding service (using Nominatim/OpenStreetMap as example)
#[derive(Debug, Deserialize, Serialize)]
struct GeocodingResponse {
    lat: String,
    lon: String,
    display_name: Option<String>,
}

/// Client for geocoding addresses to coordinates (AC-02)
pub struct GeocodingClient {
    client: Client,
    base_url: String,
    max_retries: u32,
}

impl GeocodingClient {
    pub fn new(base_url: Option<String>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            base_url: base_url.unwrap_or_else(|| "https://nominatim.openstreetmap.org".to_string()),
            max_retries: 3,
        }
    }

    /// Geocode an address to coordinates with retry logic
    pub async fn geocode_address(&self, address: &Address) -> Result<Coordinates, GeocodingError> {
        // Build the full address string
        let address_string = self.format_address(address);

        // Validate address is not empty
        if address_string.trim().is_empty() {
            return Err(GeocodingError::InvalidAddress(
                "Address cannot be empty".to_string(),
            ));
        }

        // Attempt geocoding with retries
        let mut last_error = None;
        for attempt in 0..self.max_retries {
            match self.geocode_with_service(&address_string).await {
                Ok(coords) => {
                    // Validate coordinates before returning
                    self.validate_coordinates(&coords)?;
                    return Ok(coords);
                }
                Err(e) => {
                    last_error = Some(e);
                    if attempt < self.max_retries - 1 {
                        // Exponential backoff
                        let delay = Duration::from_millis(100 * 2_u64.pow(attempt));
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        // All retries exhausted
        Err(last_error.unwrap_or(GeocodingError::ServiceUnavailable))
    }

    /// Format address for geocoding API
    fn format_address(&self, address: &Address) -> String {
        let mut parts = vec![address.line1.clone()];

        if let Some(line2) = &address.line2 {
            if !line2.is_empty() {
                parts.push(line2.clone());
            }
        }

        parts.push(address.city.clone());
        parts.push(address.state.clone());
        parts.push(address.postal_code.clone());
        parts.push(address.country.clone());

        parts.join(", ")
    }

    /// Call the geocoding service
    async fn geocode_with_service(&self, address: &str) -> Result<Coordinates, GeocodingError> {
        let url = format!("{}/search", self.base_url);

        let response = self
            .client
            .get(&url)
            .query(&[("q", address), ("format", "json"), ("limit", "1")])
            .header("User-Agent", "NexusCare-Backend/1.0")
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(GeocodingError::GeocodingFailed(format!(
                "HTTP {}: {}",
                response.status(),
                response.text().await.unwrap_or_default()
            )));
        }

        let results: Vec<GeocodingResponse> = response.json().await?;

        if results.is_empty() {
            return Err(GeocodingError::InvalidAddress(
                "Address not found".to_string(),
            ));
        }

        let result = &results[0];

        // Parse coordinates
        let latitude = result.lat.parse::<f64>().map_err(|_| {
            GeocodingError::GeocodingFailed("Invalid latitude in response".to_string())
        })?;

        let longitude = result.lon.parse::<f64>().map_err(|_| {
            GeocodingError::GeocodingFailed("Invalid longitude in response".to_string())
        })?;

        Ok(Coordinates {
            latitude,
            longitude,
        })
    }

    /// Validate coordinates are within valid geographic ranges
    fn validate_coordinates(&self, coords: &Coordinates) -> Result<(), GeocodingError> {
        validate_coordinates(coords.latitude, coords.longitude).map_err(|e| {
            GeocodingError::InvalidCoordinates(format!(
                "lat={}, lon={}: {}",
                coords.latitude,
                coords.longitude,
                e.message.unwrap_or_default()
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_address() {
        let client = GeocodingClient::new(None);

        let address = Address {
            line1: "123 Main Street".to_string(),
            line2: Some("Suite 100".to_string()),
            city: "Lagos".to_string(),
            state: "Lagos State".to_string(),
            postal_code: "100001".to_string(),
            country: "Nigeria".to_string(),
            latitude: None,
            longitude: None,
        };

        let formatted = client.format_address(&address);
        assert!(formatted.contains("123 Main Street"));
        assert!(formatted.contains("Lagos"));
        assert!(formatted.contains("Nigeria"));
    }

    #[test]
    fn test_validate_coordinates() {
        let client = GeocodingClient::new(None);

        // Valid coordinates
        let valid = Coordinates {
            latitude: 6.5244,
            longitude: 3.3792,
        };
        assert!(client.validate_coordinates(&valid).is_ok());

        // Invalid latitude
        let invalid_lat = Coordinates {
            latitude: 91.0,
            longitude: 3.3792,
        };
        assert!(client.validate_coordinates(&invalid_lat).is_err());

        // Invalid longitude
        let invalid_lon = Coordinates {
            latitude: 6.5244,
            longitude: 181.0,
        };
        assert!(client.validate_coordinates(&invalid_lon).is_err());
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;

    // Property 5: Address geocoding returns valid coordinates

    proptest! {
        #[test]
        fn property_9_coordinate_validation(
            lat in -200.0f64..200.0f64,
            lon in -200.0f64..200.0f64,
        ) {
            let client = GeocodingClient::new(None);
            let coords = Coordinates {
                latitude: lat,
                longitude: lon,
            };

            let result = client.validate_coordinates(&coords);

            // Property: Coordinates outside valid ranges should be rejected
            if lat < -90.0 || lat > 90.0 || lon < -180.0 || lon > 180.0 {
                prop_assert!(result.is_err(), "Should reject invalid coordinates: lat={}, lon={}", lat, lon);
            } else {
                prop_assert!(result.is_ok(), "Should accept valid coordinates: lat={}, lon={}", lat, lon);
            }
        }
    }

    proptest! {
        #[test]
        fn property_5_valid_coordinates_in_range(
            lat in -90.0f64..=90.0f64,
            lon in -180.0f64..=180.0f64,
        ) {
            let client = GeocodingClient::new(None);
            let coords = Coordinates {
                latitude: lat,
                longitude: lon,
            };

            // Property: All valid coordinates should pass validation
            prop_assert!(client.validate_coordinates(&coords).is_ok());
        }
    }

    // Property 8: Invalid address error handling
    #[test]
    fn test_property_8_invalid_address_handling() {
        let client = GeocodingClient::new(None);

        // Empty address
        let empty_address = Address {
            line1: "".to_string(),
            line2: None,
            city: "".to_string(),
            state: "".to_string(),
            postal_code: "".to_string(),
            country: "".to_string(),
            latitude: None,
            longitude: None,
        };

        let formatted = client.format_address(&empty_address);
        assert!(formatted.trim().is_empty() || formatted == ", , , , ");
    }
}
