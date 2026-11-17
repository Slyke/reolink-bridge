use anyhow::{Context, Result, ensure};
use reqwest::{
    Client,
    header::{HeaderMap, HeaderValue},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;
use uuid::Uuid;

const PASSWORD: &str = "wui-wire-fixture-local-password";

pub async fn run(directory: &Path, bridge: &str, broker: u16, client: &Client) -> Result<()> {
    let (Some(root), Some(node)) = (
        std::env::var_os("WUI_TEST_PROJECT"),
        std::env::var_os("WUI_TEST_NODE"),
    ) else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    let port = super::free_port()?;
    let origin = format!("http://127.0.0.1:{port}");
    let config = directory.join("wui.json5");
    let bootstrap_file = directory.join("test-bootstrap");
    std::fs::write(&bootstrap_file, PASSWORD)?;
    std::fs::write(&config,json!({
        "publicOrigin":origin,"listen":{"port":port},"auth":{"mode":"local"},
        "database":{"driver":"sqlite","path":directory.join("wui.sqlite")},
        "bridges":[{
            "label":"Test bridge","id":"fixture","commands":"http","events":"mqtt","capture":"mqtt",
            "http":{"url":bridge,"token":super::SERVICE,"adminToken":super::ADMIN},
            "mqtt":{"url":format!("mqtt://127.0.0.1:{broker}"),"baseTopic":"fixture"}
        }]
    }).to_string())?;
    let bootstrap = Command::new(&node)
        .current_dir(&root)
        .env("WUI_CONFIG", &config)
        .args([
            "dist/server/admin.js",
            "bootstrap",
            "local",
            "fixture-user",
            "Fixture owner",
        ])
        .arg(&bootstrap_file)
        .output()
        .await?;
    ensure!(bootstrap.status.success(), "WUI fixture bootstrap failed");
    let _api = Command::new(&node)
        .current_dir(&root)
        .env("WUI_CONFIG", &config)
        .arg("dist/server/main.js")
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .spawn()?;
    let until = tokio::time::Instant::now() + Duration::from_secs(12);
    loop {
        if client
            .get(format!("{origin}/readyz"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            break;
        }
        ensure!(tokio::time::Instant::now() < until, "WUI fixture not ready");
        tokio::time::sleep(Duration::from_millis(75)).await;
    }
    let base = format!("{origin}/api/v1");
    let sign_in = client
        .post(format!("{base}/session/local"))
        .header("origin", &origin)
        .header("content-type", "application/json")
        .body(json!({"username":"fixture-user","password":PASSWORD}).to_string())
        .send()
        .await?;
    ensure!(sign_in.status().is_success(), "fixture sign-in failed");
    let raw_cookie = sign_in
        .headers()
        .get("set-cookie")
        .context("fixture cookie")?
        .to_str()?;
    let cookie = raw_cookie
        .split(';')
        .next()
        .context("fixture session")?
        .to_owned();
    let session: Value = serde_json::from_str(&sign_in.text().await?)?;
    let mut headers = HeaderMap::new();
    headers.insert("cookie", HeaderValue::from_str(&cookie)?);
    headers.insert("origin", HeaderValue::from_str(&origin)?);
    headers.insert(
        "x-csrf-token",
        HeaderValue::from_str(session["csrf"].as_str().context("fixture csrf")?)?,
    );
    let admin = client
        .get(format!("{base}/admin"))
        .headers(headers.clone())
        .send()
        .await?;
    let admin: Value = serde_json::from_str(&admin.error_for_status()?.text().await?)?;
    let camera = admin["cameras"][0]["id"]
        .as_str()
        .context("fixture camera")?;
    let setting = client
        .patch(format!("{base}/cameras/{camera}/settings/leds"))
        .headers(headers.clone())
        .header("content-type", "application/json")
        .body(
            json!({"operationId":Uuid::new_v4(),"change":{"setting":"status_led","enabled":true}})
                .to_string(),
        )
        .send()
        .await?;
    let setting: Value = serde_json::from_str(&setting.error_for_status()?.text().await?)?;
    assert_eq!(setting["data"]["leds"]["status_led"], true);
    assert!(setting["bridgeCorrelationId"].is_string());

    let image = client
        .post(format!("{base}/cameras/{camera}/snapshots"))
        .headers(headers.clone())
        .header("content-type", "application/json")
        .body(json!({"operationId":Uuid::new_v4()}).to_string())
        .send()
        .await?;
    ensure!(
        image.status().is_success(),
        "bridge capture through WUI failed: {}",
        image.text().await?
    );
    assert_eq!(image.headers()["content-type"], "image/png");
    assert!(image.bytes().await?.starts_with(b"\x89PNG\r\n\x1a\n"));

    let media = client
        .post(format!("{base}/cameras/{camera}/media-sessions"))
        .headers(headers.clone())
        .header("content-type", "application/json")
        .body(json!({"operationId":Uuid::new_v4(),"stream":"main"}).to_string())
        .send()
        .await?;
    let media: Value = serde_json::from_str(&media.error_for_status()?.text().await?)?;
    let mut live = client
        .get(format!(
            "{origin}{}",
            media["data"]["url"].as_str().context("fixture media url")?
        ))
        .headers(headers.clone())
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(
        live.headers()["content-type"],
        "multipart/x-mixed-replace; boundary=wui"
    );
    assert!(live.chunk().await?.is_some());
    client
        .delete(format!("{base}/session"))
        .headers(headers.clone())
        .send()
        .await?
        .error_for_status()?;
    tokio::time::timeout(Duration::from_secs(4), async {
        while let Ok(Some(_)) = live.chunk().await {}
    })
    .await
    .context("media did not close on sign-out")?;
    assert_eq!(
        client
            .get(format!("{base}/cameras/{camera}"))
            .headers(headers)
            .send()
            .await?
            .status()
            .as_u16(),
        401
    );
    Ok(())
}
