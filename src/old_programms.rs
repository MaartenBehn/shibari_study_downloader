use std::fs::File;
use std::io::{BufRead, BufReader};
use serde::{Serialize, Deserialize};
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize)]
pub struct VideoItem {
    pub title: String,
    pub href: String,
    pub cid: Option<String>,
    pub duration: Option<String>,
    pub description: Option<String>,
    pub thumbnail: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProgramRecord {
    pub loc: String,
    pub title: String,
    pub about: Option<String>,
    pub categories: Vec<String>,
    pub videos: Vec<VideoItem>,
}

pub fn norm_title(s: &str) -> String {
    let lower = s.to_lowercase();
    let without_free = lower
        .replace("&amp;", "&")
        .replace("&nbsp;", " ")
        .replace("(free)", "");

    // Strip trailing "_free", "-free", or " free" tokens if present
    let trimmed_free = without_free
        .trim_end()
        .strip_suffix("_free")
        .or_else(|| without_free.trim_end().strip_suffix("-free"))
        .or_else(|| without_free.trim_end().strip_suffix(" free"))
        .unwrap_or(&without_free);

    trimmed_free
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

pub fn load_old_categories() -> HashMap<String, Vec<String>> {
    let mut map = HashMap::new();
    if let Ok(f) = File::open("programs_records.jsonl") {
        for line in BufReader::new(f).lines().flatten() {
            if let Ok(rec) = serde_json::from_str::<ProgramRecord>(&line) {
                map.insert(norm_title(&rec.title), rec.categories);
            }
        }
    }
    println!("Loaded categories for {} old programs.", map.len());
    map
}

