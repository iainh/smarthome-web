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
use std::sync::Arc;
use std::time::Duration;
use tddp_client::{RuleSet, ScheduleRule, SmartHomeClient, SmartPlug};
use tokio::task;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

struct AppState {
    client: SmartHomeClient,
    templates: Environment<'static>,
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
async fn main() -> Result<(), Box<dyn StdError>> {
    let state = Arc::new(AppState {
        client: SmartHomeClient::new(),
        templates: templates()?,
    });
    let app = Router::new()
        .route("/", get(index))
        .route("/refresh", post(refresh))
        .route("/plugs/{address}/relay", post(set_relay))
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
    let plugs = discover(state.client.clone()).await?;
    let page = state
        .templates
        .get_template("index.html")?
        .render(context! { plugs })?;
    Ok(Html(page))
}

async fn refresh(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    let plugs = discover(state.client.clone()).await?;
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

async fn discover(client: SmartHomeClient) -> Result<Vec<PlugView>, AppError> {
    let plugs = task::spawn_blocking(move || client.get_inventory(DISCOVERY_TIMEOUT)).await??;
    Ok(plugs.into_iter().map(PlugView::from).collect())
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
        weekday_summary: if selected_days.is_empty() {
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
        "schedule-panel.html",
        include_str!("../templates/schedule-panel.html"),
    )?;
    Ok(templates)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(page.contains("hx-target=\"#schedule-pane\""));
        assert!(page.contains("id=\"schedule-pane\""));
        assert!(page.contains("&lt;Desk lamp&gt;"));
        assert!(!page.contains("<Desk lamp>"));
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
        assert!(fragment.contains("id=\"schedule-pane\""));
        assert!(fragment.contains("class=\"schedule-pane is-open\""));
        assert!(fragment.contains("hx-target=\"closest .schedule-pane\""));
        assert!(fragment.contains("&lt;Morning&gt;"));
        assert!(!fragment.contains("<Morning>"));
    }
}
