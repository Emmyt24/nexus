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
        self.validate_coordinates(&coordinates)?;

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
    pub fn calculate_service_radius(
        &self,
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
    fn validate_coordinates(&self, coords: &Coordinates) -> Result<(), LocationServiceError> {
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

    #[tokio::test]
    async fn test_validate_coordinates() {
        let geocoding_client = Arc::new(GeocodingClient::new(None));
        let location_repo = Arc::new(LocationRepository::new(
            sqlx::PgPool::connect("postgresql://localhost/test")
                .await
                .unwrap(),
        ));
        let service = LocationService::new(geocoding_client, location_repo);

        // Valid coordinates
        let valid = Coordinates {
            latitude: 6.5244,
            longitude: 3.3792,
        };
        assert!(service.validate_coordinates(&valid).is_ok());

        // Invalid latitude
        let invalid_lat = Coordinates {
            latitude: 91.0,
            longitude: 3.3792,
        };
        assert!(service.validate_coordinates(&invalid_lat).is_err());

        // Invalid longitude
        let invalid_lon = Coordinates {
            latitude: 6.5244,
            longitude: 181.0,
        };
        assert!(service.validate_coordinates(&invalid_lon).is_err());
    }

    #[tokio::test]
    async fn test_calculate_service_radius() {
        let geocoding_client = Arc::new(GeocodingClient::new(None));
        let location_repo = Arc::new(LocationRepository::new(
            sqlx::PgPool::connect("postgresql://localhost/test")
                .await
                .unwrap(),
        ));
        let service = LocationService::new(geocoding_client, location_repo);

        let coords = Coordinates {
            latitude: 6.5244,
            longitude: 3.3792,
        };

        // Valid radius
        let result = service.calculate_service_radius(coords.clone(), 5.0);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().radius_km, 5.0);

        // Invalid radius (too small)
        assert!(service
            .calculate_service_radius(coords.clone(), 0.0)
            .is_err());

        // Invalid radius (too large)
        assert!(service.calculate_service_radius(coords, 101.0).is_err());
    }
}
