# Fixtures (hand-built)

**These are not recordings.** `sensor.json` is a cassette for `heimr_testkit::replay`, written by hand from the response shape the example adapter expects (`{"temperature": °C, "humidity": %}`). Responses are given in order; the last one repeats.

When you copy this template, replace it with a recording of your device, saved through `Cassette::save` with a `Redactor` so identifying values (serial numbers, addresses, tokens) are replaced by obvious placeholders such as `00A0000000`. Never commit real readings, schedules or presence.
