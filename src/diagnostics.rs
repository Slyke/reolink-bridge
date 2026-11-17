use std::{
    collections::BTreeMap,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::Result;
use serde_json::{Value, json};
use styleguide_logger::{
    ErrorCodeMap, ErrorOptions, LogOptions, Logger, LoggingConfig, SinkFailure,
    error_codes::parse_error_codes_with_environment,
};
use tokio::{sync::mpsc, task::JoinHandle};
use uuid::Uuid;

pub const SERVICE: &str = "reolink-bridge";
pub struct BuildInfo {
    pub version: String,
    pub hash: String,
}

pub fn build_info() -> &'static BuildInfo {
    static BUILD: LazyLock<BuildInfo> = LazyLock::new(|| {
        let value = |name| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        BuildInfo {
            version: value("APP_VERSION").unwrap_or_else(|| env!("CARGO_PKG_VERSION").into()),
            hash: value("APP_BUILD_HASH").unwrap_or_else(|| "unknown".into()),
        }
    });
    &BUILD
}

const CATALOG: &str = include_str!("../errors.json5");

static TEXT: LazyLock<BTreeMap<String, String>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../en-US.json")).expect("valid English strings")
});

pub fn text(key: &str) -> &'static str {
    TEXT.get(&format!("reolink-bridge-{key}"))
        .map_or("Unexpected bridge error.", String::as_str)
}

#[derive(Clone)]
pub struct Diagnostics {
    tx: mpsc::Sender<Record>,
    codes: Arc<ErrorCodeMap>,
    startup_runtime: Option<Value>,
    queue_full_reported: Arc<AtomicBool>,
}

enum Record {
    Log(Box<LogOptions>),
    Error(ErrorOptions),
}

impl Diagnostics {
    pub fn start(settings: Option<&Value>) -> Result<(Self, JoinHandle<()>)> {
        let settings = settings.map_or_else(|| "{}".into(), Value::to_string);
        let mut config = LoggingConfig::from_json5(&settings)?;
        let mut codes = parse_error_codes_with_environment(CATALOG, &|_| None)?;
        if let Some(path) = config.error_file.as_ref() {
            // The built-in catalog covers all bridge errors even when an extra catalog is configured.
            let extra = styleguide_logger::error_codes::load_error_codes(path)?;
            for (key, value) in extra {
                codes.entry(key).or_insert(value);
            }
        }
        for key in [
            "ONVIF_PUSH_ENDED",
            "ONVIF_PULL_ENDED",
            "ONVIF_UNSUBSCRIBE_FAILED",
            "BAICHUAN_EVENTS_ENDED",
            "BAICHUAN_DISCOVERY_FAILED",
            "BAICHUAN_METADATA_ENDED",
            "ONVIF_METADATA_ENDED",
            "MQTT_DISCONNECTED",
            "MQTT_COMMAND_SUBSCRIBE_REJECTED",
            "MQTT_PUBLISH_REJECTED",
            "CALLBACK_HTTP_REJECTED",
            "CALLBACK_DELIVERY_FAILED",
            "HA_CFG_WEBHOOK_SETUP_FAILED",
            "CGI_WEBHOOK_SETUP_FAILED",
            "CONTROL_READBACK_UNVERIFIED",
            "CONFIG_DIRECTORY_SYNC_FAILED",
            "CALLBACK_QUEUE_FULL",
            "MQTT_COMMAND_QUEUE_FULL",
            "MQTT_EVENT_QUEUE_FULL",
            "MQTT_SNAPSHOT_LAG",
            "MQTT_RESULT_QUEUE_FULL",
            "WEBSOCKET_SEND_FAILED",
        ] {
            config
                .gates
                .entry(key.into())
                .or_insert_with(|| styleguide_logger::config::Gate {
                    level: Some("warn".into()),
                    ..Default::default()
                });
        }
        let startup_runtime = if config.runtime.enabled {
            None
        } else {
            config.runtime.metadata()?
        };
        let logger = Logger::new(config, codes.clone())?;
        let (tx, mut rx) = mpsc::channel(128);
        let task = tokio::spawn(async move {
            while let Some(record) = rx.recv().await {
                let failures = match record {
                    Record::Log(options) => logger.generate_log(*options).await.failures,
                    Record::Error(options) => logger.generate_error(options).await.failures,
                };
                report_sink_failures(failures);
            }
        });
        Ok((
            Self {
                tx,
                codes: Arc::new(codes),
                startup_runtime,
                queue_full_reported: Arc::new(AtomicBool::new(false)),
            },
            task,
        ))
    }

    pub fn startup_runtime(&self) -> Option<&Value> {
        self.startup_runtime.as_ref()
    }

    pub fn log(
        &self,
        caller: &str,
        key: &str,
        level: &str,
        correlation_id: Option<String>,
        context: Value,
    ) {
        self.enqueue(Record::Log(Box::new(LogOptions {
            caller: caller.into(),
            logger_key: Some(key.into()),
            message: text(key).into(),
            level: level.into(),
            correlation_id,
            context: (!context.is_null()).then_some(context),
            ..Default::default()
        })));
    }

    pub fn error(
        &self,
        caller: &str,
        key: &str,
        correlation_id: Option<String>,
        context: Value,
        cause: Option<anyhow::Error>,
    ) -> Value {
        let options = error_options(caller, key, correlation_id, context, cause);
        let code = self
            .codes
            .get(key)
            .or_else(|| self.codes.get("ERR_UNKNOWN"))
            .cloned();
        let reply = json!({
            "ok": false,
            "error": options.reason,
            "errorKey": options.error_key,
            "errorCode": code,
            "correlation_id": options.correlation_id,
        });
        self.enqueue(Record::Error(options));
        reply
    }

    fn enqueue(&self, record: Record) {
        // A slow optional sink must not block the camera, broker, or HTTP workers.
        match self.tx.try_send(record) {
            Ok(()) => self.queue_full_reported.store(false, Ordering::Relaxed),
            Err(mpsc::error::TrySendError::Full(_)) => {
                if !self.queue_full_reported.swap(true, Ordering::Relaxed) {
                    eprintln!("{}", text("LOG_QUEUE_FULL"));
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

pub async fn startup_error(key: &str, cause: anyhow::Error) {
    let codes =
        parse_error_codes_with_environment(CATALOG, &|_| None).expect("valid bundled error codes");
    let logger = Logger::new(LoggingConfig::default(), codes).expect("default console logger");
    let options = error_options("main::startup", key, None, Value::Null, Some(cause));
    report_sink_failures(logger.generate_error(options).await.failures);
}

fn error_options(
    caller: &str,
    key: &str,
    correlation_id: Option<String>,
    context: Value,
    cause: Option<anyhow::Error>,
) -> ErrorOptions {
    ErrorOptions {
        caller: caller.into(),
        reason: text(key).into(),
        error_key: key.into(),
        correlation_id: Some(correlation_id.unwrap_or_else(|| Uuid::new_v4().to_string())),
        context: (!context.is_null()).then_some(context),
        source: cause.map(anyhow::Error::into_boxed_dyn_error),
        ..Default::default()
    }
}

fn report_sink_failures(failures: Vec<SinkFailure>) {
    for failure in failures {
        // Sink diagnostics from the shared logger are already sanitized.
        eprintln!(
            "{} {}: {}",
            text("LOG_SINK_FAILURE"),
            failure.sink,
            failure.message
        );
    }
}
