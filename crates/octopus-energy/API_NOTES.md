# Octopus API notes

Behaviour the client relies on, from the public API documentation and maintained open-source clients (Home Assistant `octopus_energy`, evcc, predbat, octopus-energy-dotnet). The tests use hand-built fixtures in the documented shapes.

## Covered

- **Host**: `https://api.octopus.energy` by default; configurable.
- **REST auth**: HTTP Basic, API key as username, empty password.
- **Rates**: returned newest first (`order_by` is ignored); `page_size` up to 1500; `payment_method` is `DIRECT_DEBIT`, `NON_DIRECT_DEBIT` or null.
- **Consumption**: `order_by=period` gives oldest first; `page_size` up to 25000; `group_by` hour, day, week, month or quarter; intervals carry `Z` or `+01:00` offsets.
- **Gas units**: SMETS1 meters report kWh, SMETS2 meters m³; only GraphQL `consumptionUnits` says which.
- **GraphQL auth**: `obtainKrakenToken(input: {APIKey})` returns a JWT valid for about 60 minutes (`payload.exp`), sent as `Authorization: <token>`.
- **GraphQL errors**: HTTP 200 with `errors[].extensions.errorCode`; `KT-CT-1199` is rate limiting, `KT-CT-1111`/`4177`/`11334` authentication, `KT-CT-1124`/`1112` token problems.
- **Telemetry**: `smartMeterTelemetry(deviceId, start, end, grouping)`; grouping `TEN_SECONDS`, `ONE_MINUTE`, `FIVE_MINUTES`, `HALF_HOURLY` or `HOURLY`; start, end and grouping are given together or not at all; decimals arrive as strings; energy in Wh, power in W.
- **Agile**: discover the current product through `/products/`; next-day prices appear around 16:00 UK time; prices are capped and can be negative.

## Known gaps

- Telemetry is limited to roughly 100–125 requests per hour per account, shared with the Octopus app; the client does not throttle for you.
- A 403 with an HTML body is CDN throttling, which can last a long time; the client reports it as `Error::Throttled` and does not retry.
- Telemetry units and the exact error codes are taken from the documentation and other clients; there is no recorded-response test yet.
