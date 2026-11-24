use chromiumoxide::Page;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::process::Command;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::io::{Write};
use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader as TokioBufReader};

use crate::common::{eval_with_retry, goto_settled, wait_for_page_settled};
use crate::old_programms::{load_old_categories, norm_title};
use crate::common::sanitize_filename;

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct CourseVideo {
    pub title: String,
    pub href: String,
    pub duration: Option<String>,
    pub thumbnail: Option<String>,
    #[serde(default)]
    pub in_this_video: Option<String>,
    #[serde(default)]
    pub hls_url: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CourseRecord {
    pub id: String,
    pub loc: String,
    pub title: String,
    pub description: Option<String>,
    pub level: Option<String>,
    pub duration: Option<String>,
    pub free: bool,
    pub authors: Vec<String>,
    pub image: Option<String>,
    pub categories: Vec<String>,
    pub about: Option<String>,
    pub safety: Option<String>,
    pub equipment: Vec<String>,
    pub prerequisites: Vec<String>,
    pub videos: Vec<CourseVideo>,
}

#[derive(Deserialize, Default, Debug)]
struct EpisodeList {
    #[serde(default)]
    episodes: Vec<CourseVideo>,
}

#[derive(Deserialize, Default, Debug)]
struct EpisodePage {
    about: Option<String>,
    in_this_video: Option<String>,
    safety: Option<String>,
    #[serde(default)]
    equipment: Vec<String>,
    #[serde(default)]
    prerequisites: Vec<String>,
    hls_url: Option<String>,
}

const EPISODE_LIST_JS: &str = r#"
(() => {
    const episodes = Array.from(document.querySelectorAll('.utv-video-list__list a.utv-content-line')).map(a => ({
        title: a.querySelector('.utv-content-line__title')?.textContent.trim() || '',
        href: a.getAttribute('href') || '',
        duration: a.querySelector('.utv-content-line__duration')?.textContent.trim() || null,
        thumbnail: a.querySelector('img')?.getAttribute('src') || null,
    }));
    return { episodes };
})()
"#;


const EPISODE_PAGE_JS: &str = r#"
(() => {
    // Text of an element without the glossary "show definition" buttons' icons / sr-only text
    const clean = (el) => {
        if (!el) return null;
        const c = el.cloneNode(true);
        c.querySelectorAll('.sr-only, svg').forEach(n => n.remove());
        return c.textContent.replace(/\u00a0/g, ' ').trim();
    };
    const heading = (tag, text) =>
        Array.from(document.querySelectorAll(tag)).find(h => h.textContent.trim() === text);
    const paraAfter = (h) => h ? clean(h.nextElementSibling?.querySelector('p')) : null;

    const equipH = heading('h3', 'Equipment');
    const equipment = equipH && equipH.nextElementSibling
        ? Array.from(equipH.nextElementSibling.querySelectorAll('li')).map(li => clean(li))
        : [];

    const prerequisites = Array.from(
        document.querySelectorAll('[data-required-foundations] li span span')
    ).map(s => clean(s)).filter(Boolean);

    return {
        about: paraAfter(heading('h2', 'About this course')),
        in_this_video: paraAfter(heading('h2', 'In this video')),
        safety: paraAfter(heading('h3', 'Safety')),
        equipment,
        prerequisites,
        hls_url: document.querySelector('video-player source[type="application/x-mpegURL"]')?.src || null,
    };
})()
"#;

async fn get_course_record(
    page: &Page,
    item: &serde_json::Value,
    categories: &HashMap<String, Vec<String>>,
) -> Result<CourseRecord, Box<dyn std::error::Error>> {
    let s = |k: &str| item.get(k).and_then(|v| v.as_str()).map(|x| x.to_string());
    let id = s("id").unwrap_or_default();
    let loc = s("href").ok_or("item has no href")?;
    let title = s("title").unwrap_or_default();
    
    // 1. Episode list from the course page
    goto_settled(page, &loc).await?;
    let list: EpisodeList = eval_with_retry(&page, EPISODE_LIST_JS).await?;
    let mut videos = list.episodes;

    // 2. Visit every episode: per-video description + stream URL.
    //    Course-level fields (about/safety/equipment) are taken from the first page that has them.
    let mut about = None;
    let mut safety = None;
    let mut equipment = Vec::new();
    let mut prerequisites = Vec::new();

    for v in videos.iter_mut() {
        goto_settled(page, &v.href).await?;
        let ep: EpisodePage = eval_with_retry(&page, EPISODE_PAGE_JS).await?;

        v.in_this_video = ep.in_this_video;
        v.hls_url = ep.hls_url;
        if about.is_none() { about = ep.about; }
        if safety.is_none() { safety = ep.safety; }
        if equipment.is_empty() { equipment = ep.equipment; }
        if prerequisites.is_empty() { prerequisites = ep.prerequisites; }
    }

    let authors = item.get("authors").and_then(|a| a.as_array())
        .map(|arr| arr.iter().filter_map(|a| a.get("name").and_then(|n| n.as_str()).map(String::from)).collect())
        .unwrap_or_default();

    Ok(CourseRecord {
        categories: categories.get(&norm_title(&title)).cloned().unwrap_or_default(),
        id, loc, title,
        description: s("description"),
        level: s("level"),
        duration: s("duration"),
        free: item.get("free").and_then(|v| v.as_bool()).unwrap_or(false),
        authors,
        image: s("image"),
        about, safety, equipment, prerequisites, videos,
    })
}

pub async fn fetch_courses(page: &Page) -> Result<(), Box<dyn std::error::Error>> {
    let categories = load_old_categories();
    let out_path = "courses_records.jsonl";

    let mut done: HashSet<String> = HashSet::new();
    if let Ok(f) = File::open(out_path) {
        for line in BufReader::new(f).lines().flatten() {
            if let Ok(r) = serde_json::from_str::<CourseRecord>(&line) { done.insert(r.id); }
        }
        println!("Resuming: {} courses already done.", done.len());
    }

    let mut out = OpenOptions::new().create(true).append(true).open(out_path)?;
    let reader = BufReader::new(File::open("library_items.jsonl")?);

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() { continue; }
        let item: serde_json::Value = serde_json::from_str(&line)?;
        
        let typ = item.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if typ != "course" {
            continue;
        }

        let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if done.contains(&id) { continue; }

        println!("Scraping course: {}", item.get("title").and_then(|v| v.as_str()).unwrap_or("?"));
        let res = get_course_record(page, &item, &categories).await?;
        dbg!(&res);

        if res.videos.is_empty() { 
            eprintln!("  ! no episodes found for {}", res.loc); 
        }
        writeln!(out, "{}", serde_json::to_string(&res)?)?;
        out.flush()?;
    }
    println!("Courses done.");
    Ok(())
}


async fn get_video_stream_url(
    page: &Page, 
    video_href: &str
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let full_url = if video_href.starts_with("http") {
        video_href.to_string()
    } else {
        format!("https://shibaristudy.com{}", video_href)
    };

    goto_settled(&page, &full_url).await?;
    wait_for_page_settled(page).await?;

    // Synchronous IIFE: Returns the string or null immediately without being a Promise
    let js_script = r#"
        (() => {
            // Check direct <source> tag inside video player
            const sourceEl = document.querySelector('video-player source[type="application/x-mpegURL"]');
            if (sourceEl && sourceEl.src) {
                return sourceEl.src;
            }

            // Check Uscreen video player component attributes as fallback
            const playerEl = document.querySelector('video-player');
            if (playerEl) {
                const streamEndpoint = playerEl.getAttribute('data-program-video-stream-url-value');
                if (streamEndpoint) {
                    return streamEndpoint;
                }
            }

            return null;
        })()
    "#;

    let stream_url: Option<String> = page.evaluate(js_script).await?.into_value()?;

    if stream_url.is_none() {
        eprintln!("Timed out finding stream URL in DOM for {}", video_href);
    }

    Ok(stream_url)
}


fn find_existing_video(
    base_videos_dir: &Path,
    course_title: &str,
    video_title: &str,
    expected_path: &Path,
) -> Option<String> {
    if expected_path.exists() {
        return Some(expected_path.to_string_lossy().to_string());
    }

    let target_norm_title = norm_title(video_title);
    if target_norm_title.is_empty() {
        return None;
    }

    let target_norm_course = norm_title(course_title);
    let exact_course_dir = base_videos_dir.join(sanitize_filename(course_title));

    let mut candidate_dirs = Vec::new();
    if exact_course_dir.is_dir() {
        candidate_dirs.push(exact_course_dir);
    }

    if let Ok(entries) = std::fs::read_dir(base_videos_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if let Some(dir_name) = p.file_name().and_then(|n| n.to_str()) {
                    if norm_title(dir_name) == target_norm_course && !candidate_dirs.contains(&p) {
                        candidate_dirs.push(p);
                    }
                }
            }
        }
    }

    for dir in candidate_dirs {
        if let Ok(files) = std::fs::read_dir(dir) {
            for file in files.flatten() {
                let fp = file.path();
                if fp.extension().and_then(|e| e.to_str()) != Some("mp4") {
                    continue;
                }
                if let Some(stem) = fp.file_stem().and_then(|s| s.to_str()) {
                    // Strip leading "<digits>_" prefix (e.g. "01_Some_Title" -> "Some_Title")
                    let without_idx = match stem.split_once('_') {
                        Some((prefix, rest)) if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) => rest,
                        _ => stem,
                    };
                    if norm_title(without_idx) == target_norm_title {
                        return Some(fp.to_string_lossy().to_string());
                    }
                }
            }
        }
    }

    None
}

async fn download_single_video(
    stream_url: &str,
    output_path: &str,
    mp: MultiProgress,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    
    // 1. Setup a temporary staging folder for active downloads
    let temp_dir = "./videos/temp";
    std::fs::create_dir_all(temp_dir)?;

    let path_obj = Path::new(output_path);
    let file_name = path_obj
        .file_name()
        .ok_or("Invalid output filename")?;
    let temp_output_path = format!("{}/{}", temp_dir, file_name.to_string_lossy());

    // Create a dedicated progress bar row for this video in the UI
    let pb = mp.add(ProgressBar::new_spinner());
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} [{elapsed_precise}] {msg}")
            .unwrap(),
    );
    
    let display_title = output_path.to_string();
    pb.set_message(format!("Starting: {}", display_title));

    // 2. Run yt-dlp writing into temp folder and extracting subtitles simultaneously
    let mut child = Command::new("yt-dlp")
        .arg(stream_url)
        .arg("--cookies")
        .arg("cookies.text")
        .arg("-f")
        .arg("bestvideo+bestaudio/best")
        .arg("-o")
        .arg(&temp_output_path)
        .arg("--merge-output-format")
        .arg("mp4")
        .arg("--write-subs")
        .arg("--write-auto-subs")
        .arg("--sub-langs")
        .arg("all")
        .arg("--convert-subs")
        .arg("srt")
        .arg("--newline")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take().unwrap();
    let mut reader = TokioBufReader::new(stdout).lines();

    while let Ok(Some(line)) = reader.next_line().await {
        if line.contains("[download]") || line.contains("[Merger]") {
            let clean_line = line.trim();
            if !clean_line.is_empty() {
                pb.set_message(format!("{}: {}", display_title, clean_line));
            }
        }
    }

    let status = child.wait().await?;

    if status.success() {
        let final_parent = path_obj.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(final_parent)?;

        // Move the completed video file from temp to its final destination folder
        std::fs::rename(&temp_output_path, output_path)?;

        // 3. Move and rename any downloaded subtitle files to match the final video name
        let temp_path_obj = Path::new(&temp_output_path);
        let final_stem = path_obj.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        
        if let Some(temp_stem) = temp_path_obj.file_stem().and_then(|s| s.to_str()) {
            let temp_dir_path = Path::new(temp_dir);
            if let Ok(entries) = std::fs::read_dir(temp_dir_path) {
                for entry in entries.flatten() {
                    if let Some(filename) = entry.file_name().to_str() {
                        if filename.starts_with(&format!("{}.", temp_stem)) && filename.ends_with(".srt") {
                            let src_sub = entry.path();
                            // Extracts language suffix, e.g., ".en.srt"
                            let suffix = &filename[temp_stem.len()..];
                            let final_sub_name = format!("{}{}", final_stem, suffix);
                            let final_sub_path = final_parent.join(final_sub_name);

                            let _ = std::fs::rename(&src_sub, &final_sub_path);
                        }
                    }
                }
            }
        }

        pb.finish_with_message(format!("✔ Completed: {}", output_path));
    } else {
        // Clean up partial temp video and subtitle files if download failed
        let _ = std::fs::remove_file(&temp_output_path);
        let temp_path_obj = Path::new(&temp_output_path);
        if let Some(temp_stem) = temp_path_obj.file_stem().and_then(|s| s.to_str()) {
            let temp_dir_path = Path::new(temp_dir);
            if let Ok(entries) = std::fs::read_dir(temp_dir_path) {
                for entry in entries.flatten() {
                    if let Some(filename) = entry.file_name().to_str() {
                        if filename.starts_with(&format!("{}.", temp_stem)) {
                            let _ = std::fs::remove_file(entry.path());
                        }
                    }
                }
            }
        }
        pb.finish_with_message(format!("✖ Failed: {}", output_path));
    }

    Ok(())
}


pub async fn scrape_videos(
    page: &Page,
    concurrency_limit: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let base_domain = "https://shibaristudy.com";
    let base_videos_dir = Path::new("./videos");

    let records_path = if Path::new("courses_records.jsonl").exists() {
        "courses_records.jsonl"
    } else if Path::new("scrape_videos.jsonl").exists() {
        "scrape_videos.jsonl"
    } else {
        "courses_records.jsonl"
    };

    let file = File::open(records_path)?;
    let reader = BufReader::new(file);

    let multi_progress = MultiProgress::new();
    let (tx, rx) = mpsc::channel::<(String, String)>(concurrency_limit.max(1));
    let rx = Arc::new(Mutex::new(rx));

    let mut worker_set = tokio::task::JoinSet::new();
    for _ in 0..concurrency_limit.max(1) {
        let receiver = Arc::clone(&rx);
        let mp = multi_progress.clone();

        worker_set.spawn(async move {
            loop {
                let item = {
                    let mut lock = receiver.lock().await;
                    lock.recv().await
                };

                match item {
                    Some((stream_url, output_filename)) => {
                        if let Err(e) = download_single_video(&stream_url, &output_filename, mp.clone()).await {
                            eprintln!("Error downloading {}: {}", output_filename, e);
                        }
                    }
                    None => break,
                }
            }
        });
    }

    for line in reader.lines() {
        let line_str = line?;
        let line_str = line_str.trim();
        if line_str.is_empty() {
            continue;
        }

        let record: CourseRecord = match serde_json::from_str(line_str) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Skipping malformed line in {}: {}", records_path, e);
                continue;
            }
        };

        let course_folder_name = sanitize_filename(&record.title);
        let course_dir = base_videos_dir.join(&course_folder_name);
        std::fs::create_dir_all(&course_dir)?;

        for (i, video) in record.videos.into_iter().enumerate() {
            let safe_title = sanitize_filename(&video.title);
            let output_filename = format!("{}/{:0>2}_{}.mp4", course_dir.display(), i + 1, safe_title);
            let output_path = Path::new(&output_filename);

            if let Some(existing) = find_existing_video(base_videos_dir, &record.title, &video.title, output_path) {
                println!("✔ Skipping (already exists): {} -> {}", video.title, existing);
                continue;
            }

            if video.href.trim().is_empty() {
                eprintln!("Skipping '{}' (empty href).", video.title);
                continue;
            }

            let full_video_url = if video.href.starts_with("http") {
                video.href.clone()
            } else {
                format!("{}{}", base_domain, video.href)
            };

            // Reuse stored hls_url if available, otherwise fetch from the episode page
            let stream_url = match video.hls_url.filter(|u| !u.trim().is_empty()) {
                Some(url) => Some(url),
                None => match get_video_stream_url(page, &full_video_url).await {
                    Ok(url) => url,
                    Err(e) => {
                        eprintln!("Error fetching stream URL for {}: {}", video.title, e);
                        None
                    }
                },
            };

            if let Some(url) = stream_url {
                if tx.send((url, output_filename)).await.is_err() {
                    eprintln!("Download worker channel closed unexpectedly.");
                    break;
                }
            } else {
                eprintln!("Skipping download due to missing stream URL: {}", video.title);
            }
        }
    }

    drop(tx);

    while let Some(res) = worker_set.join_next().await {
        if let Err(e) = res {
            eprintln!("Worker join error: {}", e);
        }
    }

    let _ = std::fs::remove_dir("./videos/temp");
    println!("\nAll courses processed!");
    Ok(())
}



#[derive(Clone)]
struct ExistingVideoCandidate {
    path: PathBuf,
    course_dir_name: String,
    cleaned_stem: String,
    norm_key: String,
}

fn strip_leading_index(stem: &str) -> &str {
    if let Some((prefix, rest)) = stem.split_once('_') {
        if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) {
            return rest;
        }
    }
    if let Some((prefix, rest)) = stem.split_once('-') {
        let p = prefix.trim();
        if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) {
            return rest.trim_start();
        }
    }
    stem
}

fn collect_existing_videos(base_videos_dir: &Path) -> Vec<ExistingVideoCandidate> {
    let mut results = Vec::new();
    let Ok(course_entries) = fs::read_dir(base_videos_dir) else {
        return results;
    };

    for course_entry in course_entries.flatten() {
        let course_path = course_entry.path();
        if !course_path.is_dir() {
            continue;
        }
        let course_dir_name = course_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        if course_dir_name == "temp" {
            continue;
        }

        let Ok(file_entries) = fs::read_dir(&course_path) else {
            continue;
        };

        for file_entry in file_entries.flatten() {
            let fp = file_entry.path();
            if fp.extension().and_then(|e| e.to_str()) != Some("mp4") {
                continue;
            }
            if let Some(stem) = fp.file_stem().and_then(|s| s.to_str()) {
                let cleaned_stem = strip_leading_index(stem).to_string();
                let norm_key = norm_title(&cleaned_stem);
                results.push(ExistingVideoCandidate {
                    path: fp,
                    course_dir_name: course_dir_name.clone(),
                    cleaned_stem,
                    norm_key,
                });
            }
        }
    }

    results
}

/// Bigram Dice similarity in `[0.0, 1.0]` over normalized strings.
fn bigram_similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.len() < 2 || b.len() < 2 {
        return if a == b { 1.0 } else { 0.0 };
    }

    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();

    let mut a_bigrams = Vec::with_capacity(a_chars.len() - 1);
    for w in a_chars.windows(2) {
        a_bigrams.push((w[0], w[1]));
    }

    let mut b_bigrams: Vec<(char, char)> = Vec::with_capacity(b_chars.len() - 1);
    for w in b_chars.windows(2) {
        b_bigrams.push((w[0], w[1]));
    }

    let mut matches = 0usize;
    for bg in a_bigrams.iter() {
        if let Some(pos) = b_bigrams.iter().position(|x| x == bg) {
            matches += 1;
            b_bigrams.swap_remove(pos);
        }
    }

    (2.0 * matches as f64) / ((a_bigrams.len() + b_bigrams.len()) as f64)
}

fn fuzzy_score(
    target_course_norm: &str,
    target_video_norm: &str,
    cand: &ExistingVideoCandidate,
) -> f64 {
    let title_sim = bigram_similarity(target_video_norm, &cand.norm_key);
    let containment_bonus = if !target_video_norm.is_empty()
        && !cand.norm_key.is_empty()
        && (cand.norm_key.contains(target_video_norm) || target_video_norm.contains(&cand.norm_key))
    {
        0.10
    } else {
        0.0
    };

    let same_course_bonus = if !target_course_norm.is_empty()
        && norm_title(&cand.course_dir_name) == target_course_norm
    {
        0.15
    } else {
        0.0
    };

    (title_sim + containment_bonus + same_course_bonus).min(1.0)
}

fn find_exact_existing<'a>(
    candidates: &'a [ExistingVideoCandidate],
    course_title: &str,
    video_title: &str,
    expected_path: &Path,
) -> Option<&'a ExistingVideoCandidate> {
    if let Some(c) = candidates.iter().find(|c| c.path == expected_path) {
        return Some(c);
    }

    let target_video_norm = norm_title(video_title);
    if target_video_norm.is_empty() {
        return None;
    }

    let target_course_norm = norm_title(course_title);
    candidates.iter().find(|c| {
        c.norm_key == target_video_norm && norm_title(&c.course_dir_name) == target_course_norm
    })
}

pub fn print_pending_videos(top_k: usize) -> Result<(), Box<dyn std::error::Error>> {
    let base_videos_dir = Path::new("./videos");
    let all_existing = collect_existing_videos(base_videos_dir);

    // Track all existing folder names under ./videos (excluding "temp")
    let mut all_existing_folders: HashSet<String> = HashSet::new();
    if let Ok(entries) = fs::read_dir(base_videos_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    if name != "temp" {
                        all_existing_folders.insert(name.to_string());
                    }
                }
            }
        }
    }

    let mut matched_folders: HashSet<String> = HashSet::new();

    let records_path = if Path::new("courses_records.jsonl").exists() {
        "courses_records.jsonl"
    } else if Path::new("scrape_videos.jsonl").exists() {
        "scrape_videos.jsonl"
    } else {
        "courses_records.jsonl"
    };

    // First pass: collect folders that already matched videos so fuzzy search only inspects unmatched folders
    {
        let file = File::open(records_path)?;
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let line_str = line?;
            let trimmed = line_str.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_str::<CourseRecord>(trimmed) else {
                continue;
            };
            let course_dir = base_videos_dir.join(sanitize_filename(&record.title));
            for (i, video) in record.videos.iter().enumerate() {
                let safe_title = sanitize_filename(&video.title);
                let output_filename = format!("{}/{:0>2}_{}.mp4", course_dir.display(), i + 1, safe_title);
                let output_path = Path::new(&output_filename);

                if let Some(exact_cand) = find_exact_existing(&all_existing, &record.title, &video.title, output_path) {
                    matched_folders.insert(exact_cand.course_dir_name.clone());
                    if let Some(parent_name) = output_path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) {
                        matched_folders.insert(parent_name.to_string());
                    }
                }
            }
        }
    }

    let unmatched_candidates: Vec<&ExistingVideoCandidate> = all_existing
        .iter()
        .filter(|c| !matched_folders.contains(&c.course_dir_name))
        .collect();

    let file = File::open(records_path)?;
    let reader = BufReader::new(file);

    let mut pending_count = 0usize;
    let mut skipped_count = 0usize;

    for line in reader.lines() {
        let line_str = line?;
        let trimmed = line_str.trim();
        if trimmed.is_empty() {
            continue;
        }

        let record: CourseRecord = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Skipping malformed line in {}: {}", records_path, e);
                continue;
            }
        };

        let new_folder_name = sanitize_filename(&record.title);
        let course_dir = base_videos_dir.join(&new_folder_name);
        let target_course_norm = norm_title(&record.title);

        for (i, video) in record.videos.iter().enumerate() {
            let safe_title = sanitize_filename(&video.title);
            let output_filename = format!("{}/{:0>2}_{}.mp4", course_dir.display(), i + 1, safe_title);
            let output_path = Path::new(&output_filename);

            if find_exact_existing(&all_existing, &record.title, &video.title, output_path).is_some() {
                skipped_count += 1;
                continue;
            }

            pending_count += 1;
            let target_video_norm = norm_title(&video.title);

            let mut scored: Vec<(f64, &ExistingVideoCandidate)> = unmatched_candidates
                .iter()
                .copied()
                .map(|c| (fuzzy_score(&target_course_norm, &target_video_norm, c), c))
                .collect();

            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

            println!("[WOULD DOWNLOAD] {} — {}", record.title, video.title);
            println!("    new folder: {}", course_dir.display());
            println!("    new file:   {}", output_filename);
            for (score, cand) in scored.iter().take(top_k) {
                let old_folder_path = base_videos_dir.join(&cand.course_dir_name);
                println!(
                    "    fuzzy {:.2}: {} (old folder: {}, stem: {})",
                    score,
                    cand.path.display(),
                    old_folder_path.display(),
                    cand.cleaned_stem
                );
            }
        }
    }

    let mut unmatched_folders: Vec<&String> = all_existing_folders
        .difference(&matched_folders)
        .collect();
    unmatched_folders.sort();

    println!(
        "\nSummary ({}): {} would be downloaded, {} already matched/skipped.",
        records_path, pending_count, skipped_count
    );

    println!("\nUnmatched existing folders ({}):", unmatched_folders.len());
    for folder in unmatched_folders {
        println!("  [UNMATCHED FOLDER] {}", base_videos_dir.join(folder).display());
    }

    Ok(())
}
