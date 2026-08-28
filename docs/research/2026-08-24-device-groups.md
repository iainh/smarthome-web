# Research: Kasa-style device groups

**Date**: 2026-08-24
**Question**: How should `smarthome-web` add device groups similar to the Kasa app?
**Status**: Complete

## Context

`smarthome-web` uses the `smarthome` protocol library to discover and control legacy Kasa smart plugs over the local TCP/UDP protocol. The goal is to let a user collect plugs under a named group and switch the reachable members together, while retaining control of each plug.

This note records the design investigation that preceded the local device-group implementation.

## Findings

### Kasa's relevant group behaviour

TP-Link's current grouping guide documents the following behaviour:

- A group has a name, location, icon, and selected devices.
- Groups appear in their own section on the home screen and look like a normal device with an on/off control.
- Plugs, switches, and bulbs can be mixed in a group.
- A group command still controls reachable devices when one or more members are offline.
- The UI marks a group when at least one member is offline.
- The Kasa app permits up to 50 groups.
- Compatible lighting controls fan out only to members that support them.

The guide does not define the displayed on/off state for a group whose reachable members have mixed states. It also does not document group schedules, timers, or automation semantics. Those details should not be claimed as Kasa-compatible without direct app testing.

For this plug-only project, the useful first slice is therefore named membership, a dedicated Groups section, online/offline status, and group on/off.

### Groups are an orchestration feature, not a plug protocol feature

The local legacy Kasa protocol addresses one device at a time. In this project, `SmartHomeClient::set_relay` sends `system.set_relay_state` to one IP address ([`src/lib.rs`](../../src/lib.rs), `SmartHomeClient::set_relay`). There is no group module in the protocol client.

The independent `python-kasa` implementation has the same shape:

- `IotPlug.turn_on` and `turn_off` send `system.set_relay_state` to one device.
- The legacy module registry contains schedules, countdown, anti-theft, cloud, and energy modules, but no device-group module.
- Its only multi-target context is for physical child outlets in a power strip, not arbitrary app-created groups.

Consequently, `smarthome-web` should persist groups locally and fan a relay command out to each currently reachable member. Adding group methods to the `smarthome` wire-protocol library would put an application concern in the wrong layer.

### Device IDs are stable membership keys; IP addresses are not

The web application currently:

- discovers inventory on every full-page load and refresh ([`src/main.rs`](../../src/main.rs), `index`, `refresh`, and `discover`);
- uses an IP address in per-device control routes;
- receives a stable `device_id` in every `SmartPlug`;
- already persists weather automation ownership by `device_id` and resolves IDs against freshly discovered plugs during evaluation ([`src/automation.rs`](../../src/automation.rs), `AutomationRule` and `AutomationEngine::evaluate`).

Groups should follow the automation precedent and store member device IDs. Each inventory refresh provides the temporary `device_id -> SmartPlug/IP` mapping used for rendering and control. Storing IP addresses in a group would make membership stale after DHCP changes.

### The application has no persistent device catalogue

Only devices responding during the current inventory query are represented. With broadcast discovery, an offline member simply does not appear. With `DEVICE_ADDRESSES`, `get_inventory_from` silently filters failed direct queries and performs those queries sequentially ([`src/lib.rs`](../../src/lib.rs), `SmartHomeClient::get_inventory_from`).

A group store can still identify an offline member because it knows the expected device IDs, but it cannot recover that member's current alias or IP. The MVP does not need a full device registry:

- group membership and counts use stored device IDs;
- online aliases/details come from current inventory;
- an absent ID is shown as an unavailable member, optionally by a shortened device ID;
- editing a group must preserve absent existing members unless the user explicitly removes them.

Persisting alias snapshots would improve offline labels, but creates duplicated metadata and stale-name behaviour. It is not needed for the first implementation.

### Group control is inherently partially successful

Kasa explicitly controls online members even if others are offline. A normal `Result<()>` with fail-fast iteration would violate that behaviour.

A group action should:

1. obtain current inventory;
2. map stored member IDs to reachable plugs;
3. issue `set_relay` independently to every reachable member;
4. collect every result rather than stop at the first error;
5. rediscover or re-query affected reachable members;
6. render the whole groups-and-plugs list with a concise outcome such as “3 controlled, 1 unavailable”.

Blocking TCP operations should run concurrently (for example, a Tokio `JoinSet` of `spawn_blocking` tasks). Sequential fan-out is especially poor because `SmartHomeClient` has a five-second default timeout. Concurrency should also be applied to configured-address inventory before relying on groups at scale; otherwise several offline configured addresses make every refresh and group action wait for their timeouts in series.

The HTTP response should remain successful when at least one member was controlled so HTMX swaps in the updated state and warning. Return an error only for a malformed/nonexistent group, storage failure, or when the action could not be attempted. “No reachable members” is best rendered as an unavailable group outcome rather than a server error.

## Recommended design

### Persistent model

Add a focused `group` module rather than extending `automation.rs`:

```rust
struct DeviceGroup {
    id: u64,
    name: String,
    device_ids: Vec<String>,
}

struct GroupStore {
    next_id: u64,
    groups: Vec<DeviceGroup>,
}

struct GroupEngine {
    path: PathBuf,
    store: Mutex<GroupStore>,
}
```

Use the same copy-update-save-swap and temporary-file rename pattern as `AutomationEngine`. Preserve declaration order for groups and member order for predictable UI. Validate:

- trimmed, non-empty name (a 64-character limit matches existing schedule naming);
- at least one member;
- unique member IDs within a group;
- no more than 50 groups, matching Kasa;
- submitted IDs are known from current inventory, except already-persisted offline members during edit.

Allow a device to belong to multiple groups unless product requirements say otherwise; the Kasa guide does not document exclusivity.

Use `GROUPS_PATH`, defaulting to `groups.json`, and configure `/data/groups.json` in the container environment so the existing `/data` volume persists it.

### View model and state

Build one inventory page model from current plugs plus stored groups. Recommended group states:

| State | Meaning |
|---|---|
| On | Every reachable member is on |
| Off | Every reachable member is off |
| Mixed | Reachable members differ |
| Unavailable | No member is reachable |

Offline status is orthogonal: show a warning whenever `reachable_count < total_count`, including for On, Off, or Mixed. This is clearer than forcing mixed or partial availability into one Boolean. Because Kasa's mixed-state display is undocumented, label this as intentional `smarthome-web` behaviour.

### Routes

A small REST-like HTMX surface is sufficient:

```text
POST   /groups                     create
POST   /groups/{id}                edit name and membership
DELETE /groups/{id}                delete
POST   /groups/{id}/relay          turn reachable members on/off
```

Creation and editing can be presented in the existing side pane. Group relay should target `#plug-list` and return the entire list because one action changes both the group card and multiple individual plug cards. Group mutations should do the same so group sections and membership counts stay coherent.

Do not expose raw device IDs as group route identifiers; use the server-generated numeric group ID.

### UI

Keep the current individual plug grid. Above it, add:

- a dedicated **Groups** section;
- a group card with name, member/reachable count, On/Off/Mixed/Unavailable state, partial-offline warning, and on/off button;
- an **Add group** action near Refresh;
- an Edit action listing current discovered plugs as checkboxes and retaining clearly marked unavailable existing members;
- delete confirmation.

A group detail page is unnecessary for the MVP. The edit pane provides membership inspection without creating another navigation layer.

### Automations, schedules, and timers

Do not include them in the first group implementation:

- Device schedules and countdown timers are stored independently on each plug, so a “group schedule” would require distributed create/edit/delete with rollback or visible per-member divergence.
- Server weather automations could later target a group, but rules need explicit semantics when membership changes and when only some members are reachable.
- TP-Link's grouping guide does not promise either behaviour.

A later server-owned group automation can safely reuse the same best-effort fan-out primitive once product semantics are decided. It should not masquerade as a device-resident schedule.

## Options considered

| Option | Pros | Cons | Effort |
|---|---|---|---|
| Local persisted groups with fan-out | Correct boundary; local-only; stable across DHCP; matches Kasa partial-offline control | Server must be running; not synchronized with Kasa cloud groups | Medium |
| Store groups on each plug | None for arbitrary groups | No such legacy protocol feature; duplicated/divergent state | Not viable |
| Import/control Kasa cloud groups | Shares native app state | Requires undocumented cloud API, credentials, Internet, and newer-device auth work outside this project's current protocol | High and fragile |
| Browser-only groups in `localStorage` | Very small backend change | Per-browser, easy to lose, unavailable to server automations, poor multi-user behaviour | Low but unsuitable |
| Add a full persistent device registry first | Better offline names/history | Broadens scope and introduces cache freshness/retention rules before they are needed | High |

## Implementation sequence

1. Add and unit-test the persistent group store (load, create, edit, delete, validation, atomic save, restart persistence).
2. Introduce a page view model that resolves groups against an injected inventory; unit-test On/Off/Mixed/Unavailable and partial-offline cases.
3. Add create/edit/delete routes and templates; test escaping, form actions, offline-member preservation, and the 50-group limit.
4. Add best-effort concurrent relay fan-out and structured results; unit-test it behind a small control trait/fake rather than requiring real plugs.
5. Update group and individual cards together after a group action.
6. Parallelize configured-address inventory queries if `DEVICE_ADDRESSES` is expected to include more than a few devices or offline devices are common.
7. Add `GROUPS_PATH` to container defaults and document volume persistence.

## Risks and decisions needed

- **Mixed-state action:** Recommended buttons always offer explicit Turn on and Turn off, avoiding ambiguous toggle semantics.
- **No reachable members:** Recommended behaviour is a rendered unavailable warning, not HTTP 500.
- **Duplicate membership:** Recommended to allow one plug in multiple groups; Kasa's official guide is silent.
- **Member limit:** Kasa documents 50 total groups but no member limit. Do not invent one unless needed for operational protection.
- **Cross-instance writes:** The existing mutex and rename pattern is safe within one process, not across multiple app instances sharing a volume. That matches the current deployment model.
- **App synchronization:** These groups will not appear in the Kasa app, and Kasa-created groups will not appear here. The UI/documentation should call them local groups if that distinction could surprise users.

## Recommendation

Implement a local `GroupEngine` keyed by stable device IDs, render groups as first-class cards above individual plugs, and use concurrent best-effort fan-out for explicit on/off actions. Preserve offline membership and report partial availability rather than failing the whole action. Keep schedules, countdowns, and weather automations device-scoped for the first release.

This is the smallest design that matches the important Kasa experience while respecting the local protocol and the current application's architecture.

## References

- [TP-Link: How to Use the Kasa App Grouping Feature to Control Multiple Devices](https://www.tp-link.com/us/support/faq/2299/) (updated 2026-07-01)
- [`smarthome` protocol client](../../src/lib.rs)
- [`smarthome-web` web application](../../src/main.rs)
- [`smarthome-web` automation persistence and ID resolution](../../src/automation.rs)
- [python-kasa repository](https://github.com/python-kasa/python-kasa)
- [python-kasa legacy plug control](https://github.com/python-kasa/python-kasa/blob/master/kasa/iot/iotplug.py)
- [python-kasa legacy module registry](https://github.com/python-kasa/python-kasa/blob/master/kasa/iot/modules/__init__.py)
- [Home Assistant TP-Link Smart Home integration](https://www.home-assistant.io/integrations/tplink/)
