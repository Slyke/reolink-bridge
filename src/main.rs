mod camera;
mod config;
mod diagnostics;
mod http;
mod hub;
mod model;
mod mqtt;
mod onvif;
mod reolink;
mod xml;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinSet, time::timeout};
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
            Ok(()) => ExitCode::SUCCESS,
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
    config: config::Config,
) -> Result<()> {
    let ids: BTreeSet<_> = config
        .cameras
        .iter()
        .map(|camera| camera.id.clone())
        .collect();
    let ws_token = config
        .websocket
        .token
        .context("missing validated WebSocket token")?;
    let stop = CancellationToken::new();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(4))
        .build()
        .context("could not create HTTP client")?;

    let push = PushRoutes::default();
    let mut native = BTreeMap::new();
    let mut secrets = vec![ws_token.clone()];
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
            host: camera.host.clone(),
            port,
            username: camera.username.clone(),
            password,
            channel: camera.channel,
        });
        devices.push((camera, onvif, baichuan, webhook, cgi_webhook));
    }

    let listener = TcpListener::bind(config.http.listen)
        .await
        .context("could not bind HTTP listener")?;
    let kubernetes = [
        "K8S_POD_NAME",
        "K8S_DEPLOYMENT",
        "K8S_NAMESPACE",
        "K8S_POD_IP",
        "K8S_POD_IPS",
        "K8S_NODE_NAME",
    ]
    .into_iter()
    .filter_map(|key| {
        std::env::var(key)
            .ok()
            .filter(|value| !value.is_empty())
            .map(|value| (key, value))
    })
    .collect::<BTreeMap<_, _>>();
    let mut boot = json!({
        "version": diagnostics::VERSION, "buildHash": diagnostics::BUILD_HASH,
        "service": diagnostics::SERVICE, "platform": std::env::consts::OS,
        "arch": std::env::consts::ARCH, "pid": std::process::id(),
        "port": listener.local_addr()?.port(),
    });
    if !kubernetes.is_empty() && !diagnostics.kubernetes_enabled() {
        boot["kubernetes"] = json!(kubernetes);
    }
    diagnostics.log("main::run", "SERVICE_BOOT_DIAGNOSTICS", "info", None, boot);

    let callbacks = hub::Callbacks::new(
        path,
        hub::CallbackFile::new(raw, document, config.callbacks),
        ids.clone(),
        client,
        stop.clone(),
        diagnostics.clone(),
    );
    let hub = hub::Hub::new(&ids, callbacks, secrets, diagnostics.clone());
    let mut controls = BTreeMap::new();
    let mut workers = JoinSet::new();
    for (camera, onvif, baichuan, webhook, cgi_webhook) in devices {
        let id = camera.id.clone();
        let base = format!("{}/{id}", config.mqtt.base_topic);
        let can_control = baichuan.is_some();
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
        .start();
        workers.spawn(worker);
        if can_control {
            controls.insert(id.clone(), controller.clone());
        }
        let options = mqtt::options(&config.mqtt, &id, &base);
        workers.spawn(mqtt::run(
            options,
            base,
            id,
            hub.clone(),
            can_control.then_some(controller),
            stop.clone(),
        ));
    }

    let app = Arc::new(http::App {
        hub,
        push,
        native,
        controls,
        camera_ids: ids,
        ws_token,
        stop: stop.clone(),
    });
    let router = http::router(app, &config.websocket.path);
    let server_stop = stop.clone();
    let server = axum::serve(listener, router)
        .with_graceful_shutdown(async move { server_stop.cancelled().await });
    let mut server = std::pin::pin!(std::future::IntoFuture::into_future(server));
    let result = tokio::select! {
        _ = shutdown_signal() => Ok(()),
        result = &mut server => result.context("HTTP server failed"),
        _ = workers.join_next() => {
            Err(anyhow::anyhow!("a bridge worker exited unexpectedly"))
        }
    };
    stop.cancel();
    if timeout(Duration::from_secs(15), async {
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
