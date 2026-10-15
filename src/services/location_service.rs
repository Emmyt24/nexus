use std::sync::Arc;
use uuid::Uuid;

use crate::models::admin_registration::{Address, Coordinates, NewLocation};
use crate::models::location::HospitalLocation;
use crate::repositories::location::{LocationError, LocationRepository};
use crate::services::geocoding::{GeocodingClient, GeocodingError};

#[derive(Debug, thiserror::Error)]
pub enum LocationServiceError {
    #[error("Geocoding failed: {0}")]
    GeocodingFailed(#[from] GeocodingError),

    #[error("Location storage failed: {0}")]
    StorageFailed(#[from] LocationError),

    #[error("Invalid service radius: {0}")]
    InvalidServiceRadius(String),
}

/// Service for geocoding addresses and storing location data (AC-02)
pub struct LocationService {
    geocoding_client: Arc<GeocodingClient>,
    location_repo: Arc<LocationRepository>,
}

impl LocationService {
    pub fn new(
        geocoding_client: Arc<GeocodingClient>,
        location_repo: Arc<LocationRepository>,
    ) -> Self {
        Self {
            geocoding_client,
            location_repo,
        }
    }

    /// Geocode address and store location with 5km service radius
    pub async fn geocode_and_store(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        hospital_id: Uuid,
        address: Address,
    ) -> Result<HospitalLocation, LocationServiceError> {
        // Step 1: exact coordinates from a map/GPS picker take precedence;
        // otherwise geocode the address.
        let coordinates = match (address.latitude, address.longitude) {
            (Some(latitude), Some(longitude)) => Coordinates {
                latitude,
                longitude,
            },
            _ => match self.geocoding_client.geocode_address(&address).await {
            Ok(coords) => coords,
            Err(e) => {
                // Log the error
                tracing::warn!(
                    "Geocoding failed for address: {} - {}. Using default coordinates for development.",
                    format!("{}, {}, {}", address.line1, address.city, address.country),
                    e
                );

                // Use default coordinates (Lagos, Nigeria) for development
                Coordinates {
                    latitude: 6.5244,
                    longitude: 3.3792,
                }
            }
            },
        };

        // Step 2: Validate coordinates (already done in geocoding_client, but double-check)
        Self::validate_coordinates(&coordinates)?;

        // Step 3: Calculate 5km service radius (as per AC-02)
        let service_radius_km = 5.0;

        // Step 4: Create location record
        let new_location = NewLocation {
            hospital_id,
            address_line1: address.line1,
            address_line2: address.line2,
            city: address.city,
            state: address.state,
            postal_code: address.postal_code,
            country: address.country,
            latitude: coordinates.latitude,
            longitude: coordinates.longitude,
            service_radius_km,
        };

        // Step 5: Store in database
        let location = self.location_repo.create(tx, new_location).await?;

        Ok(location)
    }

    /// Calculate service area based on coordinates and radius
    /// Pure: takes no `&self`, so it needs no pool, no geocoding client and no
    /// runtime to test.
    pub fn calculate_service_radius(
        coordinates: Coordinates,
        radius_km: f64,
    ) -> Result<ServiceArea, LocationServiceError> {
        if radius_km <= 0.0 || radius_km > 100.0 {
            return Err(LocationServiceError::InvalidServiceRadius(format!(
                "Radius must be between 0 and 100km, got {}",
                radius_km
            )));
        }

        Ok(ServiceArea {
            center: coordinates,
            radius_km,
        })
    }

    /// Validate coordinates are within valid ranges
    /// Pure, as above.
    fn validate_coordinates(coords: &Coordinates) -> Result<(), LocationServiceError> {
        if coords.latitude < -90.0 || coords.latitude > 90.0 {
            return Err(LocationServiceError::GeocodingFailed(
                GeocodingError::InvalidCoordinates(format!(
                    "Latitude {} out of range [-90, 90]",
                    coords.latitude
                )),
            ));
        }

        if coords.longitude < -180.0 || coords.longitude > 180.0 {
            return Err(LocationServiceError::GeocodingFailed(
                GeocodingError::InvalidCoordinates(format!(
                    "Longitude {} out of range [-180, 180]",
                    coords.longitude
                )),
            ));
        }

        Ok(())
    }
}

/// Service area defined by center coordinates and radius
#[derive(Debug, Clone)]
pub struct ServiceArea {
    pub center: Coordinates,
    pub radius_km: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    // These two tests used to build a whole `LocationService` — which meant
    // `PgPool::connect("postgresql://localhost/test").unwrap()` — in order to
    // exercise two functions that never touch the database. That made them fail
    // (or hang, depending on what is listening on 5432) on any machine without
    // that exact database, which is every machine, so `cargo test --lib` could
    // not come back green. The functions are pure; the pool was never the
    // subject. Now they call the functions directly.

    #[test]
    fn coordinates_outside_the_valid_ranges_are_rejected() {
        let valid = Coordinates {
            latitude: 6.5244,
            longitude: 3.3792,
        };
        assert!(LocationService::validate_coordinates(&valid).is_ok());

        // Latitude is bounded at ±90, longitude at ±180. Swapping the two is
        // the classic coordinate bug, and a latitude of 181 is how it surfaces.
        for bad in [
            Coordinates { latitude: 91.0, longitude: 3.3792 },
            Coordinates { latitude: -91.0, longitude: 3.3792 },
            Coordinates { latitude: 6.5244, longitude: 181.0 },
            Coordinates { latitude: 6.5244, longitude: -181.0 },
        ] {
            assert!(
                LocationService::validate_coordinates(&bad).is_err(),
                "{bad:?} is out of range and must be rejected"
            );
        }

        // The bounds are inclusive, so the poles and the antimeridian are valid.
        for edge in [
            Coordinates { latitude: 90.0, longitude: 180.0 },
            Coordinates { latitude: -90.0, longitude: -180.0 },
        ] {
            assert!(
                LocationService::validate_coordinates(&edge).is_ok(),
                "{edge:?} is on the boundary and must be accepted"
            );
        }
    }

    proptest::proptest! {
        /// The honest form of a property test that used to live in
        /// `repositories::location` as `if P { prop_assert!(P) }` — it restated
        /// its own precondition, so it could not fail, and it named itself
        /// after coordinate validation while never calling any. This sweeps
        /// past both bounds and holds the real function to the exact range.
        #[test]
        fn validation_accepts_exactly_the_valid_range(
            latitude in -200.0f64..200.0,
            longitude in -200.0f64..200.0,
        ) {
            let in_range = (-90.0..=90.0).contains(&latitude)
                && (-180.0..=180.0).contains(&longitude);
            let coords = Coordinates { latitude, longitude };

            proptest::prop_assert_eq!(
                LocationService::validate_coordinates(&coords).is_ok(),
                in_range,
                "({}, {}) should be {}",
                latitude,
                longitude,
                if in_range { "accepted" } else { "rejected" }
            );
        }
    }

    #[test]
    fn a_service_radius_must_be_within_one_hundred_km() {
        let coords = Coordinates {
            latitude: 6.5244,
            longitude: 3.3792,
        };

        let area = LocationService::calculate_service_radius(coords.clone(), 5.0)
            .expect("5km is a valid radius");
        assert_eq!(area.radius_km, 5.0);
        assert_eq!(area.center.latitude, coords.latitude);
        assert_eq!(area.center.longitude, coords.longitude);

        // Zero is rejected rather than treated as "no limit", and 100 is the
        // inclusive upper bound.
        assert!(LocationService::calculate_service_radius(coords.clone(), 0.0).is_err());
        assert!(LocationService::calculate_service_radius(coords.clone(), -1.0).is_err());
        assert!(LocationService::calculate_service_radius(coords.clone(), 100.0).is_ok());
        assert!(LocationService::calculate_service_radius(coords, 101.0).is_err());
    }
}
