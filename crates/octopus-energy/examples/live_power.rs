//! Live electricity demand from smart-meter telemetry (needs a consumer
//! access device paired with the meter).
//!
//! ```sh
//! OCTOPUS_API_KEY=sk_live_… cargo run -p octopus-energy --example live_power
//! ```
//! Optional: `OCTOPUS_ACCOUNT=A-…` (otherwise the first account on the key).
//! Polls once a minute, five times — the telemetry API allows only ~100
//! requests per hour, shared with the Octopus app.

use std::time::Duration;

use anyhow::Context;
use octopus_energy::KrakenClient;
use secrecy::SecretString;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let key = std::env::var("OCTOPUS_API_KEY").context("set OCTOPUS_API_KEY")?;
    let kraken = KrakenClient::new(SecretString::from(key))?;

    let account = match std::env::var("OCTOPUS_ACCOUNT") {
        Ok(a) => a,
        Err(_) => kraken
            .account_numbers()
            .await?
            .into_iter()
            .next()
            .context("no accounts visible to this API key")?,
    };
    let meters = kraken.meters(&account).await?;
    let device = meters
        .electricity_device_id()
        .context("no smart electricity meter with a device id (is a telemetry device paired?)")?
        .to_owned();

    for i in 0..5 {
        if i > 0 {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        match kraken.latest_telemetry(&device).await? {
            Some(t) => println!(
                "{}  demand {:>6} W  import {:>10} kWh",
                t.read_at,
                t.demand_w.map_or("-".into(), |w| format!("{w:.0}")),
                t.import_kwh().map_or("-".into(), |k| format!("{k:.3}")),
            ),
            None => println!("no telemetry yet"),
        }
    }
    Ok(())
}
