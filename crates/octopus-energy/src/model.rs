//! REST response models.
//!
//! Field names follow the API. Unknown fields are ignored and most optional
//! fields are `Option` / defaulted, because Octopus adds fields over time.
//! Money is in **pence** (floats, as returned); energy in **kWh**.

use std::collections::BTreeMap;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::tariff::{Region, TariffCode};

/// The pagination envelope used by every list endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct Page<T> {
    /// Total number of results across pages.
    pub count: Option<u64>,
    /// Absolute URL of the next page, if any. It may name a different host
    /// from the one configured; the client fetches it from the configured
    /// host.
    pub next: Option<String>,
    /// Results on this page.
    pub results: Vec<T>,
}

/// Import or export product.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Direction {
    /// Import (consumption).
    Import,
    /// Export (generation).
    Export,
}

/// Entry in `GET /products/`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Product {
    /// Product code, e.g. `AGILE-24-10-01`.
    pub code: String,
    /// Import or export.
    pub direction: Option<Direction>,
    /// Full name.
    pub full_name: String,
    /// Display name.
    pub display_name: String,
    /// Description.
    #[serde(default)]
    pub description: String,
    /// Variable-rate product.
    #[serde(default)]
    pub is_variable: bool,
    /// Green product.
    #[serde(default)]
    pub is_green: bool,
    /// Tracker product.
    #[serde(default)]
    pub is_tracker: bool,
    /// Prepayment product.
    #[serde(default)]
    pub is_prepay: bool,
    /// Business product.
    #[serde(default)]
    pub is_business: bool,
    /// Restricted (not generally available).
    #[serde(default)]
    pub is_restricted: bool,
    /// Contract term in months, if fixed.
    pub term: Option<u32>,
    /// Available from.
    pub available_from: Option<Timestamp>,
    /// Available until (`None` = still available).
    pub available_to: Option<Timestamp>,
    /// Brand, e.g. `OCTOPUS_ENERGY`.
    pub brand: Option<String>,
}

impl Product {
    /// Whether this is an Agile import product.
    pub fn is_agile_import(&self) -> bool {
        self.code.starts_with("AGILE-") && self.direction != Some(Direction::Export)
    }
}

/// `GET /products/{code}/`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProductDetail {
    /// Summary fields.
    #[serde(flatten)]
    pub product: Product,
    /// The instant the tariff maps describe.
    pub tariffs_active_at: Option<Timestamp>,
    /// Single-register electricity tariffs by region and payment method.
    #[serde(default)]
    pub single_register_electricity_tariffs: RegionalTariffs,
    /// Dual-register (day/night) electricity tariffs.
    #[serde(default)]
    pub dual_register_electricity_tariffs: RegionalTariffs,
    /// Gas tariffs.
    #[serde(default)]
    pub single_register_gas_tariffs: RegionalTariffs,
}

/// Region (`_A`..`_P`) → payment method (`direct_debit_monthly`, `varying`…)
/// → tariff summary.
pub type RegionalTariffs = BTreeMap<String, BTreeMap<String, TariffSummary>>;

impl ProductDetail {
    /// The direct-debit (or, failing that, any) single-register electricity
    /// tariff for `region`.
    pub fn electricity_tariff(&self, region: Region) -> Option<&TariffSummary> {
        pick(&self.single_register_electricity_tariffs, region)
    }

    /// The gas tariff for `region`.
    pub fn gas_tariff(&self, region: Region) -> Option<&TariffSummary> {
        pick(&self.single_register_gas_tariffs, region)
    }
}

fn pick(map: &RegionalTariffs, region: Region) -> Option<&TariffSummary> {
    let methods = map.get(&region.gsp_group_id())?;
    methods
        .get("direct_debit_monthly")
        .or_else(|| methods.values().next())
}

/// Tariff headline figures inside a product (inc./exc. VAT, pence).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TariffSummary {
    /// Full tariff code.
    pub code: TariffCode,
    /// Standing charge, p/day exc. VAT.
    pub standing_charge_exc_vat: Option<f64>,
    /// Standing charge, p/day inc. VAT.
    pub standing_charge_inc_vat: Option<f64>,
    /// Unit rate, p/kWh exc. VAT (single register).
    pub standard_unit_rate_exc_vat: Option<f64>,
    /// Unit rate, p/kWh inc. VAT (single register).
    pub standard_unit_rate_inc_vat: Option<f64>,
    /// Day rate exc. VAT (dual register).
    pub day_unit_rate_exc_vat: Option<f64>,
    /// Day rate inc. VAT (dual register).
    pub day_unit_rate_inc_vat: Option<f64>,
    /// Night rate exc. VAT (dual register).
    pub night_unit_rate_exc_vat: Option<f64>,
    /// Night rate inc. VAT (dual register).
    pub night_unit_rate_inc_vat: Option<f64>,
}

/// Payment method attached to a rate row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PaymentMethod {
    /// Direct debit.
    DirectDebit,
    /// Anything else (cash/cheque/prepay).
    NonDirectDebit,
}

/// A unit rate (p/kWh) or standing charge (p/day) valid over an interval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rate {
    /// Value exc. VAT.
    pub value_exc_vat: f64,
    /// Value inc. VAT. Agile unit rates may be negative.
    pub value_inc_vat: f64,
    /// Start (inclusive).
    pub valid_from: Timestamp,
    /// End (exclusive); `None` = open-ended.
    pub valid_to: Option<Timestamp>,
    /// Payment method this row applies to, if the tariff distinguishes.
    pub payment_method: Option<PaymentMethod>,
}

/// One consumption interval from a smart meter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Consumption {
    /// Energy in the interval. kWh for electricity and SMETS1 gas; **m³ for
    /// SMETS2 gas** (the REST API does not say which — see
    /// [`crate::kraken::Meter::consumption_units`]).
    pub consumption: f64,
    /// Interval start (the API sends a local UK offset; normalised to UTC).
    pub interval_start: Timestamp,
    /// Interval end.
    pub interval_end: Timestamp,
}

/// `GET /accounts/{number}/`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    /// Account number, `A-XXXXXXXX`.
    pub number: String,
    /// Properties (supply addresses) on the account.
    pub properties: Vec<Property>,
}

/// A supply address. Address fields are deliberately not modelled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Property {
    /// Property id.
    pub id: u64,
    /// Move-in date.
    pub moved_in_at: Option<Timestamp>,
    /// Move-out date.
    pub moved_out_at: Option<Timestamp>,
    /// Electricity meter points.
    #[serde(default)]
    pub electricity_meter_points: Vec<ElectricityMeterPoint>,
    /// Gas meter points.
    #[serde(default)]
    pub gas_meter_points: Vec<GasMeterPoint>,
}

/// An electricity supply (MPAN).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElectricityMeterPoint {
    /// MPAN (13 digits).
    pub mpan: String,
    /// Profile class.
    pub profile_class: Option<u8>,
    /// Estimated annual consumption, kWh.
    pub consumption_standard: Option<f64>,
    /// Whether this is an export MPAN.
    #[serde(default)]
    pub is_export: bool,
    /// Meters (may include old ones).
    #[serde(default)]
    pub meters: Vec<Meter>,
    /// Tariff agreements, including history.
    #[serde(default)]
    pub agreements: Vec<Agreement>,
}

/// A gas supply (MPRN).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GasMeterPoint {
    /// MPRN.
    pub mprn: String,
    /// Estimated annual consumption, kWh.
    pub consumption_standard: Option<f64>,
    /// Meters.
    #[serde(default)]
    pub meters: Vec<Meter>,
    /// Tariff agreements, including history.
    #[serde(default)]
    pub agreements: Vec<Agreement>,
}

/// A physical meter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meter {
    /// Serial number.
    pub serial_number: String,
    /// Registers (electricity only).
    #[serde(default)]
    pub registers: Vec<Register>,
}

/// A meter register.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Register {
    /// Register id, e.g. `01`.
    pub identifier: String,
    /// Rate name, e.g. `STANDARD`.
    pub rate: Option<String>,
    /// Whether this register is used for settlement.
    #[serde(default)]
    pub is_settlement_register: bool,
}

/// A tariff agreement on a meter point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agreement {
    /// Tariff code.
    pub tariff_code: TariffCode,
    /// Start.
    pub valid_from: Timestamp,
    /// End (`None` = open-ended).
    pub valid_to: Option<Timestamp>,
}

impl Agreement {
    /// Whether the agreement covers `t`.
    pub fn is_active_at(&self, t: Timestamp) -> bool {
        self.valid_from <= t && self.valid_to.is_none_or(|end| t < end)
    }
}

/// Pick the agreement active at `t`.
pub fn active_agreement(agreements: &[Agreement], t: Timestamp) -> Option<&Agreement> {
    agreements.iter().find(|a| a.is_active_at(t))
}

/// `GET /electricity-meter-points/{mpan}/`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeterPointInfo {
    /// GSP group, e.g. `_A`.
    pub gsp: Region,
    /// MPAN.
    pub mpan: String,
    /// Profile class.
    pub profile_class: Option<u8>,
}

/// Entry in `GET /industry/grid-supply-points/`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridSupplyPoint {
    /// GSP group, e.g. `_A`.
    pub group_id: Region,
}
