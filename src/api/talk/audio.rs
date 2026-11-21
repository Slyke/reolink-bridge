use super::super::*;
use crate::reolink::wire::Session;
use crate::xml;

#[derive(Clone)]
pub(super) struct Format {
    pub rate: u32,
    pub samples: usize,
    config: String,
}
impl Format {
    pub fn view(&self) -> Value {
        json!({"available":true,"sampleRate":self.rate,"blockSamples":self.samples})
    }
    pub async fn read(session: &mut Session, channel: u8) -> anyhow::Result<Option<Self>> {
        let raw = match session.command(10, Some(channel), "").await {
            Err(e)
                if e.downcast_ref::<crate::reolink::wire::Rejected>()
                    .is_some_and(|r| matches!(r.status(), 400 | 404 | 405 | 501)) =>
            {
                return Ok(None);
            }
            other => other?,
        };
        Self::parse(&raw, channel)
    }
    fn parse(raw: &str, channel: u8) -> anyhow::Result<Option<Self>> {
        let doc = xml::document(raw)?;
        let root = doc.root_element();
        let Some(ability) = xml::child(root, "TalkAbility") else {
            return Ok(None);
        };
        let Some(duplex) = ["FDX", "HDX"].into_iter().find(|v| {
            xml::child(ability, "duplexList").is_some_and(|p| {
                p.children()
                    .any(|n| n.has_tag_name("duplex") && n.text() == Some(v))
            })
        }) else {
            return Ok(None);
        };
        let Some(mode) = xml::child(ability, "audioStreamModeList")
            .and_then(|p| xml::child(p, "audioStreamMode"))
            .and_then(xml::text)
        else {
            return Ok(None);
        };
        let Some(list) = xml::child(ability, "audioConfigList") else {
            return Ok(None);
        };
        for c in list.children().filter(|n| n.has_tag_name("audioConfig")) {
            let field = |key| xml::child(c, key).and_then(xml::text);
            if field("audioType") != Some("adpcm")
                || field("samplePrecision") != Some("16")
                || field("soundTrack").is_some_and(|s| !["mono", "1"].contains(&s))
            {
                continue;
            }
            let Some(rate) = field("sampleRate")
                .and_then(|s| s.parse::<u32>().ok())
                .filter(|n| (8000..=48000).contains(n))
            else {
                continue;
            };
            let Some(length) = field("lengthPerEncoder")
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|n| (64..=2048).contains(n) && n % 4 == 0)
            else {
                continue;
            };
            let config = format!(
                "<body><TalkConfig version=\"1.1\"><channelId>{channel}</channelId><duplex>{duplex}</duplex><audioStreamMode>{}</audioStreamMode>{}</TalkConfig></body>",
                xml::escape(mode),
                &raw[c.range()]
            );
            return Ok(Some(Self {
                rate,
                samples: length + 1,
                config,
            }));
        }
        Ok(None)
    }
    pub async fn start(&self, s: &mut Session, c: u8) -> anyhow::Result<()> {
        s.command(201, Some(c), &self.config).await?;
        Ok(())
    }
}
const STEPS: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];
pub(super) fn encode(pcm: &[u8], samples: usize, index: &mut usize) -> Vec<u8> {
    let mut output = Vec::new();
    for block in pcm.chunks_exact(samples * 2) {
        let data_len = 4 + (samples - 1) / 2;
        output.extend(0x62773130_u32.to_le_bytes());
        let length = (data_len + 4) as u16;
        output.extend(length.to_le_bytes());
        output.extend(length.to_le_bytes());
        output.extend(0x0100_u16.to_le_bytes());
        output.extend((((samples - 1) / 4) as u16).to_le_bytes());
        let mut pred = i16::from_le_bytes([block[0], block[1]]) as i32;
        output.extend(&block[..2]);
        output.push(*index as u8);
        output.push(0);
        let mut nibble = |sample: i16| {
            let step = STEPS[*index];
            let delta = i32::from(sample) - pred;
            let mut d = delta.abs();
            let mut value = 0;
            let mut diff = step >> 3;
            for bit in [4, 2, 1] {
                let part = step * bit / 4;
                if d >= part {
                    value |= bit;
                    d -= part;
                    diff += part;
                }
            }
            pred = (pred + if delta < 0 { -diff } else { diff }).clamp(-32768, 32767);
            *index = (*index as i32 + [-1, -1, -1, -1, 2, 4, 6, 8][value as usize]).clamp(0, 88)
                as usize;
            (value | if delta < 0 { 8 } else { 0 }) as u8
        };
        for pair in block[2..].as_chunks::<4>().0 {
            let low = nibble(i16::from_le_bytes([pair[0], pair[1]]));
            let high = nibble(i16::from_le_bytes([pair[2], pair[3]]));
            output.push(low | high << 4);
        }
        output.resize(output.len() + (8 - data_len % 8) % 8, 0);
    }
    output
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn silence_has_a_valid_adpcm_frame_and_reference_header() {
        let bytes = encode(&vec![0; 2050], 1025, &mut 0);
        assert_eq!(
            &bytes[..12],
            &[0x30, 0x31, 0x77, 0x62, 8, 2, 8, 2, 0, 1, 0, 1]
        );
        assert_eq!(bytes.len(), 532);
        assert!(bytes[12..].iter().all(|b| *b == 0));
    }
}
