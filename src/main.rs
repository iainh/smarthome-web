mod automation;
mod database;
mod group;
mod mock;

use automation::{
    ActiveWindow, AutomationEngine, AutomationRule, AutomationTrigger, NewAutomation,
    OutsideWindowBehavior, SolarEvent, SolarForecastDay, TimeBoundary, WeatherStatus,
};
use axum::body::Body;
use axum::extract::{Form, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use database::Database;
use group::{automation_target, DeviceGroup, GroupEngine};
use minijinja::{context, AutoEscape, Environment};
use serde::{Deserialize, Serialize};
use smarthome::{CountdownRule, ScheduleRule, SmartHomeClient, SmartPlug};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::TryFrom;
use std::error::Error as StdError;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::task;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);
const WEATHER_HISTORY_RETENTION: Duration = Duration::from_secs(90 * 24 * 60 * 60);
const WEATHER_PURGE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

struct AppState {
    client: SmartHomeClient,
    templates: Environment<'static>,
    automations: Arc<AutomationEngine>,
    groups: Arc<GroupEngine>,
    database: Arc<Database>,
    device_addresses: Vec<IpAddr>,
}

#[derive(Debug, Serialize)]
struct PlugView {
    address: String,
    model: String,
    alias: String,
    device_id: String,
    software_version: String,
    relay_on: bool,
}

impl From<SmartPlug> for PlugView {
    fn from(plug: SmartPlug) -> Self {
        let address = plug.address.to_string();
        Self {
            address,
            model: plug.model,
            alias: plug.alias,
            device_id: plug.device_id,
            software_version: plug.software_version,
            relay_on: plug.relay_on,
        }
    }
}

#[derive(Deserialize)]
struct RelayForm {
    on: bool,
}

#[derive(Deserialize)]
struct GroupForm {
    name: String,
    #[serde(flatten)]
    fields: HashMap<String, String>,
}

impl GroupForm {
    fn into_parts(self) -> (String, Vec<String>) {
        let mut members: Vec<_> = self
            .fields
            .into_iter()
            .filter_map(|(name, device_id)| name.starts_with("device_").then_some(device_id))
            .collect();
        members.sort_unstable();
        (self.name, members)
    }
}

#[derive(Serialize)]
struct DeviceListView {
    groups: Vec<GroupView>,
    plugs: Vec<PlugView>,
    notice: Option<String>,
}

#[derive(Debug, Serialize)]
struct GroupView {
    id: u64,
    name: String,
    member_count: usize,
    reachable_count: usize,
    members: String,
    state: &'static str,
    state_class: &'static str,
    has_offline_members: bool,
}

#[derive(Serialize)]
struct GroupPanel {
    id: Option<u64>,
    name: String,
    devices: Vec<GroupDeviceOption>,
    editing: bool,
}

#[derive(Serialize)]
struct GroupDeviceOption {
    field_name: String,
    device_id: String,
    alias: String,
    available: bool,
    selected: bool,
}

#[derive(Deserialize)]
struct SolarAutomationForm {
    name: String,
    event: String,
    offset_minutes: i16,
    action: String,
    sun: Option<String>,
    mon: Option<String>,
    tue: Option<String>,
    wed: Option<String>,
    thu: Option<String>,
    fri: Option<String>,
    sat: Option<String>,
}

#[derive(Deserialize)]
struct LightAutomationForm {
    name: String,
    on_below: f64,
    off_above: f64,
    window_enabled: Option<String>,
    start_kind: String,
    start_time: String,
    start_offset_minutes: i16,
    end_kind: String,
    end_time: String,
    end_offset_minutes: i16,
    outside_window: String,
}

#[derive(Serialize)]
struct AutomationPanel {
    address: String,
    automation_base: String,
    location_available: bool,
    weather: Option<WeatherStatus>,
    calendar: Option<WeekCalendarView>,
    rules: Vec<AutomationView>,
    schedules: SchedulePanel,
}

#[derive(Serialize)]
struct WeekCalendarView {
    timezone: String,
    week_label: String,
    current_left: f64,
    summary: Option<CalendarSummaryView>,
    has_entries: bool,
    conflicts: Vec<CalendarConflictView>,
    days: Vec<CalendarDayView>,
}

#[derive(Serialize)]
struct CalendarDayView {
    index: usize,
    name: &'static str,
    date_label: String,
    is_today: bool,
    lane_count: usize,
    entries: Vec<CalendarEntryView>,
}

#[derive(Serialize)]
struct CalendarSummaryView {
    state: &'static str,
    next: Option<String>,
}

#[derive(Serialize)]
struct CalendarConflictView {
    detail: String,
}

#[derive(Serialize)]
struct CalendarEntryView {
    rule_id: u64,
    name: String,
    label: String,
    detail: String,
    enabled: bool,
    class: String,
    left: f64,
    width: f64,
    lane: usize,
    point: bool,
    conflict: bool,
    winner: bool,
    collision_index: usize,
}

#[derive(Serialize)]
struct AutomationView {
    id: u64,
    title: &'static str,
    kind: &'static str,
    name: String,
    enabled: bool,
    description: String,
    time: String,
    event: &'static str,
    offset_minutes: i16,
    turn_on: bool,
    sun: bool,
    mon: bool,
    tue: bool,
    wed: bool,
    thu: bool,
    fri: bool,
    sat: bool,
    on_below: f64,
    off_above: f64,
    window_enabled: bool,
    start_kind: &'static str,
    start_time: String,
    start_offset_minutes: i16,
    end_kind: &'static str,
    end_time: String,
    end_offset_minutes: i16,
    outside_turn_off: bool,
}

#[derive(Deserialize)]
struct CountdownForm {
    minutes: u64,
    action: String,
}

#[derive(Debug)]
struct CountdownInput {
    delay_seconds: u64,
    turn_on: bool,
}

#[derive(Serialize)]
struct CountdownPanel {
    address: String,
    rules: Vec<CountdownView>,
}

#[derive(Serialize)]
struct CountdownView {
    id: String,
    name: String,
    enabled: bool,
    delay: String,
    action: &'static str,
}

#[derive(Deserialize)]
struct ScheduleForm {
    name: String,
    time: String,
    action: String,
    sun: Option<String>,
    mon: Option<String>,
    tue: Option<String>,
    wed: Option<String>,
    thu: Option<String>,
    fri: Option<String>,
    sat: Option<String>,
}

#[derive(Serialize)]
struct SchedulePanel {
    migratable_count: usize,
    unsupported_count: usize,
    rules: Vec<ScheduleView>,
}

#[derive(Serialize)]
struct ScheduleView {
    name: String,
    enabled: bool,
    migratable: bool,
    time: String,
    action: &'static str,
    weekday_summary: String,
}

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)
    }
}

impl StdError for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}

impl From<smarthome::Error> for AppError {
    fn from(error: smarthome::Error) -> Self {
        Self::internal(error.to_string())
    }
}

impl From<minijinja::Error> for AppError {
    fn from(error: minijinja::Error) -> Self {
        Self::internal(error.to_string())
    }
}

impl From<task::JoinError> for AppError {
    fn from(error: task::JoinError) -> Self {
        Self::internal(format!("device operation failed: {error}"))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn StdError + Send + Sync>> {
    let mock_devices = match std::env::var("MOCK_OUTLETS").as_deref() {
        Ok("1" | "true") => Some(mock::start()?),
        _ => None,
    };
    let database_path = std::env::var_os("DATABASE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tddp-client.sqlite3"));
    let automation_path = std::env::var_os("AUTOMATIONS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| database_path.with_file_name("automations.json"));
    let group_path = std::env::var_os("GROUPS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| database_path.with_file_name("groups.json"));
    let device_addresses = match std::env::var("DEVICE_ADDRESSES") {
        Ok(value) => parse_device_addresses(&value)?,
        Err(std::env::VarError::NotPresent) => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let database = Arc::new(Database::open(database_path)?);
    database.migrate_legacy_json(automation_path, group_path)?;
    let mock_enabled = mock_devices.is_some();
    if let Some(devices) = mock_devices {
        database.remember_devices(&devices)?;
    }
    let automations = Arc::new(AutomationEngine::new(database.clone())?);
    let groups = Arc::new(GroupEngine::new(database.clone()));
    if mock_enabled && groups.groups()?.is_empty() {
        for (name, device_ids) in mock::groups() {
            groups.add(name, device_ids)?;
        }
    }
    let state = Arc::new(AppState {
        client: SmartHomeClient::new(),
        templates: templates()?,
        automations: automations.clone(),
        groups,
        database: database.clone(),
        device_addresses: device_addresses.clone(),
    });
    tokio::spawn(automations.run(state.client.clone(), device_addresses));
    tokio::spawn(purge_weather_history(database));
    let app = Router::new()
        .route("/", get(index))
        .route("/favicon.ico", get(favicon))
        .route("/manifest.webmanifest", get(web_manifest))
        .route("/service-worker.js", get(service_worker))
        .route("/offline.html", get(offline_page))
        .route("/assets/icons/{filename}", get(icon_asset))
        .route("/scan", post(scan))
        .route("/devices/{device_id}", axum::routing::delete(remove_device))
        .route("/groups", post(create_group))
        .route("/groups/new", get(new_group))
        .route(
            "/groups/{id}",
            get(edit_group).post(update_group).delete(delete_group),
        )
        .route("/groups/{id}/relay", post(set_group_relay))
        .route("/groups/{id}/automations", get(get_group_automations))
        .route(
            "/groups/{id}/automations/fixed",
            post(create_group_fixed_automation),
        )
        .route(
            "/groups/{id}/automations/solar",
            post(create_group_solar_automation),
        )
        .route(
            "/groups/{id}/automations/light",
            post(create_group_light_automation),
        )
        .route(
            "/groups/{id}/automations/{automation_id}",
            axum::routing::delete(delete_group_automation),
        )
        .route(
            "/groups/{id}/automations/{automation_id}/enabled",
            post(set_group_automation_enabled),
        )
        .route(
            "/groups/{id}/automations/{automation_id}/fixed",
            post(update_group_fixed_automation),
        )
        .route(
            "/groups/{id}/automations/{automation_id}/solar",
            post(update_group_solar_automation),
        )
        .route(
            "/groups/{id}/automations/{automation_id}/light",
            post(update_group_light_automation),
        )
        .route("/plugs/{address}/relay", post(set_relay))
        .route("/plugs/{address}/automations", get(get_automations))
        .route(
            "/plugs/{address}/automations/fixed",
            post(create_fixed_automation),
        )
        .route(
            "/plugs/{address}/automations/solar",
            post(create_solar_automation),
        )
        .route(
            "/plugs/{address}/automations/light",
            post(create_light_automation),
        )
        .route(
            "/plugs/{address}/automations/{id}",
            axum::routing::delete(delete_automation),
        )
        .route(
            "/plugs/{address}/automations/{id}/enabled",
            post(set_automation_enabled),
        )
        .route(
            "/plugs/{address}/automations/{id}/fixed",
            post(update_fixed_automation),
        )
        .route(
            "/plugs/{address}/automations/{id}/solar",
            post(update_solar_automation),
        )
        .route(
            "/plugs/{address}/automations/{id}/light",
            post(update_light_automation),
        )
        .route(
            "/plugs/{address}/automations/migrate",
            post(migrate_plug_schedules),
        )
        .route(
            "/plugs/{address}/countdown",
            get(get_countdown).post(create_countdown),
        )
        .route(
            "/plugs/{address}/countdown/{id}",
            axum::routing::delete(delete_countdown),
        )
        .with_state(state);

    let address: SocketAddr = std::env::var("BIND_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3000".to_owned())
        .parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    println!("Smart plug manager listening on http://{address}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    let view = load_device_list(&state, None)?;
    let page = state
        .templates
        .get_template("index.html")?
        .render(context! { groups => view.groups, plugs => view.plugs, notice => view.notice })?;
    Ok(Html(page))
}

async fn favicon() -> Response {
    static_response(
        "image/x-icon",
        include_bytes!("../assets/icons/favicon.ico"),
        true,
    )
}

async fn web_manifest() -> Response {
    static_response(
        "application/manifest+json",
        include_bytes!("../pwa/manifest.webmanifest"),
        false,
    )
}

async fn service_worker() -> Response {
    let mut response = static_response(
        "text/javascript; charset=utf-8",
        include_bytes!("../pwa/service-worker.js"),
        false,
    );
    response.headers_mut().insert(
        header::HeaderName::from_static("service-worker-allowed"),
        header::HeaderValue::from_static("/"),
    );
    response
}

async fn offline_page() -> Response {
    static_response(
        "text/html; charset=utf-8",
        include_bytes!("../pwa/offline.html"),
        false,
    )
}

async fn icon_asset(Path(filename): Path<String>) -> Response {
    let asset: Option<(&str, &[u8])> = match filename.as_str() {
        "icon.svg" => Some(("image/svg+xml", include_bytes!("../assets/icons/icon.svg"))),
        "icon-16.png" => Some(("image/png", include_bytes!("../assets/icons/icon-16.png"))),
        "icon-32.png" => Some(("image/png", include_bytes!("../assets/icons/icon-32.png"))),
        "icon-48.png" => Some(("image/png", include_bytes!("../assets/icons/icon-48.png"))),
        "icon-72.png" => Some(("image/png", include_bytes!("../assets/icons/icon-72.png"))),
        "icon-96.png" => Some(("image/png", include_bytes!("../assets/icons/icon-96.png"))),
        "icon-128.png" => Some(("image/png", include_bytes!("../assets/icons/icon-128.png"))),
        "icon-144.png" => Some(("image/png", include_bytes!("../assets/icons/icon-144.png"))),
        "icon-152.png" => Some(("image/png", include_bytes!("../assets/icons/icon-152.png"))),
        "icon-180.png" => Some(("image/png", include_bytes!("../assets/icons/icon-180.png"))),
        "icon-192.png" => Some(("image/png", include_bytes!("../assets/icons/icon-192.png"))),
        "icon-384.png" => Some(("image/png", include_bytes!("../assets/icons/icon-384.png"))),
        "icon-512.png" => Some(("image/png", include_bytes!("../assets/icons/icon-512.png"))),
        "icon-maskable-192.png" => Some((
            "image/png",
            include_bytes!("../assets/icons/icon-maskable-192.png"),
        )),
        "icon-maskable-512.png" => Some((
            "image/png",
            include_bytes!("../assets/icons/icon-maskable-512.png"),
        )),
        _ => None,
    };
    asset.map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |(content_type, body)| static_response(content_type, body, true),
    )
}

fn static_response(content_type: &str, body: &'static [u8], immutable: bool) -> Response {
    let cache_control = if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, cache_control)
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn scan(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    let plugs = scan_for_plugs(&state).await?;
    state
        .database
        .remember_devices(&plugs)
        .map_err(database_error)?;
    render_device_list(
        &state,
        load_device_list(
            &state,
            Some(format!(
                "Scan complete: {} device{} responded.",
                plugs.len(),
                if plugs.len() == 1 { "" } else { "s" }
            )),
        )?,
    )
}

async fn set_relay(
    State(state): State<Arc<AppState>>,
    Path(address): Path<IpAddr>,
    Form(form): Form<RelayForm>,
) -> Result<Html<String>, AppError> {
    let client = state.client.clone();
    let on = form.on;
    task::spawn_blocking(move || client.set_relay(address, on)).await??;
    state
        .database
        .update_relay(address, on)
        .map_err(database_error)?;
    render_device_list(&state, load_device_list(&state, None)?)
}

async fn remove_device(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Html<String>, AppError> {
    if !state
        .database
        .remove_device(&device_id)
        .map_err(database_error)?
    {
        return Err(AppError::not_found(format!(
            "device {device_id} was not found"
        )));
    }
    render_device_list(
        &state,
        load_device_list(&state, Some("Removed device from inventory.".to_owned()))?,
    )
}

async fn new_group(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    let plugs = remembered_plugs(&state)?;
    render_group_panel(&state, group_panel(None, &plugs))
}

async fn edit_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
) -> Result<Html<String>, AppError> {
    let group = find_group(&state, id)?;
    let plugs = remembered_plugs(&state)?;
    render_group_panel(&state, group_panel(Some(&group), &plugs))
}

async fn create_group(
    State(state): State<Arc<AppState>>,
    Form(form): Form<GroupForm>,
) -> Result<Html<String>, AppError> {
    let (name, device_ids) = form.into_parts();
    let plugs = remembered_plugs(&state)?;
    validate_group_members(
        &device_ids,
        plugs.iter().map(|plug| plug.device_id.as_str()),
    )?;
    state.groups.add(&name, device_ids).map_err(group_error)?;
    render_device_list(
        &state,
        device_list_view(
            &state,
            plugs,
            Some(format!("Created group “{}”.", name.trim())),
        )?,
    )
}

async fn update_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
    Form(form): Form<GroupForm>,
) -> Result<Html<String>, AppError> {
    let existing = find_group(&state, id)?;
    let (name, device_ids) = form.into_parts();
    let plugs = remembered_plugs(&state)?;
    validate_group_members(
        &device_ids,
        plugs
            .iter()
            .map(|plug| plug.device_id.as_str())
            .chain(existing.device_ids.iter().map(String::as_str)),
    )?;
    if !state
        .groups
        .update(id, &name, device_ids)
        .map_err(group_error)?
    {
        return Err(AppError::not_found(format!("group {id} was not found")));
    }
    render_device_list(
        &state,
        device_list_view(
            &state,
            plugs,
            Some(format!("Updated group “{}”.", name.trim())),
        )?,
    )
}

async fn delete_group(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
) -> Result<Html<String>, AppError> {
    let group = find_group(&state, id)?;
    if !state.groups.delete(id).map_err(group_error)? {
        return Err(AppError::not_found(format!("group {id} was not found")));
    }
    let view = load_device_list(&state, Some(format!("Deleted group “{}”.", group.name)))?;
    render_device_list(&state, view)
}

async fn set_group_relay(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
    Form(form): Form<RelayForm>,
) -> Result<Html<String>, AppError> {
    let group = find_group(&state, id)?;
    let members: HashSet<_> = group.device_ids.iter().map(String::as_str).collect();
    let plugs = remembered_plugs(&state)?;
    let reachable: Vec<_> = plugs
        .iter()
        .filter(|plug| members.contains(plug.device_id.as_str()))
        .map(|plug| (plug.device_id.clone(), plug.address))
        .collect();
    let reachable_count = reachable.len();
    let mut tasks = tokio::task::JoinSet::new();
    for (device_id, address) in reachable {
        let client = state.client.clone();
        let on = form.on;
        tasks.spawn_blocking(move || (device_id, client.set_relay(address, on)));
    }

    let mut controlled = HashSet::new();
    let mut failed = 0;
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok((device_id, Ok(()))) => {
                controlled.insert(device_id);
            }
            Ok((_, Err(error))) => {
                failed += 1;
                eprintln!("group relay operation failed: {error}");
            }
            Err(error) => {
                failed += 1;
                eprintln!("group relay task failed: {error}");
            }
        }
    }
    for plug in plugs
        .iter()
        .filter(|plug| controlled.contains(&plug.device_id))
    {
        state
            .database
            .update_relay(plug.address, form.on)
            .map_err(database_error)?;
    }

    let offline = group.device_ids.len().saturating_sub(reachable_count);
    let unavailable = offline + failed;
    let notice = group_relay_notice(&group.name, form.on, controlled.len(), unavailable);
    render_device_list(&state, load_device_list(&state, Some(notice))?)
}

async fn get_group_automations(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
) -> Result<Html<String>, AppError> {
    render_group_automation_panel(&state, id).await
}

async fn create_group_fixed_automation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
    Form(form): Form<ScheduleForm>,
) -> Result<Html<String>, AppError> {
    require_group_location(&state, id)?;
    state
        .automations
        .add(fixed_automation(form, automation_target(id))?)
        .map_err(automation_error)?;
    render_group_automation_panel(&state, id).await
}

async fn create_group_solar_automation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
    Form(form): Form<SolarAutomationForm>,
) -> Result<Html<String>, AppError> {
    require_group_location(&state, id)?;
    state
        .automations
        .add(solar_automation(form, automation_target(id))?)
        .map_err(automation_error)?;
    render_group_automation_panel(&state, id).await
}

async fn create_group_light_automation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
    Form(form): Form<LightAutomationForm>,
) -> Result<Html<String>, AppError> {
    require_group_location(&state, id)?;
    state
        .automations
        .add(light_automation(form, automation_target(id))?)
        .map_err(automation_error)?;
    render_group_automation_panel(&state, id).await
}

async fn update_group_fixed_automation(
    State(state): State<Arc<AppState>>,
    Path((id, automation_id)): Path<(u64, u64)>,
    Form(form): Form<ScheduleForm>,
) -> Result<Html<String>, AppError> {
    let automation = fixed_automation(form, automation_target(id))?;
    update_group_automation(&state, id, automation_id, automation).await
}

async fn update_group_solar_automation(
    State(state): State<Arc<AppState>>,
    Path((id, automation_id)): Path<(u64, u64)>,
    Form(form): Form<SolarAutomationForm>,
) -> Result<Html<String>, AppError> {
    let automation = solar_automation(form, automation_target(id))?;
    update_group_automation(&state, id, automation_id, automation).await
}

async fn update_group_light_automation(
    State(state): State<Arc<AppState>>,
    Path((id, automation_id)): Path<(u64, u64)>,
    Form(form): Form<LightAutomationForm>,
) -> Result<Html<String>, AppError> {
    let automation = light_automation(form, automation_target(id))?;
    update_group_automation(&state, id, automation_id, automation).await
}

async fn update_group_automation(
    state: &AppState,
    id: u64,
    automation_id: u64,
    automation: NewAutomation,
) -> Result<Html<String>, AppError> {
    find_group(state, id)?;
    if !state
        .automations
        .update(&automation_target(id), automation_id, automation)
        .map_err(automation_error)?
    {
        return Err(AppError::not_found(format!(
            "automation {automation_id} was not found"
        )));
    }
    render_group_automation_panel(state, id).await
}

async fn set_group_automation_enabled(
    State(state): State<Arc<AppState>>,
    Path((id, automation_id)): Path<(u64, u64)>,
    Form(form): Form<RelayForm>,
) -> Result<Html<String>, AppError> {
    find_group(&state, id)?;
    if !state
        .automations
        .set_enabled(&automation_target(id), automation_id, form.on)
        .map_err(automation_error)?
    {
        return Err(AppError::not_found(format!(
            "automation {automation_id} was not found"
        )));
    }
    render_group_automation_panel(&state, id).await
}

async fn delete_group_automation(
    State(state): State<Arc<AppState>>,
    Path((id, automation_id)): Path<(u64, u64)>,
) -> Result<Html<String>, AppError> {
    find_group(&state, id)?;
    if !state
        .automations
        .delete(&automation_target(id), automation_id)
        .map_err(automation_error)?
    {
        return Err(AppError::not_found(format!(
            "automation {automation_id} was not found"
        )));
    }
    render_group_automation_panel(&state, id).await
}

async fn get_automations(
    State(state): State<Arc<AppState>>,
    Path(address): Path<IpAddr>,
) -> Result<Html<String>, AppError> {
    let plug = get_plug(state.client.clone(), address).await?;
    let panel = load_automation_panel(&state, &plug).await?;
    render_automation_panel(&state, &panel)
}

async fn create_fixed_automation(
    State(state): State<Arc<AppState>>,
    Path(address): Path<IpAddr>,
    Form(form): Form<ScheduleForm>,
) -> Result<Html<String>, AppError> {
    let plug = get_plug(state.client.clone(), address).await?;
    require_location(&plug)?;
    let automation = fixed_automation(form, plug.device_id.clone())?;
    state
        .automations
        .add(automation)
        .map_err(automation_error)?;
    let panel = load_automation_panel(&state, &plug).await?;
    render_automation_panel(&state, &panel)
}

async fn create_solar_automation(
    State(state): State<Arc<AppState>>,
    Path(address): Path<IpAddr>,
    Form(form): Form<SolarAutomationForm>,
) -> Result<Html<String>, AppError> {
    let plug = get_plug(state.client.clone(), address).await?;
    require_location(&plug)?;
    let automation = solar_automation(form, plug.device_id.clone())?;
    state
        .automations
        .add(automation)
        .map_err(automation_error)?;
    let panel = load_automation_panel(&state, &plug).await?;
    render_automation_panel(&state, &panel)
}

async fn create_light_automation(
    State(state): State<Arc<AppState>>,
    Path(address): Path<IpAddr>,
    Form(form): Form<LightAutomationForm>,
) -> Result<Html<String>, AppError> {
    let plug = get_plug(state.client.clone(), address).await?;
    require_location(&plug)?;
    let automation = light_automation(form, plug.device_id.clone())?;
    state
        .automations
        .add(automation)
        .map_err(automation_error)?;
    let panel = load_automation_panel(&state, &plug).await?;
    render_automation_panel(&state, &panel)
}

async fn update_fixed_automation(
    State(state): State<Arc<AppState>>,
    Path((address, id)): Path<(IpAddr, u64)>,
    Form(form): Form<ScheduleForm>,
) -> Result<Html<String>, AppError> {
    let plug = get_plug(state.client.clone(), address).await?;
    let automation = fixed_automation(form, plug.device_id.clone())?;
    update_automation(&state, &plug, id, automation).await
}

async fn update_solar_automation(
    State(state): State<Arc<AppState>>,
    Path((address, id)): Path<(IpAddr, u64)>,
    Form(form): Form<SolarAutomationForm>,
) -> Result<Html<String>, AppError> {
    let plug = get_plug(state.client.clone(), address).await?;
    let automation = solar_automation(form, plug.device_id.clone())?;
    update_automation(&state, &plug, id, automation).await
}

async fn update_light_automation(
    State(state): State<Arc<AppState>>,
    Path((address, id)): Path<(IpAddr, u64)>,
    Form(form): Form<LightAutomationForm>,
) -> Result<Html<String>, AppError> {
    let plug = get_plug(state.client.clone(), address).await?;
    let automation = light_automation(form, plug.device_id.clone())?;
    update_automation(&state, &plug, id, automation).await
}

async fn update_automation(
    state: &AppState,
    plug: &SmartPlug,
    id: u64,
    automation: NewAutomation,
) -> Result<Html<String>, AppError> {
    let updated = state
        .automations
        .update(&plug.device_id, id, automation)
        .map_err(automation_error)?;
    if !updated {
        return Err(AppError::not_found(format!(
            "automation {id} was not found"
        )));
    }
    let panel = load_automation_panel(state, plug).await?;
    render_automation_panel(state, &panel)
}

async fn set_automation_enabled(
    State(state): State<Arc<AppState>>,
    Path((address, id)): Path<(IpAddr, u64)>,
    Form(form): Form<RelayForm>,
) -> Result<Html<String>, AppError> {
    let plug = get_plug(state.client.clone(), address).await?;
    let updated = state
        .automations
        .set_enabled(&plug.device_id, id, form.on)
        .map_err(automation_error)?;
    if !updated {
        return Err(AppError::not_found(format!(
            "automation {id} was not found"
        )));
    }
    let panel = load_automation_panel(&state, &plug).await?;
    render_automation_panel(&state, &panel)
}

async fn migrate_plug_schedules(
    State(state): State<Arc<AppState>>,
    Path(address): Path<IpAddr>,
) -> Result<Html<String>, AppError> {
    let plug = get_plug(state.client.clone(), address).await?;
    require_location(&plug)?;
    let client = state.client.clone();
    let automations = state.automations.clone();
    let device_id = plug.device_id.clone();
    task::spawn_blocking(move || -> Result<(), AppError> {
        let schedules = client.get_schedule_rules(address)?;
        for rule in &schedules.rules {
            let Some(automation) = migrated_automation(rule, schedules.enabled, &device_id) else {
                continue;
            };
            let Some(plug_rule_id) = rule.id.as_deref() else {
                continue;
            };
            let server_rule_id = automations.add(automation).map_err(automation_error)?;
            if let Err(error) = client.delete_schedule_rule(address, plug_rule_id) {
                if let Err(rollback_error) = automations.delete(&device_id, server_rule_id) {
                    return Err(AppError::internal(format!(
                        "could not remove plug schedule ({error}) or roll back server schedule ({rollback_error})"
                    )));
                }
                return Err(error.into());
            }
        }
        Ok(())
    })
    .await??;

    let panel = load_automation_panel(&state, &plug).await?;
    render_automation_panel(&state, &panel)
}

async fn delete_automation(
    State(state): State<Arc<AppState>>,
    Path((address, id)): Path<(IpAddr, u64)>,
) -> Result<Html<String>, AppError> {
    let plug = get_plug(state.client.clone(), address).await?;
    let deleted = state
        .automations
        .delete(&plug.device_id, id)
        .map_err(automation_error)?;
    if !deleted {
        return Err(AppError::not_found(format!(
            "automation {id} was not found"
        )));
    }
    let panel = load_automation_panel(&state, &plug).await?;
    render_automation_panel(&state, &panel)
}

async fn get_countdown(
    State(state): State<Arc<AppState>>,
    Path(address): Path<IpAddr>,
) -> Result<Html<String>, AppError> {
    let client = state.client.clone();
    let panel = task::spawn_blocking(move || load_countdown_panel(&client, address)).await??;
    render_countdown_panel(&state, &panel)
}

async fn create_countdown(
    State(state): State<Arc<AppState>>,
    Path(address): Path<IpAddr>,
    Form(form): Form<CountdownForm>,
) -> Result<Html<String>, AppError> {
    let input = CountdownInput::try_from(form)?;
    let client = state.client.clone();
    let panel = task::spawn_blocking(move || {
        client.add_countdown_rule(address, &input.rule())?;
        load_countdown_panel(&client, address)
    })
    .await??;
    render_countdown_panel(&state, &panel)
}

async fn delete_countdown(
    State(state): State<Arc<AppState>>,
    Path((address, id)): Path<(IpAddr, String)>,
) -> Result<Html<String>, AppError> {
    let client = state.client.clone();
    let panel = task::spawn_blocking(move || {
        client.delete_countdown_rule(address, &id)?;
        load_countdown_panel(&client, address)
    })
    .await??;
    render_countdown_panel(&state, &panel)
}

async fn scan_for_plugs(state: &AppState) -> Result<Vec<SmartPlug>, AppError> {
    let client = state.client.clone();
    let device_addresses = state.device_addresses.clone();
    Ok(task::spawn_blocking(move || {
        client.get_inventory_from(&device_addresses, DISCOVERY_TIMEOUT)
    })
    .await??)
}

fn remembered_plugs(state: &AppState) -> Result<Vec<SmartPlug>, AppError> {
    state.database.devices().map_err(database_error)
}

fn load_device_list(state: &AppState, notice: Option<String>) -> Result<DeviceListView, AppError> {
    let plugs = remembered_plugs(state)?;
    device_list_view(state, plugs, notice)
}

fn device_list_view(
    state: &AppState,
    plugs: Vec<SmartPlug>,
    notice: Option<String>,
) -> Result<DeviceListView, AppError> {
    let groups = state.groups.groups().map_err(group_error)?;
    let group_views = groups
        .into_iter()
        .map(|group| group_view(group, &plugs))
        .collect();
    Ok(DeviceListView {
        groups: group_views,
        plugs: plugs.into_iter().map(PlugView::from).collect(),
        notice,
    })
}

fn group_view(group: DeviceGroup, plugs: &[SmartPlug]) -> GroupView {
    let inventory: HashMap<_, _> = plugs
        .iter()
        .map(|plug| (plug.device_id.as_str(), plug))
        .collect();
    let reachable: Vec<_> = group
        .device_ids
        .iter()
        .filter_map(|device_id| inventory.get(device_id.as_str()).copied())
        .collect();
    let reachable_count = reachable.len();
    let (state, state_class) = if reachable.is_empty() {
        ("Unavailable", "state-unavailable")
    } else if reachable.iter().all(|plug| plug.relay_on) {
        ("On", "state-on")
    } else if reachable.iter().all(|plug| !plug.relay_on) {
        ("Off", "state-off")
    } else {
        ("Mixed", "state-mixed")
    };
    let members = group
        .device_ids
        .iter()
        .map(|device_id| {
            inventory
                .get(device_id.as_str())
                .map_or_else(|| short_device_id(device_id), |plug| plug.alias.clone())
        })
        .collect::<Vec<_>>()
        .join(", ");
    GroupView {
        id: group.id,
        name: group.name,
        member_count: group.device_ids.len(),
        reachable_count,
        members,
        state,
        state_class,
        has_offline_members: reachable_count < group.device_ids.len(),
    }
}

fn group_panel(group: Option<&DeviceGroup>, plugs: &[SmartPlug]) -> GroupPanel {
    let selected: HashSet<_> = group
        .into_iter()
        .flat_map(|group| group.device_ids.iter().map(String::as_str))
        .collect();
    let available: HashSet<_> = plugs.iter().map(|plug| plug.device_id.as_str()).collect();
    let mut devices: Vec<_> = plugs
        .iter()
        .map(|plug| GroupDeviceOption {
            field_name: String::new(),
            device_id: plug.device_id.clone(),
            alias: plug.alias.clone(),
            available: true,
            selected: selected.contains(plug.device_id.as_str()),
        })
        .collect();
    if let Some(group) = group {
        devices.extend(
            group
                .device_ids
                .iter()
                .filter(|device_id| !available.contains(device_id.as_str()))
                .map(|device_id| GroupDeviceOption {
                    field_name: String::new(),
                    device_id: device_id.clone(),
                    alias: short_device_id(device_id),
                    available: false,
                    selected: true,
                }),
        );
    }
    for (index, device) in devices.iter_mut().enumerate() {
        device.field_name = format!("device_{index}");
    }
    GroupPanel {
        id: group.map(|group| group.id),
        name: group.map_or_else(String::new, |group| group.name.clone()),
        devices,
        editing: group.is_some(),
    }
}

fn validate_group_members<'a>(
    device_ids: &[String],
    allowed: impl IntoIterator<Item = &'a str>,
) -> Result<(), AppError> {
    let allowed: HashSet<_> = allowed.into_iter().collect();
    if let Some(device_id) = device_ids
        .iter()
        .find(|device_id| !allowed.contains(device_id.as_str()))
    {
        return Err(AppError::bad_request(format!(
            "device {device_id} is not available for this group"
        )));
    }
    Ok(())
}

fn find_group(state: &AppState, id: u64) -> Result<DeviceGroup, AppError> {
    state
        .groups
        .get(id)
        .map_err(group_error)?
        .ok_or_else(|| AppError::not_found(format!("group {id} was not found")))
}

fn group_relay_notice(name: &str, on: bool, controlled: usize, unavailable: usize) -> String {
    if controlled == 0 {
        return format!("No members of “{name}” could be controlled.");
    }
    let action = if on { "Turned on" } else { "Turned off" };
    let noun = if controlled == 1 { "device" } else { "devices" };
    let mut notice = format!("{action} {controlled} {noun} in “{name}”.");
    if unavailable > 0 {
        let noun = if unavailable == 1 {
            "member was"
        } else {
            "members were"
        };
        notice.push_str(&format!(" {unavailable} {noun} unavailable."));
    }
    notice
}

fn short_device_id(device_id: &str) -> String {
    let suffix: String = device_id
        .chars()
        .rev()
        .take(8)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("Unavailable …{suffix}")
}

fn parse_device_addresses(value: &str) -> Result<Vec<IpAddr>, std::net::AddrParseError> {
    let mut addresses = value
        .split(',')
        .map(str::trim)
        .filter(|address| !address.is_empty())
        .map(str::parse)
        .collect::<Result<Vec<_>, _>>()?;
    addresses.sort_unstable();
    addresses.dedup();
    Ok(addresses)
}

async fn get_plug(client: SmartHomeClient, address: IpAddr) -> Result<SmartPlug, AppError> {
    Ok(task::spawn_blocking(move || client.get_sysinfo(address)).await??)
}

fn solar_automation(
    form: SolarAutomationForm,
    device_id: String,
) -> Result<NewAutomation, AppError> {
    let name = validated_schedule_name(&form.name)?;
    if !(-180..=180).contains(&form.offset_minutes) {
        return Err(AppError::bad_request(
            "solar offset must be between -180 and 180 minutes",
        ));
    }
    let event = match form.event.as_str() {
        "sunrise" => SolarEvent::Sunrise,
        "sunset" => SolarEvent::Sunset,
        _ => {
            return Err(AppError::bad_request(
                "solar event must be sunrise or sunset",
            ))
        }
    };
    let turn_on = parse_action(&form.action)?;
    let weekdays = schedule_weekdays([
        form.sun, form.mon, form.tue, form.wed, form.thu, form.fri, form.sat,
    ])?;
    Ok(NewAutomation {
        device_id,
        name,
        enabled: true,
        trigger: AutomationTrigger::Solar {
            event,
            offset_minutes: form.offset_minutes,
            weekdays,
        },
        turn_on,
    })
}

fn fixed_automation(form: ScheduleForm, device_id: String) -> Result<NewAutomation, AppError> {
    let name = validated_schedule_name(&form.name)?;
    let minute_of_day = parse_schedule_time(&form.time)?;
    let turn_on = parse_action(&form.action)?;
    let weekdays = schedule_weekdays([
        form.sun, form.mon, form.tue, form.wed, form.thu, form.fri, form.sat,
    ])?;
    Ok(NewAutomation {
        device_id,
        name,
        enabled: true,
        trigger: AutomationTrigger::FixedTime {
            minute_of_day,
            weekdays,
        },
        turn_on,
    })
}

fn light_automation(
    form: LightAutomationForm,
    device_id: String,
) -> Result<NewAutomation, AppError> {
    if !form.on_below.is_finite()
        || !form.off_above.is_finite()
        || form.on_below < 0.0
        || form.off_above > 1_500.0
        || form.on_below >= form.off_above
    {
        return Err(AppError::bad_request(
            "light thresholds must satisfy 0 ≤ on below < off above ≤ 1500 W/m²",
        ));
    }
    let name = validated_schedule_name(&form.name)?;
    let active_window = form
        .window_enabled
        .is_some()
        .then(|| {
            Ok(ActiveWindow {
                start: parse_time_boundary(
                    &form.start_kind,
                    &form.start_time,
                    form.start_offset_minutes,
                )?,
                end: parse_time_boundary(&form.end_kind, &form.end_time, form.end_offset_minutes)?,
                outside: match form.outside_window.as_str() {
                    "turn_off" => OutsideWindowBehavior::TurnOff,
                    "stop_controlling" => OutsideWindowBehavior::StopControlling,
                    _ => {
                        return Err(AppError::bad_request(
                            "outside-window behavior must be turn off or stop controlling",
                        ))
                    }
                },
            })
        })
        .transpose()?;
    Ok(NewAutomation {
        device_id,
        name,
        enabled: true,
        trigger: AutomationTrigger::LightLevel {
            on_below: form.on_below,
            off_above: form.off_above,
            active_window,
        },
        turn_on: true,
    })
}

fn parse_time_boundary(
    kind: &str,
    time: &str,
    offset_minutes: i16,
) -> Result<TimeBoundary, AppError> {
    match kind {
        "fixed" => Ok(TimeBoundary::Fixed {
            minute_of_day: parse_schedule_time(time)?,
        }),
        "sunrise" | "sunset" => {
            if !(-180..=180).contains(&offset_minutes) {
                return Err(AppError::bad_request(
                    "solar offset must be between -180 and 180 minutes",
                ));
            }
            Ok(TimeBoundary::Solar {
                event: if kind == "sunrise" {
                    SolarEvent::Sunrise
                } else {
                    SolarEvent::Sunset
                },
                offset_minutes,
            })
        }
        _ => Err(AppError::bad_request(
            "window boundary must be a fixed time, sunrise, or sunset",
        )),
    }
}

fn parse_action(action: &str) -> Result<bool, AppError> {
    match action {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(AppError::bad_request("automation action must be on or off")),
    }
}

fn require_location(plug: &SmartPlug) -> Result<(), AppError> {
    if plug.latitude.is_none() || plug.longitude.is_none() {
        return Err(AppError::bad_request(
            "this plug does not have location coordinates configured",
        ));
    }
    Ok(())
}

fn automation_error(error: Box<dyn StdError + Send + Sync>) -> AppError {
    AppError::internal(error.to_string())
}

fn database_error(error: Box<dyn StdError + Send + Sync>) -> AppError {
    AppError::internal(error.to_string())
}

fn group_error(error: Box<dyn StdError + Send + Sync>) -> AppError {
    if error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::InvalidInput)
    {
        AppError::bad_request(error.to_string())
    } else {
        AppError::internal(error.to_string())
    }
}

async fn purge_weather_history(database: Arc<Database>) {
    let mut interval = tokio::time::interval(WEATHER_PURGE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let cutoff = match database::unix_timestamp().and_then(|now| {
            i64::try_from(WEATHER_HISTORY_RETENTION.as_secs())
                .map(|retention| now - retention)
                .map_err(Into::into)
        }) {
            Ok(cutoff) => cutoff,
            Err(error) => {
                eprintln!("could not calculate Open-Meteo history retention: {error}");
                continue;
            }
        };
        match database.purge_weather_history(cutoff) {
            Ok(removed) if removed > 0 => {
                println!("Purged {removed} expired Open-Meteo observations");
            }
            Ok(_) => {}
            Err(error) => eprintln!("could not purge Open-Meteo history: {error}"),
        }
    }
}

impl TryFrom<CountdownForm> for CountdownInput {
    type Error = AppError;

    fn try_from(form: CountdownForm) -> Result<Self, Self::Error> {
        if !(1..=1_440).contains(&form.minutes) {
            return Err(AppError::bad_request(
                "timer duration must be between 1 minute and 24 hours",
            ));
        }
        let turn_on = match form.action.as_str() {
            "on" => true,
            "off" => false,
            _ => return Err(AppError::bad_request("timer action must be on or off")),
        };
        Ok(Self {
            delay_seconds: form.minutes * 60,
            turn_on,
        })
    }
}

impl CountdownInput {
    fn rule(&self) -> CountdownRule {
        CountdownRule {
            id: None,
            name: "Web timer".to_owned(),
            enabled: true,
            delay: self.delay_seconds,
            turn_on: self.turn_on,
            extra: Default::default(),
        }
    }
}

fn validated_schedule_name(value: &str) -> Result<String, AppError> {
    let name = value.trim().to_owned();
    if name.is_empty() {
        return Err(AppError::bad_request("schedule name cannot be empty"));
    }
    if name.chars().count() > 64 {
        return Err(AppError::bad_request(
            "schedule name cannot exceed 64 characters",
        ));
    }
    Ok(name)
}

fn parse_schedule_time(value: &str) -> Result<u16, AppError> {
    let (hour, minute) = value
        .split_once(':')
        .ok_or_else(|| AppError::bad_request("schedule time must use HH:MM format"))?;
    if hour.len() != 2
        || minute.len() != 2
        || !hour.bytes().all(|character| character.is_ascii_digit())
        || !minute.bytes().all(|character| character.is_ascii_digit())
    {
        return Err(AppError::bad_request("schedule time must use HH:MM format"));
    }
    let hour: u16 = hour
        .parse()
        .map_err(|_| AppError::bad_request("schedule time must use HH:MM format"))?;
    let minute: u16 = minute
        .parse()
        .map_err(|_| AppError::bad_request("schedule time must use HH:MM format"))?;
    if hour > 23 || minute > 59 {
        return Err(AppError::bad_request(
            "schedule time must be between 00:00 and 23:59",
        ));
    }
    Ok(hour * 60 + minute)
}

fn schedule_weekdays(values: [Option<String>; 7]) -> Result<[bool; 7], AppError> {
    let weekdays = values.map(|value| value.is_some());
    if !weekdays.iter().any(|selected| *selected) {
        return Err(AppError::bad_request(
            "select at least one weekday for the schedule",
        ));
    }
    Ok(weekdays)
}

async fn load_automation_panel(
    state: &AppState,
    plug: &SmartPlug,
) -> Result<AutomationPanel, AppError> {
    let rules = state
        .automations
        .rules_for(&plug.device_id)
        .map_err(automation_error)?;
    let location_available = plug.latitude.is_some() && plug.longitude.is_some();
    let weather_request = async {
        if !location_available {
            return None;
        }
        match state.automations.weather_status(plug).await {
            Ok(weather) => Some(weather),
            Err(error) => {
                eprintln!("could not load current weather: {error}");
                None
            }
        }
    };
    let schedule_client = state.client.clone();
    let address = plug.address;
    let schedule_request =
        task::spawn_blocking(move || load_schedule_panel(&schedule_client, address));
    let (weather, schedules) = tokio::join!(weather_request, schedule_request);
    let schedules = match schedules? {
        Ok(schedules) => schedules,
        Err(error) => {
            eprintln!("could not load schedules stored on the plug: {error}");
            SchedulePanel {
                migratable_count: 0,
                unsupported_count: 0,
                rules: Vec::new(),
            }
        }
    };
    let calendar = weather
        .as_ref()
        .map(|weather| week_calendar_view(&rules, weather));
    Ok(AutomationPanel {
        address: plug.address.to_string(),
        automation_base: format!("/plugs/{}/automations", plug.address),
        location_available,
        weather,
        calendar,
        rules: rules.into_iter().map(automation_view).collect(),
        schedules,
    })
}

fn group_location_plug(
    state: &AppState,
    group: &DeviceGroup,
) -> Result<Option<SmartPlug>, AppError> {
    let inventory: HashMap<_, _> = remembered_plugs(state)?
        .into_iter()
        .map(|plug| (plug.device_id.clone(), plug))
        .collect();
    let first_available = group
        .device_ids
        .iter()
        .find_map(|device_id| inventory.get(device_id));
    Ok(group
        .device_ids
        .iter()
        .filter_map(|device_id| inventory.get(device_id))
        .find(|plug| plug.latitude.is_some() && plug.longitude.is_some())
        .or(first_available)
        .cloned())
}

fn require_group_location(state: &AppState, id: u64) -> Result<SmartPlug, AppError> {
    let group = find_group(state, id)?;
    let plug = group_location_plug(state, &group)?.ok_or_else(|| {
        AppError::bad_request("the group has no remembered members with a location")
    })?;
    require_location(&plug)?;
    Ok(plug)
}

async fn render_group_automation_panel(
    state: &AppState,
    id: u64,
) -> Result<Html<String>, AppError> {
    let group = find_group(state, id)?;
    let target = automation_target(id);
    let rules = state
        .automations
        .rules_for(&target)
        .map_err(automation_error)?;
    let plug = group_location_plug(state, &group)?;
    let location_available = plug
        .as_ref()
        .is_some_and(|plug| plug.latitude.is_some() && plug.longitude.is_some());
    let weather = match plug.as_ref() {
        Some(plug) if location_available => match state.automations.weather_status(plug).await {
            Ok(weather) => Some(weather),
            Err(error) => {
                eprintln!("could not load current weather: {error}");
                None
            }
        },
        _ => None,
    };
    let calendar = weather
        .as_ref()
        .map(|weather| week_calendar_view(&rules, weather));
    render_automation_panel(
        state,
        &AutomationPanel {
            address: group.name,
            automation_base: format!("/groups/{id}/automations"),
            location_available,
            weather,
            calendar,
            rules: rules.into_iter().map(automation_view).collect(),
            schedules: SchedulePanel {
                migratable_count: 0,
                unsupported_count: 0,
                rules: Vec::new(),
            },
        },
    )
}

fn week_calendar_view(rules: &[AutomationRule], weather: &WeatherStatus) -> WeekCalendarView {
    const DAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let current_weekday = weekday_index(weather.current_day);
    let monday = weather.current_day - ((current_weekday + 6) % 7) as i64;
    let solar_days: HashMap<_, _> = weather
        .solar_days
        .iter()
        .map(|day| (day.day, *day))
        .collect();
    let mut days: Vec<_> = DAY_NAMES
        .iter()
        .copied()
        .enumerate()
        .map(|(day_offset, name)| {
            let day = monday + day_offset as i64;
            CalendarDayView {
                index: day_offset,
                name,
                date_label: calendar_date(day),
                is_today: day == weather.current_day,
                lane_count: 1,
                entries: Vec::new(),
            }
        })
        .collect();

    let timed_events = enabled_timed_events(rules, monday, &solar_days);
    push_scheduled_state_spans(&mut days, monday, &timed_events);

    for (day_offset, day_view) in days.iter_mut().enumerate() {
        let day = monday + day_offset as i64;
        let solar = solar_days.get(&day).copied();
        for rule in rules {
            if let Some((minute, kind)) = calendar_timed_event(rule, day, solar) {
                let events = timed_events.get(&(day * 24 * 60 + i64::from(minute)));
                let collision_index = events
                    .and_then(|events| events.iter().position(|event| event.rule.id == rule.id));
                let conflict = collision_index
                    .is_some_and(|_| rule.enabled && events.is_some_and(|events| events.len() > 1));
                let winner = conflict
                    && events
                        .and_then(|events| winning_timed_event(events))
                        .is_some_and(|event| event.rule.id == rule.id);
                push_calendar_point(
                    &mut day_view.entries,
                    rule,
                    minute,
                    kind,
                    conflict,
                    winner,
                    collision_index.unwrap_or(0),
                );
            }
        }
        let mut lane_count = usize::from(!day_view.entries.is_empty());
        for rule in rules {
            let AutomationTrigger::LightLevel { active_window, .. } = rule.trigger else {
                continue;
            };
            let Some((start, end)) = calendar_window(active_window, solar) else {
                continue;
            };
            let entry_count = day_view.entries.len();
            if start < end {
                push_calendar_window(&mut day_view.entries, rule, start, end, lane_count);
            } else if start > end {
                push_calendar_window(&mut day_view.entries, rule, start, 24 * 60, lane_count);
                push_calendar_window(&mut day_view.entries, rule, 0, end, lane_count);
            }
            if day_view.entries.len() > entry_count {
                lane_count += 1;
            }
        }
        day_view.lane_count = lane_count.max(1);
    }
    let has_entries = days.iter().any(|day| !day.entries.is_empty());
    let current_at = weather.current_day * 24 * 60 + i64::from(weather.current_minute);
    WeekCalendarView {
        timezone: weather.timezone.clone(),
        week_label: calendar_week_label(monday),
        current_left: calendar_percent(weather.current_minute),
        summary: calendar_summary(current_at, &timed_events),
        has_entries,
        conflicts: calendar_conflicts(monday, &timed_events),
        days,
    }
}

#[derive(Clone, Copy)]
struct TimedCalendarEvent<'a> {
    rule: &'a AutomationRule,
}

fn enabled_timed_events<'a>(
    rules: &'a [AutomationRule],
    monday: i64,
    solar_days: &HashMap<i64, SolarForecastDay>,
) -> BTreeMap<i64, Vec<TimedCalendarEvent<'a>>> {
    let mut events: BTreeMap<i64, Vec<TimedCalendarEvent<'a>>> = BTreeMap::new();
    for day in monday - 7..monday + 14 {
        let solar = solar_days.get(&day).copied();
        for rule in rules.iter().filter(|rule| rule.enabled) {
            let Some((minute, _)) = calendar_timed_event(rule, day, solar) else {
                continue;
            };
            let at = day * 24 * 60 + i64::from(minute);
            events
                .entry(at)
                .or_default()
                .push(TimedCalendarEvent { rule });
        }
    }
    for events in events.values_mut() {
        events.sort_unstable_by_key(|event| event.rule.id);
    }
    events
}

fn winning_timed_event<'a>(events: &[TimedCalendarEvent<'a>]) -> Option<TimedCalendarEvent<'a>> {
    events.last().copied()
}

fn calendar_timed_event(
    rule: &AutomationRule,
    day: i64,
    solar: Option<SolarForecastDay>,
) -> Option<(u16, &'static str)> {
    let weekday = weekday_index(day);
    match rule.trigger {
        AutomationTrigger::FixedTime {
            minute_of_day,
            weekdays,
        } if weekdays[weekday] => Some((minute_of_day, "fixed")),
        AutomationTrigger::Solar {
            event,
            offset_minutes,
            weekdays,
        } if weekdays[weekday] => {
            Some((solar_event_minute(event, offset_minutes, solar?), "solar"))
        }
        _ => None,
    }
}

fn push_scheduled_state_spans(
    days: &mut [CalendarDayView],
    monday: i64,
    events: &BTreeMap<i64, Vec<TimedCalendarEvent<'_>>>,
) {
    let week_start = monday * 24 * 60;
    let week_end = (monday + 7) * 24 * 60;
    let Some((_, initial_events)) = events.range(..week_start).next_back() else {
        return;
    };
    let Some(initial_event) = winning_timed_event(initial_events) else {
        return;
    };
    let mut state_start = week_start;
    let mut state_rule = initial_event.rule;
    for (&at, events) in events.range(week_start..week_end) {
        let Some(event) = winning_timed_event(events) else {
            continue;
        };
        if event.rule.turn_on != state_rule.turn_on {
            push_scheduled_state_span(days, monday, state_start, at, state_rule);
            state_start = at;
            state_rule = event.rule;
        }
    }
    push_scheduled_state_span(days, monday, state_start, week_end, state_rule);
}

fn push_scheduled_state_span(
    days: &mut [CalendarDayView],
    monday: i64,
    start: i64,
    end: i64,
    state_rule: &AutomationRule,
) {
    let week_start = monday * 24 * 60;
    let week_end = (monday + 7) * 24 * 60;
    let mut segment_start = start.clamp(week_start, week_end);
    let end = end.clamp(week_start, week_end);
    if segment_start >= end {
        return;
    }
    let state = if state_rule.turn_on { "on" } else { "off" };
    let detail = if start <= week_start && end >= week_end {
        format!(
            "Scheduled {state} all week · carried over from {}",
            state_rule.name
        )
    } else {
        format!(
            "Scheduled {state} {}–{} · set by {}",
            calendar_absolute_time(start),
            calendar_absolute_time(end),
            state_rule.name
        )
    };
    while segment_start < end {
        let day = segment_start.div_euclid(24 * 60);
        let segment_end = end.min((day + 1) * 24 * 60);
        let start_minute = segment_start.rem_euclid(24 * 60) as u16;
        let end_minute = if segment_end == (day + 1) * 24 * 60 {
            24 * 60
        } else {
            segment_end.rem_euclid(24 * 60) as u16
        };
        let day_index = (day - monday) as usize;
        days[day_index].entries.push(CalendarEntryView {
            rule_id: state_rule.id,
            name: state_rule.name.clone(),
            label: state.to_uppercase(),
            detail: detail.clone(),
            enabled: true,
            class: format!("calendar-entry scheduled-{state}"),
            left: calendar_percent(start_minute),
            width: calendar_percent(end_minute - start_minute),
            lane: 0,
            point: false,
            conflict: false,
            winner: false,
            collision_index: 0,
        });
        segment_start = segment_end;
    }
}

fn calendar_summary(
    current_at: i64,
    events: &BTreeMap<i64, Vec<TimedCalendarEvent<'_>>>,
) -> Option<CalendarSummaryView> {
    let (_, current_events) = events.range(..=current_at).next_back()?;
    let current = winning_timed_event(current_events)?;
    let next = events
        .range(current_at.saturating_add(1)..)
        .filter_map(|(&at, events)| Some((at, winning_timed_event(events)?)))
        .find(|(_, event)| event.rule.turn_on != current.rule.turn_on)
        .map(|(at, event)| {
            let day = at.div_euclid(24 * 60);
            let day_label = if day == current_at.div_euclid(24 * 60) {
                "today".to_owned()
            } else if day == current_at.div_euclid(24 * 60) + 1 {
                "tomorrow".to_owned()
            } else {
                calendar_day_name(day).to_owned()
            };
            format!(
                "Next: {} {day_label} at {} · {}",
                if event.rule.turn_on { "ON" } else { "OFF" },
                calendar_absolute_time(at),
                event.rule.name
            )
        });
    Some(CalendarSummaryView {
        state: if current.rule.turn_on { "ON" } else { "OFF" },
        next,
    })
}

fn calendar_conflicts(
    monday: i64,
    events: &BTreeMap<i64, Vec<TimedCalendarEvent<'_>>>,
) -> Vec<CalendarConflictView> {
    let week_start = monday * 24 * 60;
    let week_end = (monday + 7) * 24 * 60;
    events
        .range(week_start..week_end)
        .filter_map(|(&at, events)| {
            let winner = winning_timed_event(events)?;
            if events.len() < 2 {
                return None;
            }
            let losers = events
                .iter()
                .filter(|event| event.rule.id != winner.rule.id)
                .map(|event| event.rule.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Some(CalendarConflictView {
                detail: format!(
                    "{} at {}: {} wins over {} because it was added most recently.",
                    calendar_day_name(at.div_euclid(24 * 60)),
                    calendar_absolute_time(at),
                    winner.rule.name,
                    losers
                ),
            })
        })
        .collect()
}

fn calendar_absolute_time(timestamp: i64) -> String {
    let minute = timestamp.rem_euclid(24 * 60) as u16;
    automation::format_clock_time(minute / 60, minute % 60)
}

fn calendar_day_name(day: i64) -> &'static str {
    const DAY_NAMES: [&str; 7] = [
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ];
    DAY_NAMES[(weekday_index(day) + 6) % 7]
}

fn calendar_date(day: i64) -> String {
    let (month, day) = calendar_month_day(day);
    format!("{month} {day}")
}

fn calendar_week_label(monday: i64) -> String {
    let (start_month, start_day) = calendar_month_day(monday);
    let (end_month, end_day) = calendar_month_day(monday + 6);
    if start_month == end_month {
        format!("{start_month} {start_day}–{end_day}")
    } else {
        format!("{start_month} {start_day}–{end_month} {end_day}")
    }
}

fn calendar_month_day(day: i64) -> (&'static str, u8) {
    const MONTH_NAMES: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let shifted = day + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    (MONTH_NAMES[(month - 1) as usize], day as u8)
}

fn weekday_index(day: i64) -> usize {
    (day + 4).rem_euclid(7) as usize
}

fn push_calendar_point(
    entries: &mut Vec<CalendarEntryView>,
    rule: &AutomationRule,
    minute: u16,
    kind: &str,
    conflict: bool,
    winner: bool,
    collision_index: usize,
) {
    let action = if rule.turn_on { "Turn on" } else { "Turn off" };
    let mut class = calendar_entry_class(
        kind,
        if rule.turn_on { "turn-on" } else { "turn-off" },
        rule.enabled,
    );
    if conflict {
        class.push_str(" calendar-conflict");
    }
    if winner {
        class.push_str(" calendar-conflict-winner");
    }
    entries.push(CalendarEntryView {
        rule_id: rule.id,
        name: rule.name.clone(),
        label: calendar_boundary_label(rule, minute),
        detail: format!(
            "{} · {action}",
            automation::format_clock_time(minute / 60, minute % 60)
        ),
        enabled: rule.enabled,
        class,
        left: calendar_percent(minute),
        width: 0.0,
        lane: 0,
        point: true,
        conflict,
        winner,
        collision_index,
    });
}

fn push_calendar_window(
    entries: &mut Vec<CalendarEntryView>,
    rule: &AutomationRule,
    start: u16,
    end: u16,
    lane: usize,
) {
    let detail = if start == 0 && end == 24 * 60 {
        "Active all day".to_owned()
    } else {
        format!(
            "Active {}–{}",
            automation::format_clock_time(start / 60, start % 60),
            if end == 24 * 60 {
                "12:00 AM".to_owned()
            } else {
                automation::format_clock_time(end / 60, end % 60)
            }
        )
    };
    let (label, detail) = match rule.trigger {
        AutomationTrigger::LightLevel {
            on_below,
            off_above,
            ..
        } => (
            format!("AUTO · ≤ {on_below:.0} W/m²"),
            format!("{detail} · turns off at ≥ {off_above:.0} W/m²"),
        ),
        _ => (rule.name.clone(), detail),
    };
    entries.push(CalendarEntryView {
        rule_id: rule.id,
        name: rule.name.clone(),
        label,
        detail,
        enabled: rule.enabled,
        class: calendar_entry_class("light-window", "", rule.enabled),
        left: calendar_percent(start),
        width: calendar_percent(end - start),
        lane,
        point: false,
        conflict: false,
        winner: false,
        collision_index: 0,
    });
}

fn calendar_boundary_label(rule: &AutomationRule, minute: u16) -> String {
    let hour = minute / 60;
    let time = format!("{}:{:02}", (hour % 12).max(1), minute % 60);
    match rule.trigger {
        AutomationTrigger::FixedTime { .. } => time,
        AutomationTrigger::Solar {
            event: SolarEvent::Sunrise,
            ..
        } => format!("{time} ↑"),
        AutomationTrigger::Solar {
            event: SolarEvent::Sunset,
            ..
        } => format!("{time} ↓"),
        AutomationTrigger::LightLevel { .. } => time,
    }
}

fn calendar_window(
    window: Option<ActiveWindow>,
    solar: Option<SolarForecastDay>,
) -> Option<(u16, u16)> {
    match window {
        None => Some((0, 24 * 60)),
        Some(window) => Some((
            calendar_boundary_minute(window.start, solar)?,
            calendar_boundary_minute(window.end, solar)?,
        )),
    }
}

fn calendar_boundary_minute(
    boundary: TimeBoundary,
    solar: Option<SolarForecastDay>,
) -> Option<u16> {
    match boundary {
        TimeBoundary::Fixed { minute_of_day } => Some(minute_of_day),
        TimeBoundary::Solar {
            event,
            offset_minutes,
        } => Some(solar_event_minute(event, offset_minutes, solar?)),
    }
}

fn solar_event_minute(event: SolarEvent, offset_minutes: i16, solar: SolarForecastDay) -> u16 {
    let minute = match event {
        SolarEvent::Sunrise => solar.sunrise_minute,
        SolarEvent::Sunset => solar.sunset_minute,
    };
    (i32::from(minute) + i32::from(offset_minutes)).rem_euclid(24 * 60) as u16
}

fn calendar_entry_class(kind: &str, action: &str, enabled: bool) -> String {
    format!(
        "calendar-entry {kind} {action}{}",
        if enabled { "" } else { " calendar-disabled" }
    )
}

fn calendar_percent(minute: u16) -> f64 {
    (f64::from(minute) * 10000.0 / f64::from(24 * 60)).round() / 100.0
}

fn automation_view(rule: AutomationRule) -> AutomationView {
    let name = if rule.name.trim().is_empty() {
        match rule.trigger {
            AutomationTrigger::FixedTime { .. } => "Fixed-time schedule",
            AutomationTrigger::Solar { .. } => "Solar schedule",
            AutomationTrigger::LightLevel { .. } => "Outdoor light",
        }
        .to_owned()
    } else {
        rule.name.clone()
    };
    let mut view = AutomationView {
        id: rule.id,
        title: "",
        kind: "",
        name,
        enabled: rule.enabled,
        description: String::new(),
        time: String::new(),
        event: "",
        offset_minutes: 0,
        turn_on: rule.turn_on,
        sun: false,
        mon: false,
        tue: false,
        wed: false,
        thu: false,
        fri: false,
        sat: false,
        on_below: 0.0,
        off_above: 0.0,
        window_enabled: false,
        start_kind: "fixed",
        start_time: "09:00".to_owned(),
        start_offset_minutes: 0,
        end_kind: "fixed",
        end_time: "17:00".to_owned(),
        end_offset_minutes: 0,
        outside_turn_off: true,
    };
    match rule.trigger {
        AutomationTrigger::FixedTime {
            minute_of_day,
            weekdays,
        } => {
            view.title = "Fixed time";
            view.kind = "fixed";
            view.time = format!("{:02}:{:02}", minute_of_day / 60, minute_of_day % 60);
            view.description = format!(
                "{} · Turn {} · {}",
                automation::format_clock_time(minute_of_day / 60, minute_of_day % 60),
                if rule.turn_on { "on" } else { "off" },
                weekday_summary(weekdays)
            );
            set_automation_weekdays(&mut view, weekdays);
            view
        }
        AutomationTrigger::Solar {
            event,
            offset_minutes,
            weekdays,
        } => {
            let (title, event_name) = match event {
                SolarEvent::Sunrise => ("Sunrise", "sunrise"),
                SolarEvent::Sunset => ("Sunset", "sunset"),
            };
            let timing = match offset_minutes.cmp(&0) {
                std::cmp::Ordering::Less => {
                    format!("{} min before {event_name}", offset_minutes.unsigned_abs())
                }
                std::cmp::Ordering::Equal => format!("at {event_name}"),
                std::cmp::Ordering::Greater => {
                    format!("{} min after {event_name}", offset_minutes.unsigned_abs())
                }
            };
            view.title = title;
            view.kind = "solar";
            view.event = event_name;
            view.offset_minutes = offset_minutes;
            view.description = format!(
                "Turn {} {timing} · {}",
                if rule.turn_on { "on" } else { "off" },
                weekday_summary(weekdays)
            );
            set_automation_weekdays(&mut view, weekdays);
            view
        }
        AutomationTrigger::LightLevel {
            on_below,
            off_above,
            active_window,
        } => {
            view.title = "Outdoor light";
            view.kind = "light";
            view.on_below = on_below;
            view.off_above = off_above;
            view.description = match active_window {
                Some(window) => format!(
                    "{}–{} · On ≤ {on_below:.0} W/m² · Off ≥ {off_above:.0} W/m²{}",
                    time_boundary_description(window.start),
                    time_boundary_description(window.end),
                    if window.outside == OutsideWindowBehavior::TurnOff {
                        " · Off outside window"
                    } else {
                        ""
                    }
                ),
                None => format!("On ≤ {on_below:.0} W/m² · Off ≥ {off_above:.0} W/m² · All day"),
            };
            if let Some(window) = active_window {
                view.window_enabled = true;
                (view.start_kind, view.start_time, view.start_offset_minutes) =
                    time_boundary_form_values(window.start, "09:00");
                (view.end_kind, view.end_time, view.end_offset_minutes) =
                    time_boundary_form_values(window.end, "17:00");
                view.outside_turn_off = window.outside == OutsideWindowBehavior::TurnOff;
            }
            view
        }
    }
}

fn set_automation_weekdays(view: &mut AutomationView, weekdays: [bool; 7]) {
    [
        view.sun, view.mon, view.tue, view.wed, view.thu, view.fri, view.sat,
    ] = weekdays;
}

fn time_boundary_form_values(
    boundary: TimeBoundary,
    default_time: &str,
) -> (&'static str, String, i16) {
    match boundary {
        TimeBoundary::Fixed { minute_of_day } => (
            "fixed",
            format!("{:02}:{:02}", minute_of_day / 60, minute_of_day % 60),
            0,
        ),
        TimeBoundary::Solar {
            event,
            offset_minutes,
        } => (
            match event {
                SolarEvent::Sunrise => "sunrise",
                SolarEvent::Sunset => "sunset",
            },
            default_time.to_owned(),
            offset_minutes,
        ),
    }
}

fn time_boundary_description(boundary: TimeBoundary) -> String {
    match boundary {
        TimeBoundary::Fixed { minute_of_day } => {
            automation::format_clock_time(minute_of_day / 60, minute_of_day % 60)
        }
        TimeBoundary::Solar {
            event,
            offset_minutes,
        } => {
            let event = match event {
                SolarEvent::Sunrise => "sunrise",
                SolarEvent::Sunset => "sunset",
            };
            match offset_minutes.cmp(&0) {
                std::cmp::Ordering::Less => {
                    format!("{} min before {event}", offset_minutes.unsigned_abs())
                }
                std::cmp::Ordering::Equal => event.to_owned(),
                std::cmp::Ordering::Greater => {
                    format!("{} min after {event}", offset_minutes.unsigned_abs())
                }
            }
        }
    }
}

fn load_countdown_panel(
    client: &SmartHomeClient,
    address: IpAddr,
) -> Result<CountdownPanel, AppError> {
    let rules = client.get_countdown_rules(address)?;
    Ok(CountdownPanel {
        address: address.to_string(),
        rules: rules.rules.into_iter().map(countdown_view).collect(),
    })
}

fn countdown_view(rule: CountdownRule) -> CountdownView {
    let minutes = rule.delay / 60;
    let seconds = rule.delay % 60;
    CountdownView {
        id: rule.id.unwrap_or_default(),
        name: rule.name,
        enabled: rule.enabled,
        delay: if seconds == 0 {
            format!("{minutes} min")
        } else {
            format!("{minutes} min {seconds} sec")
        },
        action: if rule.turn_on { "Turn on" } else { "Turn off" },
    }
}

fn load_schedule_panel(
    client: &SmartHomeClient,
    address: IpAddr,
) -> Result<SchedulePanel, AppError> {
    let rules = client.get_schedule_rules(address)?;
    let views: Vec<_> = rules.rules.into_iter().map(schedule_view).collect();
    let migratable_count = views.iter().filter(|rule| rule.migratable).count();
    Ok(SchedulePanel {
        migratable_count,
        unsupported_count: views.len() - migratable_count,
        rules: views,
    })
}

fn schedule_view(rule: ScheduleRule) -> ScheduleView {
    let editable = is_editable_schedule(&rule);
    let solar_editable = is_editable_solar_schedule(&rule);
    let event_name = match rule.stime_opt {
        1 => "sunrise",
        2 => "sunset",
        _ => "",
    };
    let time = match rule.stime_opt {
        0 if rule.smin < 24 * 60 => automation::format_clock_time(rule.smin / 60, rule.smin % 60),
        1 | 2 => solar_schedule_time(event_name, rule.soffset.unwrap_or_default()),
        _ => "Solar/advanced".to_owned(),
    };

    ScheduleView {
        name: rule.name,
        enabled: rule.enabled,
        migratable: editable || solar_editable,
        time,
        action: match rule.sact {
            1 => "Turn on",
            0 => "Turn off",
            _ => "Advanced action",
        },
        weekday_summary: weekday_summary(rule.weekdays),
    }
}

fn weekday_summary(weekdays: [bool; 7]) -> String {
    const WEEKDAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let selected_days: Vec<_> = weekdays
        .iter()
        .zip(WEEKDAY_NAMES)
        .filter_map(|(selected, name)| selected.then_some(name))
        .collect();
    if weekdays.iter().all(|selected| *selected) {
        "Every day".to_owned()
    } else if weekdays == [false, true, true, true, true, true, false] {
        "Weekdays".to_owned()
    } else if selected_days.is_empty() {
        "No weekdays".to_owned()
    } else {
        selected_days.join(", ")
    }
}

fn is_editable_schedule(rule: &ScheduleRule) -> bool {
    rule.id.as_ref().is_some_and(|id| !id.is_empty())
        && rule.repeat
        && rule.stime_opt == 0
        && rule.smin < 24 * 60
        && matches!(rule.sact, 0 | 1)
        && rule.etime_opt == -1
}

fn is_editable_solar_schedule(rule: &ScheduleRule) -> bool {
    rule.id.as_ref().is_some_and(|id| !id.is_empty())
        && rule.repeat
        && matches!(rule.stime_opt, 1 | 2)
        && matches!(rule.sact, 0 | 1)
        && rule.etime_opt == -1
}

fn migrated_automation(
    rule: &ScheduleRule,
    schedules_enabled: bool,
    device_id: &str,
) -> Option<NewAutomation> {
    let trigger = if is_editable_schedule(rule) {
        AutomationTrigger::FixedTime {
            minute_of_day: rule.smin,
            weekdays: rule.weekdays,
        }
    } else if is_editable_solar_schedule(rule) {
        AutomationTrigger::Solar {
            event: if rule.stime_opt == 1 {
                SolarEvent::Sunrise
            } else {
                SolarEvent::Sunset
            },
            offset_minutes: rule.soffset.unwrap_or_default(),
            weekdays: rule.weekdays,
        }
    } else {
        return None;
    };

    Some(NewAutomation {
        device_id: device_id.to_owned(),
        name: if rule.name.trim().is_empty() {
            "Migrated schedule".to_owned()
        } else {
            rule.name.clone()
        },
        enabled: schedules_enabled && rule.enabled,
        trigger,
        turn_on: rule.sact == 1,
    })
}

fn solar_schedule_time(event: &str, offset_minutes: i16) -> String {
    match offset_minutes.cmp(&0) {
        std::cmp::Ordering::Less => {
            format!("{} min before {event}", offset_minutes.unsigned_abs())
        }
        std::cmp::Ordering::Equal => event.to_owned(),
        std::cmp::Ordering::Greater => format!("{offset_minutes} min after {event}"),
    }
}

fn render_device_list(state: &AppState, view: DeviceListView) -> Result<Html<String>, AppError> {
    let fragment = state
        .templates
        .get_template("plug-list.html")?
        .render(context! { groups => view.groups, plugs => view.plugs, notice => view.notice })?;
    Ok(Html(fragment))
}

fn render_group_panel(state: &AppState, panel: GroupPanel) -> Result<Html<String>, AppError> {
    let fragment = state
        .templates
        .get_template("group-panel.html")?
        .render(context! { panel })?;
    Ok(Html(fragment))
}

fn render_countdown_panel(
    state: &AppState,
    panel: &CountdownPanel,
) -> Result<Html<String>, AppError> {
    let fragment = state
        .templates
        .get_template("countdown-panel.html")?
        .render(context! { panel })?;
    Ok(Html(fragment))
}

fn render_automation_panel(
    state: &AppState,
    panel: &AutomationPanel,
) -> Result<Html<String>, AppError> {
    let fragment = state
        .templates
        .get_template("automation-panel.html")?
        .render(context! { panel })?;
    Ok(Html(fragment))
}

fn templates() -> Result<Environment<'static>, minijinja::Error> {
    let mut templates = Environment::new();
    templates.set_auto_escape_callback(|name| {
        if name.ends_with(".html") {
            AutoEscape::Html
        } else {
            AutoEscape::None
        }
    });
    templates.add_template("index.html", include_str!("../templates/index.html"))?;
    templates.add_template(
        "plug-list.html",
        include_str!("../templates/plug-list.html"),
    )?;
    templates.add_template("plug.html", include_str!("../templates/plug.html"))?;
    templates.add_template("group.html", include_str!("../templates/group.html"))?;
    templates.add_template(
        "group-panel.html",
        include_str!("../templates/group-panel.html"),
    )?;
    templates.add_template(
        "automation-panel.html",
        include_str!("../templates/automation-panel.html"),
    )?;
    templates.add_template(
        "countdown-panel.html",
        include_str!("../templates/countdown-panel.html"),
    )?;
    Ok(templates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::{LightHistory, LightPoint};

    fn smart_plug(device_id: &str, alias: &str, relay_on: bool) -> SmartPlug {
        SmartPlug {
            address: "192.0.2.1".parse().unwrap(),
            model: "HS105(US)".to_owned(),
            alias: alias.to_owned(),
            device_id: device_id.to_owned(),
            software_version: "1.5.6".to_owned(),
            relay_on,
            latitude: None,
            longitude: None,
        }
    }

    fn plug_schedule(stime_opt: i8) -> ScheduleRule {
        ScheduleRule {
            id: Some("plug-rule".to_owned()),
            name: "Morning".to_owned(),
            enabled: true,
            repeat: true,
            weekdays: [false, true, true, true, true, true, false],
            stime_opt,
            smin: 7 * 60 + 30,
            sact: 1,
            etime_opt: -1,
            emin: 0,
            eact: -1,
            soffset: Some(-20),
            eoffset: None,
            year: 0,
            month: 0,
            day: 0,
            latitude: 0.0,
            longitude: 0.0,
            force: 0,
            extra: Default::default(),
        }
    }

    fn calendar_weather(current_day: i64, current_minute: u16) -> WeatherStatus {
        WeatherStatus {
            local_time: automation::format_clock_time(current_minute / 60, current_minute % 60),
            current_minute,
            timezone: "UTC".to_owned(),
            condition: "Clear sky",
            is_day: true,
            shortwave_radiation: 200.0,
            cloud_cover: 0,
            temperature: 20.0,
            apparent_temperature: 20.0,
            precipitation: 0.0,
            sunrise: "6:00 AM".to_owned(),
            sunset: "6:00 PM".to_owned(),
            previous_day_light: None,
            current_day,
            solar_days: (current_day - 14..=current_day + 7)
                .map(|day| SolarForecastDay {
                    day,
                    sunrise_minute: 6 * 60,
                    sunset_minute: 18 * 60,
                })
                .collect(),
        }
    }

    #[test]
    fn web_manifest_has_installable_and_maskable_icons() {
        let manifest: serde_json::Value =
            serde_json::from_slice(include_bytes!("../pwa/manifest.webmanifest")).unwrap();
        assert_eq!(manifest["display"], "standalone");
        assert_eq!(manifest["start_url"], "/");

        let icons = manifest["icons"].as_array().unwrap();
        for size in ["192x192", "512x512"] {
            assert!(icons.iter().any(|icon| {
                icon["sizes"] == size
                    && icon["purpose"]
                        .as_str()
                        .is_some_and(|purpose| purpose.contains("any"))
            }));
            assert!(icons.iter().any(|icon| {
                icon["sizes"] == size
                    && icon["purpose"]
                        .as_str()
                        .is_some_and(|purpose| purpose.contains("maskable"))
            }));
        }
    }

    #[test]
    fn configured_device_addresses_are_trimmed_sorted_and_deduplicated() {
        assert_eq!(
            parse_device_addresses("192.0.2.2, 192.0.2.1,192.0.2.2").unwrap(),
            vec![
                "192.0.2.1".parse::<IpAddr>().unwrap(),
                "192.0.2.2".parse::<IpAddr>().unwrap(),
            ]
        );
        assert!(parse_device_addresses("192.0.2.1,not-an-address").is_err());
    }

    #[test]
    fn group_views_report_mixed_and_unavailable_members() {
        let plugs = vec![
            smart_plug("plug-on", "Lamp", true),
            smart_plug("plug-off", "Fan", false),
        ];
        let mixed = group_view(
            DeviceGroup {
                id: 1,
                name: "Room".to_owned(),
                device_ids: vec![
                    "plug-on".to_owned(),
                    "plug-off".to_owned(),
                    "plug-offline-12345678".to_owned(),
                ],
            },
            &plugs,
        );
        assert_eq!(mixed.state, "Mixed");
        assert_eq!(mixed.reachable_count, 2);
        assert_eq!(mixed.member_count, 3);
        assert!(mixed.has_offline_members);
        assert_eq!(mixed.members, "Lamp, Fan, Unavailable …12345678");

        let unavailable = group_view(
            DeviceGroup {
                id: 2,
                name: "Away".to_owned(),
                device_ids: vec!["missing".to_owned()],
            },
            &plugs,
        );
        assert_eq!(unavailable.state, "Unavailable");
        assert_eq!(unavailable.state_class, "state-unavailable");
        assert_eq!(unavailable.reachable_count, 0);
    }

    #[test]
    fn group_panel_preserves_unavailable_members_for_explicit_removal() {
        let group = DeviceGroup {
            id: 7,
            name: "Downstairs".to_owned(),
            device_ids: vec!["online".to_owned(), "offline-12345678".to_owned()],
        };
        let panel = group_panel(Some(&group), &[smart_plug("online", "Desk lamp", true)]);

        assert!(panel.editing);
        assert_eq!(panel.devices.len(), 2);
        assert!(panel.devices[0].available);
        assert!(panel.devices[0].selected);
        assert!(!panel.devices[1].available);
        assert!(panel.devices[1].selected);

        let fragment = templates()
            .unwrap()
            .get_template("group-panel.html")
            .unwrap()
            .render(context! { panel })
            .unwrap();
        assert!(fragment.contains("hx-post=\"/groups/7\""));
        assert!(fragment.contains("hx-delete=\"/groups/7\""));
        assert!(fragment.contains("value=\"offline-12345678\" checked"));
        assert!(fragment.contains("Unavailable …12345678"));
        assert!(fragment.contains("role=\"separator\""));
        assert!(fragment.contains("aria-label=\"Resize group pane\""));
        assert!(fragment.contains("aria-labelledby=\"group-pane-title group-pane-description\""));
        assert!(fragment.contains("placeholder=\"Living room\" autofocus"));
    }

    #[test]
    fn group_card_renders_controls_and_escapes_group_data() {
        let groups = vec![GroupView {
            id: 3,
            name: "<Downstairs>".to_owned(),
            member_count: 2,
            reachable_count: 1,
            members: "Lamp & fan".to_owned(),
            state: "On",
            state_class: "state-on",
            has_offline_members: true,
        }];

        let fragment = templates()
            .unwrap()
            .get_template("plug-list.html")
            .unwrap()
            .render(context! { groups, plugs => Vec::<PlugView>::new() })
            .unwrap();

        assert!(fragment.contains("hx-post=\"/groups/3/relay\""));
        assert!(fragment.contains("hx-get=\"/groups/3/automations\""));
        assert!(fragment.contains("name=\"on\" value=\"true\""));
        assert!(fragment.contains("name=\"on\" value=\"false\""));
        assert!(fragment.contains("hx-get=\"/groups/3\""));
        assert!(fragment.contains("&lt;Downstairs&gt;"));
        assert!(fragment.contains("Lamp &amp; fan"));
        assert!(fragment.contains("Some members are not in the remembered inventory"));
    }

    #[test]
    fn automation_panel_uses_group_routes() {
        let panel = AutomationPanel {
            address: "Living room".to_owned(),
            automation_base: "/groups/3/automations".to_owned(),
            location_available: true,
            weather: None,
            calendar: None,
            rules: vec![automation_view(AutomationRule {
                id: 9,
                device_id: automation_target(3),
                name: "Morning".to_owned(),
                enabled: true,
                trigger: AutomationTrigger::FixedTime {
                    minute_of_day: 7 * 60,
                    weekdays: [true; 7],
                },
                turn_on: true,
                last_solar_day: None,
            })],
            schedules: SchedulePanel {
                migratable_count: 0,
                unsupported_count: 0,
                rules: Vec::new(),
            },
        };

        let fragment = templates()
            .unwrap()
            .get_template("automation-panel.html")
            .unwrap()
            .render(context! { panel })
            .unwrap();

        assert!(fragment.contains("Living room"));
        assert!(fragment.contains("hx-post=\"/groups/3/automations/fixed\""));
        assert!(fragment.contains("hx-post=\"/groups/3/automations/solar\""));
        assert!(fragment.contains("hx-post=\"/groups/3/automations/light\""));
        assert!(fragment.contains("hx-post=\"/groups/3/automations/9/fixed\""));
        assert!(fragment.contains("hx-get=\"/groups/3/automations\""));
    }

    #[test]
    fn page_renders_controls_and_escapes_device_alias() {
        let plugs = vec![PlugView {
            address: "192.0.2.1".to_owned(),
            model: "HS105(US)".to_owned(),
            alias: "<Desk lamp>".to_owned(),
            device_id: "device-1".to_owned(),
            software_version: "1.5.6".to_owned(),
            relay_on: true,
        }];

        let page = templates()
            .unwrap()
            .get_template("index.html")
            .unwrap()
            .render(context! { plugs })
            .unwrap();

        assert!(page.contains("hx-post=\"/scan\""));
        assert!(page.contains("hx-get=\"/groups/new\""));
        assert!(page.contains("hx-post=\"/plugs/192.0.2.1/relay\""));
        assert!(page.contains("hx-target=\"#plug-list\""));
        assert!(page.contains("hx-get=\"/plugs/192.0.2.1/automations\""));
        assert!(page.contains("hx-get=\"/plugs/192.0.2.1/countdown\""));
        assert!(page.contains("hx-delete=\"/devices/device-1\""));
        assert!(!page.contains("hx-get=\"/plugs/192.0.2.1/schedules\""));
        assert!(page.contains("hx-target=\"#device-pane\""));
        assert!(page.contains("id=\"device-pane\""));
        assert!(page.contains("id=\"app-notification\""));
        assert!(page.contains("id=\"confirmation-dialog\""));
        assert!(page.contains("Scanning…"));
        assert!(!page.contains("window.alert"));
        assert!(page.contains("&lt;Desk lamp&gt;"));
        assert!(!page.contains("<Desk lamp>"));
    }

    #[test]
    fn automation_forms_validate_solar_offsets_and_light_hysteresis() {
        let solar = solar_automation(
            SolarAutomationForm {
                name: "Evening".to_owned(),
                event: "sunset".to_owned(),
                offset_minutes: -30,
                action: "on".to_owned(),
                sun: Some("on".to_owned()),
                mon: Some("on".to_owned()),
                tue: Some("on".to_owned()),
                wed: Some("on".to_owned()),
                thu: Some("on".to_owned()),
                fri: Some("on".to_owned()),
                sat: Some("on".to_owned()),
            },
            "plug".to_owned(),
        )
        .unwrap();
        assert_eq!(
            solar.trigger,
            AutomationTrigger::Solar {
                event: SolarEvent::Sunset,
                offset_minutes: -30,
                weekdays: [true; 7],
            }
        );
        assert!(solar.turn_on);
        assert!(solar_automation(
            SolarAutomationForm {
                name: "Evening".to_owned(),
                event: "sunset".to_owned(),
                offset_minutes: 181,
                action: "on".to_owned(),
                sun: Some("on".to_owned()),
                mon: Some("on".to_owned()),
                tue: Some("on".to_owned()),
                wed: Some("on".to_owned()),
                thu: Some("on".to_owned()),
                fri: Some("on".to_owned()),
                sat: Some("on".to_owned()),
            },
            "plug".to_owned(),
        )
        .is_err());

        let light = light_automation(
            LightAutomationForm {
                name: "Cloudy daytime".to_owned(),
                on_below: 100.0,
                off_above: 125.0,
                window_enabled: Some("on".to_owned()),
                start_kind: "fixed".to_owned(),
                start_time: "09:00".to_owned(),
                start_offset_minutes: 0,
                end_kind: "sunset".to_owned(),
                end_time: "17:00".to_owned(),
                end_offset_minutes: 0,
                outside_window: "turn_off".to_owned(),
            },
            "plug".to_owned(),
        )
        .unwrap();
        assert_eq!(
            light.trigger,
            AutomationTrigger::LightLevel {
                on_below: 100.0,
                off_above: 125.0,
                active_window: Some(ActiveWindow {
                    start: TimeBoundary::Fixed {
                        minute_of_day: 9 * 60,
                    },
                    end: TimeBoundary::Solar {
                        event: SolarEvent::Sunset,
                        offset_minutes: 0,
                    },
                    outside: OutsideWindowBehavior::TurnOff,
                }),
            }
        );
        assert!(light_automation(
            LightAutomationForm {
                name: "Cloudy daytime".to_owned(),
                on_below: 125.0,
                off_above: 125.0,
                window_enabled: Some("on".to_owned()),
                start_kind: "fixed".to_owned(),
                start_time: "09:00".to_owned(),
                start_offset_minutes: 0,
                end_kind: "sunset".to_owned(),
                end_time: "17:00".to_owned(),
                end_offset_minutes: 0,
                outside_window: "turn_off".to_owned(),
            },
            "plug".to_owned(),
        )
        .is_err());
    }

    #[test]
    fn calendar_summary_skips_same_state_events_and_marks_same_time_winner() {
        let fixed = |id, name: &str, minute_of_day, turn_on, enabled| AutomationRule {
            id,
            device_id: "plug".to_owned(),
            name: name.to_owned(),
            enabled,
            trigger: AutomationTrigger::FixedTime {
                minute_of_day,
                weekdays: [false, true, false, false, false, false, false],
            },
            turn_on,
            last_solar_day: None,
        };
        let mut sunday_on = fixed(1, "Sunday evening", 20 * 60, true, true);
        sunday_on.trigger = AutomationTrigger::FixedTime {
            minute_of_day: 20 * 60,
            weekdays: [true, false, false, false, false, false, false],
        };
        let rules = vec![
            sunday_on,
            fixed(2, "Morning", 7 * 60, false, true),
            fixed(3, "Noon backup", 12 * 60, false, true),
            fixed(4, "Evening old", 18 * 60, true, true),
            fixed(5, "Evening new", 18 * 60, true, true),
            fixed(6, "Evening disabled", 18 * 60, false, false),
        ];

        let before = week_calendar_view(&rules, &calendar_weather(4, 7 * 60 - 1));
        assert_eq!(before.summary.as_ref().unwrap().state, "ON");
        assert_eq!(
            before.summary.as_ref().unwrap().next.as_deref(),
            Some("Next: OFF today at 7:00 AM · Morning")
        );
        let at_transition = week_calendar_view(&rules, &calendar_weather(4, 7 * 60));
        assert_eq!(at_transition.summary.as_ref().unwrap().state, "OFF");
        let sunday_night = week_calendar_view(&rules, &calendar_weather(10, 21 * 60));
        assert_eq!(
            sunday_night.summary.as_ref().unwrap().next.as_deref(),
            Some("Next: OFF tomorrow at 7:00 AM · Morning")
        );

        let calendar = week_calendar_view(&rules, &calendar_weather(4, 8 * 60));

        let summary = calendar.summary.as_ref().unwrap();
        assert_eq!(summary.state, "OFF");
        assert_eq!(
            summary.next.as_deref(),
            Some("Next: ON today at 6:00 PM · Evening new")
        );
        assert_eq!(calendar.conflicts.len(), 1);
        assert_eq!(
            calendar.conflicts[0].detail,
            "Monday at 6:00 PM: Evening new wins over Evening old because it was added most recently."
        );
        let monday = &calendar.days[0];
        assert!(monday.entries.iter().any(|entry| {
            entry.class.contains("scheduled-off")
                && entry.detail == "Scheduled off 7:00 AM–6:00 PM · set by Morning"
        }));
        let conflict_points: Vec<_> = monday
            .entries
            .iter()
            .filter(|entry| entry.point && entry.conflict)
            .collect();
        assert_eq!(conflict_points.len(), 2);
        assert_eq!(
            conflict_points
                .iter()
                .find(|entry| entry.winner)
                .unwrap()
                .rule_id,
            5
        );

        let panel = AutomationPanel {
            address: "192.0.2.1".to_owned(),
            automation_base: "/plugs/192.0.2.1/automations".to_owned(),
            location_available: true,
            weather: Some(calendar_weather(4, 8 * 60)),
            calendar: Some(calendar),
            rules: rules.into_iter().map(automation_view).collect(),
            schedules: SchedulePanel {
                migratable_count: 0,
                unsupported_count: 0,
                rules: Vec::new(),
            },
        };
        let fragment = templates()
            .unwrap()
            .get_template("automation-panel.html")
            .unwrap()
            .render(context! { panel })
            .unwrap();
        assert!(fragment.contains("Schedule conflict"));
        assert!(fragment.contains(
            "Monday at 6:00 PM: Evening new wins over Evening old because it was added most recently."
        ));
        assert!(fragment.contains("data-rule-id=\"5\" data-name=\"Evening new\""));
        assert!(fragment.contains("data-winner=\"true\""));
        assert!(fragment.contains(
            "data-name=\"Evening disabled\" data-detail=\"6:00 PM · Turn off\" data-enabled=\"false\""
        ));
    }

    #[test]
    fn calendar_dates_cover_month_and_leap_day_boundaries() {
        assert_eq!(calendar_date(0), "Jan 1");
        assert_eq!(calendar_date(31), "Feb 1");
        assert_eq!(calendar_date(11_016), "Feb 29");
        assert_eq!(calendar_week_label(20_689), "Aug 24–30");
    }

    #[test]
    fn calendar_projects_a_single_weekday_state_across_the_week() {
        let rule = AutomationRule {
            id: 1,
            device_id: "plug".to_owned(),
            name: "Weekly shutdown".to_owned(),
            enabled: true,
            trigger: AutomationTrigger::FixedTime {
                minute_of_day: 7 * 60,
                weekdays: [false, true, false, false, false, false, false],
            },
            turn_on: false,
            last_solar_day: None,
        };

        let weather = calendar_weather(4, 8 * 60);
        let calendar = week_calendar_view(&[rule], &weather);

        assert!(calendar
            .days
            .iter()
            .all(|day| day.entries.iter().any(|entry| {
                entry.class.contains("scheduled-off")
                    && entry.width == 100.0
                    && entry.detail == "Scheduled off all week · carried over from Weekly shutdown"
            })));

        let panel = AutomationPanel {
            address: "192.0.2.1".to_owned(),
            automation_base: "/plugs/192.0.2.1/automations".to_owned(),
            location_available: true,
            calendar: Some(calendar),
            weather: Some(weather),
            rules: Vec::new(),
            schedules: SchedulePanel {
                migratable_count: 0,
                unsupported_count: 0,
                rules: Vec::new(),
            },
        };
        let fragment = templates()
            .unwrap()
            .get_template("automation-panel.html")
            .unwrap()
            .render(context! { panel })
            .unwrap();
        assert!(fragment.contains("No turn-on event is scheduled"));
        assert!(fragment.contains(
            "Covered devices stay off until another schedule or manual action changes them."
        ));
    }

    #[test]
    fn empty_calendar_offers_existing_schedule_forms() {
        let weather = calendar_weather(4, 8 * 60);
        let panel = AutomationPanel {
            address: "192.0.2.1".to_owned(),
            automation_base: "/plugs/192.0.2.1/automations".to_owned(),
            location_available: true,
            calendar: Some(week_calendar_view(&[], &weather)),
            weather: Some(weather),
            rules: Vec::new(),
            schedules: SchedulePanel {
                migratable_count: 0,
                unsupported_count: 0,
                rules: Vec::new(),
            },
        };

        let fragment = templates()
            .unwrap()
            .get_template("automation-panel.html")
            .unwrap()
            .render(context! { panel })
            .unwrap();

        assert!(fragment.contains("No timed schedules yet"));
        assert!(!fragment.contains("openScheduleAddMenu"));
        assert!(fragment.contains(
            "class=\"ui primary floating dropdown button schedule-add-dropdown schedule-add-button\""
        ));
        assert!(fragment
            .contains("id=\"schedule-add-menu\" class=\"menu schedule-add-menu\" role=\"menu\""));
        assert!(fragment
            .contains("<div class=\"item\" role=\"menuitem\" data-value=\"add-fixed-schedule\""));
        assert!(!fragment.contains("<button class=\"item\""));
        assert!(fragment.contains("class=\"field\"><label for=\"new-fixed-name\""));
        assert!(fragment.contains("id=\"add-fixed-schedule\" class=\"schedule-add-form\""));
        assert!(fragment.contains("id=\"add-solar-schedule\" class=\"schedule-add-form\""));
        assert!(fragment
            .contains("id=\"add-light-schedule\" class=\"schedule-add-form light-rule-details\""));
    }

    #[test]
    fn automation_panel_renders_persisted_rules_and_actions() {
        let weather = WeatherStatus {
            local_time: "8:15 PM".to_owned(),
            current_minute: 20 * 60 + 15,
            timezone: "GMT-4".to_owned(),
            condition: "Overcast",
            is_day: false,
            shortwave_radiation: 42.5,
            cloud_cover: 100,
            temperature: 13.4,
            apparent_temperature: 11.9,
            precipitation: 0.0,
            sunrise: "6:33 AM".to_owned(),
            sunset: "8:19 PM".to_owned(),
            previous_day_light: Some(LightHistory {
                points: vec![
                    LightPoint {
                        x: 40.0,
                        y: 120.0,
                        time: "12:00 AM".to_owned(),
                        radiation: 0.0,
                    },
                    LightPoint {
                        x: 176.0,
                        y: 12.0,
                        time: "12:00 PM".to_owned(),
                        radiation: 500.0,
                    },
                ],
                average_points: vec![LightPoint {
                    x: 176.0,
                    y: 33.6,
                    time: "12:00 PM".to_owned(),
                    radiation: 400.0,
                }],
                average_days: 30,
                max_radiation: 500,
                mid_radiation: 250,
                sunrise_x: 114.8,
                sunset_x: 270.1,
                sunrise: "6:36 AM".to_owned(),
                sunset: "8:18 PM".to_owned(),
            }),
            current_day: 0,
            solar_days: (-10..=3)
                .map(|day| SolarForecastDay {
                    day,
                    sunrise_minute: 6 * 60 + 30,
                    sunset_minute: 20 * 60 + 20,
                })
                .collect(),
        };
        let rules = vec![
            AutomationRule {
                id: 7,
                device_id: "plug".to_owned(),
                name: "Evening".to_owned(),
                enabled: true,
                trigger: AutomationTrigger::Solar {
                    event: SolarEvent::Sunset,
                    offset_minutes: -30,
                    weekdays: [true; 7],
                },
                turn_on: true,
                last_solar_day: None,
            },
            AutomationRule {
                id: 8,
                device_id: "plug".to_owned(),
                name: "Morning".to_owned(),
                enabled: true,
                trigger: AutomationTrigger::FixedTime {
                    minute_of_day: 7 * 60 + 30,
                    weekdays: [false, true, true, true, true, true, false],
                },
                turn_on: false,
                last_solar_day: None,
            },
            AutomationRule {
                id: 9,
                device_id: "plug".to_owned(),
                name: "Cloudy daytime".to_owned(),
                enabled: true,
                trigger: AutomationTrigger::LightLevel {
                    on_below: 80.0,
                    off_above: 120.0,
                    active_window: Some(ActiveWindow {
                        start: TimeBoundary::Fixed {
                            minute_of_day: 9 * 60,
                        },
                        end: TimeBoundary::Solar {
                            event: SolarEvent::Sunset,
                            offset_minutes: -10,
                        },
                        outside: OutsideWindowBehavior::StopControlling,
                    }),
                },
                turn_on: true,
                last_solar_day: None,
            },
        ];
        let calendar = week_calendar_view(&rules, &weather);
        assert_eq!(calendar.days.len(), 7);
        assert!(calendar.days[3].is_today);
        assert_eq!(
            calendar.days[0]
                .entries
                .iter()
                .filter(|entry| entry.point)
                .map(|entry| entry.rule_id)
                .collect::<Vec<_>>(),
            vec![7, 8]
        );
        assert_eq!(
            calendar.days[5]
                .entries
                .iter()
                .filter(|entry| entry.point)
                .map(|entry| entry.rule_id)
                .collect::<Vec<_>>(),
            vec![7]
        );
        let monday_on_spans: Vec<_> = calendar.days[0]
            .entries
            .iter()
            .filter(|entry| entry.class.contains("scheduled-on"))
            .collect();
        assert_eq!(monday_on_spans.len(), 2);
        assert_eq!(monday_on_spans[0].left, 0.0);
        assert_eq!(monday_on_spans[0].width, 31.25);
        assert_eq!(
            monday_on_spans[1].detail,
            "Scheduled on 7:50 PM–7:30 AM · set by Evening"
        );
        let monday_off_span = calendar.days[0]
            .entries
            .iter()
            .find(|entry| entry.class.contains("scheduled-off"))
            .unwrap();
        assert_eq!(monday_off_span.left, 31.25);
        assert_eq!(monday_off_span.label, "OFF");
        assert_eq!(
            monday_off_span.detail,
            "Scheduled off 7:30 AM–7:50 PM · set by Morning"
        );
        assert!(calendar.days[0]
            .entries
            .iter()
            .any(|entry| entry.label == "AUTO · ≤ 80 W/m²"));
        let panel = AutomationPanel {
            address: "192.0.2.1".to_owned(),
            automation_base: "/plugs/192.0.2.1/automations".to_owned(),
            location_available: true,
            calendar: Some(calendar),
            weather: Some(weather),
            rules: rules.into_iter().map(automation_view).collect(),
            schedules: SchedulePanel {
                migratable_count: 1,
                unsupported_count: 0,
                rules: vec![schedule_view(plug_schedule(0))],
            },
        };

        let fragment = templates()
            .unwrap()
            .get_template("automation-panel.html")
            .unwrap()
            .render(context! { panel })
            .unwrap();

        assert!(fragment.contains("8:15 PM GMT-4"));
        assert!(fragment.contains("42.5 W/m²"));
        assert!(fragment.contains("Overcast"));
        assert!(fragment.contains("Yesterday's outdoor light"));
        assert!(fragment.contains("30-day hourly average"));
        assert!(fragment.contains("class=\"light-average-line\""));
        assert!(fragment.contains("class=\"light-line\""));
        assert!(fragment.contains("class=\"solar-line sunrise-line\" x1=\"114.8\""));
        assert!(fragment.contains("Sunrise 6:36 AM"));
        assert!(fragment.contains("Sunset 8:18 PM"));
        assert!(fragment.contains("<details class=\"conditions-disclosure\">"));
        assert!(
            fragment.find("This week").unwrap()
                < fragment.find("Conditions and light history").unwrap()
        );
        assert!(fragment.contains("id=\"week-calendar-title\">This week"));
        assert!(fragment.contains("Times shown in GMT-4"));
        assert!(fragment.contains("data-calendar-view=\"state\""));
        assert!(fragment.contains("data-automation-base=\"/plugs/192.0.2.1/automations\""));
        assert!(fragment.contains("State view"));
        assert!(fragment.contains("Rule events"));
        assert!(fragment.contains("Scheduled ON now"));
        assert!(fragment.contains("Next: OFF tomorrow at 7:30 AM · Morning"));
        assert!(fragment.contains("class=\"calendar-now\" style=\"--calendar-left: 84.38%\""));
        assert!(fragment.contains("class=\"calendar-day-picker\""));
        assert!(fragment.contains("aria-controls=\"calendar-day-3\" aria-pressed=\"true\""));
        assert!(fragment.contains("calendar-entry scheduled-off calendar-span"));
        assert!(fragment.contains("data-calendar-entry data-rule-id=\"7\""));
        assert!(fragment.contains("class=\"calendar-selection\" aria-live=\"polite\" hidden"));
        assert!(fragment.contains("onclick=\"editSelectedCalendarRule(this)\""));
        assert!(fragment.contains("onclick=\"setSelectedCalendarRuleEnabled(this)\""));
        assert!(fragment.contains("onclick=\"deleteSelectedCalendarRule(this)\""));
        assert!(fragment.contains("class=\"calendar-inline-editor\" hidden"));
        assert!(fragment.contains("class=\"calendar-editor-store\" hidden"));
        assert!(fragment.contains("id=\"schedule-editor-7\""));
        assert!(fragment.contains("Evening — 7:50 PM · Turn on"));
        assert!(fragment.contains("calendar-entry scheduled-on calendar-span"));
        assert!(fragment.contains("Evening — Scheduled on 7:50 PM–7:30 AM"));
        assert!(fragment.contains("Scheduled on</span>"));
        assert!(fragment.contains("7:50 ↓"));
        assert!(fragment.contains("AUTO"));
        assert!(fragment.contains("Cloudy daytime — Active 9:00 AM–8:10 PM"));
        assert!(!fragment.contains("One server-owned schedule list"));
        assert!(!fragment.contains("class=\"timer-rule-summary\""));
        assert!(fragment.contains("name=\"start_kind\""));
        assert!(fragment.contains("name=\"outside_window\" value=\"turn_off\" checked"));
        assert!(fragment.contains("Active daily from <strong>9:00 AM</strong> until"));
        assert!(fragment.contains("role=\"separator\""));
        assert!(fragment.contains("aria-label=\"Resize automation pane\""));
        assert!(fragment.contains(
            "aria-label=\"Close automation pane\" onclick=\"this.closest('dialog').close()\" autofocus"
        ));
        assert!(fragment.contains("hx-post=\"/plugs/192.0.2.1/automations/solar\""));
        assert!(fragment.contains("hx-post=\"/plugs/192.0.2.1/automations/fixed\""));
        assert!(fragment.contains("hx-post=\"/plugs/192.0.2.1/automations/light\""));
        assert!(fragment.contains("Edit schedule"));
        assert!(fragment.contains("hx-post=\"/plugs/192.0.2.1/automations/7/solar\""));
        assert!(fragment.contains("value=\"-30\""));
        assert!(fragment.contains("value=\"sunset\" selected"));
        assert!(fragment.contains("hx-post=\"/plugs/192.0.2.1/automations/8/fixed\""));
        assert!(fragment.contains("name=\"time\" type=\"time\" required value=\"07:30\""));
        assert!(fragment.contains("hx-post=\"/plugs/192.0.2.1/automations/9/light\""));
        assert!(fragment.contains("id=\"threshold-error-9\""));
        assert!(fragment.contains("id=\"threshold-error-new\""));
        assert!(fragment.contains("name=\"end_offset_minutes\" type=\"number\" min=\"-180\" max=\"180\" required value=\"-10\""));
        assert!(fragment.contains("name=\"outside_window\" value=\"stop_controlling\" checked"));
        assert!(fragment.contains("Schedules still on the plug"));
        assert!(fragment.contains("hx-post=\"/plugs/192.0.2.1/automations/migrate\""));
        assert!(fragment.contains("Migrate 1 compatible schedule"));
        assert!(!fragment.contains("hx-delete=\"/plugs/192.0.2.1/automations/7\""));
    }

    #[test]
    fn countdown_form_validates_and_builds_protocol_rule() {
        let input = CountdownInput::try_from(CountdownForm {
            minutes: 30,
            action: "off".to_owned(),
        })
        .unwrap();
        let rule = input.rule();

        assert_eq!(rule.delay, 1_800);
        assert!(!rule.turn_on);
        assert!(rule.enabled);
        assert!(CountdownInput::try_from(CountdownForm {
            minutes: 0,
            action: "on".to_owned(),
        })
        .is_err());
        assert!(CountdownInput::try_from(CountdownForm {
            minutes: 1_441,
            action: "on".to_owned(),
        })
        .is_err());
    }

    #[test]
    fn countdown_panel_renders_timer_and_escapes_name() {
        let panel = CountdownPanel {
            address: "192.0.2.1".to_owned(),
            rules: vec![CountdownView {
                id: "timer-id".to_owned(),
                name: "<Web timer>".to_owned(),
                enabled: true,
                delay: "30 min".to_owned(),
                action: "Turn off",
            }],
        };

        let fragment = templates()
            .unwrap()
            .get_template("countdown-panel.html")
            .unwrap()
            .render(context! { panel })
            .unwrap();

        assert!(fragment.contains("hx-post=\"/plugs/192.0.2.1/countdown\""));
        assert!(fragment.contains("hx-delete=\"/plugs/192.0.2.1/countdown/timer-id\""));
        assert!(fragment.contains("Turn off in 30 min"));
        assert!(fragment.contains("aria-labelledby=\"timer-pane-title timer-pane-address\""));
        assert!(fragment.contains("class=\"ui right labeled input\""));
        assert!(fragment.contains("<label for=\"countdown-minutes\">After</label>"));
        assert!(fragment.contains("&lt;Web timer&gt;"));
        assert!(!fragment.contains("<Web timer>"));
    }

    #[test]
    fn fixed_automation_validates_and_preserves_weekdays() {
        assert_eq!(parse_schedule_time("07:15").unwrap(), 435);
        assert!(parse_schedule_time("7:15").is_err());
        assert!(parse_schedule_time("23:60").is_err());

        let fixed = fixed_automation(
            ScheduleForm {
                name: "Morning".to_owned(),
                time: "07:30".to_owned(),
                action: "on".to_owned(),
                sun: None,
                mon: Some("on".to_owned()),
                tue: Some("on".to_owned()),
                wed: Some("on".to_owned()),
                thu: Some("on".to_owned()),
                fri: Some("on".to_owned()),
                sat: None,
            },
            "plug".to_owned(),
        )
        .unwrap();
        assert_eq!(fixed.name, "Morning");
        assert_eq!(
            fixed.trigger,
            AutomationTrigger::FixedTime {
                minute_of_day: 450,
                weekdays: [false, true, true, true, true, true, false],
            }
        );

        assert!(fixed_automation(
            ScheduleForm {
                name: "Morning".to_owned(),
                time: "24:00".to_owned(),
                action: "on".to_owned(),
                sun: Some("on".to_owned()),
                mon: None,
                tue: None,
                wed: None,
                thu: None,
                fri: None,
                sat: None,
            },
            "plug".to_owned(),
        )
        .is_err());

        assert!(fixed_automation(
            ScheduleForm {
                name: "Morning".to_owned(),
                time: "07:30".to_owned(),
                action: "on".to_owned(),
                sun: None,
                mon: None,
                tue: None,
                wed: None,
                thu: None,
                fri: None,
                sat: None,
            },
            "plug".to_owned(),
        )
        .is_err());
    }

    #[test]
    fn compatible_plug_schedules_convert_without_losing_behavior() {
        let fixed = migrated_automation(&plug_schedule(0), false, "plug").unwrap();
        assert_eq!(fixed.name, "Morning");
        assert!(!fixed.enabled);
        assert!(fixed.turn_on);
        assert_eq!(
            fixed.trigger,
            AutomationTrigger::FixedTime {
                minute_of_day: 450,
                weekdays: [false, true, true, true, true, true, false],
            }
        );

        let solar = migrated_automation(&plug_schedule(2), true, "plug").unwrap();
        assert_eq!(
            solar.trigger,
            AutomationTrigger::Solar {
                event: SolarEvent::Sunset,
                offset_minutes: -20,
                weekdays: [false, true, true, true, true, true, false],
            }
        );

        let mut advanced = plug_schedule(0);
        advanced.etime_opt = 0;
        assert!(migrated_automation(&advanced, true, "plug").is_none());
    }
}
