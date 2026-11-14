//! Print the next 24 hours of forecast for a latitude/longitude.
//!
//! ```sh
//! cargo run -p weather --example forecast -- 64.15 -21.94   # Reykjavík, as an example
//! ```
//! Times are UTC. Set `OPEN_METEO_FORECAST_URL` to use another host.
//! Weather data by Open-Meteo.com (CC BY 4.0).

use anyhow::Context;
use jiff::{SignedDuration, Timestamp};
use weather::{Client, ForecastOptions, IssuedAt, Location};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: forecast <latitude> <longitude>";
    let lat: f64 = args.next().context(usage)?.parse().context(usage)?;
    let lon: f64 = args.next().context(usage)?.parse().context(usage)?;
    let location = Location::new(lat, lon)?;

    let mut builder = Client::builder();
    if let Ok(url) = std::env::var("OPEN_METEO_FORECAST_URL") {
        builder = builder.forecast_base_url(url);
    }
    let client = builder.build()?;

    let now = Timestamp::now();
    let points = client
        .forecast(location, &ForecastOptions::days(2), IssuedAt::At(now))
        .await?;
    // Rows start at 00:00 UTC today; keep the hour in progress and the next 23.
    let from = now - SignedDuration::from_hours(1);
    let fmt = |v: Option<f64>, prec: usize| v.map_or("—".to_owned(), |x| format!("{x:.prec$}"));

    println!("Forecast for {lat}, {lon} (issued {now:.0}, UTC)");
    println!(" time   temp°C  RH%  wind m/s  dir°  rain mm  cloud%  sun W/m²  hPa");
    for p in points.iter().filter(|p| p.valid_at > from).take(24) {
        println!(
            "{}  {:>6} {:>4} {:>9} {:>5} {:>8} {:>7} {:>9} {:>6}",
            p.valid_at.strftime("%H:%M"),
            fmt(p.temperature_c, 1),
            fmt(p.humidity_pct, 0),
            fmt(p.wind_speed_ms, 1),
            fmt(p.wind_dir_deg, 0),
            fmt(p.precip_mm, 1),
            fmt(p.cloud_cover_pct, 0),
            fmt(p.shortwave_wm2, 0),
            fmt(p.pressure_hpa, 0),
        );
    }
    println!("Weather data by Open-Meteo.com (CC BY 4.0)");
    Ok(())
}
