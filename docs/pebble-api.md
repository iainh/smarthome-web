# Pebble companion API

The Pebble companion uses the authenticated JSON API at `/api/v1`. The existing HTML interface is unchanged and remains unauthenticated; a reverse proxy intended for remote companion access should expose **only** `/api/v1/*`.

## Configure authentication

Generate a token and store it in a root-readable file outside the image:

```sh
umask 077
openssl rand -hex 32 > /data/pebble-api-token
```

Set `PEBBLE_API_TOKEN_FILE=/data/pebble-api-token` and restart the application. The file must contain exactly 64 lowercase hexadecimal characters (one trailing newline is accepted). If the variable is absent, API requests return `503 api_disabled`. If a configured file is missing or malformed, startup fails. Rotate the token by replacing the file, restarting the server, and updating the companion configuration. Revoke access by unsetting the variable or removing the file and restarting.

Every request requires `Authorization: Bearer TOKEN`. Tokens must not be put in URLs or logs. Responses are JSON, use `Cache-Control: no-store`, and include `X-Request-Id`. A valid caller-supplied request ID (1–64 ASCII letters, digits, `_`, or `-`) is echoed; otherwise the server creates one.

## Transport and exposure

TLS termination is intentionally outside this Rust application. Prefer an HTTPS reverse proxy whose certificate chain the paired phone trusts. PKJS cannot bypass certificate validation. Direct HTTP works when deliberately configured for a trusted network fallback, but exposes the bearer token and requests to anyone able to observe that network.

The companion stores its endpoint and token in app-scoped PKJS `localStorage`, not a secure keystore. Changing configuration must clear cached inventory and credentials. Uninstalling the watch app is not token revocation; rotate or revoke the server token after loss or compromise.

## Contract

- `GET /api/v1/info` reports API version, features, and limits.
- `GET /api/v1/inventory` probes remembered addresses only. It does not discover devices. Fresh availability is separate from preserved last-known state.
- `PUT /api/v1/devices/{device_id}/relay` accepts `{"on":true}` or `{"on":false}`.
- `PUT /api/v1/devices/{device_id}/brightness` accepts `{"brightness":1..100}`. The device must freshly identify as a dimmer with its relay on.
- `PUT /api/v1/groups/{decimal_group_id}/relay` accepts the same absolute relay body.

Mutation responses contain per-device `confirmed`, `failed`, `unknown`, or `not_attempted` results and aggregate counts. Evaluated device failures and partial groups return HTTP 200; the `outcome` field is authoritative. Request-level failures use a structured `error` envelope. Bodies are limited to 1 KiB and inventory output to 1 MiB.

Before actuation, the server resolves the remembered address from the stable device ID, probes it, verifies the returned identity, validates prerequisites, sends an absolute command, reads state back, and persists only verified observations by device ID. An address that answers with another identity is never actuated. Group control is bounded-concurrent best effort without rollback.

The supported initial watch target is Pebble Emery/Obelix (200×228 colour, 128 KiB app memory). Production qualification should cover HTTPS and deliberate HTTP phone configuration, authorization, timeouts, persistence across restart, physical dimmer behavior, Bluetooth/response loss, and offline group members.
