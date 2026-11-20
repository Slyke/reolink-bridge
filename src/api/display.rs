use super::*;
use crate::{
    reolink::wire::{Rejected, Session},
    xml,
};
use roxmltree::Node;

#[derive(Clone, Copy)]
enum Format {
    Name,
    Switch,
    Coordinate,
    Date,
}

struct Field {
    key: &'static str,
    group: &'static str,
    tag: &'static str,
    format: Format,
}
use Format::*;
const GENERAL: &str = "SystemGeneral";
const NAME: &str = "OsdChannelName";
const TIME: &str = "OsdDatetime";
const FIELDS: &[Field] = &[
    Field {
        key: "deviceName",
        group: GENERAL,
        tag: "deviceName",
        format: Name,
    },
    Field {
        key: "dateFormat",
        group: GENERAL,
        tag: "osdFormat",
        format: Date,
    },
    Field {
        key: "name",
        group: NAME,
        tag: "name",
        format: Name,
    },
    Field {
        key: "nameVisible",
        group: NAME,
        tag: "enable",
        format: Switch,
    },
    Field {
        key: "nameX",
        group: NAME,
        tag: "topLeftX",
        format: Coordinate,
    },
    Field {
        key: "nameY",
        group: NAME,
        tag: "topLeftY",
        format: Coordinate,
    },
    Field {
        key: "watermark",
        group: NAME,
        tag: "enWatermark",
        format: Switch,
    },
    Field {
        key: "background",
        group: NAME,
        tag: "enBgcolor",
        format: Switch,
    },
    Field {
        key: "timeVisible",
        group: TIME,
        tag: "enable",
        format: Switch,
    },
    Field {
        key: "timeX",
        group: TIME,
        tag: "topLeftX",
        format: Coordinate,
    },
    Field {
        key: "timeY",
        group: TIME,
        tag: "topLeftY",
        format: Coordinate,
    },
];
impl Field {
    fn encode(&self, value: &Value) -> Option<String> {
        match self.format {
            Name => value
                .as_str()
                .filter(|v| {
                    !v.trim().is_empty() && v.len() <= 31 && !v.chars().any(char::is_control)
                })
                .map(xml::escape),
            Switch => value.as_bool().map(|v| u8::from(v).to_string()),
            Coordinate => value
                .as_u64()
                .filter(|v| *v <= 65536)
                .map(|v| v.to_string()),
            Date => value
                .as_str()
                .filter(|v| matches!(*v, "MDY" | "DMY" | "YMD" | "MDY_CN" | "DMY_CN" | "YMD_CN"))
                .map(str::to_owned),
        }
    }
    fn decode(&self, raw: &str) -> Option<Value> {
        match self.format {
            Name => Some(json!(raw)),
            Date => {
                let value = json!(raw);
                self.encode(&value).map(|_| value)
            }
            Switch => crate::model::boolean(raw).map(Value::Bool),
            Coordinate => raw
                .parse::<u32>()
                .ok()
                .filter(|v| *v <= 65536)
                .map(|v| json!(v)),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Problem {
    Invalid,
    Unsupported,
    Read,
    Rejected,
    Unverified,
}
impl Problem {
    fn response(self, cid: &CorrelationId) -> Response {
        let (status, key) = match self {
            Self::Invalid => (StatusCode::UNPROCESSABLE_ENTITY, "API_DISPLAY_INVALID"),
            Self::Unsupported => (StatusCode::UNPROCESSABLE_ENTITY, "API_DISPLAY_UNSUPPORTED"),
            Self::Read => (StatusCode::BAD_GATEWAY, "API_DISPLAY_READ_FAILED"),
            Self::Rejected => (StatusCode::UNPROCESSABLE_ENTITY, "API_DISPLAY_REJECTED"),
            Self::Unverified => (StatusCode::BAD_GATEWAY, "API_DISPLAY_UNVERIFIED"),
        };
        error(status, key, cid)
    }
}
type Fields = Map<String, Value>;

type DisplayResult<T> = std::result::Result<T, Problem>;

fn parse(raw: &str, channel: u8, general: bool) -> DisplayResult<Fields> {
    let doc = xml::document(raw).map_err(|_| Problem::Read)?;
    let mut fields = Fields::new();
    for group in doc.root_element().children().filter(Node::is_element) {
        let tag = group.tag_name().name();
        if general != (tag == GENERAL) {
            continue;
        }
        if !general
            && xml::child(group, "channelId")
                .and_then(xml::text)
                .and_then(|id| id.parse::<u8>().ok())
                != Some(channel)
        {
            continue;
        }
        for field in FIELDS.iter().filter(|field| field.group == tag) {
            if let Some(node) = xml::child(group, field.tag)
                && let Some(value) = field.decode(node.text().unwrap_or(""))
            {
                fields.insert(field.key.into(), value);
            }
        }
    }
    Ok(fields)
}

async fn read(session: &mut Session, channel: u8, general: bool) -> DisplayResult<Fields> {
    let raw = match session
        .command(
            if general { 104 } else { 44 },
            (!general).then_some(channel),
            "",
        )
        .await
    {
        Ok(raw) => raw,
        Err(e) if e.is::<Rejected>() => return Ok(Fields::new()),
        Err(_) => return Err(Problem::Read),
    };
    parse(&raw, channel, general)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Change {
    fields: Fields,
}

impl Change {
    fn general(&self) -> DisplayResult<bool> {
        let general =
            self.fields.contains_key("deviceName") || self.fields.contains_key("dateFormat");
        if self.fields.is_empty()
            || !self.fields.iter().all(|(key, v)| {
                FIELDS.iter().any(|f| {
                    f.key == key && (f.group == GENERAL) == general && f.encode(v).is_some()
                })
            })
        {
            return Err(Problem::Invalid);
        }
        Ok(general)
    }

    fn body(&self, current: &Fields, channel: u8) -> DisplayResult<Option<String>> {
        if self.fields.keys().any(|key| !current.contains_key(key)) {
            return Err(Problem::Unsupported);
        }
        let mut groups = String::new();
        for group in [GENERAL, NAME, TIME] {
            let mut tags = String::new();
            for field in FIELDS.iter().filter(|f| f.group == group) {
                if let Some(value) = self.fields.get(field.key)
                    && current.get(field.key) != Some(value)
                {
                    let encoded = field.encode(value).ok_or(Problem::Invalid)?;
                    let tag = field.tag;
                    tags.push_str(&format!("<{tag}>{encoded}</{tag}>"));
                }
            }
            if !tags.is_empty() {
                let target = if group == GENERAL {
                    String::new()
                } else {
                    format!("<channelId>{channel}</channelId>")
                };
                groups.push_str(&format!(
                    r#"<{group} version="1.1">{target}{tags}</{group}>"#
                ));
            }
        }
        // These setters accept partial fields. Replaying the full SystemGeneral read would also set the camera clock.
        Ok((!groups.is_empty()).then(|| format!("<body>{groups}</body>")))
    }
}

fn publish_name(api: &Api, id: &str, fields: &Fields) {
    if let Some(name) = fields.get("deviceName")
        && let Some(hub) = &api.hub
        && hub
            .snapshot(id, "info")
            .is_none_or(|s| s.payload["display"]["name"] != *name)
    {
        hub.section(id, "info", "display", json!({"name":name}));
    }
}

pub(super) async fn get(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &cid);
    }
    let Some(device) = api.devices.get(&id) else {
        return Problem::Unsupported.response(&cid);
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    let work = async {
        let mut session = device.connect().await.map_err(|_| Problem::Read)?;
        let result = async {
            let mut fields = read(&mut session, device.channel, true).await?;
            fields.extend(read(&mut session, device.channel, false).await?);
            Ok(fields)
        }
        .await;
        session.logout().await;
        result
    };
    match timeout(Duration::from_secs(20), work)
        .await
        .unwrap_or(Err(Problem::Read))
    {
        Ok(fields) => {
            publish_name(&api, &id, &fields);
            Json(json!({"fields":fields})).into_response()
        }
        Err(problem) => problem.response(&cid),
    }
}

pub(super) async fn set(
    State(api): State<Arc<Api>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(cid): Extension<CorrelationId>,
    body: Result<Json<Change>, JsonRejection>,
) -> Response {
    if !api.authenticated(&headers) {
        return error(StatusCode::UNAUTHORIZED, "API_SCOPE_DENIED", &cid);
    }
    let Ok(Json(change)) = body else {
        return Problem::Invalid.response(&cid);
    };
    let Ok(general) = change.general() else {
        return Problem::Invalid.response(&cid);
    };
    let Some(device) = api.devices.get(&id) else {
        return Problem::Unsupported.response(&cid);
    };
    let Ok(_guard) = api.network_writes[&id].try_lock() else {
        return error(StatusCode::CONFLICT, "API_DISPLAY_BUSY", &cid);
    };
    let Ok(_slot) = api.settings_slots.try_acquire() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "API_READ_BUSY", &cid);
    };
    let work = async {
        let mut session = device.connect().await.map_err(|_| Problem::Read)?;
        let result = async {
            let current = read(&mut session, device.channel, general).await?;
            let Some(body) = change.body(&current, device.channel)? else {
                return Ok(current);
            };
            session
                .command(
                    if general { 105 } else { 45 },
                    (!general).then_some(device.channel),
                    &body,
                )
                .await
                .map_err(|e| {
                    if e.is::<Rejected>() {
                        Problem::Rejected
                    } else {
                        Problem::Unverified
                    }
                })?;
            let after = read(&mut session, device.channel, general)
                .await
                .map_err(|_| Problem::Unverified)?;
            if change
                .fields
                .iter()
                .all(|(key, v)| after.get(key) == Some(v))
            {
                Ok(after)
            } else {
                Err(Problem::Unverified)
            }
        }
        .await;
        session.logout().await;
        result
    };
    match timeout(Duration::from_secs(26), work)
        .await
        .unwrap_or(Err(Problem::Unverified))
    {
        Ok(fields) => {
            publish_name(&api, &id, &fields);
            Json(json!({"outcome":"confirmed","fields":fields,"correlation_id":cid.0}))
                .into_response()
        }
        Err(problem) => problem.response(&cid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn display_writes_only_the_selected_reported_fields_on_the_matching_channel() {
        let xml = r#"<body><OsdChannelName><channelId>1</channelId><name>Other</name></OsdChannelName><OsdChannelName><channelId>0</channelId><name>Entry</name><enWatermark>1</enWatermark></OsdChannelName></body>"#;
        let fields = parse(xml, 0, false).unwrap();
        assert_eq!(fields["name"], "Entry");
        let change = Change {
            fields: json!({"watermark":false,"name":"A & B"})
                .as_object()
                .unwrap()
                .clone(),
        };
        assert!(!change.general().unwrap());
        let out = change.body(&fields, 0).unwrap().unwrap();
        assert!(
            out.contains("<channelId>0</channelId>")
                && out.contains("<enWatermark>0</enWatermark>")
                && out.contains("A &amp; B")
        );
        let current=parse("<body><SystemGeneral><deviceName>old</deviceName><year>2026</year><osdFormat>DMY</osdFormat></SystemGeneral></body>",0,true).unwrap();
        let rename = Change {
            fields: json!({"deviceName":"new"}).as_object().unwrap().clone(),
        };
        assert!(rename.general().unwrap());
        assert_eq!(
            rename.body(&current, 0).unwrap().unwrap(),
            r#"<body><SystemGeneral version="1.1"><deviceName>new</deviceName></SystemGeneral></body>"#
        );
    }
}
