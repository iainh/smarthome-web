# Research: Calendar view scheduling improvements

**Date**: 2026-08-26
**Question**: How should `tddp-client` improve its weekly calendar so schedules are easier to understand and edit on desktop and mobile?
**Status**: Complete

## Recommendation

Evolve the existing calendar into a compact scheduling workspace rather than replacing it with a full calendar application. The first release should:

1. summarize the plug's current scheduled state and next deterministic transition;
2. add a current-time marker and stronger today treatment;
3. let a selected span or event show its details and actions beside the calendar instead of immediately scrolling to a distant form;
4. warn when several rules share a trigger time and explain which rule wins;
5. use a focused single-day layout on narrow screens.

Keep the existing **State** and **Rule events** views. Defer week navigation, direct manipulation and predicted light-level state until the basic information architecture is proven.

## Current implementation

The calendar is a server-rendered projection in the automation side pane:

```diagram
┌──────────────────────┐
│ AutomationRule rows  │
└──────────┬───────────┘
           │
           ├── fixed and solar events ──▶ deterministic ON/OFF spans
           │
           └── light-level rules ───────▶ conditional active windows
                                         (not predicted relay state)
           │
           ▼
┌──────────────────────┐       ┌──────────────────────────┐
│ WeekCalendarView     │──────▶│ automation-panel.html    │
│ seven local days     │       │ state/events CSS toggle  │
└──────────────────────┘       └────────────┬─────────────┘
                                            │ select entry
                                            ▼
                                  existing inline rule editor
```

Confirmed behaviour:

- [`week_calendar_view`](../../src/main.rs) always builds the current Monday–Sunday week from `WeatherStatus.current_day` and the plug's forecast timezone.
- Fixed and solar rules become event points. Enabled timed rules also become continuous scheduled-state spans. At identical timestamps, the highest rule ID wins, matching the UI's “most recently added rule wins” policy.
- The current uncommitted work adds explicit scheduled-off spans. This should be the baseline for the next implementation.
- Light-level automations show only their active windows and thresholds. Their future state is conditional on weather and hysteresis, so presenting them as predicted ON/OFF periods would be misleading.
- Disabled timed rules remain visible in **Rule events**, with reduced emphasis, and are omitted from **State**.
- Selecting any span or point opens and scrolls to its existing `<details>` editor.
- The calendar is unavailable when weather cannot be fetched, even though fixed-time rules do not require weather. The weather result currently supplies local day, timezone and solar forecasts as one object.
- The pane defaults to 32 rem and can be resized on desktop. At 36 rem and below the same seven 24-hour tracks are compressed into the mobile width.
- Calendar projection and template output have Rust unit coverage. There is no browser-level test for selection, the view toggle or responsive behaviour.

## Usability gaps

### The calendar shows a week but does not answer “what happens next?”

The continuous spans improve comprehension, but the viewer still has to find today, estimate the current time on the axis and inspect the next boundary. A compact summary can answer the primary operational question directly.

### Selection navigates away from context

Clicking an entry immediately scrolls to the editor lower in the pane. This is efficient for editing but poor for inspection, especially when several small or overlapping entries are close together. The native `title` text is also not dependable on touch devices.

### Conflicts are resolved but not visible

Two fixed or solar rules can occur at the same minute. The projection keeps only the highest-ID event when deriving state, while the event view renders both points at the same horizontal position. The UI explains the global precedence rule but does not identify a concrete collision or losing rule.

### Mobile compresses too much information

Seven horizontal 24-hour tracks with boundary labels do not have enough room in a phone-width pane. A mobile day selector and one expanded day preserve touch targets and labels without introducing horizontal scrolling.

### “State” mixes deterministic state and conditional control

Green and grey spans mean deterministic state set by timed rules. Blue hatched spans mean a light rule is allowed to control the plug, not that the plug will be on. The legend states the categories, but the visual hierarchy and summary should reinforce this distinction.

### Weather availability owns too much of the calendar

When weather fails, the whole calendar disappears. Fixed-time scheduling can still be projected if the application has a local timezone/day source. Decoupling this is useful, but it requires a deliberate timezone contract and should not block the first UI iteration.

## Proposed experience

### Desktop and wide pane

```text
┌──────────────────────────────────────────────────────────────────┐
│ This week · Aug 24–30                         [State] [Events]    │
│ Scheduled OFF now  •  Next: ON at 7:50 PM (Evening)             │
│ Times shown in GMT-4                                             │
├──────┬──────────────┬──────────────┬──────────────┬──────────────┤
│      │ 12 AM        │ 6 AM         │ Noon         │ 6 PM         │
│ Mon  │ OFF██████████│░░░░░░░░░░░░░░░░░░░░░░░░░░│ON██████████  │
│ Tue  │ ON███████████│OFF░░░░░░░░░░░░░░░░░░░░░░│ON██████████  │
│ Wed  │ ON███████████│OFF░░░░░░░░░░░░░░░░░░░░░░│ON██████████  │
│ Thu  │ ON███████████│OFF░░░░░░░││ now           │ON██████████  │
│      │              │    ╱╱ AUTO: Cloudy daytime ╱╱             │
│ Fri  │ ON███████████│OFF░░░░░░░░░░░░░░░░░░░░░░│ON██████████  │
├──────┴───────────────────────────────────────────────────────────┤
│ Selected: Evening                                                │
│ Sunset −30 min · Today 7:50 PM · Turn on · Every day            │
│                                                    [Edit rule]   │
└──────────────────────────────────────────────────────────────────┘
```

Interaction:

- Selecting a span, boundary or light window applies a visible selected state and updates the details tray.
- **Edit rule** opens and scrolls to the existing editor. Selection alone does not move the pane.
- The summary uses only deterministic timed events. If a light rule currently has authority, add a separate “Conditional control active” note rather than asserting the relay's future state.
- The current-time marker appears only on today and is labelled once, above or below the track to avoid covering an entry.

### Rule events and collision warning

```text
┌──────────────────────────────────────────────────────────────────┐
│ This week · Aug 24–30                         [State] [Events]    │
├──────┬───────────────────────────────────────────────────────────┤
│ Thu  │        ● 7:30 OFF       ◆ 7:50 ON                        │
│ Fri  │        ●● 7:30 conflict ◆ 7:48 ON                        │
├──────┴───────────────────────────────────────────────────────────┤
│ ⚠ Two rules run Friday at 7:30 AM. “Holiday morning” wins        │
│ because it was added most recently.                              │
│ [Morning weekday] [Holiday morning · wins]                       │
└──────────────────────────────────────────────────────────────────┘
```

The conflict is informational. Selecting either rule still opens its details; changing priority is out of scope because priority is currently implicit in insertion order.

### Mobile and narrow pane

```text
┌──────────────────────────────────┐
│ This week       [State] [Events] │
│ OFF now · ON at 7:50 PM          │
│                                  │
│ [M] [T] [W] [Thu] [F] [S] [S]   │
│ Thu 27 · Today                   │
│ 12 AM       6 AM      Noon  6 PM │
│ ON██████████│OFF░░░░░░░░│ON████ │
│              │ now               │
│       ╱╱ AUTO 9:00–8:10 ╱╱       │
│                                  │
│ Evening                          │
│ 7:50 PM · Turn on · Every day    │
│                      [Edit rule] │
└──────────────────────────────────┘
```

Use a seven-button day picker and render one day at a time below 36 rem. Default to today. This avoids horizontal scrolling, preserves the full 24-hour mental model and provides at least 44 px touch targets for the day picker and selected-entry actions. Keep all seven days in the HTML so switching days remains instant and requires no route.

### Empty and unavailable states

```text
┌──────────────────────────────────────────┐
│ This week                                │
│ No timed schedules yet.                  │
│ Add an event to show when the plug will  │
│ turn on or off.                          │
│ [Add fixed time] [Add sunrise/sunset]    │
└──────────────────────────────────────────┘
```

If solar forecasts are unavailable but fixed-time projection becomes available later, show fixed events and a scoped notice: “Sunrise, sunset and outdoor-light windows are unavailable.” Do not remove the entire calendar.

## Implementation plan

### Phase 0: finish and protect the scheduled-off baseline

Scope: complete the current uncommitted state-span work before layering new behaviour.

- Keep `push_scheduled_state_spans` as the single deterministic state projection for enabled fixed and solar rules.
- Add focused tests for an initial OFF state, same-action events that should not split a span, same-minute precedence and a week with only one selected weekday.
- Run the full test suite and commit this behavioural change separately if reviewable commits are desired.

Acceptance criteria:

- Every day with a known preceding timed event has a continuous ON or OFF state across all 24 hours.
- The state span's rule ID identifies the event that established that state.
- Light windows remain visually and semantically separate from scheduled state.

### Phase 1: add summary, current time and selection details

This is the recommended first product increment.

Backend in [`src/main.rs`](../../src/main.rs):

- Extend `WeekCalendarView` with `week_label`, the current minute/day position and an optional deterministic state summary.
- Represent the next timed transition with rule ID, rule name, action and formatted local time. Derive it from the same ordered event map used for spans so the summary cannot disagree with the chart.
- Add stable entry identifiers or data attributes for selection. Reuse `CalendarEntryView`; do not introduce a parallel calendar model.

Template in [`templates/automation-panel.html`](../../templates/automation-panel.html):

- Render the “now / next” summary under the heading.
- Add the today-only current-time marker.
- Replace direct `openScheduleEditor` calls with selection metadata and a details tray.
- Keep an explicit **Edit rule** action in the tray that calls the existing editor function.
- Add a useful empty-state action that opens the existing add-schedule `<details>` block; do not add a new creation route.

Client behaviour and styles in [`templates/index.html`](../../templates/index.html):

- Add event-delegated `selectCalendarEntry` and `editSelectedCalendarRule` behaviour so HTMX pane replacement does not require rebinding handlers.
- Apply `aria-pressed` to selectable entries and use an `aria-live="polite"` details region.
- Preserve keyboard activation through native buttons and make the selected outline independent of hover.

Tests:

- Unit-test summary derivation before, exactly at and after a transition, including a solar transition.
- Template-test current-time marker, selected-entry metadata, details tray and empty state.
- Manually check keyboard selection, focus visibility, dark/light themes and a touch viewport.

Acceptance criteria:

- A viewer can identify the scheduled state and next timed change without reading individual spans.
- Selecting an entry does not scroll the pane.
- Every selected entry exposes its rule name, timing/action detail and an edit action without relying on hover.

### Phase 2: expose concrete conflicts

Backend:

- Change the event projection to retain all fixed/solar events at a timestamp, while separately marking the highest-ID enabled event as the deterministic winner.
- Add `CalendarConflictView` only if conflict rendering cannot be expressed cleanly on entries. A conflict consists of one timestamp and at least two enabled timed rules.
- Keep disabled rules out of conflict resolution.

UI:

- Stack or slightly offset coincident points in **Rule events** so each remains selectable.
- Add a warning row under the calendar naming the winning rule and the reason.
- Add a “wins” badge in the selection tray for the effective event. Do not add manual priority controls in this phase.

Tests:

- Same-time fixed/fixed, fixed/solar and three-rule collisions.
- Highest-ID precedence and disabled-rule exclusion.
- No warning for different events that happen to produce the same ON/OFF action at different times.

Acceptance criteria:

- No enabled event is hidden by another point at the same minute.
- The displayed winner matches automation evaluation order and state-span derivation.

### Phase 3: responsive single-day mode

Template and CSS:

- Add a day picker linked to each `.calendar-day` by ID and `aria-controls`.
- Below 36 rem, show today by default and hide the other day tracks. Above 36 rem, retain the full week.
- Keep all days in the document for accessibility and instant switching; JavaScript controls `hidden` and selected state.
- Expand short span hit areas without visually changing their time width. Avoid using a minimum visual width that implies a longer duration.

Client behaviour:

- Add event-delegated day selection.
- When the viewport crosses the breakpoint, clear mobile-only `hidden` state so desktop always shows all days.
- After an HTMX refresh, default to today. Preserving the selected day in `sessionStorage` is optional and should be added only if refresh behaviour proves frustrating.

Tests and checks:

- Template-test day IDs, controls and today selection.
- Manually verify at 320, 375, 512 and 768 px widths, in portrait and landscape.
- Verify browser zoom at 200% and keyboard traversal through every day button and calendar entry.

Acceptance criteria:

- Calendar labels remain readable at 320 px.
- Touching one event does not accidentally select an adjacent event.
- Desktop remains a seven-day overview.

### Phase 4: optional week navigation and partial weather fallback

Only implement this after Phases 1–3 are used and validated.

- Accept a bounded week offset, recommended `-1`, `0` and `+1`, on the automation panel GET route.
- Preserve the offset in every HTMX mutation that replaces the pane, or reset to the current week with an explicit product decision.
- Fetch enough solar forecast/history for the allowed range. Open-Meteo forecast availability must define the forward bound; do not synthesize solar times.
- Split calendar context from `WeatherStatus` so fixed-time rules can render when the weather request fails. The application needs a trusted timezone and local-day source before doing this safely.

This phase has a larger cross-cutting cost because week choice must survive refresh, edit, enable/disable and delete responses. A client-only week switch is not sufficient because future solar times are server data.

## Deferred ideas

### Drag to reschedule

Do not add drag handles yet. Fixed times, solar offsets and light-window boundaries have different domains and validation. Dragging also needs snapping, collision feedback, touch gestures, keyboard equivalents, optimistic state and rollback on an HTMX error. The selection tray plus existing form is the simpler accessible editing path.

### Predicted light-level ON/OFF state

Do not infer future plug state from the existing light chart or shortwave forecast without explicitly modelling forecast values, hysteresis and rule precedence. Continue to label these spans as conditional control windows.

### Month view

The scheduling model repeats weekly and solar times shift gradually. A month grid would consume space without exposing more actionable detail.

### Manual rule priority

The database currently uses insertion order as implicit priority. A reorder feature needs an explicit persisted priority column and corresponding evaluation contract. Conflict visibility should come first.

## Options considered

| Option | Benefits | Costs and risks | Recommendation |
|---|---|---|---|
| Incremental workspace around the current week projection | Reuses the model, routes and editors; improves comprehension and touch access | Adds a small view-state layer in JavaScript | Choose |
| Full interactive calendar with drag/drop | Familiar direct manipulation | Large validation and accessibility surface; poor fit for solar and conditional rules | Defer |
| Event list instead of timeline | Excellent narrow-screen readability | Loses duration/state comprehension | Keep as a future alternate summary, not a replacement |
| Horizontally scrollable seven-day mobile timeline | Small CSS change | Hard to compare days; hidden content and awkward touch interaction | Reject |
| Predicted “actual state” calendar | Potentially answers what the relay will do | Cannot be accurate for conditional weather rules and state hysteresis with current data | Reject for now |

## Decisions to confirm before implementation

The plan assumes these product choices:

- “Current state” means the deterministic state implied by the latest enabled fixed/solar event, not the device's observed relay state.
- Light-level rules are shown as conditional control windows, never guaranteed state.
- Selection inspects first; editing requires the explicit **Edit rule** action.
- Mobile defaults to one day, while desktop remains a full-week view.
- Rule priority stays implicit and read-only for now.

If “current state” should instead mean the physical relay state, label it **Plug is ON/OFF** and show the deterministic value separately as **Schedule expects ON/OFF**. Combining the two would conceal manual overrides and failed device commands.

## Verification strategy

Use three layers:

1. Rust unit tests for event projection, state spans, next transition, collisions and day/week boundaries.
2. MiniJinja fragment tests for semantic HTML, data attributes, labels and empty/unavailable states.
3. Manual browser checks for interaction and responsive layout. The project has no browser-test harness; adding one solely for this iteration is not justified unless calendar JavaScript grows beyond selection, day switching and the existing view toggle.

Run `cargo test` after each phase. Use test fixtures with fixed local days and solar minutes so tests do not depend on the network or wall clock.

## References

- [`src/main.rs`](../../src/main.rs): calendar view models, projection, routes and template tests
- [`src/automation.rs`](../../src/automation.rs): rule evaluation and precedence behaviour
- [`templates/automation-panel.html`](../../templates/automation-panel.html): calendar and schedule editors
- [`templates/index.html`](../../templates/index.html): calendar styles and client interactions
- Commit `7395cad`: initial weekly state calendar
- Current uncommitted worktree: explicit scheduled-off spans and state-specific legend
