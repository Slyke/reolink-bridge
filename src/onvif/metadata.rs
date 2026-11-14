use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::Utc;
use futures_util::StreamExt;
use retina::client::{
    Credentials, PacketItem, PlayOptions, Session, SessionOptions, SetupOptions, Transport,
};
use roxmltree::Node;
use serde_json::json;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::MetadataStream;
use crate::{
    hub::Hub,
    model::{Bounds, Event, Object, Observation, timestamp},
    xml,
};

pub async fn stream(
    stream: MetadataStream,
    username: &str,
    password: &str,
    id: &str,
    hub: &Hub,
    stop: &CancellationToken,
) -> Result<()> {
    let start = async {
        let options = SessionOptions::default().creds(Some(Credentials {
            username: username.into(),
            password: password.into(),
        }));
        let mut session = Session::describe(stream.url, options)
            .await
            .map_err(|_| anyhow!("RTSP DESCRIBE failed"))?;
        let track = session
            .streams()
            .iter()
            .position(|track| {
                track
                    .encoding_name()
                    .eq_ignore_ascii_case("vnd.onvif.metadata")
            })
            .ok_or_else(|| anyhow!("RTSP SDP has no ONVIF metadata track"))?;
        session
            .setup(
                track,
                SetupOptions::default().transport(Transport::Tcp(Default::default())),
            )
            .await
            .map_err(|_| anyhow!("RTSP metadata SETUP failed"))?;
        let session = session
            .play(PlayOptions::default())
            .await
            .map_err(|_| anyhow!("RTSP metadata PLAY failed"))?;
        Ok::<_, anyhow::Error>((session, track))
    };
    let (mut session, track) = tokio::select! {
        _ = stop.cancelled() => return Ok(()),
        result = timeout(Duration::from_secs(12), start) => result??,
    };
    hub.section(
        id,
        "capabilities",
        "coordinate_stream",
        json!({"transport": "onvif_metadata", "connected": true}),
    );
    let mut bytes = Vec::new();
    let mut discard = false;
    let mut had_objects = false;
    loop {
        let next = tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            result = timeout(Duration::from_secs(45), session.next()) => result?,
        };
        let next = next
            .ok_or_else(|| anyhow!("RTSP metadata session ended"))?
            .map_err(|_| anyhow!("RTSP metadata stream failed"))?;
        let PacketItem::Rtp(packet) = next else {
            continue;
        };
        if packet.stream_id() != track {
            continue;
        }
        if packet.loss() > 0 || bytes.len() + packet.payload().len() > xml::BODY_LIMIT {
            discard = true;
            bytes.clear();
        }
        if !discard {
            bytes.extend_from_slice(packet.payload());
        }
        // ONVIF marks the last packet of each complete XML document.
        if !packet.mark() {
            continue;
        }
        if !discard
            && let Ok(message) = std::str::from_utf8(&bytes)
            && let Ok(doc) = xml::document(message)
        {
            let received = Utc::now();
            for frame in doc.descendants().filter(|node| node.has_tag_name("Frame")) {
                let detections = objects(frame, &stream.reference_frame, stream.dimensions);
                let has_objects = !detections.is_empty();
                if has_objects || had_objects {
                    let mut event = Event::new(
                        id,
                        "objects".into(),
                        "onvif_metadata",
                        "VideoAnalytics/Frame".into(),
                        received,
                    );
                    event.camera_time = frame.attribute("UtcTime").and_then(timestamp);
                    event.objects = detections;
                    event.vendor = json!({
                        "frame_source": frame.attribute("Source"),
                        "reference_frame": stream.reference_frame,
                        "rtp_timestamp": packet.header().timestamp(),
                    });
                    hub.ingest(Observation {
                        event,
                        property: None,
                        initial: false,
                        deleted: false,
                    });
                }
                had_objects = has_objects;
            }
        }
        discard = false;
        bytes.clear();
    }
}

pub fn objects(root: Node<'_, '_>, reference: &str, dimensions: Option<(u32, u32)>) -> Vec<Object> {
    let mut objects: Vec<_> = root
        .descendants()
        .filter(|node| node.has_tag_name("Object"))
        .filter_map(|node| object(node, root, reference, dimensions))
        .collect();
    if objects.is_empty()
        && let Some(object) = object(root, root, reference, dimensions)
    {
        objects.push(object);
    }
    objects
}

fn object(
    node: Node<'_, '_>,
    root: Node<'_, '_>,
    reference: &str,
    dimensions: Option<(u32, u32)>,
) -> Option<Object> {
    let simple_item = |names: &[&str]| {
        node.descendants()
            .filter(|n| n.has_tag_name("SimpleItem"))
            .find_map(|n| {
                names
                    .iter()
                    .any(|name| {
                        n.attribute("Name")
                            .is_some_and(|value| value.eq_ignore_ascii_case(name))
                    })
                    .then(|| n.attribute("Value"))
                    .flatten()
            })
    };
    let tracking_id = node
        .attribute("ObjectId")
        .or_else(|| simple_item(&["TrackingId", "ObjectId"]))
        .or_else(|| xml::value(node, "TrackingId"))
        .map(str::to_owned);
    let class_node = xml::find(node, "Class");
    let class_type = class_node.and_then(|class| xml::find(class, "Type"));
    let class = class_type
        .and_then(xml::text)
        .or_else(|| simple_item(&["ObjectClass"]))
        .map(str::to_owned);
    let confidence = class_type
        .and_then(|class| class.attribute("Likelihood"))
        .or_else(|| simple_item(&["Confidence", "Likelihood"]))
        .or_else(|| xml::value(node, "Confidence"))
        .and_then(|raw| raw.parse::<f64>().ok())
        .filter(|n| n.is_finite());
    let bounding_box = xml::find(node, "BoundingBox").and_then(|bounds| {
        let attr = |key| {
            bounds
                .attribute(key)?
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite())
        };
        let (mut x1, mut x2, mut y1, mut y2) =
            (attr("left")?, attr("right")?, attr("top")?, attr("bottom")?);
        let coordinate_space =
            if bounds.tag_name().namespace() == Some("http://www.onvif.org/ver10/schema") {
                for parent in bounds
                    .ancestors()
                    .skip(1)
                    .take_while(|n| *n != root)
                    .chain(std::iter::once(root))
                {
                    if let Some(transform) = xml::child(parent, "Transformation") {
                        let get = |node, axis, default| {
                            xml::child(transform, node)
                                .and_then(|node| node.attribute(axis)?.parse::<f64>().ok())
                                .unwrap_or(default)
                        };
                        let (sx, sy) = (get("Scale", "x", 1.0), get("Scale", "y", 1.0));
                        let (tx, ty) = (get("Translate", "x", 0.0), get("Translate", "y", 0.0));
                        (x1, x2, y1, y2) = (x1 * sx + tx, x2 * sx + tx, y1 * sy + ty, y2 * sy + ty);
                    }
                }
                (x1, x2, y1, y2) = (
                    (x1 + 1.0) / 2.0,
                    (x2 + 1.0) / 2.0,
                    (1.0 - y1) / 2.0,
                    (1.0 - y2) / 2.0,
                );
                "normalized_top_left"
            } else {
                match bounds.attribute("coordinateSpace")? {
                    "normalized_top_left" => "normalized_top_left",
                    "pixels_top_left" => "pixels_top_left",
                    _ => return None,
                }
            };
        if ![x1, x2, y1, y2].iter().all(|value| value.is_finite()) {
            return None;
        }
        let dimensions = dimensions.or_else(|| {
            Some((
                bounds.attribute("frameWidth")?.parse().ok()?,
                bounds.attribute("frameHeight")?.parse().ok()?,
            ))
        });
        if coordinate_space == "pixels_top_left" && dimensions.is_none() {
            return None;
        }
        Some(Bounds {
            left: x1.min(x2),
            top: y1.min(y2),
            right: x1.max(x2),
            bottom: y1.max(y2),
            coordinate_space,
            reference_frame: reference.into(),
            frame_width: dimensions.map(|(width, _)| width),
            frame_height: dimensions.map(|(_, height)| height),
        })
    });
    (tracking_id.is_some() || class.is_some() || confidence.is_some() || bounding_box.is_some())
        .then_some(Object {
            tracking_id,
            class,
            confidence,
            bounding_box,
        })
}
