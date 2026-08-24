mod automation;
mod database;
mod group;

use automation::{
    AutomationEngine, AutomationRule, AutomationTrigger, NewAutomation, SolarEvent, WeatherStatus,
};
use axum::body::Body;
use axum::extract::{Form, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use database::Database;
use group::{DeviceGroup, GroupEngine};
use minijinja::{context, AutoEscape, Environment};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::error::Error as StdError;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tddp_client::{CountdownRule, RuleSet, ScheduleRule, SmartHomeClient, SmartPlug};
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
    event: String,
    offset_minutes: i16,
    action: String,
}

#[derive(Deserialize)]
struct LightAutomationForm {
    on_below: f64,
    off_above: f64,
}

#[derive(Serialize)]
struct AutomationPanel {
    address: String,
    location_available: bool,
    weather: Option<WeatherStatus>,
    rules: Vec<AutomationView>,
    schedules: SchedulePanel,
}

#[derive(Serialize)]
struct AutomationView {
    id: u64,
    title: &'static str,
    description: String,
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

#[derive(Deserialize)]
struct SolarScheduleForm {
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

#[derive(Debug)]
struct ScheduleInput {
    name: String,
    minute_of_day: u16,
    turn_on: bool,
    weekdays: [bool; 7],
}

#[derive(Debug)]
struct SolarScheduleInput {
    name: String,
    event: SolarEvent,
    offset_minutes: i16,
    turn_on: bool,
    weekdays: [bool; 7],
}

#[derive(Serialize)]
struct SchedulePanel {
    address: String,
    enabled: bool,
    rules: Vec<ScheduleView>,
}

#[derive(Serialize)]
struct ScheduleView {
    id: String,
    name: String,
    enabled: bool,
    editable: bool,
    solar_editable: bool,
    time: String,
    action: &'static str,
    action_on: bool,
    solar_event: &'static str,
    solar_offset: i16,
    weekday_summary: String,
    sun: bool,
    mon: bool,
    tue: bool,
    wed: bool,
    thu: bool,
    fri: bool,
    sat: bool,
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

impl From<tddp_client::Error> for AppError {
    fn from(error: tddp_client::Error) -> Self {
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
    let automations = Arc::new(AutomationEngine::new(database.clone())?);
    let groups = Arc::new(GroupEngine::new(database.clone()));
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
        .route("/plugs/{address}/relay", post(set_relay))
        .route("/plugs/{address}/automations", get(get_automations))
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
            "/plugs/{address}/countdown",
            get(get_countdown).post(create_countdown),
        )
        .route(
            "/plugs/{address}/countdown/{id}",
            axum::routing::delete(delete_countdown),
        )
        .route("/plugs/{address}/schedules", post(create_schedule))
        .route(
            "/plugs/{address}/schedules/enabled",
            post(set_schedules_enabled),
        )
        .route(
            "/plugs/{address}/schedules/{id}",
            post(edit_schedule).delete(delete_schedule),
        )
        .route(
            "/plugs/{address}/schedules/{id}/solar",
            post(edit_solar_schedule),
        )
        .route(
            "/plugs/{address}/schedules/{id}/enabled",
            post(set_schedule_enabled),
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

async fn get_automations(
    State(state): State<Arc<AppState>>,
    Path(address): Path<IpAddr>,
) -> Result<Html<String>, AppError> {
    let plug = get_plug(state.client.clone(), address).await?;
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

async fn create_schedule(
    State(state): State<Arc<AppState>>,
    Path(address): Path<IpAddr>,
    Form(form): Form<ScheduleForm>,
) -> Result<Html<String>, AppError> {
    let input = ScheduleInput::try_from(form)?;
    let client = state.client.clone();
    let panel = task::spawn_blocking(move || {
        client.add_schedule_rule(address, &input.new_rule())?;
        load_schedule_panel(&client, address)
    })
    .await??;
    render_schedule_panel(&state, &panel)
}

async fn edit_schedule(
    State(state): State<Arc<AppState>>,
    Path((address, id)): Path<(IpAddr, String)>,
    Form(form): Form<ScheduleForm>,
) -> Result<Html<String>, AppError> {
    let input = ScheduleInput::try_from(form)?;
    let client = state.client.clone();
    let panel = task::spawn_blocking(move || {
        let rules = client.get_schedule_rules(address)?;
        let rule = find_schedule(rules, &id)?;
        if !is_editable_schedule(&rule) {
            return Err(AppError::bad_request(
                "only fixed-time weekly schedules can be edited here",
            ));
        }
        client.edit_schedule_rule(address, &input.apply_to(rule))?;
        load_schedule_panel(&client, address)
    })
    .await??;
    render_schedule_panel(&state, &panel)
}

async fn edit_solar_schedule(
    State(state): State<Arc<AppState>>,
    Path((address, id)): Path<(IpAddr, String)>,
    Form(form): Form<SolarScheduleForm>,
) -> Result<Html<String>, AppError> {
    let input = SolarScheduleInput::try_from(form)?;
    let client = state.client.clone();
    let panel = task::spawn_blocking(move || {
        let rules = client.get_schedule_rules(address)?;
        let rule = find_schedule(rules, &id)?;
        if !is_editable_solar_schedule(&rule) {
            return Err(AppError::bad_request(
                "only start-only sunrise and sunset schedules can be edited here",
            ));
        }
        client.edit_schedule_rule(address, &input.apply_to(rule))?;
        load_schedule_panel(&client, address)
    })
    .await??;
    render_schedule_panel(&state, &panel)
}

async fn delete_schedule(
    State(state): State<Arc<AppState>>,
    Path((address, id)): Path<(IpAddr, String)>,
) -> Result<Html<String>, AppError> {
    let client = state.client.clone();
    let panel = task::spawn_blocking(move || {
        client.delete_schedule_rule(address, &id)?;
        load_schedule_panel(&client, address)
    })
    .await??;
    render_schedule_panel(&state, &panel)
}

async fn set_schedule_enabled(
    State(state): State<Arc<AppState>>,
    Path((address, id)): Path<(IpAddr, String)>,
    Form(form): Form<RelayForm>,
) -> Result<Html<String>, AppError> {
    let client = state.client.clone();
    let panel = task::spawn_blocking(move || {
        let rules = client.get_schedule_rules(address)?;
        let mut rule = find_schedule(rules, &id)?;
        rule.enabled = form.on;
        client.edit_schedule_rule(address, &rule)?;
        load_schedule_panel(&client, address)
    })
    .await??;
    render_schedule_panel(&state, &panel)
}

async fn set_schedules_enabled(
    State(state): State<Arc<AppState>>,
    Path(address): Path<IpAddr>,
    Form(form): Form<RelayForm>,
) -> Result<Html<String>, AppError> {
    let client = state.client.clone();
    let panel = task::spawn_blocking(move || {
        client.set_schedules_enabled(address, form.on)?;
        load_schedule_panel(&client, address)
    })
    .await??;
    render_schedule_panel(&state, &panel)
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
    Ok(NewAutomation {
        device_id,
        trigger: AutomationTrigger::Solar {
            event,
            offset_minutes: form.offset_minutes,
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
    Ok(NewAutomation {
        device_id,
        trigger: AutomationTrigger::LightLevel {
            on_below: form.on_below,
            off_above: form.off_above,
        },
        turn_on: true,
    })
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

impl TryFrom<ScheduleForm> for ScheduleInput {
    type Error = AppError;

    fn try_from(form: ScheduleForm) -> Result<Self, Self::Error> {
        let name = validated_schedule_name(&form.name)?;
        let minute_of_day = parse_schedule_time(&form.time)?;
        let turn_on = match form.action.as_str() {
            "on" => true,
            "off" => false,
            _ => return Err(AppError::bad_request("schedule action must be on or off")),
        };
        let weekdays = schedule_weekdays([
            form.sun, form.mon, form.tue, form.wed, form.thu, form.fri, form.sat,
        ])?;

        Ok(Self {
            name,
            minute_of_day,
            turn_on,
            weekdays,
        })
    }
}

impl ScheduleInput {
    fn new_rule(&self) -> ScheduleRule {
        self.apply_to(ScheduleRule {
            id: None,
            name: String::new(),
            enabled: true,
            repeat: true,
            weekdays: [false; 7],
            stime_opt: 0,
            smin: 0,
            sact: 0,
            etime_opt: -1,
            emin: 0,
            eact: -1,
            soffset: None,
            eoffset: None,
            year: 0,
            month: 0,
            day: 0,
            latitude: 0.0,
            longitude: 0.0,
            force: 0,
            extra: Default::default(),
        })
    }

    fn apply_to(&self, mut rule: ScheduleRule) -> ScheduleRule {
        rule.name.clone_from(&self.name);
        rule.repeat = true;
        rule.weekdays = self.weekdays;
        rule.stime_opt = 0;
        rule.smin = self.minute_of_day;
        rule.sact = i8::from(self.turn_on);
        rule.etime_opt = -1;
        rule.emin = 0;
        rule.eact = -1;
        rule.soffset = None;
        rule.eoffset = None;
        rule.year = 0;
        rule.month = 0;
        rule.day = 0;
        rule
    }
}

impl TryFrom<SolarScheduleForm> for SolarScheduleInput {
    type Error = AppError;

    fn try_from(form: SolarScheduleForm) -> Result<Self, Self::Error> {
        let name = validated_schedule_name(&form.name)?;
        let event = match form.event.as_str() {
            "sunrise" => SolarEvent::Sunrise,
            "sunset" => SolarEvent::Sunset,
            _ => {
                return Err(AppError::bad_request(
                    "solar event must be sunrise or sunset",
                ))
            }
        };
        if !(-180..=180).contains(&form.offset_minutes) {
            return Err(AppError::bad_request(
                "solar offset must be between -180 and 180 minutes",
            ));
        }
        let turn_on = match form.action.as_str() {
            "on" => true,
            "off" => false,
            _ => return Err(AppError::bad_request("schedule action must be on or off")),
        };
        let weekdays = schedule_weekdays([
            form.sun, form.mon, form.tue, form.wed, form.thu, form.fri, form.sat,
        ])?;
        Ok(Self {
            name,
            event,
            offset_minutes: form.offset_minutes,
            turn_on,
            weekdays,
        })
    }
}

impl SolarScheduleInput {
    fn apply_to(&self, mut rule: ScheduleRule) -> ScheduleRule {
        rule.name.clone_from(&self.name);
        rule.repeat = true;
        rule.weekdays = self.weekdays;
        rule.stime_opt = match self.event {
            SolarEvent::Sunrise => 1,
            SolarEvent::Sunset => 2,
        };
        rule.sact = i8::from(self.turn_on);
        rule.soffset = Some(self.offset_minutes);
        rule
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
    let schedules = schedules??;
    Ok(AutomationPanel {
        address: plug.address.to_string(),
        location_available,
        weather,
        rules: rules.into_iter().map(automation_view).collect(),
        schedules,
    })
}

fn automation_view(rule: AutomationRule) -> AutomationView {
    match rule.trigger {
        AutomationTrigger::Solar {
            event,
            offset_minutes,
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
            AutomationView {
                id: rule.id,
                title,
                description: format!("Turn {} {timing}", if rule.turn_on { "on" } else { "off" }),
            }
        }
        AutomationTrigger::LightLevel {
            on_below,
            off_above,
        } => AutomationView {
            id: rule.id,
            title: "Outdoor light",
            description: format!(
                "Turn on at ≤ {on_below:.0} W/m² and off at ≥ {off_above:.0} W/m²"
            ),
        },
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

fn find_schedule(rules: RuleSet<ScheduleRule>, id: &str) -> Result<ScheduleRule, AppError> {
    rules
        .rules
        .into_iter()
        .find(|rule| rule.id.as_deref() == Some(id))
        .ok_or_else(|| AppError::not_found(format!("schedule {id} was not found")))
}

fn load_schedule_panel(
    client: &SmartHomeClient,
    address: IpAddr,
) -> Result<SchedulePanel, AppError> {
    let rules = client.get_schedule_rules(address)?;
    let address = address.to_string();
    Ok(SchedulePanel {
        address,
        enabled: rules.enabled,
        rules: rules.rules.into_iter().map(schedule_view).collect(),
    })
}

fn schedule_view(rule: ScheduleRule) -> ScheduleView {
    const WEEKDAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let selected_days: Vec<_> = rule
        .weekdays
        .iter()
        .zip(WEEKDAY_NAMES)
        .filter_map(|(selected, name)| selected.then_some(name))
        .collect();
    let editable = is_editable_schedule(&rule);
    let solar_editable = is_editable_solar_schedule(&rule);
    let (solar_event, event_name) = match rule.stime_opt {
        1 => ("sunrise", "sunrise"),
        2 => ("sunset", "sunset"),
        _ => ("", ""),
    };
    let time = match rule.stime_opt {
        0 if rule.smin < 24 * 60 => format!("{:02}:{:02}", rule.smin / 60, rule.smin % 60),
        1 | 2 => solar_schedule_time(event_name, rule.soffset.unwrap_or_default()),
        _ => "Solar/advanced".to_owned(),
    };

    ScheduleView {
        id: rule.id.unwrap_or_default(),
        name: rule.name,
        enabled: rule.enabled,
        editable,
        solar_editable,
        time,
        action: match rule.sact {
            1 => "Turn on",
            0 => "Turn off",
            _ => "Advanced action",
        },
        action_on: rule.sact == 1,
        solar_event,
        solar_offset: rule.soffset.unwrap_or_default(),
        weekday_summary: if rule.weekdays.iter().all(|selected| *selected) {
            "Every day".to_owned()
        } else if rule.weekdays == [false, true, true, true, true, true, false] {
            "Weekdays".to_owned()
        } else if selected_days.is_empty() {
            "No weekdays".to_owned()
        } else {
            selected_days.join(", ")
        },
        sun: rule.weekdays[0],
        mon: rule.weekdays[1],
        tue: rule.weekdays[2],
        wed: rule.weekdays[3],
        thu: rule.weekdays[4],
        fri: rule.weekdays[5],
        sat: rule.weekdays[6],
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

fn solar_schedule_time(event: &str, offset_minutes: i16) -> String {
    match offset_minutes.cmp(&0) {
        std::cmp::Ordering::Less => {
            format!("{} min before {event}", offset_minutes.unsigned_abs())
        }
        std::cmp::Ordering::Equal => event.to_owned(),
        std::cmp::Ordering::Greater => format!("{offset_minutes} min after {event}"),
    }
}

fn render_schedule_panel(
    state: &AppState,
    panel: &SchedulePanel,
) -> Result<Html<String>, AppError> {
    let fragment = state
        .templates
        .get_template("schedule-panel.html")?
        .render(context! { schedules => panel })?;
    Ok(Html(fragment))
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
    templates.add_template(
        "schedule-panel.html",
        include_str!("../templates/schedule-panel.html"),
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
        assert!(fragment.contains("name=\"on\" value=\"true\""));
        assert!(fragment.contains("name=\"on\" value=\"false\""));
        assert!(fragment.contains("hx-get=\"/groups/3\""));
        assert!(fragment.contains("&lt;Downstairs&gt;"));
        assert!(fragment.contains("Lamp &amp; fan"));
        assert!(fragment.contains("Some members are not in the remembered inventory"));
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
        assert!(page.contains("&lt;Desk lamp&gt;"));
        assert!(!page.contains("<Desk lamp>"));
    }

    #[test]
    fn automation_forms_validate_solar_offsets_and_light_hysteresis() {
        let solar = solar_automation(
            SolarAutomationForm {
                event: "sunset".to_owned(),
                offset_minutes: -30,
                action: "on".to_owned(),
            },
            "plug".to_owned(),
        )
        .unwrap();
        assert_eq!(
            solar.trigger,
            AutomationTrigger::Solar {
                event: SolarEvent::Sunset,
                offset_minutes: -30,
            }
        );
        assert!(solar.turn_on);
        assert!(solar_automation(
            SolarAutomationForm {
                event: "sunset".to_owned(),
                offset_minutes: 181,
                action: "on".to_owned(),
            },
            "plug".to_owned(),
        )
        .is_err());

        assert!(light_automation(
            LightAutomationForm {
                on_below: 75.0,
                off_above: 125.0,
            },
            "plug".to_owned(),
        )
        .is_ok());
        assert!(light_automation(
            LightAutomationForm {
                on_below: 125.0,
                off_above: 125.0,
            },
            "plug".to_owned(),
        )
        .is_err());
    }

    #[test]
    fn automation_panel_renders_persisted_rules_and_actions() {
        let panel = AutomationPanel {
            address: "192.0.2.1".to_owned(),
            location_available: true,
            weather: Some(WeatherStatus {
                local_time: "20:15".to_owned(),
                timezone: "GMT-4".to_owned(),
                condition: "Overcast",
                is_day: false,
                shortwave_radiation: 42.5,
                cloud_cover: 100,
                temperature: 13.4,
                apparent_temperature: 11.9,
                precipitation: 0.0,
                sunrise: "06:33".to_owned(),
                sunset: "20:19".to_owned(),
                previous_day_light: Some(LightHistory {
                    points: vec![
                        LightPoint {
                            x: 40.0,
                            y: 120.0,
                            time: "00:00".to_owned(),
                            radiation: 0.0,
                        },
                        LightPoint {
                            x: 176.0,
                            y: 12.0,
                            time: "12:00".to_owned(),
                            radiation: 500.0,
                        },
                    ],
                    average_points: vec![LightPoint {
                        x: 176.0,
                        y: 33.6,
                        time: "12:00".to_owned(),
                        radiation: 400.0,
                    }],
                    average_days: 30,
                    max_radiation: 500,
                    mid_radiation: 250,
                    sunrise_x: 114.8,
                    sunset_x: 270.1,
                    sunrise: "06:36".to_owned(),
                    sunset: "20:18".to_owned(),
                }),
            }),
            rules: vec![AutomationView {
                id: 7,
                title: "Sunset",
                description: "Turn on 30 min before sunset".to_owned(),
            }],
            schedules: SchedulePanel {
                address: "192.0.2.1".to_owned(),
                enabled: true,
                rules: Vec::new(),
            },
        };

        let fragment = templates()
            .unwrap()
            .get_template("automation-panel.html")
            .unwrap()
            .render(context! { panel })
            .unwrap();

        assert!(fragment.contains("Turn on 30 min before sunset"));
        assert!(fragment.contains("20:15 GMT-4"));
        assert!(fragment.contains("42.5 W/m²"));
        assert!(fragment.contains("Overcast"));
        assert!(fragment.contains("Yesterday's outdoor light"));
        assert!(fragment.contains("30-day hourly average"));
        assert!(fragment.contains("class=\"light-average-line\""));
        assert!(fragment.contains("class=\"light-line\""));
        assert!(fragment.contains("class=\"solar-line sunrise-line\" x1=\"114.8\""));
        assert!(fragment.contains("Sunrise 06:36"));
        assert!(fragment.contains("Sunset 20:18"));
        assert!(fragment.contains("id=\"schedule-panel\""));
        assert!(fragment.contains("Device schedules"));
        assert!(fragment.contains("Weather rules"));
        assert!(fragment.contains("role=\"separator\""));
        assert!(fragment.contains("aria-label=\"Resize automation pane\""));
        assert!(fragment.contains("hx-post=\"/plugs/192.0.2.1/automations/solar\""));
        assert!(fragment.contains("hx-post=\"/plugs/192.0.2.1/automations/light\""));
        assert!(fragment.contains("hx-delete=\"/plugs/192.0.2.1/automations/7\""));
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
        assert!(fragment.contains("&lt;Web timer&gt;"));
        assert!(!fragment.contains("<Web timer>"));
    }

    #[test]
    fn schedule_form_validates_time_and_weekdays() {
        assert_eq!(parse_schedule_time("07:15").unwrap(), 435);
        assert!(parse_schedule_time("7:15").is_err());
        assert!(parse_schedule_time("23:60").is_err());

        let invalid_time = ScheduleForm {
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
        };
        assert!(ScheduleInput::try_from(invalid_time).is_err());

        let no_weekdays = ScheduleForm {
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
        };
        assert!(ScheduleInput::try_from(no_weekdays).is_err());
    }

    #[test]
    fn fixed_schedule_input_builds_weekly_protocol_rule() {
        let input = ScheduleInput {
            name: "Weekday morning".to_owned(),
            minute_of_day: 450,
            turn_on: true,
            weekdays: [false, true, true, true, true, true, false],
        };

        let rule = input.new_rule();
        assert_eq!(rule.name, "Weekday morning");
        assert_eq!(rule.smin, 450);
        assert_eq!(rule.sact, 1);
        assert_eq!(rule.etime_opt, -1);
        assert!(rule.enabled);
        assert_eq!(rule.weekdays, input.weekdays);
    }

    #[test]
    fn schedule_panel_renders_htmx_actions_and_escapes_names() {
        let panel = SchedulePanel {
            address: "192.0.2.1".to_owned(),
            enabled: true,
            rules: vec![ScheduleView {
                id: "rule-id".to_owned(),
                name: "<Morning>".to_owned(),
                enabled: true,
                editable: true,
                solar_editable: false,
                time: "07:30".to_owned(),
                action: "Turn on",
                action_on: true,
                solar_event: "",
                solar_offset: 0,
                weekday_summary: "Mon, Tue".to_owned(),
                sun: false,
                mon: true,
                tue: true,
                wed: false,
                thu: false,
                fri: false,
                sat: false,
            }],
        };

        let fragment = templates()
            .unwrap()
            .get_template("schedule-panel.html")
            .unwrap()
            .render(context! { schedules => panel })
            .unwrap();

        assert!(fragment.contains("hx-post=\"/plugs/192.0.2.1/schedules/rule-id\""));
        assert!(fragment.contains("hx-delete=\"/plugs/192.0.2.1/schedules/rule-id\""));
        assert!(fragment.contains("id=\"schedule-panel\""));
        assert!(fragment.contains("hx-target=\"#schedule-panel\""));
        assert!(!fragment.contains("<dialog"));
        assert!(fragment.contains("name=\"time\" type=\"time\" required value=\"07:30\""));
        assert!(!fragment.contains("name=\"hour\""));
        assert!(!fragment.contains("name=\"minute\""));
        assert!(fragment.contains("name=\"mon\" checked>Mon"));
        assert!(fragment.contains("name=\"sun\" >Sun"));
        assert!(fragment.contains("&lt;Morning&gt;"));
        assert!(!fragment.contains("<Morning>"));
    }

    #[test]
    fn solar_schedule_edit_preserves_unexposed_firmware_fields() {
        let mut extra = serde_json::Map::new();
        extra.insert("firmware_field".to_owned(), serde_json::json!(7));
        let rule = ScheduleRule {
            id: Some("solar-id".to_owned()),
            name: "Schedule Rule".to_owned(),
            enabled: false,
            repeat: true,
            weekdays: [true; 7],
            stime_opt: 2,
            smin: 1_184,
            sact: 1,
            etime_opt: -1,
            emin: 17,
            eact: -1,
            soffset: Some(-30),
            eoffset: Some(9),
            year: 2026,
            month: 8,
            day: 24,
            latitude: 46.4,
            longitude: -81.0,
            force: 3,
            extra,
        };
        let input = SolarScheduleInput::try_from(SolarScheduleForm {
            name: "Morning light".to_owned(),
            event: "sunrise".to_owned(),
            offset_minutes: 15,
            action: "off".to_owned(),
            sun: None,
            mon: Some("on".to_owned()),
            tue: Some("on".to_owned()),
            wed: Some("on".to_owned()),
            thu: Some("on".to_owned()),
            fri: Some("on".to_owned()),
            sat: None,
        })
        .unwrap();

        let updated = input.apply_to(rule);

        assert_eq!(updated.name, "Morning light");
        assert_eq!(updated.stime_opt, 1);
        assert_eq!(updated.soffset, Some(15));
        assert_eq!(updated.sact, 0);
        assert_eq!(
            updated.weekdays,
            [false, true, true, true, true, true, false]
        );
        assert!(!updated.enabled);
        assert_eq!(updated.smin, 1_184);
        assert_eq!(updated.emin, 17);
        assert_eq!(updated.eoffset, Some(9));
        assert_eq!(updated.year, 2026);
        assert_eq!(updated.force, 3);
        assert_eq!(updated.extra["firmware_field"], serde_json::json!(7));
    }

    #[test]
    fn solar_schedule_view_describes_and_edits_sunset_offset() {
        let rule = ScheduleRule {
            id: Some("solar-id".to_owned()),
            name: "Schedule Rule".to_owned(),
            enabled: true,
            repeat: true,
            weekdays: [true; 7],
            stime_opt: 2,
            smin: 1_184,
            sact: 1,
            etime_opt: -1,
            emin: 0,
            eact: -1,
            soffset: Some(-30),
            eoffset: Some(0),
            year: 0,
            month: 0,
            day: 0,
            latitude: 0.0,
            longitude: 0.0,
            force: 0,
            extra: Default::default(),
        };
        let panel = SchedulePanel {
            address: "192.0.2.1".to_owned(),
            enabled: true,
            rules: vec![schedule_view(rule)],
        };

        let fragment = templates()
            .unwrap()
            .get_template("schedule-panel.html")
            .unwrap()
            .render(context! { schedules => panel })
            .unwrap();

        assert!(fragment.contains("30 min before sunset · Turn on · Every day"));
        assert!(fragment.contains("Edit solar schedule"));
        assert!(fragment.contains("hx-post=\"/plugs/192.0.2.1/schedules/solar-id/solar\""));
        assert!(fragment.contains("value=\"sunset\" selected"));
        assert!(fragment.contains("name=\"offset_minutes\""));
        assert!(fragment.contains("value=\"-30\""));
        assert!(!fragment.contains("not rewritten"));
    }
}
