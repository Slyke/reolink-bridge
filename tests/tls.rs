#![cfg(unix)]

#[path = "support/baichuan.rs"]
mod baichuan;

use std::{
    fs,
    net::{SocketAddr, TcpListener as StdListener},
    os::unix::fs::PermissionsExt,
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    process::{Child, Command},
    time::{sleep, timeout},
};
use tokio_tungstenite::{
    Connector, connect_async_tls_with_config,
    tungstenite::{Message, client::IntoClientRequest},
};

const TOKEN: &str = "fixture-server-token";

#[tokio::test]
#[ignore = "requires Mosquitto on PATH"]
async fn authenticates_https_wss_and_mqtt_tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let directory = tempfile::tempdir().unwrap();
    let local = directory.path().join("local-only");
    fs::create_dir(&local).unwrap();

    let addr = free_address();
    let mqtt = free_address();
    let camera_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let camera_port = camera_listener.local_addr().unwrap().port();
    let camera = Arc::new(baichuan::Camera::default());
    tokio::spawn(camera.serve(camera_listener));

    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let broker_cert = local.join("broker.crt");
    let broker_key = local.join("broker.key");
    fs::write(&broker_cert, cert.pem()).unwrap();
    fs::write(&broker_key, signing_key.serialize_pem()).unwrap();
    fs::set_permissions(&broker_key, fs::Permissions::from_mode(0o600)).unwrap();
    let broker_config = local.join("mosquitto.conf");
    fs::write(
        &broker_config,
        format!(
            "listener {} 127.0.0.1\nallow_anonymous true\ncertfile {}\nkeyfile {}\n",
            mqtt.port(),
            broker_cert.display(),
            broker_key.display(),
        ),
    )
    .unwrap();
    let mut broker = Command::new("mosquitto")
        .arg("-c")
        .arg(broker_config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let path = local.join("bridge.json5");
    let mut config = json!({
        "http": {"listen":addr.to_string(),"tls":{},"token":"TOKEN_REF","websocket":{"path":"/socket"}},
        "mqtt": {"url":format!("mqtts://{mqtt}"),"baseTopic":"tls/cameras"},
        "cameras":[{"id":"cam","host":"127.0.0.1","username":"admin","password":"PASSWORD_REF",
            "onvifPort":null,"baichuanPort":camera_port,"eventTransport":"baichuan"}],
    });
    config["http"]["token"] = format!("{}{{FIXTURE_TOKEN}}", '$').into();
    config["cameras"][0]["password"] = format!("{}{{FIXTURE_CAMERA_PASSWORD}}", '$').into();

    fs::write(&path, config.to_string()).unwrap();
    let mut untrusted = bridge(&path);
    let generated = local.join("https");
    let server_cert = generated.join("server.crt");
    let server_key = generated.join("server.key");
    timeout(Duration::from_secs(5), async {
        while !server_cert.exists() {
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        fs::metadata(&server_key).unwrap().permissions().mode() & 0o077,
        0
    );
    let pem = fs::read(&server_cert).unwrap();
    let trust = reqwest::Certificate::from_pem(&pem).unwrap();
    let https = reqwest::Client::builder()
        .no_proxy()
        .add_root_certificate(trust)
        .build()
        .unwrap();

    check_readiness(&https, addr, false).await;
    assert!(
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("https://{addr}/livez"))
            .send()
            .await
            .is_err()
    );
    stop(&mut untrusted).await;

    config["mqtt"]["tls"] = json!({"caFile":broker_cert});
    fs::write(&path, config.to_string()).unwrap();
    let mut trusted = bridge(&path);
    check_readiness(&https, addr, true).await;
    assert_eq!(fs::read(&server_cert).unwrap(), pem);

    let mut roots = rustls::RootCertStore::empty();
    for certificate in rustls_pemfile::certs(&mut pem.as_slice()) {
        roots.add(certificate.unwrap()).unwrap();
    }
    let websocket_tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let url = format!("wss://{addr}/socket");
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let (mut ws, _) = connect_async_tls_with_config(
        request,
        None,
        false,
        Some(Connector::Rustls(Arc::new(websocket_tls))),
    )
    .await
    .unwrap();
    ws.send(Message::Text(r#"{"id":"tls","op":"snapshot"}"#.into()))
        .await
        .unwrap();
    let message = timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let snapshot: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
    assert_eq!(snapshot["ok"], true);
    ws.close(None).await.unwrap();
    stop(&mut trusted).await;

    // The leaf is trusted but matches 127.0.0.1 only. The name in the URL must still match.
    config["mqtt"]["url"] = format!("mqtts://localhost:{}", mqtt.port()).into();
    // Use the already generated PEM pair as mounted files.
    config["http"]["tls"] = json!({"certFile":server_cert,"keyFile":server_key});
    fs::write(&path, config.to_string()).unwrap();
    let mut wrong_name = bridge(&path);
    check_readiness(&https, addr, false).await;
    stop(&mut wrong_name).await;

    broker.kill().await.unwrap();
}

fn bridge(config: &std::path::Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_reolink-mqtt-bridge"))
        .arg(config)
        .env("FIXTURE_TOKEN", TOKEN)
        .env("FIXTURE_CAMERA_PASSWORD", baichuan::PASSWORD)
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

async fn check_readiness(client: &reqwest::Client, addr: SocketAddr, mqtt: bool) {
    let response = timeout(Duration::from_secs(7), async {
        loop {
            if let Ok(reply) = client.get(format!("https://{addr}/readyz")).send().await {
                let body: Value = serde_json::from_str(&reply.text().await.unwrap()).unwrap();
                if body["checks"] == json!({"mqtt":mqtt,"cameras":true}) {
                    break body;
                }
            }
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("bridge did not report the expected TLS connection state");
    assert_eq!(response["ok"], mqtt);
    assert!(
        client
            .get(format!("https://{addr}/livez"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
}

fn free_address() -> SocketAddr {
    StdListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

async fn stop(bridge: &mut Child) {
    let pid = bridge.id().unwrap();
    assert!(
        Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .await
            .unwrap()
            .success()
    );
    assert!(
        timeout(Duration::from_secs(12), bridge.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
