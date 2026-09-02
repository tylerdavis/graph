use crate::commands::outcome::{report, Outcome};
use anyhow::Result;
use serde_json::json;
use std::sync::LazyLock;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct Format {
    pub name: &'static str,
    pub current: u32,
    pub oldest: u32,
}

pub fn formats() -> [Format; 4] {
    [
        Format {
            name: "config",
            current: graph_config::CONFIG_FORMAT,
            oldest: graph_config::CONFIG_FORMAT_OLDEST,
        },
        Format {
            name: "plan",
            current: graph_core::format::PLAN_FORMAT,
            oldest: graph_core::format::PLAN_FORMAT_OLDEST,
        },
        Format {
            name: "tool",
            current: graph_core::format::TOOL_FORMAT,
            oldest: graph_core::format::TOOL_FORMAT_OLDEST,
        },
        Format {
            name: "store",
            current: graph_store::STORE_FORMAT,
            oldest: graph_store::STORE_FORMAT_OLDEST,
        },
    ]
}

pub fn formats_line() -> String {
    let parts: Vec<String> = formats()
        .iter()
        .map(|format| {
            if format.oldest == format.current {
                format!("{} {}", format.name, format.current)
            } else {
                format!(
                    "{} {} (reads {}-{})",
                    format.name, format.current, format.oldest, format.current
                )
            }
        })
        .collect();
    format!("formats: {}", parts.join(", "))
}

pub static LONG_VERSION: LazyLock<String> =
    LazyLock::new(|| format!("{VERSION}\n{}", formats_line()));

pub fn run(json: bool) -> Result<()> {
    report(outcome(), json)
}

pub fn outcome() -> Outcome {
    let formats = formats();
    let body = json!({
        "version": VERSION,
        "formats": formats.iter().map(|format| (format.name.to_string(), json!({
            "current": format.current,
            "oldest": format.oldest,
        }))).collect::<serde_json::Map<String, serde_json::Value>>(),
    });
    Outcome::raw(format!("graph {VERSION}\n{}\n", formats_line()), body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_envelope_names_every_format_with_its_range() {
        let body = outcome().body;
        assert_eq!(body["version"], json!(VERSION));
        for name in ["config", "plan", "tool", "store"] {
            let entry = &body["formats"][name];
            assert!(entry["current"].as_u64().unwrap() >= entry["oldest"].as_u64().unwrap());
            assert!(entry["oldest"].as_u64().unwrap() >= 1);
        }
    }

    #[test]
    fn the_text_rendering_leads_with_the_binary_version() {
        let text = outcome().raw.unwrap();
        assert!(
            text.starts_with(&format!("graph {VERSION}\nformats: config ")),
            "{text}"
        );
        assert!(LONG_VERSION.starts_with(VERSION));
    }
}
