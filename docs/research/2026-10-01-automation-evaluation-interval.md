# Research: Automation evaluation interval

**Date**: 2026-10-01
**Question**: Can server-owned schedules be evaluated every 30 seconds instead of every five minutes?
**Status**: Complete

## Findings

- `AutomationEngine::run` evaluates serially, so a slow pass cannot overlap the next pass. Missed ticks are skipped.
- Fixed and solar rules persist `last_solar_day`, preventing repeated actions during their 20-minute trigger window.
- Every evaluation discovers all configured or remembered devices and fetches Open-Meteo data once per unique location.
- A direct interval change would increase those operations tenfold. At one unique location, weather traffic would rise from 288 to 2,880 requests per day.
- Open-Meteo's free API limit is 10,000 calls per day and 300,000 per month. Four continuously active unique locations would exceed both limits at a 30-second cadence.
- Open-Meteo documents current conditions as based on 15-minute weather-model data, so 30-second weather refreshes would provide little additional freshness.

## Recommendation

Evaluate timed schedules every 30 seconds, but decouple weather retrieval from the scheduler clock. Use the system clock for fixed and solar trigger timing and retain a slower cached weather refresh for solar forecasts and light-level rules. Device state refresh can remain serial, but should be measured or separately cached if the device count grows.

## References

- `src/automation.rs`: evaluation loop, device discovery, Open-Meteo retrieval, and once-per-day timed-rule guard
- `templates/automation-panel.html`: user-facing five-minute interval description
- <https://open-meteo.com/en/terms>
- <https://open-meteo.com/en/docs>
