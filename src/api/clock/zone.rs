use crate::xml;
use roxmltree::Node;
use serde::Deserialize;

const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Transition {
    month: u8,
    week: u8,
    weekday: u8,
    hour: u8,
    minute: u8,
}

impl Transition {
    fn valid(&self) -> bool {
        (1..=12).contains(&self.month)
            && (1..=5).contains(&self.week)
            && self.weekday < 7
            && self.hour < 24
            && self.minute < 60
    }
    fn read(node: Node<'_, '_>, prefix: &str) -> Option<Self> {
        let field = |name: &str| xml::value(node, &format!("{prefix}{name}"));
        let weekday = DAYS.iter().position(|day| Some(*day) == field("Weekday"))? as u8;
        if field("Second").is_some_and(|value| value.parse::<u8>() != Ok(0)) {
            return None;
        }
        let result = Self {
            month: field("Month")?.parse().ok()?,
            week: field("WeekIndex")?.parse().ok()?,
            weekday,
            hour: field("Hour")?.parse().ok()?,
            minute: field("Minute")?.parse().ok()?,
        };
        result.valid().then_some(result)
    }
    fn xml(&self, prefix: &str) -> String {
        let Self {
            month,
            week,
            weekday,
            hour,
            minute,
        } = self;
        let day = DAYS[usize::from(*weekday)];
        format!(
            "<{prefix}Month>{month}</{prefix}Month><{prefix}WeekIndex>{week}</{prefix}WeekIndex><{prefix}Weekday>{day}</{prefix}Weekday><{prefix}Hour>{hour}</{prefix}Hour><{prefix}Minute>{minute}</{prefix}Minute><{prefix}Second>0</{prefix}Second>"
        )
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Schedule {
    hours: u8,
    start: Transition,
    end: Transition,
}

impl Schedule {
    pub(super) fn read(node: Node<'_, '_>) -> Option<Self> {
        Some(Self {
            hours: xml::value(node, "offset")?.parse().ok()?,
            start: Transition::read(node, "start")?,
            end: Transition::read(node, "end")?,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Zone {
    pub(super) utc_offset_minutes: i32,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(super) daylight_saving: Option<Schedule>,
}

impl Zone {
    pub(super) fn valid(&self) -> bool {
        (-720..=840).contains(&self.utc_offset_minutes)
            && self.daylight_saving.as_ref().is_none_or(|s| {
                matches!(s.hours, 1 | 2) && s.start.valid() && s.end.valid() && s.start != s.end
            })
    }
    pub(super) fn dst_xml(&self) -> String {
        let fields = self.daylight_saving.as_ref().map_or_else(
            || "<enable>0</enable>".to_owned(),
            |s| {
                format!(
                    "<enable>1</enable><offset>{}</offset>{}{}",
                    s.hours,
                    s.start.xml("start"),
                    s.end.xml("end")
                )
            },
        );
        format!(r#"<body><Dst version="1.1">{fields}</Dst></body>"#)
    }
}
