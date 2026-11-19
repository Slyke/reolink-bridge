//! Camera-reported physical identity, independent of the configured route and interface.
use crate::{config::valid_id, onvif::Onvif, reolink::wire::Device, xml};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HardwareId {
    pub value: String,
    pub source: &'static str,
}

impl HardwareId {
    pub fn serial(raw: &str) -> Option<Self> {
        let value = raw.trim().to_ascii_uppercase();
        (valid_id(&value)
            && !matches!(
                value.as_str(),
                "UNKNOWN"
                    | "NONE"
                    | "NULL"
                    | "DEFAULT"
                    | "NA"
                    | "12345678"
                    | "01234567"
                    | "00000001"
            )
            && !value.bytes().all(|c| c == b'0')
            && !value.bytes().all(|c| c == b'F'))
        .then_some(Self {
            value,
            source: "serial",
        })
    }

    pub fn mac(raw: &str) -> Option<Self> {
        let value = raw.trim().replace([':', '-'], "").to_ascii_uppercase();
        (value.len() == 12
            && value.bytes().all(|c| c.is_ascii_hexdigit())
            && u8::from_str_radix(&value[..2], 16).is_ok_and(|n| n & 1 == 0)
            && value != "000000000000")
            .then_some(Self {
                value,
                source: "mac",
            })
    }
}

async fn native(device: &Device) -> Option<HardwareId> {
    let mut session = device.connect().await.ok()?;
    // Channel-specific information on recorders. A recorder's serial cannot identify every channel.
    let commands = [(318, Some(device.channel)), (80, None)];
    for (command, channel) in commands {
        if channel.is_none() && device.channel != 0 {
            continue;
        }
        if let Ok(raw) = session.command(command, channel, "").await
            && let Ok(doc) = xml::document(&raw)
            && let Some(serial) = ["serialNumber", "serialNo"]
                .iter()
                .find_map(|name| xml::value(doc.root_element(), name).and_then(HardwareId::serial))
        {
            return Some(serial);
        }
    }
    let raw = session.command(76, Some(device.channel), "").await.ok()?;
    let doc = xml::document(&raw).ok()?;
    HardwareId::mac(xml::value(doc.root_element(), "mac")?)
}

pub async fn read(
    onvif: Option<&Onvif>,
    device: Option<&Device>,
    channel: u8,
) -> Option<HardwareId> {
    let (soap, native) = tokio::join!(
        async {
            if channel != 0 {
                return None;
            }
            onvif?.hardware_id().await
        },
        async { native(device?).await }
    );
    preferred(soap, native)
}

fn preferred(soap: Option<HardwareId>, native: Option<HardwareId>) -> Option<HardwareId> {
    soap.iter()
        .chain(native.iter())
        .find(|id| id.source == "serial")
        .cloned()
        .or(native)
        .or(soap)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn serial_survives_interface_changes_and_invalid_identifiers_are_excluded() {
        let serial = HardwareId::serial("  fixture-a123 ").unwrap();
        assert_eq!(serial.value, "FIXTURE-A123");
        for mac in ["02:01:02:03:04:05", "04-01-02-03-04-05"] {
            for (a, b) in [
                (HardwareId::mac(mac), Some(serial.clone())),
                (Some(serial.clone()), HardwareId::mac(mac)),
            ] {
                assert_eq!(preferred(a, b), Some(serial.clone()));
            }
        }
        assert_eq!(
            HardwareId::mac("02:01:02:03:04:05").unwrap().value,
            "020102030405"
        );
        assert!(HardwareId::mac("00:00:00:00:00:00").is_none());
        assert!(HardwareId::mac("ff:ff:ff:ff:ff:ff").is_none());
        for placeholder in [
            "", "00000001", "00000000", "FFFFFFFF", "unknown", "12345678", "n/a",
        ] {
            assert!(HardwareId::serial(placeholder).is_none());
        }
    }
}
