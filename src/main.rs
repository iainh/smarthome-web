mod automation;

use automation::{
    AutomationEngine, AutomationRule, AutomationTrigger, NewAutomation, SolarEvent, WeatherStatus,
};
use axum::extract::{Form, Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use minijinja::{context, AutoEscape, Environment};
use serde::{Deserialize, Serialize};
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

struct AppState {
    client: SmartHomeClient,
    templates: Environment<'static>,
    automations: Arc<AutomationEngine>,
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
    hour: u8,
    minute: u8,
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
    time: String,
    action: &'static str,
    action_on: bool,
    weekday_summary: String,
    hour: u16,
    minute: u16,
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
    let automation_path = std::env::var_os("AUTOMATIONS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("automations.json"));
    let device_addresses = match std::env::var("DEVICE_ADDRESSES") {
        Ok(value) => parse_device_addresses(&value)?,
        Err(std::env::VarError::NotPresent) => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let automations = Arc::new(AutomationEngine::load(automation_path)?);
    let state = Arc::new(AppState {
        client: SmartHomeClient::new(),
        templates: templates()?,
        automations: automations.clone(),
        device_addresses: device_addresses.clone(),
    });
    tokio::spawn(automations.run(state.client.clone(), device_addresses));
    let app = Router::new()
        .route("/", get(index))
        .route("/refresh", post(refresh))
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
        .route(
            "/plugs/{address}/schedules",
            get(get_schedules).post(create_schedule),
        )
        .route(
            "/plugs/{address}/schedules/enabled",
            post(set_schedules_enabled),
        )
        .route(
            "/plugs/{address}/schedules/{id}",
            post(edit_schedule).delete(delete_schedule),
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
    let plugs = discover(state.client.clone(), state.device_addresses.clone()).await?;
    let page = state
        .templates
        .get_template("index.html")?
        .render(context! { plugs })?;
    Ok(Html(page))
}

async fn refresh(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    let plugs = discover(state.client.clone(), state.device_addresses.clone()).await?;
    let fragment = state
        .templates
        .get_template("plug-list.html")?
        .render(context! { plugs })?;
    Ok(Html(fragment))
}

async fn set_relay(
    State(state): State<Arc<AppState>>,
    Path(address): Path<IpAddr>,
    Form(form): Form<RelayForm>,
) -> Result<Html<String>, AppError> {
    let client = state.client.clone();
    let plug = task::spawn_blocking(move || {
        client.set_relay(address, form.on)?;
        client.get_sysinfo(address)
    })
    .await??;
    let plug = PlugView::from(plug);
    let fragment = state
        .templates
        .get_template("plug.html")?
        .render(context! { plug })?;
    Ok(Html(fragment))
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

async fn get_schedules(
    State(state): State<Arc<AppState>>,
    Path(address): Path<IpAddr>,
) -> Result<Html<String>, AppError> {
    let client = state.client.clone();
    let panel = task::spawn_blocking(move || load_schedule_panel(&client, address)).await??;
    render_schedule_panel(&state, &panel)
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

async fn discover(
    client: SmartHomeClient,
    device_addresses: Vec<IpAddr>,
) -> Result<Vec<PlugView>, AppError> {
    let plugs = task::spawn_blocking(move || {
        client.get_inventory_from(&device_addresses, DISCOVERY_TIMEOUT)
    })
    .await??;
    Ok(plugs.into_iter().map(PlugView::from).collect())
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
        let name = form.name.trim().to_owned();
        if name.is_empty() {
            return Err(AppError::bad_request("schedule name cannot be empty"));
        }
        if name.chars().count() > 64 {
            return Err(AppError::bad_request(
                "schedule name cannot exceed 64 characters",
            ));
        }
        if form.hour > 23 || form.minute > 59 {
            return Err(AppError::bad_request(
                "schedule time must be between 00:00 and 23:59",
            ));
        }
        let turn_on = match form.action.as_str() {
            "on" => true,
            "off" => false,
            _ => return Err(AppError::bad_request("schedule action must be on or off")),
        };
        let weekdays = [
            form.sun.is_some(),
            form.mon.is_some(),
            form.tue.is_some(),
            form.wed.is_some(),
            form.thu.is_some(),
            form.fri.is_some(),
            form.sat.is_some(),
        ];
        if !weekdays.iter().any(|selected| *selected) {
            return Err(AppError::bad_request(
                "select at least one weekday for the schedule",
            ));
        }

        Ok(Self {
            name,
            minute_of_day: u16::from(form.hour) * 60 + u16::from(form.minute),
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

async fn load_automation_panel(
    state: &AppState,
    plug: &SmartPlug,
) -> Result<AutomationPanel, AppError> {
    let rules = state
        .automations
        .rules_for(&plug.device_id)
        .map_err(automation_error)?;
    let location_available = plug.latitude.is_some() && plug.longitude.is_some();
    let weather = if location_available {
        match state.automations.weather_status(plug).await {
            Ok(weather) => Some(weather),
            Err(error) => {
                eprintln!("could not load current weather: {error}");
                None
            }
        }
    } else {
        None
    };
    Ok(AutomationPanel {
        address: plug.address.to_string(),
        location_available,
        weather,
        rules: rules.into_iter().map(automation_view).collect(),
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
    let time = if rule.stime_opt == 0 && rule.smin < 24 * 60 {
        format!("{:02}:{:02}", rule.smin / 60, rule.smin % 60)
    } else {
        "Solar/advanced".to_owned()
    };

    ScheduleView {
        id: rule.id.unwrap_or_default(),
        name: rule.name,
        enabled: rule.enabled,
        editable,
        time,
        action: match rule.sact {
            1 => "Turn on",
            0 => "Turn off",
            _ => "Advanced action",
        },
        action_on: rule.sact == 1,
        weekday_summary: if rule.weekdays.iter().all(|selected| *selected) {
            "Every day".to_owned()
        } else if rule.weekdays == [false, true, true, true, true, true, false] {
            "Weekdays".to_owned()
        } else if selected_days.is_empty() {
            "No weekdays".to_owned()
        } else {
            selected_days.join(", ")
        },
        hour: rule.smin / 60,
        minute: rule.smin % 60,
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

fn render_schedule_panel(
    state: &AppState,
    panel: &SchedulePanel,
) -> Result<Html<String>, AppError> {
    let fragment = state
        .templates
        .get_template("schedule-panel.html")?
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

        assert!(page.contains("hx-post=\"/refresh\""));
        assert!(page.contains("hx-post=\"/plugs/192.0.2.1/relay\""));
        assert!(page.contains("hx-get=\"/plugs/192.0.2.1/automations\""));
        assert!(page.contains("hx-get=\"/plugs/192.0.2.1/countdown\""));
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
        assert!(fragment.contains("class=\"light-line\""));
        assert!(fragment.contains("class=\"solar-line sunrise-line\" x1=\"114.8\""));
        assert!(fragment.contains("Sunrise 06:36"));
        assert!(fragment.contains("Sunset 20:18"));
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
        let invalid_time = ScheduleForm {
            name: "Morning".to_owned(),
            hour: 24,
            minute: 0,
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
            hour: 7,
            minute: 30,
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
                time: "07:30".to_owned(),
                action: "Turn on",
                action_on: true,
                weekday_summary: "Mon, Tue".to_owned(),
                hour: 7,
                minute: 30,
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
            .render(context! { panel })
            .unwrap();

        assert!(fragment.contains("hx-post=\"/plugs/192.0.2.1/schedules/rule-id\""));
        assert!(fragment.contains("hx-delete=\"/plugs/192.0.2.1/schedules/rule-id\""));
        assert!(fragment.contains("id=\"device-pane\""));
        assert!(fragment.contains("<dialog id=\"device-pane\" class=\"device-pane\""));
        assert!(fragment.contains("hx-target=\"closest .device-pane\""));
        assert!(fragment.contains("&lt;Morning&gt;"));
        assert!(!fragment.contains("<Morning>"));
    }
}
