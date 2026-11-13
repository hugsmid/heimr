//! Tariff codes and regions.
//!
//! A tariff code looks like `E-1R-AGILE-24-10-01-A`:
//! fuel (`E`/`G`) – registers (`1R` single, `2R` day/night) – product code –
//! region letter (the grid supply point group, `A`..`P` without `I`/`O`).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Fuel of a tariff or meter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fuel {
    /// Electricity.
    Electricity,
    /// Gas.
    Gas,
}

/// Register configuration of an electricity tariff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Registers {
    /// One rate register (`1R`): standard or half-hourly rates.
    Single,
    /// Two registers (`2R`): day/night (Economy 7).
    Dual,
}

/// A grid supply point group (DNO region), `A`..`P` excluding `I` and `O`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Region(char);

impl Region {
    /// All fourteen regions.
    pub const ALL: [char; 14] = [
        'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'J', 'K', 'L', 'M', 'N', 'P',
    ];

    /// Parse from a letter (`'A'`) or a GSP group id (`"_A"`).
    pub fn new(c: char) -> Result<Self, TariffCodeError> {
        let c = c.to_ascii_uppercase();
        if Self::ALL.contains(&c) {
            Ok(Self(c))
        } else {
            Err(TariffCodeError::Region(c.to_string()))
        }
    }

    /// The region letter.
    pub fn letter(self) -> char {
        self.0
    }

    /// The key used in product tariff maps and GSP lookups, e.g. `_A`.
    pub fn gsp_group_id(self) -> String {
        format!("_{}", self.0)
    }
}

impl FromStr for Region {
    type Err = TariffCodeError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.strip_prefix('_').unwrap_or(s);
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Self::new(c),
            _ => Err(TariffCodeError::Region(s.to_owned())),
        }
    }
}

impl TryFrom<String> for Region {
    type Error = TariffCodeError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Region> for String {
    fn from(r: Region) -> String {
        r.gsp_group_id()
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A parsed tariff code.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TariffCode {
    fuel: Fuel,
    registers: Registers,
    product: String,
    region: Region,
}

/// Error parsing a tariff code or region.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TariffCodeError {
    /// The overall shape is wrong.
    #[error("malformed tariff code {0:?}")]
    Malformed(String),
    /// Unknown region letter.
    #[error("unknown region {0:?}")]
    Region(String),
}

impl TariffCode {
    /// Build a tariff code for `product` in `region`.
    pub fn new(
        fuel: Fuel,
        registers: Registers,
        product: impl Into<String>,
        region: Region,
    ) -> Self {
        Self {
            fuel,
            registers,
            product: product.into(),
            region,
        }
    }

    /// Fuel.
    pub fn fuel(&self) -> Fuel {
        self.fuel
    }

    /// Registers.
    pub fn registers(&self) -> Registers {
        self.registers
    }

    /// Product code, e.g. `AGILE-24-10-01`.
    pub fn product(&self) -> &str {
        &self.product
    }

    /// Region.
    pub fn region(&self) -> Region {
        self.region
    }

    /// Whether this is an Agile (half-hourly, wholesale-tracking) tariff.
    pub fn is_agile(&self) -> bool {
        self.product.starts_with("AGILE-")
    }
}

impl FromStr for TariffCode {
    type Err = TariffCodeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || TariffCodeError::Malformed(s.to_owned());
        let (fuel, rest) = match s.split_once('-') {
            Some(("E", r)) => (Fuel::Electricity, r),
            Some(("G", r)) => (Fuel::Gas, r),
            _ => return Err(bad()),
        };
        let (registers, rest) = match rest.split_once('-') {
            Some(("1R", r)) => (Registers::Single, r),
            Some(("2R", r)) => (Registers::Dual, r),
            _ => return Err(bad()),
        };
        let (product, region) = rest.rsplit_once('-').ok_or_else(bad)?;
        if product.is_empty() {
            return Err(bad());
        }
        Ok(Self {
            fuel,
            registers,
            product: product.to_owned(),
            region: region.parse()?,
        })
    }
}

impl TryFrom<String> for TariffCode {
    type Error = TariffCodeError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<TariffCode> for String {
    fn from(t: TariffCode) -> String {
        t.to_string()
    }
}

impl fmt::Display for TariffCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fuel = match self.fuel {
            Fuel::Electricity => 'E',
            Fuel::Gas => 'G',
        };
        let reg = match self.registers {
            Registers::Single => "1R",
            Registers::Dual => "2R",
        };
        write!(f, "{fuel}-{reg}-{}-{}", self.product, self.region)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parses_known_codes() {
        let t: TariffCode = "E-1R-AGILE-24-10-01-A".parse().unwrap();
        assert_eq!(t.fuel(), Fuel::Electricity);
        assert_eq!(t.registers(), Registers::Single);
        assert_eq!(t.product(), "AGILE-24-10-01");
        assert_eq!(t.region().letter(), 'A');
        assert!(t.is_agile());

        let g: TariffCode = "G-1R-SILVER-25-09-02-M".parse().unwrap();
        assert_eq!((g.fuel(), g.product()), (Fuel::Gas, "SILVER-25-09-02"));
        assert!(!g.is_agile());

        let e7: TariffCode = "E-2R-VAR-22-11-01-A".parse().unwrap();
        assert_eq!(e7.registers(), Registers::Dual);
    }

    #[test]
    fn rejects_bad_codes() {
        for bad in [
            "",
            "E-1R-A",
            "X-1R-AGILE-24-10-01-A",
            "E-3R-AGILE-A",
            "E-1R-AGILE-24-10-01-I",
            "E-1R-AGILE-24-10-01-AA",
        ] {
            assert!(bad.parse::<TariffCode>().is_err(), "{bad}");
        }
    }

    #[test]
    fn region_accepts_gsp_ids() {
        assert_eq!("_A".parse::<Region>().unwrap().letter(), 'A');
        assert_eq!("a".parse::<Region>().unwrap().gsp_group_id(), "_A");
        assert!("_O".parse::<Region>().is_err());
    }

    proptest! {
        #[test]
        fn display_round_trips(
            gas in any::<bool>(),
            dual in any::<bool>(),
            product in "[A-Z]{2,8}(-[0-9]{2}){0,3}",
            r in 0usize..14,
        ) {
            let code = TariffCode::new(
                if gas { Fuel::Gas } else { Fuel::Electricity },
                if dual { Registers::Dual } else { Registers::Single },
                product,
                Region::new(Region::ALL[r]).unwrap(),
            );
            prop_assert_eq!(code.to_string().parse::<TariffCode>().unwrap(), code);
        }
    }
}
