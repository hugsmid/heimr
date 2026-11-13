# Fixtures (hand-built)

**These are not recordings.** They were built by hand from:
- the public REST and GraphQL documentation (field names, envelopes, enums);
- response shapes used by maintained open-source clients (BottlecapDave/HomeAssistant-OctopusEnergy, evcc, markheydon/octopus-energy-dotnet).

Quirks reproduced on purpose:
- absolute pagination `next` links naming `api.octopus.energy`, so tests can check the client follows them on the configured host;
- rate endpoints newest-first; consumption intervals with a local `+01:00` offset in BST and `Z` in GMT;
- microsecond timestamps (`tariffs_active_at`, `moved_in_at`);
- Kraken `Decimal` values as JSON strings, with occasional numbers and nulls;
- Agile day 2026-10-25 (clocks go back): 50 slots, including negative prices;
- both direct-debit and non-direct-debit standing-charge rows.

Consumption values are generated, not measured; region letters are arbitrary.

All identifiers are placeholders: MPAN `0000000000000`, MPRN `0000000000`, serials `00A0000000` / `G4A00000000000`, account `A-00000000`, device ids `00-00-00-00-00-00-00-0x`, address `1 Example Street`, e-mail `user@example.com`. The JWT is fake.
