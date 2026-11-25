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
    TransientRead,
    Rejected(u16),
    Unverified,
}
impl Problem {
    fn transport(error: anyhow::Error) -> Self {
        if crate::reolink::retry::transient(&error) {
            Self::TransientRead
        } else {
            Self::Read
        }
    }
    fn response(self, cid: &CorrelationId) -> Response {
        let (status, key) = match self {
            Self::Invalid => (StatusCode::UNPROCESSABLE_ENTITY, "API_DISPLAY_INVALID"),
            Self::Unsupported => (StatusCode::UNPROCESSABLE_ENTITY, "API_DISPLAY_UNSUPPORTED"),
            Self::Read | Self::TransientRead => {
                (StatusCode::BAD_GATEWAY, "API_DISPLAY_READ_FAILED")
            }
            Self::Rejected(_) => (StatusCode::UNPROCESSABLE_ENTITY, "API_DISPLAY_REJECTED"),
            Self::Unverified => (StatusCode::BAD_GATEWAY, "API_DISPLAY_UNVERIFIED"),
        };
        error(status, key, cid)
    }
}
type Fields = Map<String, Value>;

type DisplayResult<T> = std::result::Result<T, Problem>;

fn overlay_for(group: Node<'_, '_>, channel: u8) -> bool {
    matches!(group.tag_name().name(), NAME | TIME)
        && xml::child(group, "channelId")
            .and_then(xml::text)
            .and_then(|id| id.parse::<u8>().ok())
            == Some(channel)
}

fn parse(raw: &str, channel: u8, general: bool) -> DisplayResult<Fields> {
    let doc = xml::document(raw).map_err(|_| Problem::Read)?;
    let mut fields = Fields::new();
    let matches = |group: &Node<'_, '_>| {
        if general {
            group.has_tag_name(GENERAL)
        } else {
            overlay_for(*group, channel)
        }
    };
    for group in doc
        .root_element()
        .children()
        .filter(Node::is_element)
        .filter(matches)
    {
        let tag = group.tag_name().name();
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

async fn read_raw(session: &mut Session, channel: u8, general: bool) -> DisplayResult<String> {
    match session
        .command(
            if general { 104 } else { 44 },
            (!general).then_some(channel),
            "",
        )
        .await
    {
        Ok(raw) => Ok(raw),
        Err(e) if e.is::<Rejected>() => Ok("<body/>".into()),
        Err(error) => Err(Problem::transport(error)),
    }
}

async fn read(session: &mut Session, channel: u8, general: bool) -> DisplayResult<Fields> {
    parse(
        &read_raw(session, channel, general).await?,
        channel,
        general,
    )
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

    fn body(
        &self,
        current: &Fields,
        raw: &str,
        channel: u8,
        general: bool,
    ) -> DisplayResult<Option<String>> {
        if self.fields.keys().any(|key| !current.contains_key(key)) {
            return Err(Problem::Unsupported);
        }
        let mut edits = Vec::new();
        for field in FIELDS {
            if let Some(value) = self.fields.get(field.key)
                && current.get(field.key) != Some(value)
            {
                let value = field.encode(value).ok_or(Problem::Invalid)?;
                let tag = field.tag;
                edits.push((field, format!("<{tag}>{value}</{tag}>")));
            }
        }
        if edits.is_empty() {
            return Ok(None);
        }
        if general {
            // The general read also contains the clock and login policy; only send edited general fields.
            let tags: String = edits.into_iter().map(|(_, element)| element).collect();
            return Ok(Some(format!(
                r#"<body><SystemGeneral version="1.1">{tags}</SystemGeneral></body>"#
            )));
        }

        // Command 45 takes the channel-name and date/time blocks together. Preserve the
        // camera's layout, including position values outside the editable API range.
        let doc = xml::document(raw).map_err(|_| Problem::Read)?;
        let mut body = String::from("<body>");
        for group in doc
            .root_element()
            .children()
            .filter(Node::is_element)
            .filter(|g| overlay_for(*g, channel))
        {
            let mut replacements = Vec::new();
            for (field, element) in &edits {
                if field.group == group.tag_name().name() {
                    let node = xml::child(group, field.tag).ok_or(Problem::Unsupported)?;
                    replacements.push((node, element.clone()));
                }
            }
            body.push_str(&xml::replace_nodes(group, replacements));
        }
        body.push_str("</body>");
        Ok(Some(body))
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
    let read_camera = || async {
        let mut session = device.connect().await.map_err(Problem::transport)?;
        let result = async {
            let mut fields = read(&mut session, device.channel, true).await?;
            fields.extend(read(&mut session, device.channel, false).await?);
            Ok(fields)
        }
        .await;
        session.logout().await;
        result
    };
    match timeout(
        Duration::from_secs(20),
        device
            .read_retries
            .read_if("display", read_camera, |problem| {
                matches!(problem, Problem::TransientRead)
            }),
    )
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
            let raw = read_raw(&mut session, device.channel, general).await?;
            let current = parse(&raw, device.channel, general)?;
            let Some(body) = change.body(&current, &raw, device.channel, general)? else {
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
                    e.downcast_ref::<Rejected>()
                        .map_or(Problem::Unverified, |rejection| {
                            Problem::Rejected(rejection.status())
                        })
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
        Err(Problem::Rejected(status)) => {
            let command = if general { 105 } else { 45 };
            let mut report = api.hub().diagnostics.error(
                "api::display::set", "API_DISPLAY_REJECTED", Some(cid.0.clone()),
                json!({"cameraId":id, "channel":device.channel, "command":command, "cameraStatus":status,
                    "requestedFields":change.fields.keys().collect::<Vec<_>>(), "httpStatus":422}), None,
            );
            report["cameraStatus"] = json!(status);
            (StatusCode::UNPROCESSABLE_ENTITY, Json(report)).into_response()
        }
        Err(problem) => problem.response(&cid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn display_changes_preserve_the_channel_layout_and_keep_general_writes_partial() {
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
        let out = change.body(&fields, xml, 0, false).unwrap().unwrap();
        assert!(
            out.contains("<channelId>0</channelId>")
                && out.contains("<enWatermark>0</enWatermark>")
                && out.contains("A &amp; B")
        );
        assert!(!out.contains("Other"));
        let original = r#"<body><OsdChannelName version="1.2"><channelId>0</channelId><name>A &amp; B</name><enWatermark>1</enWatermark><enBgcolor>0</enBgcolor></OsdChannelName><OsdDatetime version="1.0"><channelId>0</channelId><topLeftX>-32768</topLeftX><language>English</language><extra>untouched</extra></OsdDatetime></body>"#;
        let toggles = Change {
            fields: json!({"watermark":false,"background":true})
                .as_object()
                .unwrap()
                .clone(),
        };
        let before = parse(original, 0, false).unwrap();
        let saved = toggles.body(&before, original, 0, false).unwrap().unwrap();
        assert_eq!(
            saved,
            original
                .replace("<enWatermark>1", "<enWatermark>0")
                .replace("<enBgcolor>0", "<enBgcolor>1")
        );
        assert!(
            toggles
                .body(&parse(&saved, 0, false).unwrap(), &saved, 0, false)
                .unwrap()
                .is_none()
        );
        let general_raw = "<body><SystemGeneral><deviceName>old</deviceName><year>2026</year><osdFormat>DMY</osdFormat></SystemGeneral></body>";
        let current = parse(general_raw, 0, true).unwrap();
        let rename = Change {
            fields: json!({"deviceName":"new"}).as_object().unwrap().clone(),
        };
        assert!(rename.general().unwrap());
        assert_eq!(
            rename
                .body(&current, general_raw, 0, true)
                .unwrap()
                .unwrap(),
            r#"<body><SystemGeneral version="1.1"><deviceName>new</deviceName></SystemGeneral></body>"#
        );
    }
}
