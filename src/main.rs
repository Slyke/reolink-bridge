mod api;
mod camera;
mod config;
mod diagnostics;
mod http;
mod hub;
mod identity;
mod model;
mod mqtt;
mod onvif;
mod reolink;
mod tls;
mod xml;

use std::{
    collections::{BTreeMap, BTreeSet},
    net::TcpListener,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::{sync::Semaphore, task::JoinSet, time::timeout};
use tokio_util::sync::CancellationToken;

use camera::CameraTask;
use diagnostics::{Diagnostics, startup_error};
use onvif::{Onvif, events::PushRoutes};
use reolink::{events::AlarmDecoder, wire::Device};

#[tokio::main]
async fn main() -> ExitCode {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let path = match config_path() {
        Ok(Some(path)) => path,
        Ok(None) => {
            println!("{}", diagnostics::text("USAGE"));
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            startup_error("CLI_ARGUMENTS_INVALID", error).await;
            return ExitCode::FAILURE;
        }
    };
    let path = match std::fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) => {
            startup_error("CONFIG_PATH_UNAVAILABLE", error.into()).await;
            return ExitCode::FAILURE;
        }
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) => {
            startup_error("CONFIG_READ_FAILED", error.into()).await;
            return ExitCode::FAILURE;
        }
    };
    let document = match config::document(&raw) {
        Ok(document) => document,
        Err(error) => {
            startup_error("CONFIG_SYNTAX_INVALID", error).await;
            return ExitCode::FAILURE;
        }
    };
    let (diagnostics, log_worker) = match Diagnostics::start(document.get("logging")) {
        Ok(logger) => logger,
        Err(error) => {
            startup_error("LOG_CONFIG_INVALID", error).await;
            return ExitCode::FAILURE;
        }
    };
    let env = |name: &str| std::env::var(name).ok();
    let code = match config::parse(document.clone(), &env) {
        Ok(config) => match run(&diagnostics, path, raw, document, config).await {
            Ok(code) => ExitCode::from(code),
            Err(error) => {
                diagnostics.error(
                    "main::run",
                    "SERVICE_RUN_FAILED",
                    None,
                    Value::Null,
                    Some(error),
                );
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            diagnostics.error(
                "main::config",
                "CONFIG_VALIDATION_FAILED",
                None,
                Value::Null,
                Some(error),
            );
            ExitCode::FAILURE
        }
    };
    drop(diagnostics);
    let mut log_worker = log_worker;
    if timeout(Duration::from_secs(3), &mut log_worker)
        .await
        .is_err()
    {
        log_worker.abort();
    }
    code
}

fn config_path() -> Result<Option<PathBuf>> {
    let mut args = std::env::args_os().skip(1);
    let argument = args.next();
    ensure!(
        args.next().is_none(),
        "supply a single JSON5 configuration path"
    );
    if argument
        .as_deref()
        .is_some_and(|path| path == "--help" || path == "-h")
    {
        return Ok(None);
    }
    let path = argument.map(PathBuf::from).unwrap_or_else(|| {
        let path = Path::new("local-only/bridge.json5");
        if !path.exists() && Path::new("bridge.local.json5").is_file() {
            // Older local runs used this location. Explicit CLI paths also remain supported.
            PathBuf::from("bridge.local.json5")
        } else {
            path.into()
        }
    });
    Ok(Some(path))
}

async fn run(
    diagnostics: &Diagnostics,
    path: PathBuf,
    raw: String,
    document: Value,
    mut config: config::Config,
) -> Result<u8> {
    let stop = CancellationToken::new();
    let auth_token = config
        .http
        .token
        .as_ref()
        .context("missing validated HTTP bearer token")?;
    let snapshot_slot = Arc::new(Semaphore::new(1));
    let api = api::Api::prepare(
        config.api.take().unwrap_or_default(),
        diagnostics,
        &mut config.cameras,
        auth_token,
        snapshot_slot.clone(),
        stop.clone(),
    )?;
    let camera_ids: std::collections::BTreeSet<_> = config
        .cameras
        .iter()
        .map(|camera| camera.id.clone())
        .collect();
    let ws_path = &config
        .http
        .websocket
        .as_ref()
        .context("missing WebSocket options")?
        .path;
    let local_tls_dir = path
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "local-only"))
        .unwrap_or_else(|| Path::new("local-only"))
        .join("https");
    let tls_name = config
        .http
        .advertise_base_url
        .as_ref()
        .and_then(|url| url.host_str())
        .map(|name| name.trim_matches(['[', ']']).to_owned())
        .or_else(|| {
            (!config.http.listen.ip().is_unspecified()).then(|| config.http.listen.ip().to_string())
        });
    let https = tls::server(config.http.tls.as_ref(), tls_name, &local_tls_dir)
        .context("could not configure HTTPS")?;
    let mqtt_transport = if config.mqtt.url.scheme() == "mqtts" {
        let ca_file = config
            .mqtt
            .tls
            .as_ref()
            .and_then(|tls| tls.ca_file.as_deref());
        tls::mqtt_transport(ca_file).context("could not configure MQTT TLS")?
    } else {
        rumqttc::Transport::tcp()
    };
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(4))
        .build()
        .context("could not create HTTP client")?;

    let push = PushRoutes::default();
    let mut native = BTreeMap::new();
    let mut secrets = vec![auth_token.clone()];
    secrets.extend(config.mqtt.password.clone());
    let mut devices = Vec::new();
    for mut camera in config.cameras {
        let password = camera
            .password
            .take()
            .context("missing validated camera password")?;
        secrets.push(password.clone());
        let (webhook, cgi_webhook) = if let Some(settings) = camera.native_webhook.take() {
            let token = settings.token.context("missing validated webhook token")?;
            let base = config
                .http
                .advertise_base_url
                .as_ref()
                .context("missing advertised HTTP URL")?;
            let target =
                config::advertised(base, &format!("/ingest/reolink/{}/{token}", camera.id));
            secrets.push(token.clone());
            let (parser, ha_cfg, cgi) = match settings.protocol {
                config::WebhookProtocol::HaCfg => (
                    http::NativeParser::HaCfg(Mutex::new(AlarmDecoder::new(
                        camera.channel,
                        "reolink_webhook",
                        BTreeSet::new(),
                    ))),
                    Some(target),
                    None,
                ),
                config::WebhookProtocol::Cgi => (
                    http::NativeParser::Cgi(camera.channel),
                    None,
                    Some(reolink::cgi::Webhook {
                        client: client.clone(),
                        base: settings.cgi_base_url.context("missing CGI base URL")?,
                        consumer: target,
                        username: camera.username.clone(),
                        password: password.clone(),
                        channel: camera.channel,
                    }),
                ),
            };
            native.insert(camera.id.clone(), http::NativeRoute { token, parser });
            (ha_cfg, cgi)
        } else {
            (None, None)
        };
        let onvif = camera
            .onvif_port
            .map(|port| {
                Onvif::new(
                    client.clone(),
                    &camera.host,
                    port,
                    camera.username.clone(),
                    password.clone(),
                )
            })
            .transpose()?;
        let baichuan = camera.baichuan_port.map(|port| Device {
            read_retries: api.read_retries.for_camera(&camera.id),
            host: camera.host.clone(),
            port,
            username: camera.username.clone(),
            password,
            channel: camera.channel,
        });
        devices.push((camera, onvif, baichuan, webhook, cgi_webhook));
    }

    let listener = TcpListener::bind(config.http.listen).context("could not bind HTTP listener")?;
    listener.set_nonblocking(true)?;
    let build = diagnostics::build_info();
    let mut boot = json!({
        "version": build.version, "buildHash": build.hash,
        "service": diagnostics::SERVICE, "platform": std::env::consts::OS,
        "arch": std::env::consts::ARCH, "pid": std::process::id(),
        "port": listener.local_addr()?.port(),
        "https": https.config.is_some(), "generatedCertificate": https.generated,
        "mqttTls": config.mqtt.url.scheme() == "mqtts",
    });
    if let Some(runtime) = diagnostics.startup_runtime() {
        boot["runtime"] = runtime.clone();
    }
    diagnostics.log("main::run", "SERVICE_BOOT_DIAGNOSTICS", "info", None, boot);

    let callbacks = hub::Callbacks::new(
        path,
        hub::CallbackFile::new(raw, document, config.callbacks),
        camera_ids.clone(),
        client,
        stop.clone(),
        diagnostics.clone(),
    );
    let hub = hub::Hub::new(camera_ids, callbacks, secrets, diagnostics.clone());
    let mut controls = BTreeMap::new();
    let mut workers = JoinSet::new();
    for (camera, onvif, baichuan, webhook, cgi_webhook) in devices {
        let id = camera.id.clone();
        let base = format!("{}/{id}", config.mqtt.base_topic);
        let control_device = baichuan.clone();

        let (controller, worker) = CameraTask {
            config: camera,
            onvif,
            baichuan,
            advertise: config.http.advertise_base_url.clone(),
            webhook,
            cgi_webhook,
            routes: push.clone(),
            hub: hub.clone(),
            stop: stop.clone(),
        }
        .start(api.enrollment_gate(&id));
        workers.spawn(worker);
        let services = control_device.map(|device| {
            controls.insert(id.clone(), controller.clone());
            mqtt::CameraCommands {
                camera_id: id.clone(),
                hub: hub.clone(),
                control: controller,
                device,
                snapshot_slot: snapshot_slot.clone(),
                snapshot_max_payload_bytes: config.mqtt.snapshot_max_payload_bytes,
            }
        });
        let options = mqtt::options(&config.mqtt, &id, &base, mqtt_transport.clone());
        workers.spawn(mqtt::run(
            options,
            base,
            id,
            hub.clone(),
            services,
            stop.clone(),
        ));
    }

    let mut shutdown_requested = api.shutdown_requested.subscribe();
    let app = Arc::new(http::App {
        api: api.attach(hub.clone(), &mut workers),
        hub,
        push,
        native,
        controls,
        stop: stop.clone(),
    });
    let router = http::router(app, ws_path);
    let handle = axum_server::Handle::new();
    let server = async {
        match https.config {
            Some(tls) => {
                axum_server::from_tcp_rustls(listener, tls)?
                    .handle(handle.clone())
                    .serve(router.into_make_service())
                    .await
            }
            None => {
                axum_server::from_tcp(listener)?
                    .handle(handle.clone())
                    .serve(router.into_make_service())
                    .await
            }
        }
    };
    let mut server = std::pin::pin!(server);
    let (result, server_exited) = tokio::select! {
        Ok(()) = shutdown_requested.changed() => (Ok(shutdown_requested.borrow_and_update().unwrap_or_default()), false),
        _ = shutdown_signal() => (Ok(0), false),
        result = &mut server => (result.context("HTTP server failed").map(|()| 0), true),
        _ = workers.join_next(), if !workers.is_empty() => (Err(anyhow::anyhow!("a bridge worker exited unexpectedly")), false),
    };
    stop.cancel();
    handle.graceful_shutdown(Some(Duration::from_secs(4)));
    if timeout(Duration::from_secs(15), async {
        let finish_workers = async {
            while let Some(result) = workers.join_next().await {
                if result.is_err() {
                    diagnostics.error(
                        "main::shutdown",
                        "SERVICE_WORKER_SHUTDOWN_FAILED",
                        None,
                        Value::Null,
                        None,
                    );
                }
            }
        };
        let finish_server = async {
            if !server_exited {
                let _ = (&mut server).await;
            }
        };
        tokio::join!(finish_workers, finish_server);
    })
    .await
    .is_err()
    {
        diagnostics.error(
            "main::shutdown",
            "SERVICE_SHUTDOWN_TIMEOUT",
            None,
            Value::Null,
            None,
        );
    }
    result
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {
                _ = terminate.recv() => {},
                _ = tokio::signal::ctrl_c() => {},
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
