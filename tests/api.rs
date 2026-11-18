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
    let request = json!({"id":enrollment,"target":"factory","method":"initialize","uid":UID,"confirmation":true});
    let enrolled = response(
        &client,
        &url("/enrollments"),
        PRIMARY,
        Some(request.clone()),
    )
    .await;
    assert_eq!(enrolled.1["status"], "restart-required", "{:?}", enrolled);
    assert_eq!(enrolled.1["camera_id"], "new");
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
        response(&client, &url("/inventory"), PRIMARY, None).await.1["cameras"],
        json!([{"id":"new","stableId":"new","active":true}])
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
        response(&client, &url("/inventory"), PRIMARY, None).await.1["cameras"],
        json!([{"id":"new","stableId":"new","active":false}])
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
        "id":"garage","stableId":"new","host":"127.0.0.1","baichuanPort":address.port(),
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
        response(&client, &url("/inventory"), PRIMARY, None).await.1["cameras"],
        json!([{"id":"garage","stableId":"new","active":true}])
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
        "api":{"previews":{"cache":thumbnail_cache},"recordings":{"temporaryDirectory":workspace,"cache":{"maxBytes":1_048_576,"ttlSeconds":2}}},
        "logging":{"sinks":{"console":{"enabled":false},"file":{"enabled":true,"format":"json","path":log_file}}}
    }).to_string())?;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(20))
        .build()?;
    let mut bridge = start(&config, &client, &origin).await?;
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
    assert_eq!(samples.len(), 8);
    assert_eq!(samples[1]["offsetMs"], 250);
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
        let _restarted = start(&config, &client, &origin).await?;
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
    Ok(())
}
