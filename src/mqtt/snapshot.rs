use std::{io::Cursor, sync::Arc};

use anyhow::{Context, Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use image::{
    DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, Limits, RgbImage,
    codecs::png::{CompressionType, FilterType, PngEncoder},
    imageops,
};
use serde_json::json;
use tokio::sync::Semaphore;

use crate::reolink::wire::Device;

const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;

pub type Failure = (&'static str, anyhow::Error);

pub async fn capture(
    device: &Device,
    slot: &Arc<Semaphore>,
    budget: usize,
    id: String,
    correlation: String,
) -> std::result::Result<Bytes, Failure> {
    if budget < 512 {
        return Err(too_small());
    }
    let permit = slot
        .clone()
        .try_acquire_owned()
        .map_err(|error| ("SNAPSHOT_CAPTURE_BUSY", error.into()))?;
    let mut session = device
        .connect()
        .await
        .map_err(|error| ("SNAPSHOT_CAPTURE_FAILED", error))?;
    let jpeg = session
        .snapshot_jpeg(device.channel)
        .await
        .map_err(|error| ("SNAPSHOT_CAPTURE_FAILED", error))?;
    let captured_at = Utc::now();
    session.logout().await;

    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let image = decode_jpeg(jpeg).map_err(|error| ("SNAPSHOT_IMAGE_INVALID", error))?;
        fit_image(image, budget, id, correlation, captured_at)
    })
    .await
    .map_err(|error| ("SNAPSHOT_ENCODE_FAILED", error.into()))?
}

fn decode_jpeg(jpeg: Vec<u8>) -> Result<RgbImage> {
    let mut reader = ImageReader::with_format(Cursor::new(jpeg), ImageFormat::Jpeg);
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let decoder = reader
        .into_decoder()
        .context("could not parse snapshot JPEG")?;
    let (width, height) = decoder.dimensions();
    ensure!(
        width > 0 && height > 0 && u64::from(width) * u64::from(height) <= 16_000_000,
        "snapshot dimensions exceed the JPEG decode budget"
    );
    Ok(DynamicImage::from_decoder(decoder)
        .context("could not decode snapshot JPEG")?
        .into_rgb8())
}

fn fit_image(
    original: RgbImage,
    budget: usize,
    id: String,
    correlation: String,
    captured_at: DateTime<Utc>,
) -> std::result::Result<Bytes, Failure> {
    let (original_width, original_height) = original.dimensions();
    let mut reply = json!({
        "id": id, "ok": true, "correlation_id": correlation,
        "result": {
            "action": "snapshot", "content_type": "image/png", "encoding": "base64",
            "width": original_width, "height": original_height, "resized": false,
            "original_width": original_width, "original_height": original_height,
            "captured_at": captured_at, "data_base64": "",
        },
    });
    let height_at_max_width = (original_height * MAX_WIDTH / original_width).max(1);
    let (mut width, mut height) = if original_width <= MAX_WIDTH && original_height <= MAX_HEIGHT {
        (original_width, original_height)
    } else if height_at_max_width <= MAX_HEIGHT {
        (MAX_WIDTH, height_at_max_width)
    } else {
        (
            (original_width * MAX_HEIGHT / original_height).max(1),
            MAX_HEIGHT,
        )
    };
    let mut png = Vec::new();
    loop {
        let resized = (width != original_width || height != original_height)
            .then(|| imageops::resize(&original, width, height, imageops::FilterType::Triangle));
        let frame = resized.as_ref().unwrap_or(&original);
        png.clear();
        PngEncoder::new_with_quality(&mut png, CompressionType::Default, FilterType::Adaptive)
            .write_image(
                frame.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgb8,
            )
            .map_err(|error| ("SNAPSHOT_ENCODE_FAILED", error.into()))?;
        reply["result"]["width"] = width.into();
        reply["result"]["height"] = height.into();
        reply["result"]["resized"] = resized.is_some().into();

        let envelope_bytes = reply.to_string().len();
        let encoded_bytes = base64::encoded_len(png.len(), true);
        if encoded_bytes.is_some_and(|bytes| bytes + envelope_bytes <= budget) {
            reply["result"]["data_base64"] = STANDARD.encode(png).into();
            return serde_json::to_vec(&reply)
                .map(Bytes::from)
                .map_err(|error| ("SNAPSHOT_ENCODE_FAILED", error.into()));
        }
        let largest_side = width.max(height);
        if largest_side == 1 {
            return Err(too_small());
        }
        // PNG size tends to follow the number of pixels. Estimate the next scale
        // from the measured reply, then encode again and check the exact size.
        let encoded_bytes = encoded_bytes.unwrap_or(usize::MAX);
        let available = budget.saturating_sub(envelope_bytes);
        let scale = ((available as f64 / encoded_bytes as f64).sqrt() * 0.95).min(0.90);
        let next_side = ((f64::from(largest_side) * scale) as u32).clamp(1, largest_side - 1);
        if original_width >= original_height {
            width = next_side;
            height = (original_height * width / original_width).max(1);
        } else {
            height = next_side;
            width = (original_width * height / original_height).max(1);
        }
    }
}

fn too_small() -> Failure {
    (
        "SNAPSHOT_PAYLOAD_TOO_SMALL",
        anyhow!("the MQTT payload budget is too small for a Base64 PNG"),
    )
}
