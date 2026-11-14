#![cfg(unix)]

#[path = "support/baichuan.rs"]
mod baichuan;

use std::{
    net::{SocketAddr, TcpListener as StdListener},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use futures_util::{SinkExt, StreamExt};
use rumqttc::v5::{
    AsyncClient, Event, MqttOptions,
    mqttbytes::{
        QoS,
        v5::{Packet, Publish},
    },
};
use serde_json::{Value, json};
use tokio::{
    net::{TcpListener, TcpStream},
    process::Command,
    sync::{mpsc, oneshot},
    time::{sleep, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

const WS_TOKEN: &str = "fixture-websocket-token";
const WEBHOOK_TOKEN: &str = "fixture-native-webhook-token";
const PERSON_TOPIC: &str = "tns1:RuleEngine/MyRuleDetector/PeopleDetect";
const VENDOR_TOPIC: &str = "tns1:RuleEngine/FutureDetector/ParcelMoved";

struct Fixture {
    addr: SocketAddr,
    consumer: Mutex<Option<String>>,
    renewals: AtomicUsize,
    unsubscribes: AtomicUsize,
    delivered: mpsc::UnboundedSender<Value>,
}

async fn soap(State(fixture): State<Arc<Fixture>>, body: String) -> Response {
    let doc = roxmltree::Document::parse(&body).unwrap();
    let operation = doc
        .descendants()
        .find(|n| n.has_tag_name("Body"))
        .unwrap()
        .children()
        .find(roxmltree::Node::is_element)
        .unwrap()
        .tag_name()
        .name();
    let response = match operation {
        "GetServices" => return StatusCode::OK.into_response(),
        "GetCapabilities" => format!("<GetCapabilitiesResponse><Capabilities><Events><XAddr>http://{}/onvif/event_service</XAddr></Events></Capabilities></GetCapabilitiesResponse>", fixture.addr),
        "GetDeviceInformation" => "<GetDeviceInformationResponse><Manufacturer>Reolink</Manufacturer><Model>Test Camera</Model></GetDeviceInformationResponse>".into(),
        "GetEventProperties" => "<GetEventPropertiesResponse><TopicSet><Motion/></TopicSet></GetEventPropertiesResponse>".into(),
        "GetServiceCapabilities" => "<GetServiceCapabilitiesResponse><Capabilities MaxPullPoints=\"2\"/></GetServiceCapabilitiesResponse>".into(),
        "GetProfiles" => "<GetProfilesResponse><Profiles token=\"sub\"><VideoAnalyticsConfiguration token=\"fixture-analytics\"/></Profiles></GetProfilesResponse>".into(),
        "GetRules" => r#"<GetRulesResponse><Rule Name="000" Type="CellMotionDetector"><Parameters>
            <SimpleItem Name="Enable" Value="true"/><ElementItem Name="Layout"><Grid Columns="80" Rows="45"/></ElementItem>
            </Parameters></Rule></GetRulesResponse>"#.into(),
        "Subscribe" => {
            let consumer = doc.descendants().find(|n| n.has_tag_name("ConsumerReference")).unwrap()
                .descendants().find(|n| n.has_tag_name("Address")).unwrap().text().unwrap();
            *fixture.consumer.lock().unwrap() = Some(consumer.into());
            format!("<SubscribeResponse><SubscriptionReference><wsa:Address>http://{}/subscription</wsa:Address>\
                <wsa:ReferenceParameters><r:Id>fixture</r:Id></wsa:ReferenceParameters></SubscriptionReference>\
                <TerminationTime>PT3S</TerminationTime></SubscribeResponse>", fixture.addr)
        }
        "Renew" | "Unsubscribe" => {
            let reference = doc.descendants().find(|n| n.has_tag_name(("urn:fixture-ref", "Id"))).unwrap();
            assert_eq!(reference.text(), Some("fixture"));
            assert_eq!(reference.attribute(("http://www.w3.org/2005/08/addressing", "IsReferenceParameter")), Some("true"));
            if operation == "Renew" {
                fixture.renewals.fetch_add(1, Ordering::SeqCst);
                "<RenewResponse><TerminationTime>PT3S</TerminationTime></RenewResponse>".into()
            } else {
                fixture.unsubscribes.fetch_add(1, Ordering::SeqCst);
                "<UnsubscribeResponse/>".into()
            }
        }
        _ => return StatusCode::NOT_IMPLEMENTED.into_response(),
    };
    ([("content-type", "application/soap+xml")], format!(
        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:wsa="http://www.w3.org/2005/08/addressing" xmlns:r="urn:fixture-ref">
        <s:Body>{response}</s:Body></s:Envelope>"#
    )).into_response()
}

async fn callback(
    State(fixture): State<Arc<Fixture>>,
    axum::Json(event): axum::Json<Value>,
) -> StatusCode {
    fixture.delivered.send(event).unwrap();
    StatusCode::NO_CONTENT
}

async fn slow() -> StatusCode {
    sleep(Duration::from_secs(30)).await;
    StatusCode::NO_CONTENT
}

#[tokio::test]
#[ignore = "requires Mosquitto on PATH"]
async fn local_broker_http_websocket_and_camera_controls() {
    let address = free_address();
    let mqtt = free_address();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json5");
    let (delivered, mut callback_events) = mpsc::unbounded_channel();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Arc::new(Fixture {
        addr: listener.local_addr().unwrap(),
        consumer: Mutex::new(None),
        renewals: AtomicUsize::new(0),
        unsubscribes: AtomicUsize::new(0),
        delivered,
    });
    let router = Router::new()
        .route("/onvif/{service}", post(soap))
        .route("/subscription", post(soap))
        .route("/callback", post(callback))
        .route("/slow", post(slow))
        .with_state(fixture.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let mock = Arc::new(baichuan::Camera::default());
    let baichuan_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let baichuan_port = baichuan_listener.local_addr().unwrap().port();
    tokio::spawn(mock.clone().serve(baichuan_listener));

    let mut broker = Command::new("mosquitto")
        .arg("-p")
        .arg(mqtt.port().to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    until(|| StdListener::bind(mqtt).is_err()).await;
    let (publisher, mut observed) = mqtt_client(mqtt, "early").await;
    let retained_command =
        json!({"id":"preloaded","command":{"setting":"status_led","enabled":true}});
    publisher
        .publish(
            "site/cameras/lab/command/exec",
            QoS::AtLeastOnce,
            true,
            retained_command.to_string(),
        )
        .await
        .unwrap();
    next_mqtt(&mut observed, "site/cameras/lab/command/exec").await;
    let callback_url = format!("http://{}/callback", fixture.addr);
    std::fs::write(&path, json!({
        "mqtt":{"url":format!("mqtt://{mqtt}"),"baseTopic":"site/cameras"},
        "http":{"listen":address.to_string(),"advertiseBaseUrl":format!("http://{address}")},
        "websocket":{"path":"/ws","token":"${FIXTURE_WS}"},
        "cameras":[{"id":"lab","host":"127.0.0.1","username":"admin","password":"${FIXTURE_PASSWORD}",
            "onvifPort":fixture.addr.port(),"baichuanPort":baichuan_port,
            "eventTransport":"onvif_push","nativeWebhook":{"token":"${FIXTURE_NATIVE}"}}],
        "callbacks":[
            {"id":"slow","url":"${FIXTURE_CALLBACK}","events":["person"]},
            {"id":"good","url":callback_url,"cameras":["lab"],"events":["person"]}
        ]
    }).to_string()).unwrap();
    let mut bridge = Command::new(env!("CARGO_BIN_EXE_reolink-mqtt-bridge"))
        .arg(&path)
        .env("FIXTURE_WS", WS_TOKEN)
        .env("FIXTURE_PASSWORD", baichuan::PASSWORD)
        .env("FIXTURE_NATIVE", WEBHOOK_TOKEN)
        .env(
            "FIXTURE_CALLBACK",
            format!("http://{}/slow?token=fixture-callback-secret", fixture.addr),
        )
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    until(|| fixture.consumer.lock().unwrap().is_some()).await;

    let url = format!("ws://{address}/ws");
    let error = connect_async(&url).await.unwrap_err();
    assert!(
        matches!(error, tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == 401)
    );
    let mut auth = url.into_client_request().unwrap();
    auth.headers_mut().insert(
        "authorization",
        format!("Bearer {WS_TOKEN}").parse().unwrap(),
    );
    let (mut websocket, _) = connect_async(auth).await.unwrap();
    let reply = request(&mut websocket, json!({"id":"subscribe","op":"subscribe","cameras":["lab"],"events":["person", VENDOR_TOPIC]})).await;
    assert_eq!(reply["ok"], true, "{reply}");
    let consumer = fixture.consumer.lock().unwrap().clone().unwrap();
    let http = reqwest::Client::new();
    let (status, live) = probe(&http, address, "/livez").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(live["ok"], true);
    assert_eq!(live["service"], "reolink-mqtt-bridge");
    assert_eq!(live["version"], env!("CARGO_PKG_VERSION"));
    assert!(live["buildHash"].as_str().is_some());
    assert!(live.get("checks").is_none());
    let (status, waiting) = probe(&http, address, "/readyz").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(waiting["checks"]["cameras"], false);
    let failure = http
        .post(&consumer)
        .body("fixture-invalid")
        .send()
        .await
        .unwrap();
    assert_eq!(failure.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let returned_id = failure.headers()["x-correlation-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let error: Value = serde_json::from_str(&failure.text().await.unwrap()).unwrap();
    assert_eq!(error["errorKey"], "ONVIF_NOTIFY_TYPE_INVALID");
    assert_eq!(error["correlation_id"], returned_id);
    assert_code(&error);
    notify(
        &http,
        &consumer,
        include_str!("fixtures/notify-initial.xml"),
    )
    .await;
    let (_, initial) = next_mqtt(&mut observed, "site/cameras/lab/state/person").await;
    assert_eq!(initial["active"], false);
    while let Ok(packet) = observed.try_recv() {
        assert_ne!(packet.topic.as_ref(), b"site/cameras/lab/events");
    }
    let (_, mut late) = mqtt_client(mqtt, "late").await;
    let retained = [
        "info",
        "capabilities",
        "availability",
        "rules",
        "state/person",
    ];
    let mut retained_seen = std::collections::BTreeMap::new();
    timeout(Duration::from_secs(8), async {
        while retained_seen.len() < retained.len() {
            let packet = late.recv().await.unwrap();
            let topic = std::str::from_utf8(&packet.topic)
                .unwrap()
                .strip_prefix("site/cameras/lab/")
                .unwrap();
            if retained.contains(&topic) && packet.retain {
                retained_seen.insert(
                    topic.to_owned(),
                    serde_json::from_slice::<Value>(&packet.payload).unwrap(),
                );
            }
            assert_ne!(topic, "events");
        }
    })
    .await
    .unwrap();
    assert_eq!(retained_seen["availability"]["transport"], "onvif_push");
    assert_eq!(retained_seen["rules"]["onvif"][0]["enabled"], true);
    assert_eq!(
        retained_seen["rules"]["onvif"][0]["configuration_id"],
        "fixture-analytics"
    );
    assert_eq!(retained_seen["state/person"]["active"], false);
    let (status, ready) = probe(&http, address, "/readyz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ready["checks"], json!({"cameras":true,"mqtt":true}));

    for (body, expected) in [
        (include_str!("fixtures/notify-active.xml"), true),
        (include_str!("fixtures/notify-clear.xml"), false),
    ] {
        notify(&http, &consumer, body).await;
        let (retained, event) = next_mqtt(&mut observed, "site/cameras/lab/events").await;
        assert!(!retained);
        assert_eq!(event["active"], expected);
        assert_eq!(event["vendor_topic"], PERSON_TOPIC);
        assert!(event.get("rule_id").is_none());
        assert_eq!(
            next_ws(&mut websocket, |message| message["op"] == "event").await["event"],
            event
        );
        assert_eq!(
            timeout(Duration::from_secs(2), callback_events.recv())
                .await
                .unwrap()
                .unwrap(),
            event
        );
    }

    let edit = json!({"id":"new","url":callback_url,"events":[VENDOR_TOPIC]});
    assert_eq!(
        request(
            &mut websocket,
            json!({"id":"add","op":"callbacks_add","callback":edit})
        )
        .await["ok"],
        true
    );
    assert_eq!(
        request(
            &mut websocket,
            json!({"id":"remove","op":"callbacks_remove","callback_id":"good"})
        )
        .await["ok"],
        true
    );
    let changed = json!({"id":"new","url":callback_url,"cameras":["lab"],"events":[VENDOR_TOPIC]});
    assert_eq!(
        request(
            &mut websocket,
            json!({"id":"edit","op":"callbacks_update","callback":changed})
        )
        .await["ok"],
        true
    );
    let stored: Value = json5::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(stored["callbacks"][1], changed);
    assert_eq!(stored["callbacks"][0]["url"], "${FIXTURE_CALLBACK}");
    assert_eq!(stored["cameras"][0]["password"], "${FIXTURE_PASSWORD}");
    let listed = request(&mut websocket, json!({"id":"list","op":"callbacks_list"})).await;
    assert_eq!(listed["callbacks"].as_array().unwrap().len(), 2);
    assert!(
        listed["callbacks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == "slow" && item["url"] == "${FIXTURE_CALLBACK}")
    );
    assert!(!listed.to_string().contains("fixture-callback-secret"));
    notify(&http, &consumer, include_str!("fixtures/notify-vendor.xml")).await;
    let (_, vendor) = next_mqtt(&mut observed, "site/cameras/lab/events").await;
    assert_eq!(vendor["vendor_topic"], VENDOR_TOPIC);
    assert_eq!(vendor["objects"][0]["tracking_id"], "42");
    assert!(!vendor.to_string().contains("test-secret"));
    assert_eq!(
        timeout(Duration::from_secs(2), callback_events.recv())
            .await
            .unwrap()
            .unwrap(),
        vendor
    );
    assert_eq!(
        next_ws(&mut websocket, |message| message["op"] == "event").await["event"],
        vendor
    );

    let before = mock.writes.load(Ordering::SeqCst);
    assert_eq!(before, 0);
    let refused = request(&mut websocket, json!({"id":"forbidden","op":"control","camera_id":"lab","command":{"setting":"wifi","enabled":false}})).await;
    assert_eq!(refused["ok"], false);
    assert_eq!(refused["errorKey"], "WEBSOCKET_REQUEST_INVALID");
    assert_code(&refused);
    let reply = request(&mut websocket, json!({"id":"led","op":"control","camera_id":"lab","command":{"setting":"status_led","enabled":true}})).await;
    assert_eq!(reply["verified"], true, "{reply}");
    assert!(mock.led.load(Ordering::SeqCst));

    let command = json!({"id":"mqtt-led","command":{"setting":"status_led","enabled":false}});
    publisher
        .publish(
            "site/cameras/lab/command/exec",
            QoS::AtLeastOnce,
            true,
            command.to_string(),
        )
        .await
        .unwrap();
    assert!(
        timeout(
            Duration::from_millis(350),
            next_mqtt(&mut observed, "site/cameras/lab/command/result")
        )
        .await
        .is_err()
    );
    assert!(mock.led.load(Ordering::SeqCst));
    publisher
        .publish(
            "site/cameras/lab/command/exec",
            QoS::AtLeastOnce,
            false,
            command.to_string(),
        )
        .await
        .unwrap();
    let (retained, result) = next_mqtt(&mut observed, "site/cameras/lab/command/result").await;
    assert!(!retained);
    assert_eq!(result["id"], "mqtt-led");
    assert_eq!(result["verified"], true, "{result}");
    assert!(!mock.led.load(Ordering::SeqCst));
    assert_eq!(mock.writes.load(Ordering::SeqCst), 2);
    let wrong_topic =
        json!({"id":"root-ignored","command":{"setting":"status_led","enabled":true}});
    publisher
        .publish(
            "site/cameras/lab/command",
            QoS::AtLeastOnce,
            false,
            wrong_topic.to_string(),
        )
        .await
        .unwrap();
    assert!(
        timeout(
            Duration::from_millis(300),
            next_mqtt(&mut observed, "site/cameras/lab/command/result")
        )
        .await
        .is_err()
    );
    assert_eq!(mock.writes.load(Ordering::SeqCst), 2);

    let invalid = json!({"id":"invalid","command":{"setting":"motion_detection","sensitivity":0}});
    publisher
        .publish(
            "site/cameras/lab/command/exec",
            QoS::AtLeastOnce,
            false,
            invalid.to_string(),
        )
        .await
        .unwrap();
    let (_, failure) = next_mqtt(&mut observed, "site/cameras/lab/command/result").await;
    assert_eq!(failure["id"], "invalid");
    assert_eq!(failure["ok"], false);
    assert_eq!(failure["errorKey"], "CONTROL_VALIDATION_FAILED");
    assert_code(&failure);
    assert!(!failure.to_string().contains("cause"));

    until(|| mock.webhook.lock().unwrap().is_some()).await;
    let native = mock.webhook.lock().unwrap().clone().unwrap();
    assert!(native.ends_with(&format!("/ingest/reolink/lab/{WEBHOOK_TOKEN}")));
    let response = http
        .post(native)
        .header("content-type", "application/json")
        .body(r#"{"data":{"event":"wake","reason":"doorbell"}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let (_, awake) = next_mqtt(&mut observed, "site/cameras/lab/events").await;
    let (_, doorbell) = next_mqtt(&mut observed, "site/cameras/lab/events").await;
    assert_eq!(awake["type"], "awake");
    assert_eq!(doorbell["type"], "doorbell");

    until(|| fixture.renewals.load(Ordering::SeqCst) > 0).await;
    broker.kill().await.unwrap();
    timeout(Duration::from_secs(7), async {
        loop {
            let (status, failed) = probe(&http, address, "/readyz").await;
            if status == StatusCode::INTERNAL_SERVER_ERROR && failed["checks"]["mqtt"] == false {
                assert_eq!(failed["checks"]["cameras"], true);
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("readiness stayed green during the broker outage");
    assert_eq!(probe(&http, address, "/livez").await.0, StatusCode::OK);
    let mut broker = Command::new("mosquitto")
        .arg("-p")
        .arg(mqtt.port().to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    timeout(Duration::from_secs(8), async {
        while probe(&http, address, "/readyz").await.0 != StatusCode::OK {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("readiness did not recover after the broker restarted");

    assert!(
        Command::new("kill")
            .arg("-TERM")
            .arg(bridge.id().unwrap().to_string())
            .status()
            .await
            .unwrap()
            .success()
    );
    assert!(
        timeout(Duration::from_secs(10), bridge.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert_eq!(fixture.unsubscribes.load(Ordering::SeqCst), 1);
    let (_, offline) = mqtt_client(mqtt, "offline").await;
    let (_, state) = next_mqtt(&mut { offline }, "site/cameras/lab/availability").await;
    assert_eq!(state["online"], false);
    broker.kill().await.unwrap();
}

async fn probe(client: &reqwest::Client, address: SocketAddr, path: &str) -> (StatusCode, Value) {
    let id = "1dbde7cd-7d1f-40d8-a530-45c2bcc98d1a";
    let response = client
        .get(format!("http://{address}{path}"))
        .header("x-correlation-id", id)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["x-correlation-id"], id);
    let status = response.status();
    let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(body["correlation_id"], id);
    (status, body)
}

fn assert_code(failure: &Value) {
    let code = failure["errorCode"].as_str().unwrap();
    assert_eq!(code.len(), 16);
    assert_ne!(code, "0000000000000000");
    assert!(code.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(uuid::Uuid::parse_str(failure["correlation_id"].as_str().unwrap()).is_ok());
}

async fn notify(client: &reqwest::Client, consumer: &str, body: &str) {
    let response = client
        .post(consumer)
        .header("content-type", "application/soap+xml")
        .body(body.to_owned())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn request(ws: &mut Ws, command: Value) -> Value {
    let id = command["id"].clone();
    ws.send(Message::Text(command.to_string().into()))
        .await
        .unwrap();
    next_ws(ws, |value| {
        value["id"] == id || value.get("id").is_some_and(Value::is_null)
    })
    .await
}

async fn next_ws(ws: &mut Ws, matches: impl Fn(&Value) -> bool) -> Value {
    timeout(Duration::from_secs(8), async {
        loop {
            let message = ws.next().await.unwrap().unwrap();
            let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            if matches(&value) {
                return value;
            }
        }
    })
    .await
    .expect("timed out waiting for WebSocket response")
}

async fn mqtt_client(
    addr: SocketAddr,
    id: &str,
) -> (AsyncClient, mpsc::UnboundedReceiver<Publish>) {
    let (client, mut connection) =
        AsyncClient::new(MqttOptions::new(id, "127.0.0.1", addr.port()), 16);
    client
        .subscribe("site/cameras/lab/#", QoS::AtLeastOnce)
        .await
        .unwrap();
    let (ready, connected) = oneshot::channel();
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut ready = Some(ready);
        while let Ok(event) = connection.poll().await {
            match event {
                Event::Incoming(Packet::SubAck(_)) => {
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(());
                    }
                }
                Event::Incoming(Packet::Publish(packet)) => {
                    let _ = sender.send(packet);
                }
                _ => {}
            }
        }
    });
    timeout(Duration::from_secs(5), connected)
        .await
        .unwrap()
        .unwrap();
    (client, receiver)
}

async fn next_mqtt(observer: &mut mpsc::UnboundedReceiver<Publish>, topic: &str) -> (bool, Value) {
    timeout(Duration::from_secs(8), async {
        loop {
            let packet = observer.recv().await.unwrap();
            if packet.topic.as_ref() == topic.as_bytes() {
                return (
                    packet.retain,
                    serde_json::from_slice(&packet.payload).unwrap(),
                );
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for MQTT topic {topic}"))
}

fn free_address() -> SocketAddr {
    StdListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

async fn until(condition: impl Fn() -> bool) {
    timeout(Duration::from_secs(8), async {
        while !condition() {
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("fixture did not become ready");
}
