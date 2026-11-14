//! Validate `open-meteo` settings the way `heimr check-config` does, and
//! print what an instance would emit. Makes no request.
//!
//! ```bash
//! cargo run -p heimr-weather --example settings
//! ```

use heimr_core::Adapter;
use heimr_weather::{Config, OpenMeteo};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Generic placeholder coordinates, not a real site.
    let settings = serde_json::json!({
        "latitude": 0.0,
        "longitude": 0.0,
        "forecast_interval": "1h",
        "forecast_days": 3,
    });
    let config: Config = serde_json::from_value(settings)?;
    OpenMeteo::new(&config)?;
    let d = OpenMeteo::descriptor();
    println!("kind {:?}: {}", d.kind, d.summary);
    println!(
        "forecasts every {:#} for {} days; recent hours every {:#}",
        config.forecast_interval, config.forecast_days, config.observed_interval
    );
    for bad in [
        serde_json::json!({ "latitude": 95.0, "longitude": 0.0 }),
        serde_json::json!({ "latitude": 0.0, "longitude": 0.0, "forecast_interval": "1m" }),
    ] {
        let config: Config = serde_json::from_value(bad.clone())?;
        if let Err(e) = OpenMeteo::new(&config) {
            println!("{bad}: {e}");
        }
    }
    Ok(())
}
