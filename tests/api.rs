#![cfg(unix)]

#[path = "support/baichuan.rs"]
mod baichuan;
#[path = "support/rtsp.rs"]
mod rtsp;
#[path = "support/wui_smoke.rs"]
mod wui_smoke;

use anyhow::{Context, Result, ensure};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde_json::{Value, json};
use std::{
    net::TcpListener as StdListener,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Stdio,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    process::{Child, Command},
    time::{Instant, sleep},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use uuid::Uuid;

const SERVICE: &str = "api-test-service-token-for-scoped-tests";
const ADMIN: &str = "api-test-admin-token-for-scoped-tests";
const PRIMARY: &str = "api-test-legacy-token-for-scoped-tests";
const UID: &str = "952700FIXTURE0001";

fn free_port() -> Result<u16> {
    Ok(StdListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

async fn response(client: &Client, url: &str, token: &str, body: Option<Value>) -> (u16, Value) {
    let request = if let Some(body) = body {
        client
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_string())
    } else {
        client.get(url)
    };
    let reply = request.bearer_auth(token).send().await.unwrap();
    let status = reply.status().as_u16();
    let reply = serde_json::from_str(&reply.text().await.unwrap()).unwrap_or_default();
    (status, reply)
}

async fn event_message(response: &mut reqwest::Response, buffer: &mut Vec<u8>) -> Result<Value> {
    loop {
        if let Some(end) = buffer.windows(2).position(|pair| pair == b"\n\n") {
            let frame: Vec<_> = buffer.drain(..end + 2).collect();
            let frame = std::str::from_utf8(&frame)?;
            if let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) {
                return Ok(serde_json::from_str(data)?);
            }
        } else {
            let chunk = tokio::time::timeout(Duration::from_secs(5), response.chunk())
                .await??
                .context("event stream ended")?;
            buffer.extend_from_slice(&chunk);
            ensure!(buffer.len() < 2_000_000, "event frame limit");
        }
    }
}

async fn start(config: &Path, client: &Client, origin: &str) -> Result<Child> {
    let mut bridge = Command::new(env!("CARGO_BIN_EXE_reolink-bridge"))
        .arg(config)
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .spawn()?;
    let until = Instant::now() + Duration::from_secs(8);
    while Instant::now() < until {
        if let Some(status) = bridge.try_wait()? {
            let log = std::fs::read_to_string(config.with_file_name("requests.jsonl"))
                .unwrap_or_default();
            anyhow::bail!(
                "fixture bridge exited with {status}: {}",
                log.lines().last().unwrap_or("")
            );
        }
        if client
            .get(format!("{origin}/livez"))
            .send()
            .await
            .is_ok_and(|reply| reply.status().is_success())
        {
            return Ok(bridge);
        }
        sleep(Duration::from_millis(40)).await;
    }
    anyhow::bail!("fixture bridge did not start")
}

async fn discovered(client: &Client, url: &str) -> Value {
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        let cameras = response(client, url, PRIMARY, None).await.1["cameras"].clone();
        if cameras.as_array().is_some_and(|list| {
            list.iter()
                .all(|c| c["active"] == false || !c["stableId"].is_null())
        }) {
            return cameras;
        }
        assert!(
            Instant::now() < until,
            "hardware identity discovery timed out"
        );
        sleep(Duration::from_millis(30)).await;
    }
}

#[tokio::test]
#[ignore = "requires Mosquitto on PATH; BRIDGE_TEST_VALKEY_URL enables the Valkey run"]
async fn background_previews_are_authenticated_and_wait_for_each_image() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut image = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut image).encode(
        &[91; 48],
        4,
        4,
        image::ExtendedColorType::Rgb8,
    )?;
    let camera = Arc::new(baichuan::Camera {
        jpeg: Some(image.clone()),
        snapshot_delay: Duration::from_millis(175),
        ..Default::default()
    });
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let baichuan = listener.local_addr()?.port();
    tokio::spawn(camera.clone().serve(listener));
    let broker = free_port()?;
    let file = directory.path().join("mosquitto.conf");
    std::fs::write(
        &file,
        format!("listener {broker} 127.0.0.1\nallow_anonymous true\n"),
    )?;
    let _broker = Command::new("mosquitto")
        .arg("-c")
        .arg(file)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let port = free_port()?;
    let origin = format!("http://127.0.0.1:{port}");
    let contract = format!("{origin}/api/contract");
    let preview = format!("{origin}/api/cameras/fixture/preview");
    let config = directory.path().join("bridge.json5");
    let valkey = std::env::var("BRIDGE_TEST_VALKEY_URL").ok();
    let cache = match &valkey {
        Some(url) => {
            json!({"backend":"valkey","url":url,"keyPrefix":format!("bridge-test-{}", Uuid::new_v4())})
        }
        None => json!({"backend":"memory"}),
    };
    std::fs::write(&config, json!({
        "mqtt":{"url":format!("mqtt://127.0.0.1:{broker}"),"baseTopic":"fixture"},
        "http":{"listen":format!("127.0.0.1:{port}"),"token":PRIMARY},
        "cameras":[{"id":"fixture","host":"127.0.0.1","username":"admin","password":baichuan::PASSWORD,
            "onvifPort":null,"baichuanPort":baichuan,"eventTransport":"baichuan"}],
        "api":{"serviceToken":SERVICE,"cameraIds":["fixture"],
            "previews":{"cache":cache,"cameras":{"fixture":{"intervalMs":500}}}}
    }).to_string())?;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?;
    let mut bridge = start(&config, &client, &origin).await?;

    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if camera.snapshot_requests.lock().unwrap().len() >= 2 {
            break;
        }
        ensure!(
            Instant::now() < until,
            "camera was not polled with no preview clients"
        );
        sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        client
            .get(format!("{origin}/api/events"))
            .bearer_auth(SERVICE)
            .send()
            .await?
            .status(),
        401
    );
    let mut events = client
        .get(format!("{origin}/api/events"))
        .bearer_auth(PRIMARY)
        .send()
        .await?;
    let ready = event_message(&mut events, &mut Vec::new()).await?;
    let notified = ready["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["topic"] == "state/preview")
        .expect("preview refresh notification");
    assert!(
        notified["payload"]["preview"]["etag"]
            .as_str()
            .unwrap()
            .starts_with('"')
    );
    drop(events);
    let starts = camera.snapshot_requests.lock().unwrap().clone();
    assert!(starts[1].duration_since(starts[0]) >= Duration::from_millis(675));
    assert_eq!(
        response(&client, &contract, PRIMARY, None).await.1["previewCameras"],
        json!(["fixture"])
    );
    assert_eq!(
        client
            .get(&preview)
            .bearer_auth(PRIMARY)
            .send()
            .await?
            .status(),
        200
    );
    let frame = client
        .get(&preview)
        .bearer_auth(PRIMARY)
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(frame.headers()["content-type"], "image/jpeg");
    assert!(
        chrono::DateTime::parse_from_rfc3339(frame.headers()["x-preview-captured-at"].to_str()?)
            .is_ok()
    );
    let tag = frame.headers()["etag"].clone();
    assert_eq!(frame.bytes().await?.as_ref(), image.as_slice());
    let metadata = client
        .head(&preview)
        .bearer_auth(PRIMARY)
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(metadata.headers()["etag"], tag);
    assert!(metadata.bytes().await?.is_empty());
    let unchanged = client
        .get(&preview)
        .bearer_auth(PRIMARY)
        .header("if-none-match", tag.clone())
        .send()
        .await?;
    assert_eq!(unchanged.status(), 304);
    assert!(unchanged.bytes().await?.is_empty());
    let manual = client
        .get(format!("{preview}?refresh=true"))
        .bearer_auth(PRIMARY)
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(manual.headers()["etag"], tag);
    assert_eq!(manual.bytes().await?.as_ref(), image.as_slice());

    camera.reject_snapshots.store(true, Ordering::SeqCst);
    bridge.kill().await?;
    let mut restart = start(&config, &client, &origin).await?;
    if valkey.is_some() {
        let until = Instant::now() + Duration::from_secs(4);
        loop {
            let response = client.get(&preview).bearer_auth(PRIMARY).send().await?;
            if response.status().is_success() {
                assert_eq!(response.headers()["etag"], tag);
                assert_eq!(response.bytes().await?.as_ref(), image.as_slice());
                break;
            }
            ensure!(
                Instant::now() < until,
                "Valkey JPEG did not load on restart"
            );
            sleep(Duration::from_millis(40)).await;
        }
    } else {
        assert_eq!(
            client
                .get(&preview)
                .bearer_auth(PRIMARY)
                .send()
                .await?
                .status(),
            503
        );
    }
    restart.kill().await?;
    restart.wait().await?;
    camera.reject_snapshots.store(false, Ordering::SeqCst);
    let log_file = directory.path().join("cache-errors.jsonl");
    let mut degraded: Value = serde_json::from_slice(&std::fs::read(&config)?)?;
    degraded["api"]["previews"]["cache"] = json!({"backend":"valkey","url":"redis://127.0.0.1:1"});
    degraded["logging"] = json!({"sinks":{"console":{"enabled":false},"file":{"enabled":true,"format":"json","path":log_file}}});
    std::fs::write(&config, degraded.to_string())?;
    let _degraded = start(&config, &client, &origin).await?;
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let frame = client.get(&preview).bearer_auth(PRIMARY).send().await?;
        let logged = std::fs::read_to_string(&log_file)
            .unwrap_or_default()
            .contains("API_PREVIEW_CACHE_FAILED");
        if frame.status().is_success() && logged {
            break;
        }
        ensure!(
            Instant::now() < until,
            "Valkey failure must be logged while memory capture remains usable"
        );
        sleep(Duration::from_millis(40)).await;
    }
    Ok(())
}

#[tokio::test]
async fn configured_camera_enrolls_in_place_by_address() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let vault = directory.path().join("vault");
    std::fs::create_dir(&vault)?;
    std::fs::set_permissions(&vault, std::fs::Permissions::from_mode(0o700))?;
    let version = "<body><VersionInfo><name>E1 Pro</name><itemNo>E Series E330</itemNo><hardwareVersion>IPC_NT1NA45MP</hardwareVersion><firmwareVersion>v3.1.0.4417_2412122130</firmwareVersion><serialNumber>FIXTURE-SERIAL</serialNumber></VersionInfo></body>";
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg).encode(
        &[91; 48],
        4,
        4,
        image::ExtendedColorType::Rgb8,
    )?;
    let camera = Arc::new(baichuan::Camera {
        version_info: Some(version.into()).into(),
        password: Some(String::new()).into(),
        jpeg: Some(jpeg),
        ..Default::default()
    });
    let socket = TcpListener::bind("127.0.0.1:0").await?;
    let port = socket.local_addr()?.port();
    tokio::spawn(camera.clone().serve(socket));
    let http = free_port()?;
    let origin = format!("http://127.0.0.1:{http}");
    let configured_password = "Fixture!Desired22";
    let config = directory.path().join("bridge.json5");
    let mut body = json!({
        "mqtt":{"url":"mqtt://127.0.0.1:1","baseTopic":"fixture"},
        "http":{"listen":format!("127.0.0.1:{http}"),"token":PRIMARY},
        "api":{"storageDirectory":vault,"previews":{"cameras":{"e1-pro":{"intervalMs":500}}}},
        "cameras":[{"id":"e1-pro","stableId":"previous-name","host":"127.0.0.1","username":"admin","password":configured_password,
            "onvifPort":null,"baichuanPort":port}]
    });
    std::fs::write(&config, body.to_string())?;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(12))
        .build()?;
    let mut bridge = start(&config, &client, &origin).await?;
    let targets = format!("{origin}/api/enrollment-targets");
    let lookup = format!("{targets}/identify?address=127.0.0.1&method=initialize");
    let enroll = format!("{origin}/api/enrollments");
    let available = response(&client, &targets, PRIMARY, None).await.1;
    assert_eq!(available["targets"], json!([]));
    assert_eq!(available["addressLookup"], true);
    assert_eq!(response(&client, &lookup, "wrong", None).await.0, 401);
    assert_eq!(
        response(
            &client,
            &format!("{targets}/identify?address=127.0.0.2&method=initialize"),
            PRIMARY,
            None
        )
        .await
        .0,
        404
    );
    assert_eq!(
        response(&client, &format!("{lookup}&port=0"), PRIMARY, None)
            .await
            .0,
        404
    );
    let found = response(&client, &lookup, PRIMARY, None).await.1;
    assert_eq!(found["target"], "e1-pro");
    assert_eq!(found["port"], port);
    assert_eq!(found["uid"], UID);
    assert_eq!(found["serial"], "FIXTURE-SERIAL");
    assert_eq!(found["model"], "E1 Pro");
    assert!(!found.to_string().contains(configured_password));
    let id = Uuid::new_v4();
    let mut intent = json!({"id":id,"target":"e1-pro","address":"127.0.0.1","port":port,
        "method":"initialize","uid":UID,"serial":"FIXTURE-SERIAL","confirmation":false});
    assert_eq!(
        response(&client, &enroll, PRIMARY, Some(intent.clone()))
            .await
            .0,
        422
    );
    intent["confirmation"] = true.into();
    let mut wrong = intent.clone();
    wrong["address"] = "127.0.0.2".into();
    assert_eq!(
        response(&client, &enroll, PRIMARY, Some(wrong)).await.0,
        422
    );
    let mut mismatch = intent.clone();
    let old_id = Uuid::new_v4();
    mismatch["id"] = json!(old_id);
    mismatch["uid"] = "WRONGFULLUID".into();
    assert_eq!(
        response(&client, &enroll, PRIMARY, Some(mismatch)).await.1["status"],
        "identity-mismatch"
    );
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), 0);
    let enrolled = response(&client, &enroll, PRIMARY, Some(intent.clone()))
        .await
        .1;
    assert_eq!(enrolled["camera_id"], "e1-pro");
    assert_eq!(enrolled["status"], "active");
    assert!(!enrolled.to_string().contains(configured_password));
    assert_eq!(
        camera.password.lock().unwrap().as_deref(),
        Some(configured_password)
    );
    assert_eq!(
        response(&client, &enroll, PRIMARY, Some(intent)).await.1["status"],
        "active"
    );
    assert_eq!(
        response(
            &client,
            &format!("{enroll}/{old_id}/resume"),
            PRIMARY,
            Some(json!({}))
        )
        .await
        .1["status"],
        "identity-mismatch"
    );
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), 1);
    let inventory = response(&client, &format!("{origin}/api/inventory"), PRIMARY, None)
        .await
        .1;
    assert_eq!(inventory["cameras"][0]["stableId"], "FIXTURE-SERIAL");
    assert_eq!(
        response(
            &client,
            &format!("{origin}/api/cameras/e1-pro/users"),
            PRIMARY,
            None
        )
        .await
        .1["users"][0]["username"],
        "admin"
    );
    let until = Instant::now() + Duration::from_secs(6);
    while camera.snapshot_requests.lock().unwrap().is_empty() {
        assert!(
            Instant::now() < until,
            "camera preview worker did not activate after enrollment"
        );
        sleep(Duration::from_millis(25)).await;
    }
    bridge.kill().await?;
    // An ordinary later restart retains the verified binding. An optional port now disambiguates two cameras on one address.
    let other = Arc::new(baichuan::Camera {
        version_info: Some(version.replace("FIXTURE-SERIAL", "FIXTURE-OTHER")).into(),
        ..Default::default()
    });
    let other_socket = TcpListener::bind("127.0.0.1:0").await?;
    let other_port = other_socket.local_addr()?.port();
    body["api"]["rotationIds"] = json!({"already-set":{"serial":"FIXTURE-OTHER"}});
    body["api"]["targets"] =
        json!({"existing-name":{"cameraId":"previous-name","host":"127.0.0.1","port":port}});
    tokio::spawn(other.clone().serve(other_socket));
    body["cameras"].as_array_mut().unwrap().push(
        json!({"id":"already-set","host":"127.0.0.1","username":"admin",
        "password":baichuan::PASSWORD,"baichuanPort":other_port,"onvifPort":null}),
    );
    std::fs::write(&config, body.to_string())?;
    let _bridge = start(&config, &client, &origin).await?;
    assert_eq!(
        response(&client, &format!("{enroll}/{id}"), PRIMARY, None)
            .await
            .1["status"],
        "active"
    );
    assert_eq!(
        response(&client, &targets, PRIMARY, None).await.1["targets"],
        json!(["existing-name"])
    );
    let lookup = format!("{targets}/identify?address=127.0.0.1&method=import");
    assert_eq!(response(&client, &lookup, PRIMARY, None).await.0, 409);
    let found = response(
        &client,
        &format!("{lookup}&port={other_port}"),
        PRIMARY,
        None,
    )
    .await
    .1;
    assert_eq!(found["target"], "already-set");
    let imported = response(
        &client,
        &enroll,
        PRIMARY,
        Some(
            json!({"id":Uuid::new_v4(),"target":"already-set","address":"127.0.0.1",
        "port":other_port,"uid":found["uid"],"method":"import","confirmation":true}),
        ),
    )
    .await
    .1;
    assert_eq!(imported["status"], "active");
    assert_eq!(other.credential_writes.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn e1_pro_factory_enrollment_uses_configured_password_and_hands_off_to_static_config()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let vault = directory.path().join("vault");
    std::fs::create_dir(&vault)?;
    std::fs::set_permissions(&vault, std::fs::Permissions::from_mode(0o700))?;
    let version = "<body><VersionInfo><name>E1 Pro</name><itemNo>E Series E330</itemNo><hardwareVersion>IPC_NT1NA45MP</hardwareVersion><firmwareVersion>v3.1.0.4417_2412122130</firmwareVersion><serialNumber>FIXTURE-SERIAL</serialNumber></VersionInfo></body>";
    let camera = Arc::new(baichuan::Camera {
        version_info: Some(version.replace("4417", "9999")).into(),
        password: Some(String::new()).into(),
        ..Default::default()
    });
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let camera_port = listener.local_addr()?.port();
    tokio::spawn(camera.clone().serve(listener));

    let port = free_port()?;
    let origin = format!("http://127.0.0.1:{port}");
    let config = directory.path().join("bridge.json5");
    let first_password = "Fixture<Pass&123>";
    let mut body = json!({
        "http":{"listen":format!("127.0.0.1:{port}"),"token":PRIMARY},
        "mqtt":{"url":"mqtt://127.0.0.1:1","baseTopic":"fixture"},
        "cameras":[],
        "api":{"storageDirectory":vault,"targets":{"e1-pro":{
            "cameraId":"e1-pro","host":"127.0.0.1","port":camera_port,"initialPassword":first_password
        }}}
    });
    std::fs::write(&config, body.to_string())?;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()?;
    let mut bridge = start(&config, &client, &origin).await?;
    let targets = response(
        &client,
        &format!("{origin}/api/enrollment-targets"),
        PRIMARY,
        None,
    )
    .await
    .1;
    assert_eq!(targets["targets"], json!(["e1-pro"]));
    assert!(!targets.to_string().contains(first_password));
    let id = Uuid::new_v4();
    let enrollments = format!("{origin}/api/enrollments");
    let resume = format!("{enrollments}/{id}/resume");
    let request =
        json!({"id":id,"target":"e1-pro","method":"initialize","uid":UID,"confirmation":true});
    let unsupported = response(&client, &enrollments, PRIMARY, Some(request.clone())).await;
    assert_eq!(unsupported.1["status"], "local-setup-required");
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), 0);
    *camera.version_info.lock().unwrap() = Some(version.into());
    camera
        .users
        .lock()
        .unwrap()
        .insert("existing".into(), (false, "fixture".into()));
    let configured = response(&client, &resume, PRIMARY, Some(json!({}))).await;
    assert_eq!(configured.1["status"], "local-setup-required");
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), 0);
    camera.users.lock().unwrap().clear();
    let initialized = response(&client, &resume, PRIMARY, Some(json!({}))).await;
    assert_eq!(initialized.1["status"], "restart-required");
    assert!(!initialized.1.to_string().contains(first_password));
    assert_eq!(
        camera.password.lock().unwrap().as_deref(),
        Some(first_password)
    );
    let replay = response(&client, &enrollments, PRIMARY, Some(request)).await;
    assert_eq!(replay.1["status"], "restart-required");
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), 1);
    bridge.kill().await?;

    body["api"]["targets"] = json!({});
    body["cameras"] = json!([{"id":"e1-pro","host":"127.0.0.1","username":"admin","password":first_password,
        "channel":0,"baichuanPort":camera_port,"onvifPort":null,"eventTransport":"auto","ptz":"auto"}]);
    std::fs::write(&config, body.to_string())?;
    let _bridge = start(&config, &client, &origin).await?;
    let status = response(&client, &format!("{enrollments}/{id}"), PRIMARY, None).await;
    assert_eq!(status.1["status"], "active");
    let accounts = response(
        &client,
        &format!("{origin}/api/cameras/e1-pro/users"),
        PRIMARY,
        None,
    )
    .await;
    assert_eq!(accounts.1["users"][0]["username"], "admin");
    assert_eq!(accounts.1["manageAccounts"], true);
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), 1);
    let users = format!("{origin}/api/cameras/e1-pro/users");
    let add = json!({"id":Uuid::new_v4(),"action":"add","role":"user","username":"viewer","password":"Fixture-user-123","confirmation":true});
    assert_eq!(
        response(&client, &users, PRIMARY, Some(add)).await.1["status"],
        "verified"
    );
    let remove =
        json!({"id":Uuid::new_v4(),"action":"remove","username":"viewer","confirmation":true});
    assert_eq!(
        response(&client, &users, PRIMARY, Some(remove)).await.1["status"],
        "verified"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires Mosquitto on PATH"]
async fn single_psk_enrollment_and_interrupted_rotation_recover_after_restart() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let vault = directory.path().join("vault");
    std::fs::create_dir(&vault)?;
    std::fs::set_permissions(&vault, std::fs::Permissions::from_mode(0o700))?;
    let mut image = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut image).encode(
        &[95; 48],
        4,
        4,
        image::ExtendedColorType::Rgb8,
    )?;
    let camera = Arc::new(baichuan::Camera {
        password: Some(String::new()).into(),
        jpeg: Some(image),
        ..Default::default()
    });
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(camera.clone().serve(listener));
    let imported = Arc::new(baichuan::Camera::default());
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let second = listener.local_addr()?;
    tokio::spawn(imported.clone().serve(listener));

    let rtsp_listener = TcpListener::bind("127.0.0.1:0").await?;
    let rtsp_port = rtsp_listener.local_addr()?.port();
    tokio::spawn(rtsp::fixture(rtsp_listener));
    let broker = free_port()?;
    let mqtt_config = directory.path().join("mosquitto.conf");
    std::fs::write(
        &mqtt_config,
        format!("listener {broker} 127.0.0.1\nallow_anonymous true\npersistence false\n"),
    )?;
    let _broker = Command::new("mosquitto")
        .arg("-c")
        .arg(mqtt_config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("fixture Mosquitto")?;
    let port = free_port()?;
    let origin = format!("http://127.0.0.1:{port}");
    let config = directory.path().join("bridge.json5");
    let body = json!({
      "mqtt":{"url":format!("mqtt://127.0.0.1:{broker}"),"baseTopic":"fixture"},
      "http":{"listen":format!("127.0.0.1:{port}"),"token":PRIMARY},
      "cameras":[],
      "api":{
        "serviceToken":SERVICE,"adminToken":ADMIN,"storageDirectory":vault,"cameraIds":["new","imported"],"commands":true,"events":true,
        "media": {"new":{"port":rtsp_port,"streams":["main"]}},
        "callbackDestinations": ["http://127.0.0.1:9/events"],
        "targets":{
          "factory":{"host":"127.0.0.1","port":address.port(),"cameraId":"new"},
          "import":{"host":"127.0.0.1","port":second.port(),"cameraId":"imported","importPassword":baichuan::PASSWORD}
        }
      }
    });
    std::fs::write(&config, body.to_string())?;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()?;
    let mut bridge = start(&config, &client, &origin).await?;
    let base = format!("{origin}/api");
    let url = |path: &str| format!("{base}{path}");
    assert_eq!(
        response(&client, &url("/contract"), PRIMARY, None).await.0,
        200
    );
    for retired in [SERVICE, ADMIN] {
        for endpoint in [
            "/contract",
            "/inventory",
            "/enrollment-targets",
            "/callbacks",
            "/events",
        ] {
            assert_eq!(
                response(&client, &url(endpoint), retired, None).await.0,
                401,
                "{endpoint}"
            );
        }
        let mut ws = format!("ws://127.0.0.1:{port}/ws").into_client_request()?;
        ws.headers_mut()
            .insert("authorization", format!("Bearer {retired}").parse()?);
        assert!(
            matches!(connect_async(ws).await, Err(tokio_tungstenite::tungstenite::Error::Http(reply)) if reply.status() == 401)
        );
    }
    assert_eq!(
        response(&client, &url("/contract"), PRIMARY, None).await.1["version"],
        1
    );
    assert_eq!(
        response(&client, &url("/contract"), PRIMARY, None).await.1["callbacks"],
        true
    );
    let callback =
        json!({"id":"receiver","url":"http://127.0.0.1:9/events","events":["fixture-only"]});
    let mut other_destination = callback.clone();
    other_destination["url"] = json!("http://127.0.0.1:9/unlisted");
    assert_eq!(
        response(
            &client,
            &url("/callbacks"),
            PRIMARY,
            Some(other_destination)
        )
        .await
        .0,
        403
    );
    let added = response(&client, &url("/callbacks"), PRIMARY, Some(callback.clone())).await;
    assert_eq!(added.1["ok"], true, "{added:?}");
    let mut updated = callback;
    updated["events"] = json!(["another-fixture-event"]);
    let updates = client
        .put(url("/callbacks/receiver"))
        .bearer_auth(PRIMARY)
        .header("content-type", "application/json")
        .body(updated.to_string())
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let updates: Value = serde_json::from_str(&updates)?;
    assert_eq!(updates["callbacks"][0]["events"], updated["events"]);
    assert_eq!(
        response(&client, &url("/callbacks"), PRIMARY, None).await.1["callbacks"][0]["id"],
        "receiver"
    );
    let removed = client
        .delete(url("/callbacks/receiver"))
        .bearer_auth(PRIMARY)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let removed: Value = serde_json::from_str(&removed)?;
    assert_eq!(removed["callbacks"], json!([]));

    let rejected = json!({"id":Uuid::new_v4(),"target":"factory","method":"initialize","uid":UID,"confirmation":false});
    assert_eq!(
        response(&client, &url("/enrollments"), PRIMARY, Some(rejected))
            .await
            .0,
        422
    );
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), 0);
    let enrollment = Uuid::new_v4();
    let request = json!({"id":enrollment,"target":"factory","method":"initialize","uid":UID,"confirmation":true,"enableRtsp":true});
    let enrolled = response(
        &client,
        &url("/enrollments"),
        PRIMARY,
        Some(request.clone()),
    )
    .await;
    assert_eq!(enrolled.1["status"], "restart-required", "{:?}", enrolled);
    assert_eq!(enrolled.1["camera_id"], "new");
    assert_eq!(enrolled.1["rtsp"], "enabled");
    assert_eq!(camera.rtsp_writes.load(Ordering::SeqCst), 1);
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), 1);
    let current_password = camera.password.lock().unwrap().clone().unwrap();
    assert!(!enrolled.1.to_string().contains(&current_password));
    assert_eq!(
        std::fs::metadata(vault.join("vault.json"))?
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        response(&client, &url("/enrollments"), PRIMARY, Some(request))
            .await
            .0,
        200
    );
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), 1);

    let invalid = json!({"id":Uuid::new_v4(),"target":"import","method":"import","serial":"WRONG-SERIAL","confirmation":true});
    assert_eq!(
        response(&client, &url("/enrollments"), PRIMARY, Some(invalid))
            .await
            .1["status"],
        "identity-mismatch"
    );
    assert_eq!(imported.credential_writes.load(Ordering::SeqCst), 0);

    bridge.kill().await?;
    bridge = start(&config, &client, &origin).await?;
    assert_eq!(
        response(
            &client,
            &url(&format!("/enrollments/{enrollment}")),
            PRIMARY,
            None
        )
        .await
        .1["status"],
        "active"
    );
    assert_eq!(
        discovered(&client, &url("/inventory")).await,
        json!([{"id":"new","stableId":"FIXTURE-SERIAL","identitySource":"serial","previousId":"new","active":true}])
    );
    let until = Instant::now() + Duration::from_secs(6);
    loop {
        let reply = response(&client, &url("/cameras/new/settings/leds"), PRIMARY, None).await;
        if reply.0 == 200 {
            assert_eq!(reply.1["values"]["leds"]["status_led"], false);
            assert!(reply.1["values"]["leds"].get("vendor").is_none());
            break;
        }
        ensure!(Instant::now() < until, "settings discovery failed");
        sleep(Duration::from_millis(50)).await;
    }

    assert_eq!(
        client
            .get(url("/events"))
            .bearer_auth("invalid-token")
            .send()
            .await?
            .status(),
        401
    );
    assert_eq!(
        client
            .get(url("/events?cameras=outside-scope"))
            .bearer_auth(PRIMARY)
            .send()
            .await?
            .status(),
        403
    );
    let mut event_stream = client
        .get(url("/events?cameras=new"))
        .bearer_auth(PRIMARY)
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(event_stream.headers()["content-type"], "text/event-stream");
    assert_eq!(event_stream.headers()["cache-control"], "no-store, private");
    let mut event_buffer = Vec::new();
    let initial = event_message(&mut event_stream, &mut event_buffer).await?;
    assert_eq!(initial["op"], "ready");
    let snapshots = initial["snapshots"]
        .as_array()
        .context("initial snapshots")?;
    assert!(!snapshots.is_empty());
    assert!(snapshots.iter().all(|item| item["camera_id"] == "new"));
    let reply = response(
        &client,
        &url("/cameras/new/command/exec"),
        PRIMARY,
        Some(json!({
            "id":Uuid::new_v4(),"command":{"setting":"status_led","enabled":false}
        })),
    )
    .await;
    assert_eq!(reply.0, 200, "{:?}", reply);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let message = event_message(&mut event_stream, &mut event_buffer).await?;
        if message["op"] == "snapshot" && message["snapshot"]["topic"] == "state/settings" {
            assert_eq!(message["snapshot"]["camera_id"], "new");
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "HTTP event stream did not forward live state"
        );
    }
    drop(event_stream);

    let mut ws_request = format!("ws://127.0.0.1:{port}/ws").into_client_request()?;
    ws_request
        .headers_mut()
        .insert("authorization", format!("Bearer {PRIMARY}").parse()?);
    let (mut ws, _) = connect_async(ws_request).await?;
    ws.send(Message::Text(
        json!({"id":"wrong","op":"subscribe","cameras":["outside-scope"]})
            .to_string()
            .into(),
    ))
    .await?;
    let reply: Value = serde_json::from_str(ws.next().await.unwrap()?.to_text()?)?;
    assert_eq!(reply["ok"], false);
    assert_eq!(reply["errorKey"], "WEBSOCKET_FILTER_INVALID");
    ws.close(None).await?;

    let video = client
        .get(url("/cameras/new/media/main"))
        .bearer_auth(PRIMARY)
        .send()
        .await?;
    ensure!(
        video.status().is_success(),
        "fixture RTSP relay failed: {}",
        video.text().await?
    );
    assert_eq!(
        video.headers()["content-type"],
        "multipart/x-mixed-replace; boundary=reolink-frame"
    );
    let mut video = video;
    let mut received = Vec::new();
    while received.len() < 256 {
        let chunk = video.chunk().await?.context("fixture media ended")?;
        received.extend_from_slice(&chunk);
    }
    assert!(received.starts_with(b"--reolink-frame"));
    assert!(received.windows(3).any(|part| part == b"\xff\xd8\xff"));
    drop(video);

    wui_smoke::run(directory.path(), &origin, broker, &client).await?;

    let rotation = url("/cameras/new/rotation");
    camera.block_after_write.store(true, Ordering::SeqCst);
    let rotate = json!({"id":Uuid::new_v4(),"confirmation":true});
    let interrupted = response(&client, &rotation, PRIMARY, Some(rotate)).await;
    assert_eq!(interrupted.1["status"], "recovery-required");
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), 2);
    bridge.kill().await?;
    bridge = start(&config, &client, &origin).await?;
    assert_eq!(
        discovered(&client, &url("/inventory")).await,
        json!([{"id":"new","stableId":"FIXTURE-SERIAL","identitySource":"serial","previousId":"new","active":false}])
    );
    camera.block_login.store(false, Ordering::SeqCst);
    let recovered = response(
        &client,
        &format!("{rotation}/reconcile"),
        PRIMARY,
        Some(json!({"id":Uuid::new_v4(),"confirmation":true})),
    )
    .await;
    assert_eq!(recovered.1["status"], "restart-required");
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), 2);
    bridge.kill().await?;
    // Move the provisioned camera into static config under a new public ID. Its verified
    // credential and completed rotation still live under the original stable identity.
    let mut moved = body;
    moved["cameras"] = json!([{
        "id":"garage","host":"127.0.0.1","baichuanPort":address.port(),
        "onvifPort":null,"eventTransport":"baichuan","username":"admin"
    }]);
    moved["api"]["targets"]
        .as_object_mut()
        .unwrap()
        .remove("factory");
    moved["api"]["media"] = json!({"garage":{"port":rtsp_port,"streams":["main"]}});
    std::fs::write(&config, moved.to_string())?;
    let _bridge = start(&config, &client, &origin).await?;
    assert_eq!(
        discovered(&client, &url("/inventory")).await,
        json!([{"id":"garage","stableId":"FIXTURE-SERIAL","identitySource":"serial","previousId":"new","active":true}])
    );
    assert_eq!(
        response(&client, &url("/cameras/garage/rotation"), PRIMARY, None)
            .await
            .1["status"],
        "verified"
    );
    let progress = response(
        &client,
        &url(&format!("/enrollments/{enrollment}")),
        PRIMARY,
        None,
    )
    .await
    .1;
    assert_eq!(progress["status"], "active");
    assert_eq!(progress["camera_id"], "garage");
    let blocked = client
        .get(url("/cameras/garage/settings/leds"))
        .bearer_auth(PRIMARY)
        .header("x-camera-stable-id", "replacement")
        .send()
        .await?;
    assert_eq!(blocked.status(), 409);
    assert_eq!(
        serde_json::from_str::<Value>(&blocked.text().await?)?["errorKey"],
        "API_CAMERA_IDENTITY_CHANGED"
    );
    assert_eq!(
        response(&client, &url("/cameras/new/rotation"), PRIMARY, None)
            .await
            .0,
        404
    );
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), 2);
    camera
        .users
        .lock()
        .unwrap()
        .insert("guest".into(), (false, "fixture-guest-before".into()));
    let rtsp = url("/cameras/garage/network/rtsp");
    let before = response(&client, &rtsp, PRIMARY, None).await;
    assert_eq!(
        before.1,
        json!({"supported":true,"enabled":true,"port":8554})
    );
    for enabled in [true, false, false] {
        let res = client
            .put(&rtsp)
            .bearer_auth(PRIMARY)
            .header("content-type", "application/json")
            .body(json!({"enabled":enabled}).to_string())
            .send()
            .await?;
        assert_eq!(res.status(), 200);
        let verified: Value = serde_json::from_str(&res.text().await?)?;
        assert_eq!(verified["enabled"], enabled);
    }
    assert_eq!(camera.rtsp_writes.load(Ordering::SeqCst), 2);
    assert_eq!(
        response(
            &client,
            &url(&format!("/enrollments/{enrollment}")),
            PRIMARY,
            None
        )
        .await
        .1["rtsp"],
        "enabled"
    );
    assert!(!camera.rtsp_enabled.load(Ordering::SeqCst));
    let encoding = url("/cameras/garage/encoding");
    let current = response(&client, &encoding, PRIMARY, None).await.1;
    assert_eq!(current["profiles"][0]["current"]["fps"], 20);
    assert_eq!(current["profiles"][0]["options"][0]["fps"], json!([20, 15]));
    for (fps, expected) in [(19, 422), (15, 200)] {
        let result = client
            .put(&encoding)
            .bearer_auth(PRIMARY)
            .header("content-type", "application/json")
            .body(json!({"stream":"main","settings":{"fps":fps}}).to_string())
            .send()
            .await?;
        assert_eq!(result.status().as_u16(), expected);
    }
    let current = response(&client, &encoding, PRIMARY, None).await.1;
    assert_eq!(current["profiles"][0]["current"]["fps"], 15);
    assert_eq!(current["profiles"][1]["current"]["fps"], 4);
    let clock = url("/cameras/garage/clock");
    assert_eq!(
        response(&client, &clock, PRIMARY, None).await.1["cameraTime"],
        "2026-11-17T12:30:00"
    );
    let change_clock = |change: Value| {
        client
            .put(&clock)
            .bearer_auth(PRIMARY)
            .header("content-type", "application/json")
            .body(change.to_string())
            .send()
    };
    let result = change_clock(json!({"cameraTime":"2028-02-29T23:47:06"})).await?;
    assert_eq!(result.status(), 200);
    let after = response(&client, &clock, PRIMARY, None).await.1;
    assert!(after["cameraTime"].as_str().unwrap() >= "2028-02-29T23:47:07");
    assert_eq!(
        after["settings"],
        json!({"utcOffsetMinutes":-480,"daylightSavingEnabled":false,"ntpEnabled":false})
    );

    let spring = json!({"month":3,"week":2,"weekday":0,"hour":2,"minute":0});
    let fall = json!({"month":11,"week":1,"weekday":0,"hour":2,"minute":0});
    let seasonal =
        json!({"utcOffsetMinutes":-480,"daylightSaving":{"hours":1,"start":spring,"end":fall}});
    let change = json!({"cameraTime":after["cameraTime"],"timeZone":seasonal,"ntpEnabled":true});
    assert_eq!(change_clock(change).await?.status(), 200);
    assert!(
        camera
            .clock_dst
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .contains("<startWeekday>Sunday</startWeekday>")
    );
    let result = change_clock(json!({"cameraTime":"2028-02-29T22:47:06"})).await?;
    assert_eq!(
        serde_json::from_str::<Value>(&result.text().await?)?["errorKey"],
        "API_CLOCK_NTP_ACTIVE"
    );

    let fixed = json!({"utcOffsetMinutes":-420,"daylightSaving":null});
    let change = json!({"cameraTime":"2028-02-29T21:47:06","timeZone":fixed,"ntpEnabled":true});
    assert_eq!(change_clock(change).await?.status(), 200);
    let after = response(&client, &clock, PRIMARY, None).await.1;
    assert_eq!(
        after["settings"],
        json!({"utcOffsetMinutes":-420,"daylightSavingEnabled":false,"ntpEnabled":true})
    );
    assert_eq!(*camera.clock_zone_west.lock().unwrap(), Some(25200));
    let manual = json!({"cameraTime":"2028-02-29T20:47:06","ntpEnabled":false});
    assert_eq!(change_clock(manual).await?.status(), 200);
    assert!(!camera.clock_ntp.load(Ordering::SeqCst));

    for fault in [&camera.forget_clock_on_login, &camera.freeze_clock] {
        fault.store(true, Ordering::SeqCst);
        let result = change_clock(json!({"cameraTime":"2028-02-29T22:47:06"})).await?;
        assert_eq!(result.status(), 502);
        assert_eq!(
            serde_json::from_str::<Value>(&result.text().await?)?["errorKey"],
            "API_CLOCK_UNVERIFIED"
        );
        fault.store(false, Ordering::SeqCst);
    }
    let network = url("/cameras/garage/network");
    let before = response(&client, &network, PRIMARY, None).await;
    assert_eq!(before.0, 200);
    assert_eq!(before.1["link"]["address"], "192.0.2.7");
    assert_eq!(before.1["wifi"]["ssid"], "Fixture network");
    assert!(!before.1.to_string().contains("fixture-old-wifi"));
    let change = json!({"ssid":"Wi-Fi & <Lab>","password":"fixture<&>pass","open":false,"confirmation":true});
    assert_eq!(
        client
            .put(&network)
            .header("content-type", "application/json")
            .body(change.to_string())
            .send()
            .await?
            .status(),
        401
    );
    let rejected = client
        .put(&network)
        .bearer_auth(PRIMARY)
        .header("content-type", "application/json")
        .body(json!({"ssid":"Test","password":"","open":false,"confirmation":true}).to_string())
        .send()
        .await?;
    assert_eq!(rejected.status(), 422);
    assert!(camera.wifi_xml.lock().unwrap().is_none());
    camera.reject_read_command.store(116, Ordering::SeqCst);
    assert!(response(&client, &network, PRIMARY, None).await.1["wifi"].is_null());
    assert_eq!(
        client
            .put(&network)
            .bearer_auth(PRIMARY)
            .header("content-type", "application/json")
            .body(change.to_string())
            .send()
            .await?
            .status(),
        422
    );
    camera.reject_read_command.store(117, Ordering::SeqCst);
    assert_eq!(
        client
            .put(&network)
            .bearer_auth(PRIMARY)
            .header("content-type", "application/json")
            .body(change.to_string())
            .send()
            .await?
            .status(),
        422
    );
    camera.reject_read_command.store(0, Ordering::SeqCst);
    let saved = client
        .put(&network)
        .bearer_auth(PRIMARY)
        .header("content-type", "application/json")
        .body(change.to_string())
        .send()
        .await?;
    assert_eq!(saved.status(), 200);
    assert_eq!(
        serde_json::from_str::<Value>(&saved.text().await?)?["outcome"],
        "confirmed"
    );
    let after = response(&client, &network, PRIMARY, None).await.1;
    assert_eq!(after["wifi"]["ssid"], change["ssid"]);
    assert!(!after.to_string().contains("fixture<&>pass"));
    let accounts = url("/cameras/garage/users");
    let listed = response(&client, &accounts, PRIMARY, None).await;
    assert_eq!(listed.0, 200);
    assert_eq!(listed.1["writable"], true);
    assert_eq!(
        listed.1["users"][0],
        json!({"username":"admin","administrator":true,"bridge_login":true})
    );
    assert_eq!(listed.1["users"][1]["username"], "guest");
    assert!(!listed.1.to_string().contains("fixture-hidden"));
    assert_eq!(response(&client, &accounts, SERVICE, None).await.0, 401);
    assert_eq!(listed.1["manageAccounts"], true);
    let initial = camera.credential_writes.load(Ordering::SeqCst);
    let add = json!({"id":Uuid::new_v4(),"action":"add","role":"administrator","username":"second","password":"fixture-<&>Pass","confirmation":true});
    let remove =
        json!({"id":Uuid::new_v4(),"action":"remove","username":"second","confirmation":true});
    assert_eq!(
        response(&client, &accounts, SERVICE, Some(add.clone()))
            .await
            .0,
        401
    );
    for name in ["admin", "ADMIN", "absent"] {
        let mut invalid = remove.clone();
        invalid["username"] = json!(name);
        assert_eq!(
            response(&client, &accounts, PRIMARY, Some(invalid)).await.0,
            422
        );
    }
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), initial);
    camera.block_after_write.store(true, Ordering::SeqCst);
    assert_eq!(
        response(&client, &accounts, PRIMARY, Some(add.clone()))
            .await
            .1["status"],
        "recovery-required"
    );
    camera.block_login.store(false, Ordering::SeqCst);
    let mut conflicting = add.clone();
    conflicting["password"] = json!("wrong-fixture-99");
    assert_eq!(
        response(&client, &accounts, PRIMARY, Some(conflicting))
            .await
            .0,
        422
    );
    assert_eq!(
        response(&client, &accounts, PRIMARY, Some(add.clone()))
            .await
            .1["status"],
        "verified"
    );
    let list = response(&client, &accounts, PRIMARY, None).await.1;
    assert!(
        list["users"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["username"] == "second" && u["administrator"] == true)
    );
    assert!(!list.to_string().contains("fixture-<&>Pass"));
    let mut duplicate = add.clone();
    duplicate["id"] = json!(Uuid::new_v4());
    duplicate["username"] = json!("SECOND");
    assert_eq!(
        response(&client, &accounts, PRIMARY, Some(duplicate))
            .await
            .0,
        422
    );
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), initial + 1);
    camera.block_after_write.store(true, Ordering::SeqCst);
    assert_eq!(
        response(&client, &accounts, PRIMARY, Some(remove.clone()))
            .await
            .1["status"],
        "recovery-required"
    );
    camera.block_login.store(false, Ordering::SeqCst);
    assert_eq!(
        response(&client, &accounts, PRIMARY, Some(remove.clone()))
            .await
            .1["status"],
        "verified"
    );
    assert_eq!(
        response(&client, &accounts, PRIMARY, Some(remove.clone()))
            .await
            .1["status"],
        "verified"
    );
    assert_eq!(
        response(&client, &accounts, PRIMARY, None).await.1["users"],
        listed.1["users"]
    );
    let baseline = camera.credential_writes.load(Ordering::SeqCst);
    assert_eq!(baseline, initial + 2);
    for (name, status, writes) in [
        ("guest", "verified", baseline + 1),
        ("admin", "restart-required", baseline + 2),
    ] {
        let password = format!("fixture-{name}<&>99");
        let change =
            json!({"id":Uuid::new_v4(),"username":name,"password":password,"confirmation":true});
        let result = response(
            &client,
            &format!("{accounts}/password"),
            PRIMARY,
            Some(change),
        )
        .await;
        assert_eq!(result.1["status"], status, "{result:?}");
        assert!(!result.1.to_string().contains(&password));
        assert_eq!(camera.credential_writes.load(Ordering::SeqCst), writes);
        if name == "admin" {
            assert_eq!(
                response(&client, &url("/cameras/garage/rotation"), PRIMARY, None)
                    .await
                    .1["status"],
                "restart-required"
            );
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Mosquitto and FFmpeg"]
async fn recordings_storage_ranges_and_verified_image_orientation() -> Result<()> {
    let encoded = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=blue:size=160x120:rate=8",
            "-frames:v",
            "1",
            "-c:v",
            "libx264",
            "-threads",
            "1",
            "-x264-params",
            "keyint=1",
            "-f",
            "h264",
            "pipe:1",
        ])
        .output()
        .await?;
    ensure!(encoded.status.success(), "synthetic camera encoder");
    let tlv = |tag: u8, value: &[u8]| {
        let mut output = vec![tag];
        output.extend((value.len() as u16).to_le_bytes());
        output.extend(value);
        output
    };
    let dimensions: Vec<_> = [896u16, 480]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect();
    let bounds: Vec<_> = [100u16, 50, 450, 250, 92]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect();
    let mut raw = tlv(3, &dimensions);
    raw.extend(tlv(1, &tlv(3, &tlv(4, &bounds))));
    let mut lz4 = lz4_flex::frame::FrameEncoder::new(Vec::new());
    std::io::Write::write_all(&mut lz4, &raw)?;
    let mut payload = tlv(1, &[1, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    payload.extend(tlv(2, &tlv(2, &lz4.finish()?)));
    let mut extra = vec![0; 8];
    extra.extend(tlv(255, &payload));
    let mut recording = Vec::new();
    for index in 0..16u32 {
        recording.extend(b"00dcH264");
        recording.extend((encoded.stdout.len() as u32).to_le_bytes());
        recording.extend((extra.len() as u32).to_le_bytes());
        recording.extend((index * 125_000).to_le_bytes());
        recording.extend(0u32.to_le_bytes());
        recording.extend(&extra);
        recording.extend(&encoded.stdout);
        recording.extend(std::iter::repeat_n(
            0,
            encoded.stdout.len().next_multiple_of(8) - encoded.stdout.len(),
        ));
    }
    let camera = Arc::new(baichuan::Camera {
        gallery_jpeg: {
            let mut out = std::io::Cursor::new(Vec::new());
            image::RgbImage::from_pixel(13, 9, image::Rgb([0, 70, 120]))
                .write_to(&mut out, image::ImageFormat::Jpeg)?;
            out.into_inner()
        },
        recording: Some(recording),
        ..Default::default()
    });
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let bc = listener.local_addr()?.port();
    tokio::spawn(camera.clone().serve(listener));
    let directory = tempfile::tempdir()?;
    let broker = free_port()?;
    let mqtt_config = directory.path().join("mqtt.conf");
    std::fs::write(
        &mqtt_config,
        format!("listener {broker} 127.0.0.1\nallow_anonymous true\n"),
    )?;
    let _mqtt = Command::new("mosquitto")
        .arg("-c")
        .arg(&mqtt_config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let port = free_port()?;
    let origin = format!("http://127.0.0.1:{port}");
    let config = directory.path().join("bridge.json5");
    let workspace = directory.path().join("recording-workspace");
    std::fs::create_dir(&workspace)?;
    let log_file = directory.path().join("requests.jsonl");
    let thumbnail_cache = std::env::var("BRIDGE_TEST_VALKEY_URL").map_or_else(
        |_| json!({"backend":"memory"}),
        |url| json!({"backend":"valkey","url":url,"keyPrefix":format!("recording-test-{}",Uuid::new_v4())})
    );
    std::fs::write(&config,json!({
        "mqtt":{"url":format!("mqtt://127.0.0.1:{broker}"),"baseTopic":"fixture"},
        "http":{"listen":format!("127.0.0.1:{port}"),"token":PRIMARY},
        "cameras":[{"id":"fixture","host":"127.0.0.1","baichuanPort":bc,"onvifPort":null,"username":"admin","password":baichuan::PASSWORD,"eventTransport":"baichuan"}],
        "api":{"media":{"fixture":{"port":free_port()?,"streams":["main","sub","third"]}},"previews":{"cache":thumbnail_cache},"recordings":{"temporaryDirectory":workspace,"cache":{"maxBytes":1_048_576,"ttlSeconds":2}}},
        "logging":{"sinks":{"console":{"enabled":false},"file":{"enabled":true,"format":"json","path":log_file}}}
    }).to_string())?;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(20))
        .build()?;
    let mut bridge = start(&config, &client, &origin).await?;
    discovered(&client, &format!("{origin}/api/inventory")).await;
    let display = format!("{origin}/api/cameras/fixture/display");
    let save_overlay = |fields: Value| {
        client
            .put(&display)
            .bearer_auth(PRIMARY)
            .header("content-type", "application/json")
            .body(json!({"fields":fields}).to_string())
            .send()
    };
    let saved = save_overlay(json!({"watermark":false,"background":true})).await?;
    assert_eq!(saved.status(), 200);
    let result: Value = serde_json::from_slice(&saved.bytes().await?)?;
    assert_eq!(result["outcome"], "confirmed");
    assert_eq!(result["fields"]["watermark"], false);
    assert_eq!(result["fields"]["background"], true);
    assert_eq!(result["fields"]["name"], "Fixture overlay");
    camera.reject_display.store(true, Ordering::SeqCst);
    let rejected = save_overlay(json!({"watermark":true})).await?;
    assert_eq!(rejected.status(), 422);
    let rejected: Value = serde_json::from_slice(&rejected.bytes().await?)?;
    assert_eq!(rejected["errorKey"], "API_DISPLAY_REJECTED");
    assert_eq!(rejected["cameraStatus"], 403);
    let failure_id = rejected["correlation_id"]
        .as_str()
        .context("display reference")?;
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let logs = std::fs::read_to_string(&log_file).unwrap_or_default();
        if let Some(entry) = logs
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|line| line["correlationId"] == failure_id)
        {
            assert_eq!(entry["context"]["cameraId"], "fixture");
            assert_eq!(entry["context"]["command"], 45);
            assert_eq!(entry["context"]["cameraStatus"], 403);
            assert_eq!(entry["context"]["requestedFields"], json!(["watermark"]));
            assert!(!entry.to_string().contains("Fixture overlay"));
            break;
        }
        ensure!(
            Instant::now() < until,
            "missing correlated display rejection"
        );
        sleep(Duration::from_millis(20)).await;
    }
    for quality in ["main", "sub", "third"] {
        let mut live = client
            .get(format!("{origin}/api/cameras/fixture/media/{quality}"))
            .bearer_auth(PRIMARY)
            .send()
            .await?;
        assert_eq!(live.status(), 200, "Baichuan fallback for {quality}");
        let mut first = Vec::new();
        while !first.windows(2).any(|marker| marker == [0xff, 0xd9]) {
            first.extend(live.chunk().await?.context("early end of live preview")?);
            ensure!(first.len() < 500_000);
        }
        assert!(
            first.starts_with(b"--reolink-frame") && first.windows(3).any(|p| p == b"\xff\xd8\xff")
        );
    }
    assert_eq!(
        &*camera.live_qualities.lock().unwrap(),
        &[(0, 0), (1, 256), (0, 1024)]
    );
    let gallery = format!("{origin}/api/cameras/fixture/time-lapse/photos");
    let listing = format!("{gallery}?date=2026-11-17&from=11:00:00&to=11:59:59");
    assert_eq!(response(&client, &listing, "wrong", None).await.0, 401);
    let (status, list) = response(&client, &listing, PRIMARY, None).await;
    assert_eq!(status, 200, "{list}");
    camera.empty_gallery_uid.store(true, Ordering::SeqCst);
    let (status, local_sd) = response(&client, &listing, PRIMARY, None).await;
    assert_eq!(status, 200, "{local_sd}");
    assert_eq!(local_sd, list);
    let photos = local_sd["photos"].as_array().unwrap();
    assert_eq!(photos.len(), 17);
    assert!(!list.to_string().contains("private-image"));
    let by_time = |at: &str| photos.iter().find(|p| p["capturedAt"] == at).unwrap();
    by_time("2026-11-17T09:15:16");
    let id = by_time("2026-11-17T09:15:00")["id"].as_str().unwrap();
    let thumbnail = client
        .get(format!("{gallery}/{id}?preview=true"))
        .bearer_auth(PRIMARY)
        .send()
        .await?;
    assert_eq!(thumbnail.status(), 200);
    assert_eq!(thumbnail.headers()["content-type"], "image/jpeg");
    let thumb = thumbnail.bytes().await?;
    assert_eq!(image::guess_format(&thumb)?, image::ImageFormat::Jpeg);
    let decoded = image::load_from_memory(&thumb)?;
    assert_eq!((decoded.width(), decoded.height()), (13, 9));
    let photo = client
        .get(format!("{gallery}/{id}"))
        .bearer_auth(PRIMARY)
        .send()
        .await?;
    assert_eq!(photo.status(), 200);
    assert_eq!(photo.headers()["content-type"], "image/png");
    let pixels = image::load_from_memory(&photo.bytes().await?)?;
    assert_eq!((pixels.width(), pixels.height()), (13, 9));
    assert_eq!(
        response(
            &client,
            &format!("{gallery}?date=2026-11-17&from=12:00:00&to=11:00:00"),
            PRIMARY,
            None
        )
        .await
        .0,
        400
    );
    let storage = format!("{origin}/api/cameras/fixture/storage");
    assert_eq!(response(&client, &storage, "wrong", None).await.0, 401);
    let card = response(&client, &storage, PRIMARY, None).await;
    assert_eq!(card.0, 200, "{card:?}");
    assert_eq!(card.1["cards"][0]["totalBytes"], 63_500_000_000u64);
    assert_eq!(card.1["cards"][0]["usedBytes"], 39_700_000_000u64);
    assert_eq!(card.1["cameraTime"], "2026-11-17T12:30:00");
    let denied = response(&client, &storage, "wrong", None).await;
    let correlation = denied.1["correlation_id"].as_str().unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        let logs = std::fs::read_to_string(&log_file).unwrap_or_default();
        if logs.contains(correlation) {
            assert!(logs.contains("API_SCOPE_DENIED"));
            assert!(!logs.contains(baichuan::PASSWORD) && !logs.contains(PRIMARY));
            break;
        }
        ensure!(Instant::now() < until, "API failure was not logged");
        sleep(Duration::from_millis(20)).await;
    }
    // Correlation IDs connect traces; reusing one must not suppress a later failed request.
    let repeated = Uuid::new_v4().to_string();
    for _ in 0..2 {
        assert_eq!(
            client
                .get(&storage)
                .bearer_auth("wrong")
                .header("x-correlation-id", &repeated)
                .send()
                .await?
                .status(),
            401
        );
    }
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        let logs = std::fs::read_to_string(&log_file).unwrap_or_default();
        if logs.lines().filter(|line| line.contains(&repeated)).count() == 2 {
            break;
        }
        ensure!(Instant::now() < until, "repeated failures were suppressed");
        sleep(Duration::from_millis(20)).await;
    }
    let search = format!("{origin}/api/cameras/fixture/recordings?date=2026-11-17&stream=sub");
    // Storage statistics, the camera clock and the recording index are independent reads.
    for (command, url, status, key) in [
        (102, &storage, 502, "API_STORAGE_READ_FAILED"),
        (104, &storage, 200, "API_CAMERA_CLOCK_READ_FAILED"),
        (14, &search, 502, "API_RECORDING_SEARCH_FAILED"),
    ] {
        camera.reject_read_command.store(command, Ordering::SeqCst);
        let (actual, body) = response(&client, url, PRIMARY, None).await;
        assert_eq!(actual, status, "{body:?}");
        if command == 104 {
            assert!(body["cameraTime"].is_null());
            assert_eq!(body["cards"][0]["totalBytes"], 63_500_000_000u64);
        } else {
            assert_eq!(body["errorKey"], key);
        }
        let correlation = body["correlation_id"].as_str().unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            let logs = std::fs::read_to_string(&log_file)?;
            if let Some(line) = logs
                .lines()
                .find(|line| line.contains(correlation) && line.contains(key))
            {
                assert!(line.contains(&format!("command {command} returned status 405")));
                assert!(!line.contains(baichuan::PASSWORD));
                break;
            }
            ensure!(Instant::now() < until, "missing correlated cause for {key}");
            sleep(Duration::from_millis(20)).await;
        }
        if command == 102 {
            assert_eq!(response(&client, &search, PRIMARY, None).await.0, 200);
        }
    }
    camera.reject_read_command.store(0, Ordering::SeqCst);
    let clips = response(&client, &search, PRIMARY, None).await;
    assert_eq!(clips.0, 200, "{clips:?}");
    assert_eq!(clips.1["recordings"].as_array().unwrap().len(), 1);
    let id = clips.1["recordings"][0]["id"].as_str().unwrap();
    let video = format!("{origin}/api/cameras/fixture/recordings/{id}/video");
    let preview = format!("{origin}/api/cameras/fixture/recordings/{id}/preview");
    assert_eq!(
        client
            .get(&preview)
            .bearer_auth("wrong")
            .send()
            .await?
            .status(),
        401
    );
    for _ in 0..2 {
        let response = client.get(&preview).bearer_auth(PRIMARY).send().await?;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "image/jpeg");
        let jpeg = response.bytes().await?;
        assert!(jpeg.starts_with(&[0xff, 0xd8]) && jpeg.ends_with(&[0xff, 0xd9]));
    }
    assert_eq!(camera.recording_previews.load(Ordering::SeqCst), 1);
    assert_eq!(camera.recording_downloads.load(Ordering::SeqCst), 0);
    let result = client.get(&video).bearer_auth(PRIMARY).send().await?;
    ensure!(
        result.status().is_success(),
        "recording media returned {}",
        result.status()
    );
    let data = result.bytes().await?;
    assert!(data.windows(4).any(|w| w == b"ftyp"));
    let partial = client
        .get(&video)
        .bearer_auth(PRIMARY)
        .header("range", "bytes=0-31")
        .send()
        .await?;
    assert_eq!(partial.status(), 206);
    assert_eq!(partial.bytes().await?, data[..32]);
    assert_eq!(camera.recording_downloads.load(Ordering::SeqCst), 1);
    let metadata = response(
        &client,
        &format!("{origin}/api/cameras/fixture/recordings/{id}/metadata"),
        PRIMARY,
        None,
    )
    .await;
    assert_eq!(metadata.0, 200);
    assert_eq!(metadata.1["source"], "recording");
    let samples = metadata.1["frames"].as_array().unwrap();
    assert_eq!(samples.len(), 16);
    assert_eq!(samples[1]["offsetMs"], 125);
    assert_eq!(samples[0]["objects"][0]["class"], "person");
    assert!(
        (samples[0]["objects"][0]["bounding_box"]["top"]
            .as_f64()
            .unwrap()
            - 50.0 / 480.0)
            .abs()
            < 1e-12
    );
    assert_eq!(
        camera.recording_downloads.load(Ordering::SeqCst),
        1,
        "metadata reuses the prepared download"
    );
    assert!(
        std::fs::read_dir(&workspace)?.any(|entry| entry.is_ok_and(|entry| entry
            .file_name()
            .to_string_lossy()
            .starts_with("reolink-recording-")))
    );
    let archived = directory.path().join("archived-workspace");
    std::fs::rename(&workspace, &archived)?;
    let contract = response(&client, &format!("{origin}/api/contract"), PRIMARY, None)
        .await
        .1;
    assert_eq!(contract["storageCameras"], json!(["fixture"]));
    assert_eq!(contract["recordingsCameras"], json!([]));
    assert_eq!(
        response(&client, &storage, PRIMARY, None).await.1["playbackAvailable"],
        false
    );
    assert_eq!(
        client
            .get(&video)
            .bearer_auth(PRIMARY)
            .send()
            .await?
            .status(),
        422
    );
    assert_eq!(camera.recording_downloads.load(Ordering::SeqCst), 1);
    std::fs::rename(&archived, &workspace)?;
    assert_eq!(
        response(&client, &format!("{origin}/api/contract"), PRIMARY, None)
            .await
            .1["recordingsCameras"],
        json!(["fixture"])
    );

    camera.recording_end_status.store(331, Ordering::SeqCst);
    sleep(Duration::from_millis(2100)).await;
    let expired = client
        .get(&video)
        .bearer_auth(PRIMARY)
        .header("range", "bytes=0-31")
        .send()
        .await?;
    assert_eq!(expired.status(), 206);
    assert_eq!(expired.bytes().await?, data[..32]);
    assert_eq!(camera.recording_downloads.load(Ordering::SeqCst), 2);

    let until = Instant::now() + Duration::from_secs(8);
    loop {
        if response(
            &client,
            &format!("{origin}/api/cameras/fixture/settings/image"),
            PRIMARY,
            None,
        )
        .await
        .0 == 200
        {
            break;
        }
        ensure!(Instant::now() < until, "orientation discovery incomplete");
        sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(camera.writes.load(Ordering::SeqCst), 0);
    for action in ["hflip", "vflip", "clockwise", "counterclockwise"] {
        let changed = response(
            &client,
            &format!("{origin}/api/cameras/fixture/command/exec"),
            PRIMARY,
            Some(json!({
                "id":Uuid::new_v4(),"command":{"setting":"image_orientation","action":action}
            })),
        )
        .await;
        assert_eq!(changed.1["ok"], true, "{changed:?}");
        assert_eq!(changed.1["verified"], true, "{changed:?}");
    }
    assert!(camera.mirror.load(Ordering::SeqCst));
    assert!(camera.flip.load(Ordering::SeqCst));
    assert_eq!(camera.rotation.load(Ordering::SeqCst), 0);
    assert_eq!(camera.writes.load(Ordering::SeqCst), 4);

    bridge.kill().await?;
    bridge.wait().await?;
    let limited_workspace = directory.path().join("limited-recording-workspace");
    std::fs::create_dir(&limited_workspace)?;
    let mut limited: Value = serde_json::from_slice(&std::fs::read(&config)?)?;
    limited["api"]["recordings"] =
        json!({"temporaryDirectory":limited_workspace,"cache":{"maxBytes":32,"ttlSeconds":600}});
    std::fs::write(&config, limited.to_string())?;
    let mut limited_bridge = start(&config, &client, &origin).await?;
    assert_eq!(response(&client, &search, PRIMARY, None).await.0, 200);
    let oversized = client.get(&video).bearer_auth(PRIMARY).send().await?;
    assert_eq!(oversized.status(), 413);
    let details: Value = serde_json::from_str(&oversized.text().await?)?;
    assert_eq!(details["stage"], "cache-limit");
    assert_eq!(camera.recording_downloads.load(Ordering::SeqCst), 3);
    assert_eq!(std::fs::read_dir(&limited_workspace)?.count(), 0);
    camera.reject_recording_cover.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        let thumbnail = client
            .get(format!("{preview}?offsetMs=1000"))
            .bearer_auth(PRIMARY)
            .send()
            .await?;
        assert_eq!(thumbnail.status(), 200);
        assert_eq!(thumbnail.headers()["x-preview-offset-ms"], "0");
        assert!(thumbnail.bytes().await?.starts_with(&[0xff, 0xd8]));
    }
    assert_eq!(
        camera.recording_downloads.load(Ordering::SeqCst),
        4,
        "fallback reads one keyframe and caches it"
    );
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        let logs = std::fs::read_to_string(&log_file).unwrap_or_default();
        if logs.contains("API_RECORDING_PREVIEW_FALLBACK") {
            assert!(logs.contains("returned status 405"));
            break;
        }
        ensure!(
            Instant::now() < until,
            "thumbnail fallback cause was not logged"
        );
        sleep(Duration::from_millis(20)).await;
    }
    if std::env::var("BRIDGE_TEST_VALKEY_URL").is_ok() {
        limited_bridge.kill().await?;
        limited_bridge.wait().await?;
        limited_bridge = start(&config, &client, &origin).await?;
        assert_eq!(response(&client, &search, PRIMARY, None).await.0, 200);
        assert_eq!(
            client
                .get(format!("{preview}?offsetMs=1000"))
                .bearer_auth(PRIMARY)
                .send()
                .await?
                .status(),
            200
        );
        assert_eq!(
            camera.recording_downloads.load(Ordering::SeqCst),
            4,
            "Valkey thumbnails survive bridge restart"
        );
    }
    assert_eq!(camera.writes.load(Ordering::SeqCst), 4);
    assert_exact_media_deletion(&client, &origin, &camera).await?;
    limited_bridge.kill().await?;
    Ok(())
}

async fn assert_exact_media_deletion(
    client: &Client,
    origin: &str,
    camera: &baichuan::Camera,
) -> Result<()> {
    let media = format!("{origin}/api/cameras/fixture/media");
    let caps = format!("{media}/capabilities");
    let delete = format!("{media}/delete");
    let list = |kind: &str| format!("{media}/files?kind={kind}&date=2026-11-17");
    let (status, flags) = response(client, &caps, PRIMARY, None).await;
    assert_eq!(status, 200, "{flags}");
    assert_eq!(flags["timeLapse"], true);
    assert_eq!(flags["video"], true);
    let body = |kind: &str, files: Value| json!({"kind":kind,"source":flags["source"],"date":"2026-11-17","files":files});
    let call = |payload| response(client, &delete, PRIMARY, Some(payload));
    for query in ["kind=video&month=2026-11-01", "kind=timeLapse"] {
        let (status, data) =
            response(client, &format!("{media}/days?{query}"), PRIMARY, None).await;
        assert_eq!(status, 200, "{data}");
        assert_eq!(data["days"], json!(["2026-11-17"]));
    }
    assert_eq!(
        response(client, &delete, "wrong", Some(json!({}))).await.0,
        401
    );
    camera.reject_read_command.store(114, Ordering::SeqCst);
    assert_eq!(
        call(body("video", json!([]))).await,
        (200, json!({"absent":[],"deleted":[]}))
    );
    camera.reject_read_command.store(0, Ordering::SeqCst);
    assert_eq!(call(body("video", json!(["camera-path"]))).await.0, 400);
    let (status, original) = response(client, &list("timeLapse"), PRIMARY, None).await;
    assert_eq!(status, 200, "{original}");
    assert!(!original.to_string().contains("private-"));
    assert_eq!(
        original["skipped"], 1,
        "the running movie cannot be selected"
    );
    let files = original["files"].as_array().unwrap();
    assert_eq!(files.len(), 18);
    let mut selected = Vec::new();
    for time in ["09:15:00", "09:15:01", "09:15:40"] {
        let file = files
            .iter()
            .find(|f| f["completedAt"].as_str().unwrap().ends_with(time))
            .unwrap();
        selected.push(file["id"].as_str().unwrap().to_owned());
    }
    let mut wrong = body("timeLapse", json!(selected));
    wrong["source"] = json!("0".repeat(64));
    assert_eq!(call(wrong).await.0, 409);
    assert!(camera.deleted_media.lock().unwrap().is_empty());
    let unknown = "f".repeat(64);
    let mut requested = selected.clone();
    requested.extend([selected[0].clone(), unknown.clone()]);
    let (status, done) = call(body("timeLapse", json!(requested))).await;
    assert_eq!(status, 200, "{done}");
    selected.sort();
    assert_eq!(done, json!({"absent":[unknown],"deleted":selected}));
    let (status, again) = call(body("timeLapse", json!(selected))).await;
    assert_eq!(status, 200);
    assert_eq!(again, json!({"absent":selected,"deleted":[]}));
    let remaining = response(client, &list("timeLapse"), PRIMARY, None).await.1;
    assert_eq!(remaining["files"].as_array().unwrap().len(), 15);
    assert_eq!(remaining["skipped"], 1);
    let videos = response(client, &list("video"), PRIMARY, None).await.1;
    assert_eq!(
        videos["files"].as_array().unwrap().len(),
        1,
        "both streams alias one file"
    );
    let id = &videos["files"][0]["id"];
    camera.video_delete_disabled.store(true, Ordering::SeqCst);
    assert_eq!(call(body("video", json!([id]))).await.0, 422);
    camera.video_delete_disabled.store(false, Ordering::SeqCst);
    let mut large: Vec<String> = (0..1023).map(|i| format!("{i:064x}")).collect();
    large.push(id.as_str().unwrap().into());
    let (status, result) = call(body("video", json!(large))).await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["deleted"], json!([id]));
    assert_eq!(result["absent"].as_array().unwrap().len(), 1023);
    assert_eq!(camera.deleted_media.lock().unwrap().len(), 4);
    Ok(())
}
