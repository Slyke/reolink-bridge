pub mod cgi;
pub mod control;
pub mod events;
pub mod metadata;
pub mod wire;

use anyhow::{Result, anyhow, ensure};
use roxmltree::Node;
use serde_json::{Map, Value, json};
use url::Url;

use crate::{
    hub::Hub,
    model::{Rule, boolean, kind},
    xml,
};
use control::{AiClass, Settings};
use wire::{Device, Session};

const ADVANCED_RULES: [(u32, &str); 5] = [
    (527, "crossline"),
    (529, "intrusion"),
    (531, "loitering"),
    (549, "legacy"),
    (551, "loss"),
];

pub async fn discover(device: &Device, webhook: Option<&Url>, id: &str, hub: &Hub) -> Result<()> {
    let mut session = device.connect().await?;
    let channel = device.channel;
    if let Ok(raw) = session.command(80, None, "").await
        && let Ok(doc) = xml::document(&raw)
        && let Some(info) = xml::find(doc.root_element(), "VersionInfo")
    {
        hub.section(
            id,
            "info",
            "baichuan",
            json!({
                "manufacturer": "Reolink",
                "name": xml::value(info, "name"),
                "model": xml::value(info, "type"),
                "hardware": xml::value(info, "hardwareVersion"),
                "firmware": xml::value(info, "firmwareVersion"),
                "item_number": xml::value(info, "itemNo"),
                "vendor": xml::tree(info),
            }),
        );
    }
    let (ai_types, ai_configuration) = events::ai_types(&mut session, channel).await;
    let mut controls = Map::new();
    let mut settings = vec![Settings::Leds, Settings::Spotlight, Settings::Motion];
    settings.extend(
        ai_types
            .iter()
            .filter_map(|class| AiClass::from_vendor(class))
            .map(Settings::Ai),
    );
    for setting in settings {
        if let Some((state, supported)) = setting.inspect(&mut session, channel).await {
            hub.section(id, "state/settings", setting.key(), state);
            controls.insert(setting.key().into(), supported);
        }
    }

    let (rules, rule_capabilities) = rules(&mut session, channel).await;
    if !rule_capabilities.is_empty() {
        hub.section(id, "rules", "baichuan", json!(rules));
    }
    let native_webhook = webhook_status(&mut session, webhook).await;
    if webhook.is_some() && native_webhook["verified"] != true {
        hub.diagnostics.error(
            "reolink::discover",
            "HA_CFG_WEBHOOK_SETUP_FAILED",
            None,
            json!({"cameraId":id}),
            None,
        );
    }
    hub.section(
        id,
        "capabilities",
        "baichuan",
        json!({
            "ai_types": ai_types.iter().map(|class| kind(class)).collect::<Vec<_>>(),
            "ai_configuration": ai_configuration,
            "controls": controls,
            "advanced_rules": rule_capabilities,
            "native_webhook": native_webhook,
        }),
    );
    session.logout().await;
    Ok(())
}

async fn rules(session: &mut Session, channel: u8) -> (Vec<Rule>, Map<String, Value>) {
    let mut rules = Vec::new();
    let mut capabilities = Map::new();
    for (command, kind) in ADVANCED_RULES {
        let Ok(reply) = session.command(command, Some(channel), "").await else {
            continue;
        };
        let Ok(doc) = xml::document(&reply) else {
            continue;
        };
        let item_tag = format!("{kind}DetectItem");
        let Some(config) = doc.root_element().children().find(Node::is_element) else {
            continue;
        };
        let mut feature = json!({"reported":true});
        for name in [
            "maxNum",
            "pointMinNum",
            "pointMaxNum",
            "pointMaxNUm",
            "supportTargetSize",
        ] {
            if let Some(value) = xml::value(config, name) {
                feature[name] = json!(value);
            }
        }
        capabilities.insert(kind.into(), feature);
        for item in config
            .descendants()
            .filter(|node| node.tag_name().name().eq_ignore_ascii_case(&item_tag))
        {
            let id = xml::value(item, "location")
                .or_else(|| xml::value(item, "id"))
                .map(|value| format!("{kind}:{value}"));
            let geometry = item
                .children()
                .filter(|node| {
                    let tag = node.tag_name().name().to_ascii_lowercase();
                    tag.contains("point")
                        || tag.contains("area")
                        || tag.contains("polygon")
                        || tag.contains("region")
                })
                .map(xml::tree)
                .collect();
            rules.push(Rule {
                configuration_id: None,
                source: "baichuan",
                id,
                kind: kind.into(),
                geometry,
                name: xml::value(item, "name").map(str::to_owned),
                enabled: xml::value(item, "enable").and_then(boolean),
                parameters: xml::tree(item),
            });
        }
    }
    (rules, capabilities)
}

async fn webhook_status(session: &mut Session, target: Option<&Url>) -> Value {
    let original = read_webhook(session).await;
    if original.is_err() {
        return json!({"protocol":"ha_cfg","reported":false,"requested":target.is_some()});
    }
    let Some(target) = target else {
        return json!({"protocol":"ha_cfg","reported":true,"requested":false});
    };
    let target = target.as_str();
    let verified = async {
        if original? != (true, target.into()) {
            let url = xml::escape(target);
            let verify_cert = u8::from(target.starts_with("https://"));
            let body = format!(
                r#"<body><HaCfg version="1.1"><enable>1</enable>
                <url>{url}</url><verify_cert>{verify_cert}</verify_cert></HaCfg></body>"#
            );
            session.command(807, None, &body).await?;
            ensure!(
                read_webhook(session).await? == (true, target.into()),
                "webhook verification failed"
            );
        }
        Ok::<_, anyhow::Error>(())
    }
    .await
    .is_ok();
    json!({"protocol":"ha_cfg","reported":true,"requested":true,"verified":verified})
}

async fn read_webhook(session: &mut Session) -> Result<(bool, String)> {
    let raw = session.command(806, None, "").await?;
    let doc = xml::document(&raw)?;
    let node = xml::find(doc.root_element(), "HaCfg")
        .ok_or_else(|| anyhow!("camera does not report HaCfg"))?;
    let enable = xml::value(node, "enable")
        .and_then(boolean)
        .ok_or_else(|| anyhow!("invalid HaCfg reply"))?;
    Ok((enable, xml::value(node, "url").unwrap_or("").into()))
}
