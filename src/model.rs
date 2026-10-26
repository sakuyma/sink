// Shared data types and small helpers between the PipeWire backend and the UI.

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub id: u32,
    pub title: String,
    pub subtitle: String,
    pub icon: String,
    /// 0..=100, cubic-volume percentage as shown by wpctl/pavucontrol.
    pub volume: f32,
    pub muted: bool,
    pub is_default: bool,
}

#[derive(Clone, Debug)]
pub struct AppInfo {
    pub id: u32,
    pub title: String,
    pub icon: String,
    pub volume: f32,
    pub muted: bool,
    /// Resolved output, or the followed default when the stream is idle.
    pub sink_id: Option<u32>,
    pub follow_default: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub sinks: Vec<DeviceInfo>,
    pub sources: Vec<DeviceInfo>,
    pub apps: Vec<AppInfo>,
    pub ready: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub enum Action {
    SetDefaultSink(u32),
    SetDefaultSource(u32),
    SetVolume { id: u32, percent: f32 },
    SetMute { id: u32, muted: bool },
    MoveStream { id: u32, sink_id: Option<u32> },
}

/// Convert a PipeWire linear channel gain to the percentage users expect.
///
/// WirePlumber stores a cubic gain: 50% is 0.125, not 0.5.
pub fn percent_from_gain(gain: f32) -> f32 {
    if gain <= 0.0 {
        0.0
    } else {
        (gain.cbrt() * 100.0).clamp(0.0, 100.0)
    }
}

pub fn gain_from_percent(percent: f32) -> f32 {
    let p = (percent.clamp(0.0, 100.0) / 100.0) as f64;
    (p * p * p) as f32
}

/// Extract `{"name":"..."}` out of WirePlumber's default-device metadata.
/// Deliberately tiny: the value is always a small JSON object.
pub fn parse_default_name(json: &str) -> Option<String> {
    let key = json.find("\"name\"")?;
    let after_key = &json[key + 6..];
    let after_colon = &after_key[after_key.find(':')? + 1..];
    let mut chars = after_colon.trim_start().strip_prefix('"')?.chars();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

pub fn json_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 12);
    out.push_str("{\"name\":\"");
    for c in name.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push_str("\"}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_roundtrip() {
        for pct in [0.0, 1.0, 25.0, 42.0, 50.0, 99.0, 100.0] {
            let roundtrip = percent_from_gain(gain_from_percent(pct));
            assert!((roundtrip - pct).abs() < 0.001, "{pct} -> {roundtrip}");
        }
        assert!((gain_from_percent(50.0) - 0.125).abs() < 1e-6);
        assert!((gain_from_percent(100.0) - 1.0).abs() < 1e-6);
        assert_eq!(percent_from_gain(0.0), 0.0);
    }

    #[test]
    fn parses_default_metadata() {
        assert_eq!(
            parse_default_name(r#"{"name":"alsa_output.usb-foo.analog-stereo"}"#).as_deref(),
            Some("alsa_output.usb-foo.analog-stereo")
        );
        assert_eq!(
            parse_default_name(r#"{ "name": "speaker\"s" }"#).as_deref(),
            Some("speaker\"s")
        );
        assert_eq!(parse_default_name("{}"), None);
        assert_eq!(parse_default_name("garbage"), None);
    }

    #[test]
    fn escapes_default_metadata() {
        assert_eq!(json_name(r#"a"b\c"#), r#"{"name":"a\"b\\c"}"#);
    }
}
