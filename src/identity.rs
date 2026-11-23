//! Camera-reported physical identity, independent of the configured route and interface.
use crate::{
    config::valid_id,
    onvif::Onvif,
    reolink::wire::{Device, Rejected, Session},
    xml,
};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HardwareId {
    pub value: String,
    pub source: &'static str,
    /// A serial observed on the same authenticated camera, for upgrading existing clients.
    pub serial_alias: Option<String>,
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
            serial_alias: None,
        })
    }

    pub fn uid(raw: &str) -> Option<Self> {
        let mut id = Self::serial(raw)?;
        if !(8..=40).contains(&id.value.len())
            || !id.value.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return None;
        }
        id.source = "uid";
        Some(id)
    }

    pub fn with_serial(mut self, serial: Option<Self>) -> Self {
        self.serial_alias = serial
            .filter(|id| id.source == "serial" && id.value != self.value)
            .map(|id| id.value);
        self
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
                serial_alias: None,
            })
    }
}

// Reolink VersionInfo also puts decimal device numbers in serialNumber. On the E1 Pro
// that number changes on reboot. Only use an unambiguous serial from this protocol.
pub fn native_serial(root: roxmltree::Node<'_, '_>) -> Option<HardwareId> {
    ["serialNumber", "serialNo"]
        .into_iter()
        .filter_map(|field| xml::value(root, field).and_then(HardwareId::serial))
        .find(|id| id.value.len() >= 4 && !id.value.bytes().all(|b| b.is_ascii_digit()))
}

async fn native(
    device: &Device,
    session: &mut Session,
    soap: Option<HardwareId>,
) -> Option<HardwareId> {
    // A recorder can supply a channel-specific identity. Never use its chassis UID on another channel.
    if let Ok(raw) = session.command(318, Some(device.channel), "").await
        && let Ok(doc) = xml::document(&raw)
        && let Some(channel) = doc
            .descendants()
            .find(|node| node.has_tag_name("ChnTypeInfo"))
    {
        let serial = native_serial(channel);
        if let Some(uid) = xml::value(channel, "uid").and_then(HardwareId::uid) {
            return Some(uid.with_serial(serial));
        }
        if serial.is_some() {
            return serial;
        }
    }
    if device.channel != 0 {
        return None;
    }
    let uid = match session.command(114, None, "").await {
        Ok(raw) => Some(HardwareId::uid(xml::value(
            xml::document(&raw).ok()?.root_element(),
            "uid",
        )?)?),
        Err(error)
            if error
                .downcast_ref::<Rejected>()
                .is_some_and(|e| matches!(e.status(), 400 | 404 | 405 | 501)) =>
        {
            None
        }
        Err(_) => return None,
    };
    let version = session.command(80, None, "").await.ok();
    let serial =
        soap.or_else(|| native_serial(xml::document(version.as_ref()?).ok()?.root_element()));
    match uid {
        Some(uid) => Some(uid.with_serial(serial)),
        None => serial,
    }
}

pub async fn read(
    onvif: Option<&Onvif>,
    device: Option<&Device>,
    channel: u8,
) -> Option<HardwareId> {
    let read_onvif = async {
        if channel == 0 {
            onvif?.hardware_id().await
        } else {
            None
        }
    };
    let Some(device) = device else {
        return read_onvif.await;
    };
    let (soap, session) = tokio::join!(read_onvif, device.connect());
    let mut session = session.ok()?;
    let id = native(device, &mut session, soap).await;
    session.logout().await;
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn physical_identifiers_exclude_placeholders_and_volatile_device_numbers() {
        for device_number in ["141484730899624", "141484730899590", "00326417514496"] {
            let xml =
                format!("<VersionInfo><serialNumber>{device_number}</serialNumber></VersionInfo>");
            assert_eq!(
                native_serial(xml::document(&xml).unwrap().root_element()),
                None
            );
        }
        let id = HardwareId::uid(" 952700test000001 ")
            .unwrap()
            .with_serial(HardwareId::serial("factory-123"));
        assert_eq!(id.value, "952700TEST000001");
        assert_eq!(id.source, "uid");
        assert_eq!(id.serial_alias.as_deref(), Some("FACTORY-123"));
        for placeholder in [
            "", "00000001", "00000000", "FFFFFFFF", "unknown", "12345678", "n/a",
        ] {
            assert!(HardwareId::serial(placeholder).is_none());
            assert!(HardwareId::uid(placeholder).is_none());
        }
        assert!(HardwareId::uid("SW6S").is_none());
        assert!(HardwareId::uid("FIXTURE-SERIAL").is_none());
        assert_eq!(
            HardwareId::mac("02:01:02:03:04:05").unwrap().value,
            "020102030405"
        );
        assert!(HardwareId::mac("ff:ff:ff:ff:ff:ff").is_none());
    }
}
