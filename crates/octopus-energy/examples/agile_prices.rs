//! Today's Agile prices for a region, in UK time, with the cheapest
//! 2-hour window. Public data: no API key needed.
//!
//! ```sh
//! cargo run -p octopus-energy --example agile_prices -- <region>
//! ```
//! The region is the grid supply point letter (A–P). Set `OCTOPUS_BASE_URL`
//! to use a different API host.

use anyhow::Context;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use octopus_energy::prices::{cheapest_window, slot_prices};
use octopus_energy::slot::local_day_bounds;
use octopus_energy::{Client, Period, ProductFilter, Region, TariffCode, tariff};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let region: Region = std::env::args()
        .nth(1)
        .context("usage: agile_prices <region letter A–P>")?
        .parse()?;
    let mut builder = Client::builder();
    if let Ok(url) = std::env::var("OCTOPUS_BASE_URL") {
        builder = builder.base_url(url);
    }
    let client = builder.build()?;

    // Discover the current Agile import product rather than hard-coding it.
    let products = client
        .products(&ProductFilter {
            is_variable: Some(true),
            ..Default::default()
        })
        .await?;
    let agile = products
        .iter()
        .filter(|p| p.is_agile_import() && !p.is_business)
        .max_by_key(|p| p.available_from)
        .context("no Agile import product found")?;
    let tariff = TariffCode::new(
        tariff::Fuel::Electricity,
        tariff::Registers::Single,
        &agile.code,
        region,
    );

    let uk = TimeZone::get("Europe/London")?;
    let today = Timestamp::now().to_zoned(uk.clone()).date();
    let (from, to) = local_day_bounds(today, &uk)?;
    let rates = client.unit_rates(&tariff, Period::new(from, to)).await?;
    let slots = slot_prices(&rates, from, to)?;

    println!(
        "{tariff} — {today} (Europe/London), {} slots published",
        slots.len()
    );
    for s in &slots {
        let local = s.slot.start().to_zoned(uk.clone());
        let bar = "█".repeat((s.inc_vat.max(0.0) / 2.0).round() as usize);
        println!("{}  {:>7.2}p  {bar}", local.strftime("%H:%M"), s.inc_vat);
    }
    if let Some((window, mean)) = cheapest_window(&slots, 4) {
        let start = window[0].slot.start().to_zoned(uk.clone());
        println!(
            "\nCheapest 2 h: from {} at {mean:.2}p/kWh average",
            start.strftime("%H:%M")
        );
    }
    Ok(())
}
