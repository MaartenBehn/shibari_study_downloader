use chromiumoxide::{Page};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::io::{Write};
use std::time::Duration;
use serde::{Serialize, Deserialize};

use crate::common::{download_file_with_curl, eval_with_retry, goto_settled, sanitize_filename};

#[derive(Debug, Serialize, Deserialize, Default)]
struct HarnessGuideRecord {
    id: String,
    loc: String,
    title: String,
    level: Option<String>,
    subtitle: Option<String>,
    tags_line: Option<String>,
    about: Option<String>,
    guide_url: Option<String>,
    image_urls: Vec<String>,
    downloaded_files: Vec<String>,
}

#[derive(Deserialize, Default, Debug)]
struct ExtractedGuidePage {
    title: String,
    level: Option<String>,
    subtitle: Option<String>,
    tags_line: Option<String>,
    about: Option<String>,
    guide_url: Option<String>,
    #[serde(default)]
    image_urls: Vec<String>,
}

const HARNESS_GUIDE_JS: &str = r#"
(() => {
    const cleanText = (el) => el ? el.textContent.replace(/\u00a0/g, ' ').trim() : null;

    const h1 = document.querySelector('header h1') || document.querySelector('h1');
    const title = cleanText(h1) || '';

    // Level text sits next to the level indicator dots inside header <p>
    const headerParas = Array.from(document.querySelectorAll('header p'));
    let level = null;
    for (const p of headerParas) {
        const spans = Array.from(p.querySelectorAll('span'));
        const lvlSpan = spans.find(s => /Level\s*\d+/i.test(s.textContent || ''));
        if (lvlSpan) {
            level = cleanText(lvlSpan);
            break;
        }
    }

    // Secondary lines in header (e.g. "Follow step-by-step illustrated instructions" & "Upper body · Armbinders")
    const extraParas = headerParas
        .filter(p => !/Level\s*\d+/i.test(p.textContent || ''))
        .map(p => cleanText(p))
        .filter(Boolean);

    const subtitle = extraParas[0] || null;
    const tags_line = extraParas[1] || null;

    const aboutHeading = Array.from(document.querySelectorAll('[data-harness-details="true"] h2, h2'))
        .find(h => (h.textContent || '').trim() === 'About this harness');
    const about = aboutHeading
        ? cleanText(aboutHeading.nextElementSibling?.querySelector('p') || aboutHeading.nextElementSibling)
        : null;

    // Flip-book iframe or "Open guide in a new tab" link
    const iframe = document.querySelector('section[aria-label="Illustrated harness guide"] iframe, iframe[src*="heyzine.com"]');
    const guideLink = document.querySelector('section[aria-label="Illustrated harness guide"] a[href*="heyzine.com"], a[href*="heyzine.com"]');
    const guide_url = iframe?.getAttribute('src') || guideLink?.getAttribute('href') || null;

    // Collect any direct <img> sources on the page (excluding tiny icon SVGs)
    const image_urls = Array.from(document.querySelectorAll('img'))
        .map(img => img.currentSrc || img.getAttribute('src') || '')
        .map(src => src.trim())
        .filter(Boolean);

    return {
        title,
        level,
        subtitle,
        tags_line,
        about,
        guide_url,
        image_urls: Array.from(new Set(image_urls)),
    };
})()
"#;

pub async fn scrape_harness_guides(page: &Page) -> Result<(), Box<dyn std::error::Error>> {
    let out_path = "harness_guides.jsonl";
    let base_images_dir = Path::new("./harness_guides");
    fs::create_dir_all(base_images_dir)?;

    let mut done: HashSet<String> = HashSet::new();
    if let Ok(f) = File::open(out_path) {
        for line in BufReader::new(f).lines().flatten() {
            if let Ok(rec) = serde_json::from_str::<HarnessGuideRecord>(&line) {
                done.insert(rec.id);
            }
        }
        println!("Resuming: {} harness guides already done.", done.len());
    }

    let mut out = OpenOptions::new().create(true).append(true).open(out_path)?;
    let reader = BufReader::new(File::open("library_items.jsonl")?);

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let item: serde_json::Value = serde_json::from_str(&line)?;
        let typ = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // Adjust this check if the API uses a slightly different type name (e.g. "harness_guide" or "guide")
        if !typ.contains("harness") && !typ.contains("guide") {
            continue;
        }

        let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if !id.is_empty() && done.contains(&id) {
            continue;
        }

        let loc = item
            .get("href")
            .and_then(|v| v.as_str())
            .ok_or("harness guide item has no href")?
            .to_string();

        let fallback_title = item
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("untitled_guide")
            .to_string();

        println!("Scraping harness guide: {}", fallback_title);
        goto_settled(page, &loc).await?;

        let extracted: ExtractedGuidePage = eval_with_retry(page, HARNESS_GUIDE_JS).await?;
        let final_title = if extracted.title.trim().is_empty() {
            fallback_title
        } else {
            extracted.title
        };

        let guide_dir = base_images_dir.join(sanitize_filename(&final_title));
        fs::create_dir_all(&guide_dir)?;

        let mut downloaded_files = Vec::new();
        for (idx, img_url) in extracted.image_urls.iter().enumerate() {
            let resolved_url = if img_url.starts_with("http") {
                img_url.clone()
            } else {
                format!("https://shibaristudy.com{}", img_url)
            };

            // Guess extension from URL path, defaulting to .jpg
            let ext = resolved_url
                .split('?')
                .next()
                .and_then(|p| Path::new(p).extension())
                .and_then(|e| e.to_str())
                .filter(|e| matches!(e.to_lowercase().as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif"))
                .unwrap_or("jpg");

            let file_path = guide_dir.join(format!("{:0>2}.{}", idx + 1, ext));
            if file_path.exists() {
                downloaded_files.push(file_path.to_string_lossy().to_string());
                continue;
            }

            match download_file_with_curl(&resolved_url, &file_path).await {
                Ok(()) => {
                    println!("  ✔ Downloaded image: {}", file_path.display());
                    downloaded_files.push(file_path.to_string_lossy().to_string());
                }
                Err(e) => {
                    eprintln!("  ✖ Failed to download {}: {}", resolved_url, e);
                }
            }
        }

        let record = HarnessGuideRecord {
            id: id.clone(),
            loc,
            title: final_title,
            level: extracted.level,
            subtitle: extracted.subtitle,
            tags_line: extracted.tags_line,
            about: extracted.about,
            guide_url: extracted.guide_url,
            image_urls: extracted.image_urls,
            downloaded_files,
        };

        writeln!(out, "{}", serde_json::to_string(&record)?)?;
        out.flush()?;
        if !id.is_empty() {
            done.insert(id);
        }

        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    println!("Harness guides done.");
    Ok(())
}
