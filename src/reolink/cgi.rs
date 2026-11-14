use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use url::Url;

use crate::{
    model::{Event, Observation, kind, reolink_timestamp},
    xml,
};

pub struct Webhook {
    pub client: reqwest::Client,
    pub base: Url,
    pub consumer: Url,
    pub username: String,
    pub password: String,
    pub channel: u8,
}

impl Webhook {
    pub async fn configure(&self) -> Value {
        let login = self
            .call(
                "Login",
                None,
                json!({
                    "User": {"userName": self.username, "password": self.password},
                }),
            )
            .await;
        let Some(token) = login
            .as_ref()
            .ok()
            .and_then(|value| value["Token"]["name"].as_str())
            .filter(|value| !value.is_empty())
        else {
            return json!({"protocol":"cgi","requested":true,"verified":false,"status":"login_failed"});
        };
        let result = self.register(token).await;
        let _ = self.call("Logout", Some(token), json!({})).await;
        match result {
            Ok(()) => json!({"protocol":"cgi","requested":true,"reported":true,"verified":true}),
            Err(error) => {
                json!({"protocol":"cgi","requested":true,"verified":false,"status":error.to_string()})
            }
        }
    }

    async fn register(&self, token: &str) -> Result<()> {
        let url = self.consumer.as_str();
        let entries = self.entries(token).await?;
        let same = |entry: &Value| entry["hookUrl"].as_str() == Some(url);
        let verified = |entry: &Value| {
            same(entry)
                && entry["indexEnable"].as_i64() == Some(1)
                && entry["bCustom"].as_i64() != Some(1)
        };
        if entries.iter().any(verified) {
            return Ok(());
        }
        let slot = entries
            .iter()
            .find(|entry| same(entry))
            .or_else(|| {
                entries
                    .iter()
                    .find(|entry| entry["indexEnable"].as_i64() == Some(-1))
            })
            .and_then(|entry| entry["index"].as_u64())
            .ok_or_else(|| anyhow!("no available camera webhook slot"))?;
        self.call(
            "SetWebHook",
            Some(token),
            json!({"WebHook":{
                "channel":self.channel, "index":slot, "indexEnable":1, "hookUrl":url,
                "bCustom":0, "hookBody":"",
            }}),
        )
        .await?;
        ensure!(
            self.entries(token).await?.iter().any(verified),
            "camera did not confirm the webhook URL"
        );
        Ok(())
    }

    async fn entries(&self, token: &str) -> Result<Vec<Value>> {
        let value = self
            .call("GetWebHook", Some(token), json!({"channel":self.channel}))
            .await?;
        value["WebHook"]
            .as_array()
            .cloned()
            .ok_or_else(|| anyhow!("camera did not report CGI webhooks"))
    }

    async fn call(&self, cmd: &'static str, token: Option<&str>, param: Value) -> Result<Value> {
        let mut url = self.base.join("/cgi-bin/api.cgi").expect("valid CGI URL");
        url.query_pairs_mut().append_pair("cmd", cmd);
        if let Some(token) = token {
            url.query_pairs_mut().append_pair("token", token);
        }
        let body = json!([{"cmd":cmd, "action":0, "param":param}]);
        let mut response = self
            .client
            .post(url)
            .timeout(Duration::from_secs(5))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|_| anyhow!("camera CGI connection failed"))?;
        ensure!(
            response.status().is_success(),
            "camera CGI returned HTTP {}",
            response.status().as_u16()
        );
        let mut payload = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow!("cannot read CGI response"))?
        {
            ensure!(
                payload.len() + chunk.len() <= xml::BODY_LIMIT,
                "camera CGI response too large"
            );
            payload.extend_from_slice(&chunk);
        }
        let value: Value =
            serde_json::from_slice(&payload).map_err(|_| anyhow!("invalid CGI response"))?;
        let item = &value[0];
        ensure!(item["code"].as_i64() == Some(0), "camera rejected {cmd}");
        let reply = &item["value"];
        ensure!(
            reply["rspCode"].as_i64().is_none_or(|code| code == 200),
            "camera rejected {cmd}"
        );
        Ok(reply.clone())
    }
}

pub fn parse_notification(
    raw: &[u8],
    id: &str,
    channel: u8,
    received: DateTime<Utc>,
) -> Result<Vec<Observation>> {
    let mut payload: Value =
        serde_json::from_slice(raw).map_err(|_| anyhow!("invalid Reolink CGI webhook body"))?;
    let Some(alarm) = payload.get_mut("alarm").filter(|alarm| alarm.is_object()) else {
        ensure!(
            payload["type"].as_str() == Some("test"),
            "Reolink CGI webhook is missing the alarm"
        );
        return Ok(Vec::new());
    };
    let integer = |value: &Value| value.as_u64().or_else(|| value.as_str()?.parse().ok());
    ensure!(
        integer(&alarm["channel"]).is_none_or(|value| value == u64::from(channel)),
        "wrong Reolink channel"
    );
    let label = alarm["type"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| alarm["type"].as_u64().map(|n| n.to_string()))
        .ok_or_else(|| anyhow!("Reolink CGI webhook is missing the alarm type"))?;
    let normalized = match label.to_ascii_lowercase().as_str() {
        "ai_people" | "ai_person" => "person".into(),
        "ai_vehicle" => "vehicle".into(),
        "ai_dog_cat" | "ai_pet" => "pet".into(),
        "md" | "motion" => "motion".into(),
        _ => kind(&label),
    };
    let mut event = Event::new(
        id,
        normalized,
        "reolink_webhook",
        format!("WebHook/{label}"),
        received,
    );
    event.channel = Some(channel);
    event.camera_time = ["alarmTime", "time"].into_iter().find_map(|field| {
        alarm[field]
            .as_str()
            .and_then(reolink_timestamp)
            .or_else(|| {
                alarm[field]
                    .as_u64()
                    .and_then(|time| reolink_timestamp(&time.to_string()))
            })
    });
    xml::strip_sensitive_fields(alarm);
    event.vendor = alarm.clone();
    // The CGI webhook reports alarm notices. There is no corresponding clear message.
    Ok(vec![Observation {
        event,
        property: None,
        initial: false,
        deleted: false,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cgi_registration_uses_a_free_slot_and_verifies_it() {
        use axum::{Json, Router, extract::State, routing::post};
        use std::sync::{Arc, Mutex};

        let entries = Arc::new(Mutex::new(vec![
            json!({"index":0,"indexEnable":1,"hookUrl":"http://existing.local/"}),
            json!({"index":1,"indexEnable":-1,"hookUrl":""}),
        ]));
        let camera =
            Router::new()
                .route(
                    "/cgi-bin/api.cgi",
                    post(
                        |State(entries): State<Arc<Mutex<Vec<Value>>>>,
                         Json(request): Json<Value>| async move {
                            let command = request[0]["cmd"].as_str().unwrap();
                            let mut entries = entries.lock().unwrap();
                            let value = match command {
                                "Login" => json!({"Token":{"name":"fixture-session-token"}}),
                                "GetWebHook" => json!({"WebHook": *entries}),
                                "SetWebHook" => {
                                    let slot = &request[0]["param"]["WebHook"];
                                    assert_eq!(slot["index"], 1);
                                    assert_eq!(slot["channel"], 0);
                                    entries[1] = slot.clone();
                                    json!({"rspCode":200})
                                }
                                "Logout" => json!({"rspCode":200}),
                                other => panic!("unexpected camera command {other}"),
                            };
                            Json(json!([{"cmd":command,"code":0,"value":value}]))
                        },
                    ),
                )
                .with_state(entries.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move { axum::serve(listener, camera).await.unwrap() });
        let consumer = Url::parse("http://bridge.local/ingest/reolink/lab/fixture").unwrap();
        let webhook = Webhook {
            client: reqwest::Client::new(),
            base,
            consumer,
            username: "admin".into(),
            password: "fixture-password".into(),
            channel: 0,
        };
        let status = webhook.configure().await;
        assert_eq!(status["verified"], true, "{status}");
        let slots = entries.lock().unwrap();
        assert_eq!(slots[0]["hookUrl"], "http://existing.local/");
        assert_eq!(slots[1]["hookUrl"], webhook.consumer.as_str());
        assert_eq!(slots[1]["indexEnable"], 1);
        assert!(!status.to_string().contains("fixture-session-token"));
    }

    #[test]
    fn cgi_alarm_is_a_single_notice_with_camera_time() {
        let events = parse_notification(
            br#"{"alarm":{"type":"AI_PEOPLE","channel":0,"alarmTime":"2026-11-11T17:33:06Z"},
            "secret":"fixture-only"}"#,
            "cam",
            0,
            Utc::now(),
        )
        .unwrap();
        let event = &events[0];
        assert_eq!(event.event.kind, "person");
        assert!(event.event.camera_time.is_some());
        assert!(event.property.is_none());
        assert!(!event.event.vendor.to_string().contains("fixture-only"));
    }
}
