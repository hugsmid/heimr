# Security

## Reporting a vulnerability

Use GitHub's **private vulnerability reporting**: on this repository, open the *Security* tab and choose *Report a vulnerability*. Please do not open a public issue for anything that could put a home at risk.

Include what you found, how to reproduce it, and the commit you tested. Handling is best effort: this is a volunteer project and there is no response-time guarantee and no bounty.

## Supported versions

Nothing is released yet. Only the `main` branch is supported; fixes land there.

## Threat model (summary)

An attack on a home needs two things: **knowledge** of a device and a **path** to it. heimr is designed so that the path stays closed.

| Path | Design response |
|---|---|
| Internet → home | **Nothing listens on the internet, by design.** No port forwards, no public URL. Remote access is only over a private overlay network / VPN the site operator runs. heimr has no network-facing front-end yet; when one lands it is meant for the LAN or that private network only |
| Vendor clouds | Outside heimr's control. Prefer local adapters; use unique passwords and two-factor authentication for any cloud account an adapter uses |
| Someone on the LAN, or a compromised device | Keep devices on a separate network segment where possible; authenticate the database and any message broker. heimr treats what adapters report as input to validate: records are typed and checked, and an instance cannot speak for another instance or for the core |
| Supply chain (this repository, dependencies, CI) | Changes are reviewed before merge; CI uses no secrets and has read-only repository access; GitHub Actions are pinned to commit SHAs; `cargo-deny` checks advisories, licences, bans and sources. Deploy a pinned commit you have reviewed, never `main` directly |

Safety that limits the damage of a bug or a hostile command:

- **Heating fails safe:** keep the heating system's own schedule configured as the fallback. heimr only adjusts within a configured setpoint band, and every command that overrides a device carries a hold (until the next schedule change, or at most `max_hold`, 2 hours by default) that adapters map to the device's own timed override, so the device reverts even if heimr stops.
- **Shadow mode** is the default for every write-capable adapter instance and is enforced by the core, not the adapter: a shadow instance has no command queue at all.
- **One gate for every command.** Deciders get a submit-only handle bound to their actor name, which is recorded on every command; names reserved for the core cannot be used. The actuator checks capabilities, the safety policy and rate limits (rebuilt from the audit journal after a restart, with a hold-off after start), then appends the decision to an audit journal and fsyncs it before delivering; a command whose record cannot be made durable in time is withheld.
- **Containment:** adapters cannot emit as another source or actor, or forge core events, and the core's own names (`actuator`, `core`, `heimr…`) cannot be used as instance names. An adapter can report its health but cannot change its run state. Each instance runs under its own supervisor; a failing one is restarted with backoff or kept down, and the rest of heimr keeps running.

Known limits:

- Rate limits survive a restart only as far as the audit journal does. Lines it cannot read count as accepted commands, but if the journal is missing altogether (deleted, or a new state directory), the hourly-cap history is lost and the hold-off after start is the only limit until new history builds up. Keep the state directory on persistent storage and back it up.

## What this repository never contains

Secrets, real meter or account numbers, real addresses or locations, recordings of real homes, schedules or presence data. Fixtures are hand-built. CI runs gitleaks over the full history. If you find something that looks like real personal data here, report it as a vulnerability.

## Running heimr safely

- Keep heimr off the public internet. Do not add port forwards or tunnels to it.
- Keep secrets in an encrypted secret store, never in plain text in your configuration repository.
- Run new adapters in shadow mode until you have checked their decisions.
- Pin the heimr revision you deploy and update deliberately.
