//! Naira on the wire, kobo in the ledger.
//!
//! Every amount in this system is stored as an integer count of **kobo**, which
//! is the standard way to hold money: a minor-unit integer cannot drift the way
//! a float can, and it is what SafeHaven, Paystack and every other processor
//! keeps internally. But kobo is not how anyone reads a price. A hospital types
//! ₦8,000 an hour, the FRS is written in naira throughout, and a client that has
//! to remember which fields are hundredths will eventually get one wrong — a
//! 100x error in either direction.
//!
//! So the API speaks naira and the ledger keeps kobo, and [`Naira`] is the one
//! place the two meet.
//!
//! The important property: `Naira` stores the **exact kobo integer** and only
//! renders naira at the serialisation boundary. It is not a float with a
//! different name. A percentage fee is routinely fractional — 10% of ₦69,664 is
//! ₦6,966.40, as the FRS's own worked example shows — and that is representable
//! here because the underlying value is still `696640` kobo. Converting the
//! *storage* to integer naira would have truncated those 40 kobo on every
//! payout.
//!
//! JSON has no decimal type, so the wire form is a number with two decimal
//! places. That is exact for any amount this platform will ever hold: an f64
//! carries integers up to 2^53 without loss, so kobo stays exact past ₦90
//! trillion. Arithmetic still happens in kobo, never on the rendered value.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Kobo per naira.
const KOBO: i64 = 100;

/// An amount of money, held as kobo and read and written as naira.
///
/// `#[sqlx(transparent)]` makes this decode straight from the existing
/// `*_kobo BIGINT` columns, so a struct that is both a database row and an API
/// response needs only a field rename and `#[sqlx(rename = "...")]` — the
/// stored unit does not change and no data migration is involved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, sqlx::Type)]
#[sqlx(transparent)]
pub struct Naira(i64);

impl Naira {
    /// Wrap an exact kobo amount. The usual constructor, since everything
    /// internal is already kobo.
    pub const fn from_kobo(kobo: i64) -> Self {
        Self(kobo)
    }

    /// The exact kobo value. Use this for all arithmetic and all comparisons.
    pub const fn kobo(self) -> i64 {
        self.0
    }

    /// Whole naira, truncated. For display and for the few provider APIs that
    /// take naira (SafeHaven's transfer endpoint).
    pub const fn whole_naira(self) -> i64 {
        self.0 / KOBO
    }

    /// Build from a naira figure that may carry kobo as decimals.
    ///
    /// Rounds to the nearest kobo rather than truncating: a client sending
    /// `8000.005` means ₦8,000.01 far more often than it means ₦8,000.00, and
    /// truncation would quietly favour one side of every transaction.
    pub fn from_naira_f64(naira: f64) -> Self {
        Self((naira * KOBO as f64).round() as i64)
    }

    /// The naira value as a float, for rendering only.
    pub fn as_naira_f64(self) -> f64 {
        self.0 as f64 / KOBO as f64
    }
}

impl Naira {
    /// Convenience for the very common `Option<i64>` kobo column.
    pub fn from_kobo_opt(kobo: Option<i64>) -> Option<Self> {
        kobo.map(Self::from_kobo)
    }
}

impl From<i64> for Naira {
    /// Kobo in, since every internal amount is kobo.
    fn from(kobo: i64) -> Self {
        Self::from_kobo(kobo)
    }
}

impl Serialize for Naira {
    /// A JSON number in naira. Rounded to two places so the rendered value
    /// never carries float noise like `6966.400000000001`.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let naira = (self.as_naira_f64() * 100.0).round() / 100.0;
        s.serialize_f64(naira)
    }
}

impl<'de> Deserialize<'de> for Naira {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let naira = f64::deserialize(d)?;
        if !naira.is_finite() {
            return Err(serde::de::Error::custom(
                "amount must be a finite number of naira",
            ));
        }
        Ok(Self::from_naira_f64(naira))
    }
}

// Naira renders as a decimal number in the OpenAPI document, with the unit said
// out loud — the field names alone carried that before and a client could still
// misread them.
impl utoipa::PartialSchema for Naira {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::SchemaType::Type(
                utoipa::openapi::schema::Type::Number,
            ))
            .format(Some(utoipa::openapi::SchemaFormat::KnownFormat(
                utoipa::openapi::KnownFormat::Double,
            )))
            .description(Some(
                "An amount in NAIRA (not kobo), with up to two decimal places.",
            ))
            .into()
    }
}

impl utoipa::ToSchema for Naira {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kobo_is_preserved_exactly() {
        // The whole point: the wire unit changes, the stored value does not.
        for kobo in [0, 1, 99, 100, 800_000, 6_966_400, i64::MAX / 2] {
            assert_eq!(Naira::from_kobo(kobo).kobo(), kobo);
        }
    }

    #[test]
    fn a_fractional_naira_fee_survives_the_round_trip() {
        // 10% of ₦69,664 is ₦6,966.40 — the FRS's own example. Integer naira
        // storage would have lost the 40 kobo.
        let fee = Naira::from_kobo(696_640);
        assert_eq!(fee.as_naira_f64(), 6966.40);
        assert_eq!(fee.kobo(), 696_640);
        assert_eq!(fee.whole_naira(), 6966, "display truncates; storage does not");
    }

    #[test]
    fn naira_in_becomes_kobo() {
        assert_eq!(Naira::from_naira_f64(8000.0).kobo(), 800_000);
        assert_eq!(Naira::from_naira_f64(500.0).kobo(), 50_000);
        assert_eq!(Naira::from_naira_f64(0.0).kobo(), 0);
        // Decimals are kobo.
        assert_eq!(Naira::from_naira_f64(6966.40).kobo(), 696_640);
        assert_eq!(Naira::from_naira_f64(0.01).kobo(), 1);
    }

    #[test]
    fn sub_kobo_input_rounds_to_the_nearest_kobo() {
        // Truncating here would quietly favour one side of every transaction,
        // so this rounds. Note what is *not* asserted: a "round half up"
        // contract. A JSON `1.005` is not a half — the nearest double is
        // 1.00499999999999989, so it legitimately rounds down. Pinning an
        // exact-half case would encode a belief about decimal literals that
        // binary floats do not honour, and the test would be wrong rather than
        // the code. Values either side of a kobo are what matters.
        assert_eq!(Naira::from_naira_f64(1.014).kobo(), 101);
        assert_eq!(Naira::from_naira_f64(1.016).kobo(), 102);
        assert_eq!(Naira::from_naira_f64(1.004).kobo(), 100);
        assert_eq!(Naira::from_naira_f64(-1.016).kobo(), -102);
        // Sub-kobo dust rounds away rather than accumulating.
        assert_eq!(Naira::from_naira_f64(0.001).kobo(), 0);
    }

    #[test]
    fn serialises_as_a_naira_number() {
        assert_eq!(serde_json::to_string(&Naira::from_kobo(800_000)).unwrap(), "8000.0");
        assert_eq!(serde_json::to_string(&Naira::from_kobo(696_640)).unwrap(), "6966.4");
        assert_eq!(serde_json::to_string(&Naira::from_kobo(1)).unwrap(), "0.01");
        assert_eq!(serde_json::to_string(&Naira::from_kobo(0)).unwrap(), "0.0");
    }

    #[test]
    fn deserialises_from_a_naira_number() {
        assert_eq!(
            serde_json::from_str::<Naira>("8000").unwrap().kobo(),
            800_000
        );
        assert_eq!(
            serde_json::from_str::<Naira>("6966.40").unwrap().kobo(),
            696_640
        );
    }

    #[test]
    fn a_json_round_trip_does_not_drift() {
        // Every realistic amount, including ones a float would mangle if the
        // value were stored as naira rather than kobo.
        // 99_999_999_999 kobo is ₦999,999,999.99 — grouped as kobo, since that
        // is the unit the loop variable is in.
        for kobo in [1, 7, 99, 100, 101, 696_640, 3_200_000, 6_966_400, 99_999_999_999] {
            let json = serde_json::to_string(&Naira::from_kobo(kobo)).unwrap();
            let back: Naira = serde_json::from_str(&json).unwrap();
            assert_eq!(back.kobo(), kobo, "{kobo} kobo drifted through {json}");
        }
    }

    #[test]
    fn a_non_finite_amount_is_refused() {
        // serde_json rejects NaN/Infinity in input anyway; this covers the
        // explicit guard so it cannot be removed silently.
        assert!(serde_json::from_str::<Naira>("1e400").is_err());
    }
}
