# octopus-energy

Rust client for the Octopus Energy (UK) APIs.

- **REST**: products and tariffs, **Agile** half-hourly prices, standing charges, smart-meter **consumption** (electricity and gas), account and meter-point lookups.
- **Kraken GraphQL**: live smart-meter telemetry (needs a compatible consumer access device paired with the meter) and smart-meter discovery.
- **Slot maths**: DST-safe half-hour alignment (`HalfHour`), per-slot price expansion, cheapest slots/windows.

```rust,no_run
use jiff::{Timestamp, ToSpan};
use octopus_energy::{prices, Client, Period, TariffCode};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let client = Client::public()?;
let tariff: TariffCode = "E-1R-AGILE-24-10-01-B".parse()?;
let now = Timestamp::now();
let rates = client.unit_rates(&tariff, Period::new(now, now + 24.hours())).await?;
for p in prices::cheapest(&prices::slot_prices(&rates, now, now + 24.hours())?, 3) {
    println!("{} {:.2}p", p.slot, p.inc_vat);
}
# Ok(()) }
```

Examples: `cargo run --example agile_prices -- <region>` (public data; the argument is the region letter, A–P), `OCTOPUS_API_KEY=… cargo run --example live_power`.

## Behaviour worth knowing
- **Host**: defaults to `https://api.octopus.energy`; the base URL is configurable (`ClientBuilder::base_url`). Pagination links are always followed on the configured host, so credentials never go to a host named in a response.
- **Time**: everything is `jiff::Timestamp` (UTC). Consumption intervals arrive with UK offsets and are normalised. A UK local day has 46/48/50 half-hour slots.
- **Units**: rates in pence (inc./exc. VAT, Agile can be negative); consumption in kWh, **except SMETS2 gas in m³** — check `kraken::Meter::consumption_units`. Telemetry is in Wh and W.
- **Limits**: live telemetry is limited to ~100–125 requests/hour per account (shared with the Octopus app). HTTP 429 and Kraken `KT-CT-1199` map to `Error::RateLimited`; a 403 with an HTML body maps to `Error::Throttled` (CDN/WAF throttling, not bad credentials).
- **Secrets**: the API key is a `secrecy::SecretString` and never appears in `Debug` output.

## Testing
Contract tests run against a mock server with fixtures under `tests/fixtures/` (hand-built from the documented schema; see that README) and `insta` snapshots. Slot and price maths are property-tested.

Not affiliated with or endorsed by Octopus Energy.

## Licence
MIT OR Apache-2.0.
