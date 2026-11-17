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

async fn start(config: &Path, client: &Client, origin: &str) -> Result<Child> {
    let bridge = Command::new(env!("CARGO_BIN_EXE_reolink-bridge"))
        .arg(config)
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .spawn()?;
    let until = Instant::now() + Duration::from_secs(8);
    while Instant::now() < until {
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
async fn background_previews_are_scoped_and_wait_for_each_image() -> Result<()> {
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
    let starts = camera.snapshot_requests.lock().unwrap().clone();
    assert!(starts[1].duration_since(starts[0]) >= Duration::from_millis(675));
    assert_eq!(
        response(&client, &contract, SERVICE, None).await.1["previewCameras"],
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
        .bearer_auth(SERVICE)
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
        .bearer_auth(SERVICE)
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(metadata.headers()["etag"], tag);
    assert!(metadata.bytes().await?.is_empty());
    let unchanged = client
        .get(&preview)
        .bearer_auth(SERVICE)
        .header("if-none-match", tag.clone())
        .send()
        .await?;
    assert_eq!(unchanged.status(), 304);
    assert!(unchanged.bytes().await?.is_empty());
    let manual = client
        .get(format!("{preview}?refresh=true"))
        .bearer_auth(SERVICE)
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(manual.headers()["etag"], tag);
    assert_eq!(manual.bytes().await?.as_ref(), image.as_slice());

    camera.reject_snapshots.store(true, Ordering::SeqCst);
    bridge.kill().await?;
    let _restart = start(&config, &client, &origin).await?;
    if valkey.is_some() {
        let until = Instant::now() + Duration::from_secs(4);
        loop {
            let response = client.get(&preview).bearer_auth(SERVICE).send().await?;
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
                .bearer_auth(SERVICE)
                .send()
                .await?
                .status(),
            503
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires Mosquitto on PATH"]
async fn scoped_contract_enrollment_and_interrupted_rotation_recover_after_restart() -> Result<()> {
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
    assert_eq!(
        response(&client, &url("/contract"), ADMIN, None).await.0,
        401
    );
    assert_eq!(
        response(&client, &url("/contract"), SERVICE, None).await.1["version"],
        1
    );
    assert_eq!(
        response(&client, &url("/enrollment-targets"), SERVICE, None)
            .await
            .0,
        401
    );

    let callback =
        json!({"id":"receiver","url":"http://127.0.0.1:9/events","events":["fixture-only"]});
    assert_eq!(
        response(&client, &url("/contract"), SERVICE, None).await.1["callbacks"],
        false
    );
    assert_eq!(
        response(&client, &url("/contract"), PRIMARY, None).await.1["callbacks"],
        true
    );
    let denied = response(&client, &url("/callbacks"), SERVICE, Some(callback.clone())).await;
    assert_eq!(denied.0, 403);
    assert_eq!(denied.1["errorKey"], "CALLBACK_DESTINATION_DENIED");
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
        response(&client, &url("/enrollments"), ADMIN, Some(rejected))
            .await
            .0,
        422
    );
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), 0);
    let enrollment = Uuid::new_v4();
    let request = json!({"id":enrollment,"target":"factory","method":"initialize","uid":UID,"confirmation":true});
    let enrolled = response(&client, &url("/enrollments"), ADMIN, Some(request.clone())).await;
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
        response(&client, &url("/enrollments"), ADMIN, Some(request))
            .await
            .0,
        200
    );
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), 1);

    let invalid = json!({"id":Uuid::new_v4(),"target":"import","method":"import","serial":"WRONG-SERIAL","confirmation":true});
    assert_eq!(
        response(&client, &url("/enrollments"), ADMIN, Some(invalid))
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
            ADMIN,
            None
        )
        .await
        .1["status"],
        "active"
    );
    assert_eq!(
        response(&client, &url("/inventory"), SERVICE, None).await.1["cameras"],
        json!([{"id":"new","active":true}])
    );
    let until = Instant::now() + Duration::from_secs(6);
    loop {
        let reply = response(&client, &url("/cameras/new/settings/leds"), SERVICE, None).await;
        if reply.0 == 200 {
            assert_eq!(reply.1["values"]["leds"]["status_led"], false);
            assert!(reply.1["values"]["leds"].get("vendor").is_none());
            break;
        }
        ensure!(Instant::now() < until, "settings discovery failed");
        sleep(Duration::from_millis(50)).await;
    }

    let mut ws_request = format!("ws://127.0.0.1:{port}/ws").into_client_request()?;
    ws_request
        .headers_mut()
        .insert("authorization", format!("Bearer {SERVICE}").parse()?);
    let (mut ws, _) = connect_async(ws_request).await?;
    ws.send(Message::Text(
        json!({"id":"wrong","op":"subscribe","cameras":["outside-scope"]})
            .to_string()
            .into(),
    ))
    .await?;
    let reply: Value = serde_json::from_str(ws.next().await.unwrap()?.to_text()?)?;
    assert_eq!(reply["ok"], false);
    assert_eq!(reply["errorKey"], "API_SCOPE_DENIED");
    ws.close(None).await?;

    let video = client
        .get(url("/cameras/new/media/main"))
        .bearer_auth(SERVICE)
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
    let interrupted = response(&client, &rotation, ADMIN, Some(rotate)).await;
    assert_eq!(interrupted.1["status"], "recovery-required");
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), 2);
    bridge.kill().await?;
    bridge = start(&config, &client, &origin).await?;
    assert_eq!(
        response(&client, &url("/inventory"), SERVICE, None).await.1["cameras"],
        json!([{"id":"new","active":false}])
    );
    camera.block_login.store(false, Ordering::SeqCst);
    let recovered = response(
        &client,
        &format!("{rotation}/reconcile"),
        ADMIN,
        Some(json!({"id":Uuid::new_v4(),"confirmation":true})),
    )
    .await;
    assert_eq!(recovered.1["status"], "restart-required");
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), 2);
    bridge.kill().await?;
    let _bridge = start(&config, &client, &origin).await?;
    assert_eq!(
        response(&client, &rotation, ADMIN, None).await.1["status"],
        "verified"
    );
    assert_eq!(camera.credential_writes.load(Ordering::SeqCst), 2);
    Ok(())
}
