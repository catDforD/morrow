use agent_config::{AmapConfig, LarkConfig, QWeatherConfig, RobotConfig};
use agent_runtime::LarkToolConfig;
use chrono::{DateTime, Datelike, Duration as ChronoDuration, FixedOffset, TimeZone, Timelike};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tokio::time;

const ROBOT_TICK_SECS: u64 = 60;
const PROCESS_TIMEOUT_SECS: u64 = 30;
const MAX_PROCESS_OUTPUT_BYTES: usize = 64_000;

#[derive(Debug, Clone)]
pub struct RobotServerOptions {
    pub robot: RobotConfig,
    pub lark: LarkConfig,
    pub qweather: QWeatherConfig,
    pub amap: AmapConfig,
    pub lark_tools: LarkToolConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RobotNotice {
    pub id: String,
    pub timestamp_ms: u64,
    pub kind: RobotNoticeKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RobotNoticeKind {
    MeetingReminder,
    FieldworkReminder,
    TravelReminder,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RobotDoctorReport {
    pub ok: bool,
    pub checks: Vec<RobotDoctorCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RobotDoctorCheck {
    pub name: String,
    pub ok: bool,
    pub message: String,
}

#[derive(Debug)]
pub struct RobotScheduler {
    options: RobotServerOptions,
    state_path: PathBuf,
    state: RobotState,
    client: Client,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct RobotState {
    sent_keys: HashSet<String>,
    last_travel_check_date: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct CalendarEvent {
    event_id: String,
    summary: String,
    start: DateTime<FixedOffset>,
    end: Option<DateTime<FixedOffset>>,
    location: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RoutePlan {
    duration_minutes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WeatherBrief {
    text: String,
}

impl RobotScheduler {
    pub fn new(options: RobotServerOptions, state_path: PathBuf) -> Self {
        let state = load_state(&state_path).unwrap_or_default();
        Self {
            options,
            state_path,
            state,
            client: Client::new(),
        }
    }

    pub async fn tick(&mut self, now: DateTime<FixedOffset>) -> Vec<RobotNotice> {
        if !self.options.robot.enabled {
            return Vec::new();
        }

        let events = match self.load_events(now, now + ChronoDuration::hours(24)).await {
            Ok(events) => events,
            Err(_) => return Vec::new(),
        };
        let mut notices = Vec::new();
        notices.extend(self.meeting_notices(now, &events));
        notices.extend(self.fieldwork_notices(now, &events).await);
        notices.extend(self.travel_notices(now, &events).await);
        if !notices.is_empty() {
            let _ = save_state(&self.state_path, &self.state);
        }
        notices
    }

    async fn load_events(
        &self,
        start: DateTime<FixedOffset>,
        end: DateTime<FixedOffset>,
    ) -> Result<Vec<CalendarEvent>, String> {
        let start_text = start.to_rfc3339();
        let end_text = end.to_rfc3339();
        let output = run_lark_json(
            &self.options.lark.cli_path,
            &[
                "calendar",
                "+agenda",
                "--as",
                &self.options.lark.calendar_identity,
                "--calendar-id",
                &self.options.lark.calendar_id,
                "--start",
                &start_text,
                "--end",
                &end_text,
                "--format",
                "json",
            ],
        )?;
        Ok(parse_calendar_events(&output, *start.offset()))
    }

    fn meeting_notices(
        &mut self,
        now: DateTime<FixedOffset>,
        events: &[CalendarEvent],
    ) -> Vec<RobotNotice> {
        let mut notices = Vec::new();
        for event in events.iter().filter(|event| is_meeting(event)) {
            let minutes_until = (event.start - now).num_minutes();
            if minutes_until < 0 {
                continue;
            }
            for reminder in &self.options.robot.meeting_reminder_minutes {
                let reminder = i64::from(*reminder);
                if (minutes_until - reminder).abs() <= 1 {
                    let key = format!("meeting:{}:{reminder}", event.event_id);
                    if self.state.sent_keys.insert(key.clone()) {
                        notices.push(notice(
                            key,
                            RobotNoticeKind::MeetingReminder,
                            now,
                            sanitize_tts_text(format!(
                                "你有一个会议将在{reminder}分钟后开始，主题是{}。请提前准备。",
                                event.summary
                            )),
                        ));
                    }
                }
            }
        }
        notices
    }

    async fn fieldwork_notices(
        &mut self,
        now: DateTime<FixedOffset>,
        events: &[CalendarEvent],
    ) -> Vec<RobotNotice> {
        let mut notices = Vec::new();
        for event in events.iter().filter(|event| is_fieldwork(event)) {
            let minutes_until = (event.start - now).num_minutes();
            let reminder = i64::from(self.options.robot.fieldwork_reminder_minutes);
            if minutes_until < 0 || (minutes_until - reminder).abs() > 1 {
                continue;
            }
            let key = format!("fieldwork:{}", event.event_id);
            if !self.state.sent_keys.insert(key.clone()) {
                continue;
            }
            let destination = event
                .location
                .as_deref()
                .filter(|location| !location.trim().is_empty())
                .unwrap_or(&event.summary);
            let route = self
                .route_plan(&self.options.robot.default_origin, destination)
                .await
                .ok();
            let weather = self.weather_brief(destination).await.ok();
            notices.push(notice(
                key,
                RobotNoticeKind::FieldworkReminder,
                now,
                sanitize_tts_text(render_fieldwork_notice(event, route, weather)),
            ));
        }
        notices
    }

    async fn travel_notices(
        &mut self,
        now: DateTime<FixedOffset>,
        events: &[CalendarEvent],
    ) -> Vec<RobotNotice> {
        let workday_end = parse_workday_end(&self.options.robot.workday_end_time);
        if now.hour() != workday_end.0 || now.minute() != workday_end.1 {
            return Vec::new();
        }
        let today_key = now.format("%Y-%m-%d").to_string();
        if self.state.last_travel_check_date.as_deref() == Some(&today_key) {
            return Vec::new();
        }
        self.state.last_travel_check_date = Some(today_key.clone());

        let tomorrow = (now + ChronoDuration::days(1)).date_naive();
        let travel_events = events
            .iter()
            .filter(|event| event.start.date_naive() == tomorrow && is_travel(event))
            .collect::<Vec<_>>();
        if travel_events.is_empty() {
            return Vec::new();
        }

        let event = travel_events[0];
        let destination = event.location.as_deref().unwrap_or(&event.summary);
        let weather = self.weather_brief(destination).await.ok();
        let key = format!("travel:{today_key}");
        if !self.state.sent_keys.insert(key.clone()) {
            return Vec::new();
        }
        vec![notice(
            key,
            RobotNoticeKind::TravelReminder,
            now,
            sanitize_tts_text(render_travel_notice(event, weather)),
        )]
    }

    async fn route_plan(&self, origin: &str, destination: &str) -> Result<RoutePlan, String> {
        let key = amap_key(&self.options.amap)?;
        let origin = self.geocode_amap(origin, &key).await?;
        let destination = self.geocode_amap(destination, &key).await?;
        let url = format!("{}/v5/direction/driving", self.options.amap.base_url);
        let value = self
            .client
            .get(url)
            .query(&[
                ("key", key.as_str()),
                ("origin", origin.as_str()),
                ("destination", destination.as_str()),
                ("show_fields", "cost"),
            ])
            .send()
            .await
            .map_err(|err| format!("amap route request failed: {err}"))?
            .json::<Value>()
            .await
            .map_err(|err| format!("amap route JSON failed: {err}"))?;
        Ok(RoutePlan {
            duration_minutes: parse_amap_route_duration_minutes(&value)?,
        })
    }

    async fn geocode_amap(&self, address: &str, key: &str) -> Result<String, String> {
        let url = format!("{}/v3/geocode/geo", self.options.amap.base_url);
        let value = self
            .client
            .get(url)
            .query(&[("key", key), ("address", address)])
            .send()
            .await
            .map_err(|err| format!("amap geocode request failed: {err}"))?
            .json::<Value>()
            .await
            .map_err(|err| format!("amap geocode JSON failed: {err}"))?;
        value
            .pointer("/geocodes/0/location")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "amap geocode response did not include a location".to_string())
    }

    async fn weather_brief(&self, location: &str) -> Result<WeatherBrief, String> {
        let token = qweather_token(&self.options.qweather)?;
        let lookup_url = format!("{}/geo/v2/city/lookup", self.options.qweather.base_url);
        let lookup = self
            .client
            .get(lookup_url)
            .query(&[("location", location), ("key", token.as_str())])
            .send()
            .await
            .map_err(|err| format!("qweather lookup request failed: {err}"))?
            .json::<Value>()
            .await
            .map_err(|err| format!("qweather lookup JSON failed: {err}"))?;
        let location_id = lookup
            .pointer("/location/0/id")
            .and_then(Value::as_str)
            .ok_or_else(|| "qweather lookup response did not include location id".to_string())?;
        let weather_url = format!("{}/v7/weather/3d", self.options.qweather.base_url);
        let weather = self
            .client
            .get(weather_url)
            .query(&[("location", location_id), ("key", token.as_str())])
            .send()
            .await
            .map_err(|err| format!("qweather request failed: {err}"))?
            .json::<Value>()
            .await
            .map_err(|err| format!("qweather JSON failed: {err}"))?;
        let daily = weather
            .pointer("/daily/0")
            .ok_or_else(|| "qweather response did not include daily forecast".to_string())?;
        let text = format!(
            "{}，{}到{}度",
            daily
                .get("textDay")
                .and_then(Value::as_str)
                .unwrap_or("天气未知"),
            daily
                .get("tempMin")
                .and_then(Value::as_str)
                .unwrap_or("未知"),
            daily
                .get("tempMax")
                .and_then(Value::as_str)
                .unwrap_or("未知")
        );
        Ok(WeatherBrief { text })
    }
}

pub async fn run_scheduler_loop<F>(mut scheduler: RobotScheduler, mut on_notice: F)
where
    F: FnMut(RobotNotice) + Send + 'static,
{
    let offset = FixedOffset::east_opt(8 * 3600).expect("valid offset");
    let mut interval = time::interval(Duration::from_secs(ROBOT_TICK_SECS));
    loop {
        interval.tick().await;
        let now = chrono::Utc::now().with_timezone(&offset);
        for notice in scheduler.tick(now).await {
            on_notice(notice);
        }
    }
}

pub async fn robot_doctor(options: RobotServerOptions) -> RobotDoctorReport {
    let checks = vec![
        check_process(
            "lark_cli",
            run_lark_json(&options.lark.cli_path, &["--version"]),
        ),
        check_process(
            "lark_doctor",
            run_lark_json(&options.lark.cli_path, &["doctor"]),
        ),
        check_process(
            "lark_auth_status",
            run_lark_json(&options.lark.cli_path, &["auth", "status"]),
        ),
        check_process(
            "lark_user_scopes",
            run_lark_json(
                &options.lark.cli_path,
                &[
                    "auth",
                    "check",
                    "--json",
                    "--scope",
                    "calendar:calendar:read calendar:calendar.event:read calendar:calendar.event:create calendar:calendar.event:update contact:user:search im:chat:read im:chat.members:read",
                ],
            ),
        ),
        check_process(
            "lark_user_message_scope",
            run_lark_json(
                &options.lark.cli_path,
                &[
                    "auth",
                    "check",
                    "--json",
                    "--scope",
                    "im:message im:message.send_as_user",
                ],
            ),
        ),
        check_secret_config(
            "qweather_token",
            options.qweather.token.as_deref(),
            &options.qweather.token_env,
            "[qweather].token",
        ),
        check_secret_config(
            "amap_key",
            options.amap.key.as_deref(),
            &options.amap.key_env,
            "[amap].key",
        ),
    ];

    RobotDoctorReport {
        ok: checks.iter().all(|check| check.ok),
        checks,
    }
}

fn check_process(name: &str, result: Result<Value, String>) -> RobotDoctorCheck {
    match result {
        Ok(value) => RobotDoctorCheck {
            name: name.to_string(),
            ok: true,
            message: summarize_json(&value),
        },
        Err(error) => RobotDoctorCheck {
            name: name.to_string(),
            ok: false,
            message: error,
        },
    }
}

fn qweather_token(config: &QWeatherConfig) -> Result<String, String> {
    configured_secret(
        config.token.as_deref(),
        &config.token_env,
        "[qweather].token",
    )
}

fn amap_key(config: &AmapConfig) -> Result<String, String> {
    configured_secret(config.key.as_deref(), &config.key_env, "[amap].key")
}

fn configured_secret(
    direct_value: Option<&str>,
    env_var: &str,
    config_field: &str,
) -> Result<String, String> {
    if let Some(value) = direct_value
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(value.to_string());
    }
    match std::env::var(env_var) {
        Ok(value) if !value.trim().is_empty() => Ok(value.trim().to_string()),
        _ => Err(format!("set {config_field} or {env_var}")),
    }
}

fn check_secret_config(
    name: &str,
    direct_value: Option<&str>,
    env_var: &str,
    config_field: &str,
) -> RobotDoctorCheck {
    if direct_value
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
    {
        return RobotDoctorCheck {
            name: name.to_string(),
            ok: true,
            message: format!("{config_field} is configured in morrow.toml"),
        };
    }
    match std::env::var(env_var) {
        Ok(value) if !value.trim().is_empty() => RobotDoctorCheck {
            name: name.to_string(),
            ok: true,
            message: format!("{env_var} is set"),
        },
        _ => RobotDoctorCheck {
            name: name.to_string(),
            ok: false,
            message: format!("set {config_field} or {env_var}"),
        },
    }
}

fn summarize_json(value: &Value) -> String {
    if let Some(ok) = value.get("ok").and_then(Value::as_bool) {
        return format!("ok={ok}");
    }
    if let Some(version) = value.get("version").and_then(Value::as_str) {
        return version.to_string();
    }
    "completed".to_string()
}

fn parse_amap_route_duration_minutes(value: &Value) -> Result<u32, String> {
    let duration_secs = value
        .pointer("/route/paths/0/duration")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .pointer("/route/paths/0/cost/duration")
                .and_then(Value::as_str)
        })
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| "amap route response did not include duration".to_string())?;
    Ok(duration_secs.div_ceil(60))
}

fn run_lark_json(program: &str, args: &[&str]) -> Result<Value, String> {
    let output = run_process(program, args, Duration::from_secs(PROCESS_TIMEOUT_SECS))?;
    if !output.status_success {
        return Err(format!(
            "exit_code={}: {}{}",
            output
                .exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "none".to_string()),
            output.stderr.trim(),
            if output.stdout.trim().is_empty() {
                String::new()
            } else {
                format!("; stdout: {}", output.stdout.trim())
            }
        ));
    }
    parse_json_or_text(&output.stdout)
}

#[derive(Debug)]
struct ProcessOutput {
    exit_code: Option<i32>,
    status_success: bool,
    stdout: String,
    stderr: String,
}

fn run_process(program: &str, args: &[&str], timeout: Duration) -> Result<ProcessOutput, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to spawn {program}: {err}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("failed to capture {program} stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("failed to capture {program} stderr"))?;
    let stdout_reader = std::thread::spawn(move || read_limited(stdout));
    let stderr_reader = std::thread::spawn(move || read_limited(stderr));
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child
            .try_wait()
            .map_err(|err| format!("failed to wait for {program}: {err}"))?
        {
            Some(status) => break status,
            None if started.elapsed() >= timeout => {
                timed_out = true;
                let _ = child.kill();
                break child
                    .wait()
                    .map_err(|err| format!("failed to wait for killed {program}: {err}"))?;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| format!("failed to join {program} stdout reader"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| format!("failed to join {program} stderr reader"))??;
    if timed_out {
        return Err(format!("{program} timed out after {}s", timeout.as_secs()));
    }
    Ok(ProcessOutput {
        exit_code: status.code(),
        status_success: status.success(),
        stdout,
        stderr,
    })
}

fn read_limited(mut reader: impl Read) -> Result<String, String> {
    let mut buffer = [0_u8; 8192];
    let mut output = Vec::new();
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|err| format!("failed to read process output: {err}"))?;
        if read == 0 {
            break;
        }
        let remaining = MAX_PROCESS_OUTPUT_BYTES.saturating_sub(output.len());
        if remaining > 0 {
            output.extend_from_slice(&buffer[..read.min(remaining)]);
        }
        if read > remaining {
            return Err("process output exceeded capture limit".to_string());
        }
    }
    Ok(String::from_utf8_lossy(&output).to_string())
}

fn parse_json_or_text(output: &str) -> Result<Value, String> {
    let output = output.trim();
    let Some(start) = output.find(['{', '[']) else {
        return Ok(Value::String(output.to_string()));
    };
    serde_json::from_str(&output[start..])
        .map_err(|err| format!("failed to parse JSON output: {err}"))
}

fn parse_calendar_events(value: &Value, offset: FixedOffset) -> Vec<CalendarEvent> {
    let items = if let Some(items) = value.get("data").and_then(Value::as_array) {
        items
    } else if let Some(items) = value.as_array() {
        items
    } else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| parse_calendar_event(item, offset))
        .collect()
}

fn parse_calendar_event(value: &Value, offset: FixedOffset) -> Option<CalendarEvent> {
    let summary = get_string(value, &["summary", "title", "subject"])?;
    let event_id = get_string(value, &["event_id", "id"]).unwrap_or_else(|| summary.clone());
    let start_value = value
        .get("start_time")
        .or_else(|| value.get("start"))
        .or_else(|| value.get("startTime"))?;
    let start = parse_event_time(start_value, offset)?;
    let end = value
        .get("end_time")
        .or_else(|| value.get("end"))
        .or_else(|| value.get("endTime"))
        .and_then(|value| parse_event_time(value, offset));
    let location = value
        .get("location")
        .and_then(|location| {
            if let Some(text) = location.as_str() {
                Some(text.to_string())
            } else {
                get_string(location, &["name", "address"])
            }
        })
        .filter(|location| !location.trim().is_empty());
    Some(CalendarEvent {
        event_id,
        summary,
        start,
        end,
        location,
    })
}

fn parse_event_time(value: &Value, offset: FixedOffset) -> Option<DateTime<FixedOffset>> {
    if let Some(text) = value.as_str() {
        return DateTime::parse_from_rfc3339(text).ok();
    }
    if let Some(datetime) = value.get("datetime").and_then(Value::as_str) {
        return DateTime::parse_from_rfc3339(datetime).ok();
    }
    if let Some(timestamp) = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|timestamp| timestamp.parse::<i64>().ok())
    {
        return offset.timestamp_opt(timestamp, 0).single();
    }
    if let Some(timestamp) = value.get("timestamp").and_then(Value::as_i64) {
        return offset.timestamp_opt(timestamp, 0).single();
    }
    if let Some(date) = value.get("date").and_then(Value::as_str)
        && let Ok(date) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
    {
        return offset
            .with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
            .single();
    }
    None
}

fn get_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::to_string)
}

fn is_meeting(event: &CalendarEvent) -> bool {
    contains_any(
        &event.summary,
        &["会", "会议", "同步", "评审", "周会", "项目"],
    )
}

fn is_fieldwork(event: &CalendarEvent) -> bool {
    contains_any(
        &format!(
            "{} {}",
            event.summary,
            event.location.as_deref().unwrap_or_default()
        ),
        &["外勤", "客户", "拜访", "现场", "调研"],
    )
}

fn is_travel(event: &CalendarEvent) -> bool {
    let text = format!(
        "{} {}",
        event.summary,
        event.location.as_deref().unwrap_or_default()
    );
    contains_any(&text, &["出差", "差旅", "航班", "高铁"])
        || event
            .location
            .as_ref()
            .is_some_and(|location| !location.contains("深圳"))
}

fn contains_any(text: &str, keywords: &[&str]) -> bool {
    keywords.iter().any(|keyword| text.contains(keyword))
}

fn render_fieldwork_notice(
    event: &CalendarEvent,
    route: Option<RoutePlan>,
    weather: Option<WeatherBrief>,
) -> String {
    let mut text = format!("你待会有一个外勤日程，主题是{}", event.summary);
    if let Some(location) = event.location.as_ref() {
        text.push_str(&format!("，地点是{location}"));
    }
    if let Some(route) = route {
        text.push_str(&format!(
            "。从默认出发地过去预计需要{}分钟",
            route.duration_minutes
        ));
    }
    if let Some(weather) = weather {
        text.push_str(&format!("。目的地天气是{}", weather.text));
    }
    text.push_str("。建议提前出发并检查随身物品。");
    text
}

fn render_travel_notice(event: &CalendarEvent, weather: Option<WeatherBrief>) -> String {
    let mut text = format!("明天有异地出差安排，主题是{}", event.summary);
    if let Some(location) = event.location.as_ref() {
        text.push_str(&format!("，目的地是{location}"));
    }
    if let Some(weather) = weather {
        text.push_str(&format!("。当地天气是{}", weather.text));
    }
    text.push_str("。建议今晚准备证件、充电器、电脑、电源、差旅票据和换洗衣物。");
    text
}

fn sanitize_tts_text(input: impl Into<String>) -> String {
    input
        .into()
        .lines()
        .map(|line| {
            line.trim_start_matches(|ch: char| {
                matches!(ch, '-' | '*' | '|' | '#' | '`' | '•') || ch.is_ascii_digit()
            })
            .replace(['|', '`'], " ")
        })
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn notice(
    id: String,
    kind: RobotNoticeKind,
    now: DateTime<FixedOffset>,
    text: String,
) -> RobotNotice {
    RobotNotice {
        id,
        timestamp_ms: now.timestamp_millis().max(0) as u64,
        kind,
        text,
    }
}

fn parse_workday_end(value: &str) -> (u32, u32) {
    let mut parts = value.split(':');
    let hour = parts
        .next()
        .and_then(|hour| hour.parse::<u32>().ok())
        .filter(|hour| *hour < 24)
        .unwrap_or(18);
    let minute = parts
        .next()
        .and_then(|minute| minute.parse::<u32>().ok())
        .filter(|minute| *minute < 60)
        .unwrap_or(0);
    (hour, minute)
}

fn load_state(path: &Path) -> Result<RobotState, String> {
    let content =
        fs::read_to_string(path).map_err(|err| format!("failed to read robot state: {err}"))?;
    serde_json::from_str(&content).map_err(|err| format!("failed to parse robot state: {err}"))
}

fn save_state(path: &Path, state: &RobotState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("failed to create state dir: {err}"))?;
    }
    let content = serde_json::to_vec_pretty(state)
        .map_err(|err| format!("failed to serialize robot state: {err}"))?;
    fs::write(path, content).map_err(|err| format!("failed to write robot state: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shanghai() -> FixedOffset {
        FixedOffset::east_opt(8 * 3600).expect("offset")
    }

    #[test]
    fn parses_calendar_events_from_agenda_data() {
        let value = serde_json::json!({
            "ok": true,
            "data": [{
                "event_id": "event-1",
                "summary": "项目会",
                "start_time": {"timestamp": "1783216800"},
                "end_time": {"timestamp": "1783220400"},
                "location": {"name": "深圳"}
            }]
        });

        let events = parse_calendar_events(&value, shanghai());

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id, "event-1");
        assert_eq!(events[0].summary, "项目会");
        assert_eq!(events[0].location.as_deref(), Some("深圳"));
    }

    #[test]
    fn parses_calendar_events_from_agenda_datetime_fields() {
        let value = serde_json::json!({
            "ok": true,
            "data": [{
                "event_id": "event-1",
                "summary": "morrow 测试会议",
                "start_time": {
                    "datetime": "2026-07-05T19:15:00+08:00",
                    "timezone": "Asia/Shanghai"
                },
                "end_time": {
                    "datetime": "2026-07-05T19:45:00+08:00",
                    "timezone": "Asia/Shanghai"
                }
            }]
        });

        let events = parse_calendar_events(&value, shanghai());

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary, "morrow 测试会议");
        assert_eq!(
            events[0].start,
            DateTime::parse_from_rfc3339("2026-07-05T19:15:00+08:00").expect("datetime")
        );
    }

    #[test]
    fn meeting_notice_dedupes_by_event_and_window() {
        let mut scheduler = RobotScheduler::new(
            test_options(),
            std::env::temp_dir().join("morrow-robot-test-state.json"),
        );
        let now = shanghai()
            .with_ymd_and_hms(2026, 7, 5, 9, 45, 0)
            .single()
            .expect("time");
        let event = CalendarEvent {
            event_id: "event-1".to_string(),
            summary: "项目会".to_string(),
            start: now + ChronoDuration::minutes(15),
            end: None,
            location: None,
        };

        let first = scheduler.meeting_notices(now, std::slice::from_ref(&event));
        let second = scheduler.meeting_notices(now, &[event]);

        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
    }

    #[test]
    fn sanitizes_tts_text() {
        let text = sanitize_tts_text("- 第一行\n| 表格 | 内容 |\n```");

        assert_eq!(text, "第一行 表格 内容");
    }

    #[test]
    fn parses_amap_route_duration_from_cost_fields() {
        let value = serde_json::json!({
            "route": {
                "paths": [{
                    "distance": "14113",
                    "cost": {"duration": "2195"}
                }]
            }
        });

        assert_eq!(
            parse_amap_route_duration_minutes(&value).expect("duration"),
            37
        );
    }

    #[test]
    fn configured_secret_uses_direct_config_without_env() {
        assert_eq!(
            configured_secret(Some(" direct-secret "), "MISSING_ENV", "[service].secret")
                .expect("direct secret"),
            "direct-secret"
        );

        assert_eq!(
            configured_secret(None, "MISSING_ENV", "[service].secret").expect_err("missing"),
            "set [service].secret or MISSING_ENV"
        );
    }

    fn test_options() -> RobotServerOptions {
        RobotServerOptions {
            robot: RobotConfig {
                enabled: true,
                timezone: "Asia/Shanghai".to_string(),
                default_origin: "深圳福田区".to_string(),
                meeting_reminder_minutes: vec![15, 5],
                fieldwork_reminder_minutes: 60,
                workday_end_time: "18:00".to_string(),
            },
            lark: LarkConfig {
                cli_path: "lark-cli".to_string(),
                calendar_identity: "user".to_string(),
                message_identity: "user".to_string(),
                calendar_id: "primary".to_string(),
            },
            qweather: QWeatherConfig {
                token: None,
                token_env: "QWEATHER_TOKEN".to_string(),
                base_url: "https://devapi.qweather.com".to_string(),
            },
            amap: AmapConfig {
                key: None,
                key_env: "AMAP_API_KEY".to_string(),
                base_url: "https://restapi.amap.com".to_string(),
            },
            lark_tools: LarkToolConfig {
                cli_path: "lark-cli".to_string(),
                calendar_identity: "user".to_string(),
                message_identity: "user".to_string(),
                calendar_id: "primary".to_string(),
            },
        }
    }
}
