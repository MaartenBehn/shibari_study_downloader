use chromiumoxide::cdp::browser_protocol::network::{SetCookieParams, TimeSinceEpoch};
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio::task::JoinSet;
use tokio::time::sleep;
use std::collections::HashSet;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::io::{Write};
use serde::{Serialize, Deserialize};
use tokio::process::Command;
use tokio::io::{AsyncBufReadExt, BufReader as TokioBufReader};

async fn load_cookies(page: &Page) -> Result<(), Box<dyn std::error::Error>> {

    let cookie_file_path = "cookies.txt";
    if let Ok(contents) = fs::read_to_string(cookie_file_path) {
        for line in contents.lines() {
            // Ignore comment lines and empty lines
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }

            let parts: Vec<&str> = line.split('\t').collect();
            // Netscape cookies.txt format typically has 7 tab-separated fields:
            // domain, include_subdomains, path, secure, expires, name, value
            if parts.len() >= 7 {
                let domain = parts[0];
                let path = parts[2];
                let secure = parts[3].parse::<bool>().unwrap_or(false);
                let expires = parts[4].parse::<f64>().unwrap_or(0.0);
                let name = parts[5];
                let value = parts[6];

                // Construct CDP SetCookie parameters
                let mut cookie_param = SetCookieParams::builder()
                    .name(name.to_string())
                    .value(value.to_string())
                    .domain(domain.to_string())
                    .path(path.to_string())
                    .secure(secure)
                    .build()?;

                if expires > 0.0 {
                    cookie_param.expires = Some(TimeSinceEpoch::new(expires));
                }

                let _ = page.execute(cookie_param).await;
            }
        }
        println!("Successfully injected cookies from {}", cookie_file_path);
    } else {
        eprintln!("Could not read {}", cookie_file_path);
    }

    Ok(())
}

async fn wait_for_page_settled(page: &chromiumoxide::Page) -> Result<(), Box<dyn std::error::Error>> {
    // This JS snippet waits until:
    // 1. document.readyState is 'complete'
    // 2. No DOM mutations have happened for at least 500ms
    let js_wait = r#"
        () => {
            return new Promise((resolve) => {
                let timeout = 500;
                let lastMutationTime = Date.now();

                // Observe DOM mutations (lazy loading, dynamic elements appearing)
                const observer = new MutationObserver(() => {
                    lastMutationTime = Date.now();
                });

                observer.observe(document.body, {
                    childList: true,
                    subtree: true,
                    attributes: true
                });

                // Check periodically if the DOM has stabilized
                const interval = setInterval(() => {
                    if (Date.now() - lastMutationTime > timeout) {
                        if (document.readyState === 'complete') {
                            observer.disconnect();
                            clearInterval(interval);
                            resolve(true);
                        }
                    }
                }, 100);
            });
        }
    "#;

    // Execute the script and wait for the promise to resolve
    page.evaluate(js_wait).await?;
    Ok(())
}

async fn print_prgramms(page: &Page) -> Result<(), Box<dyn std::error::Error>> {
    page.goto("https://shibaristudy.com/catalog/search").await?;
    page.wait_for_navigation().await?;

    let mut found_links: HashSet<String> = HashSet::new();
    let mut last_height: i64 = 0;
    
    println!("Starting infinite scroll and link collection...");

    loop {
        // Extract all current href matching the program card links
        let links: Vec<String> = page
            .evaluate(
                r#"
                Array.from(document.querySelectorAll('a[href*="/programs/"]'))
                     .map(a => a.getAttribute('href'))
                     .filter(href => href !== null)
                "#
            )
            .await?
            .into_value()?;

        for link in links {
            if found_links.insert(link.clone()) {
                println!("Found program: {}", link);
            }
        }

        // Scroll down smoothly by 800px
        page.evaluate("window.scrollBy(0, 800);").await?;
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // Get current total scroll height
        let current_height: i64 = page
            .evaluate("document.body.scrollHeight")
            .await?
            .into_value()?;

        // Get current scroll position
        let scroll_position: i64 = page
            .evaluate("window.innerHeight + window.scrollY")
            .await?
            .into_value()?;

        // Break if we have reached the bottom of the page and no new height is loaded
        if scroll_position >= current_height && current_height == last_height {
            println!("Reached the end of the page.");
            break;
        }

        last_height = current_height;
    }

    println!("\nCollection Complete! Total unique links: {}", found_links.len());
    for link in &found_links {
        println!("{}", link);
    }

    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct VideoItem {
    title: String,
    href: String,
    cid: Option<String>,
    duration: Option<String>,
    description: Option<String>,
    thumbnail: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProgramRecord {
    loc: String,
    title: String,
    about: Option<String>,
    categories: Vec<String>,
    videos: Vec<VideoItem>,
}

async fn get_programm_record(page: &Page, loc: &str) -> Result<ProgramRecord, Box<dyn std::error::Error>> {
    page.goto(&format!("https://shibaristudy.com{}", loc)).await?;
    wait_for_page_settled(&page).await?;

    let js_script = r#"
        () => {
            const titleEl = document.querySelector('[data-area="collection-title"]');
            const title = titleEl ? titleEl.innerHTML.trim() : null;
            
            const aboutEl = document.querySelector('[data-area="about-description"] .editor-content');
            const about = aboutEl ? aboutEl.innerHTML.trim() : null;

            const categoryEls = document.querySelectorAll('[data-area="program-categories"] [data-area="category-item"]');
            const categories = Array.from(categoryEls).map(el => el.textContent.trim());

            const videoMap = new Map();
            document.querySelectorAll('a.content-item').forEach(el => {
                const href = el.getAttribute('href');
                if (!href || videoMap.has(href)) return;

                videoMap.set(href, {
                    title: el.querySelector('.content-item-title')?.textContent.trim() || '',
                    href: href,
                    cid: el.getAttribute('data-cid'),
                    duration: el.querySelector('.badge .badge-item')?.textContent.trim() || null,
                    description: el.querySelector('.content-item-description')?.textContent.trim() || null,
                    thumbnail: el.querySelector('img')?.getAttribute('src') || null
                });
            });

            const loc = "";

            return { loc, title, about, categories, videos: Array.from(videoMap.values()) };
        }
    "#;

    let mut record: ProgramRecord = page.evaluate(js_script).await?.into_value()?;
    record.loc = loc.to_string();

    Ok(record)
}


async fn scrape_programs(page: &Page) -> Result<(), Box<dyn std::error::Error>> {
    
    let mut completed_locs = HashSet::new();
    if let Ok(file) = File::open("programs_records.jsonl") {
        let reader = BufReader::new(file);
        for line in reader.lines() {
            if let Ok(line_str) = line {
                // Parse each JSON line to extract the 'loc' field
                if let Ok(record) = serde_json::from_str::<ProgramRecord>(&line_str) {
                    completed_locs.insert(record.loc);
                }
            }
        }
        println!("Resuming: Found {} already completed programs in output file.", completed_locs.len());
    }

    let in_file = File::open("programs.txt")?;
    let reader = BufReader::new(in_file);

    let mut out_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("programs_records.jsonl")?;
    
    for line in reader.lines() {
        let loc = line?;
        let loc = loc.trim();
        
        // Skip empty lines or comment lines
        if loc.is_empty() || loc.starts_with('#') {
            continue;
        }

        // 2. Skip if the program path is already in the completed set
        if completed_locs.contains(loc) {
            println!("Skipping (already scraped): {}", loc);
            continue;
        }

        println!("Scraping program: {}", loc);

        match get_programm_record(page, loc).await {
            Ok(record) => {
                let json_line = serde_json::to_string(&record)?;
                writeln!(out_file, "{}", json_line)?;
                
                completed_locs.insert(loc.to_string());
            },
            Err(e) => {
                eprintln!("Failed to scrape {}: {}", loc, e);
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    println!("Scraping batch complete!");
    Ok(())
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .filter(|c| {
            // Keep alphanumeric characters, spaces, and safe common punctuation
            c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_'
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join("_")
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

    page.goto(&full_url).await?;
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
    
    let display_title = if output_path.len() > 50 {
        format!("{}...", &output_path[..47])
    } else {
        output_path.to_string()
    };

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

async fn scrape_videos(
    page: &Page,
    concurrency_limit: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let base_domain = "https://shibaristudy.com";
    
    let file = File::open("programs_records.jsonl")?;
    let reader = BufReader::new(file);

    let multi_progress = MultiProgress::new();

    // Create a bounded channel with capacity equal to the concurrency limit.
    let (tx, rx) = mpsc::channel::<(String, String)>(1);
    let rx = Arc::new(Mutex::new(rx));

    // Spawn worker tasks to handle concurrent downloads from the shared receiver queue
    let mut worker_set = tokio::task::JoinSet::new();
    for _ in 0..concurrency_limit {
        let receiver = Arc::clone(&rx);
        let mp = multi_progress.clone();
        
        worker_set.spawn(async move {
            loop {
                // Lock the receiver just long enough to pull the next job
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
                    None => break, // Channel closed and empty
                }
            }
        });
    }

    // 2. Producer Loop: Scrapes stream URLs just-in-time as download slots open
    for line in reader.lines() {
        let line_str = line?;
        let line_str = line_str.trim();
        if line_str.is_empty() {
            continue;
        }

        let record: ProgramRecord = match serde_json::from_str(line_str) {
            Ok(r) => r,
            Err(_) => continue,
        };
        
        //println!("\n--- Processing Program: {} (Videos: {}) ---", record.loc, record.videos.len());

        let program_folder_name = sanitize_filename(&record.title);
        let program_dir = format!("./videos/{}", program_folder_name);
        std::fs::create_dir_all(&program_dir)?;

        for (i, video) in record.videos.into_iter().enumerate() {
            let full_video_url = format!("{}{}", base_domain, video.href);
            let safe_title = sanitize_filename(&video.title);
            let output_filename = format!("{}/{:0>2}_{}.mp4", program_dir, i + 1, safe_title);

            if Path::new(&output_filename).exists() {
                //println!("✔ Skipping (file already exists): {}", video.title);
                continue;
            }

            //println!("Fetching fresh stream URL for: \"{}\"", video.title);
            let stream_url = match get_video_stream_url(page, &full_video_url).await {
                Ok(url) => url,
                Err(e) => {
                    eprintln!("Error fetching stream URL for {}: {}", video.title, e);
                    None
                }
            };
            
            if let Some(url) = stream_url {
                if tx.send((url, output_filename)).await.is_err() {
                    eprintln!("Download worker channel closed unexpectedly.");
                    break;
                }
            } else {
                eprintln!("Skipping download due to missing stream URL.");
            }
        }    
    }

    // Drop the sender so workers finish once the queue is empty
    drop(tx);

    // Wait for all remaining download workers to finish
    while let Some(res) = worker_set.join_next().await {
        if let Err(e) = res {
            eprintln!("Worker join error: {}", e);
        }
    }

    let _ = std::fs::remove_dir("./videos/temp");
    println!("\nAll programs successfully scraped, downloaded, and subbed!");
    Ok(())
}

// Simple XML entity escaper to prevent malformed nfo files
fn html_escape(input: &str) -> String {
    input.replace('&', "&amp;")
         .replace('<', "&lt;")
         .replace('>', "&gt;")
         .replace('"', "&quot;")
         .replace('\'', "&apos;")
}

async fn create_jellyfin_hardlinks() -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open("programs_records.jsonl")?;
    let reader = BufReader::new(file);

    let jellyfin_root = "./jellyfin_library";
    println!("Starting Jellyfin hardlink restructuring into '{}'...", jellyfin_root);

    for line in reader.lines() {
        let line_str = line?;
        if line_str.trim().is_empty() {
            continue;
        }

        let record: ProgramRecord = serde_json::from_str(&line_str)?;
        let program_folder_name = sanitize_filename(&record.title);
        
        let program_dest_dir = format!("{}/{}", jellyfin_root, program_folder_name);
        let season_dest_dir = format!("{}/Season 01", program_dest_dir);
        
        // 1. Create target directories
        std::fs::create_dir_all(&season_dest_dir)?;

        // 2. Write Program-Level tvshow.nfo
        let tvshow_nfo_path = format!("{}/tvshow.nfo", program_dest_dir);
        let about_text = record.about.as_deref().unwrap_or("No program description available.");
        let tvshow_xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<tvshow>
    <title>{}</title>
    <plot>{}</plot>
    <genre>{}</genre>
</tvshow>
"#,
            record.title,
            html_escape(about_text),
            record.categories.join("</genre>\n    <genre>")
        );
        std::fs::write(tvshow_nfo_path, tvshow_xml)?;

        let program_dir = format!("./videos/{}", program_folder_name);

        // 3. Process videos and create hardlinks
        for (index, video) in record.videos.iter().enumerate() {
            let episode_num = index + 1;
            let sanitized_title = sanitize_filename(&video.title);
            let structured_filename = format!("{} - S01E{:02}.mp4", sanitized_title, episode_num);

            let original_path = format!("{}/{:0>2}_{}.mp4", program_dir, episode_num, sanitized_title);
            let target_video_path = format!("{}/{}", season_dest_dir, structured_filename);
            
            // Skip if the original downloaded file doesn't exist yet
            if !Path::new(&original_path).exists() {
                println!("Skipping hardlink (original not found): {}", original_path);
                continue;
            }
            
            // Create hardlink (remove existing target link first if it exists)
            if Path::new(&target_video_path).exists() {
                let _ = std::fs::remove_file(&target_video_path);
            }

            match std::fs::hard_link(&original_path, &target_video_path) {
                Ok(_) => println!("✔ Hardlinked: {} -> {}", original_path, target_video_path),
                Err(e) => eprintln!("✖ Failed to hardlink {}: {}", video.title, e),
            }

            // 4. Write episode .nfo file in the structured path
            let target_nfo_path = format!("{}.nfo", target_video_path.strip_suffix(".mp4").unwrap());
            let video_desc = video.description.as_deref().unwrap_or("No description available.");
            let episode_xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<episodedetails>
    <title>{}</title>
    <season>1</season>
    <episode>{}</episode>
    <plot>{}</plot>
    <duration>{}</duration>
</episodedetails>
"#,
                video.title,
                episode_num,
                html_escape(video_desc),
                video.duration.as_deref().unwrap_or("")
            );
            std::fs::write(target_nfo_path, episode_xml)?;

            // 5. If subtitle files exist for the original, hardlink them too!
            link_subtitles_if_exist(&original_path, &target_video_path);
        }
    }

    println!("\nJellyfin hardlink library successfully built at '{}'!", jellyfin_root);
    Ok(())
}

/// Helper to carry over any downloaded .srt subtitles to the new hardlinked path
fn link_subtitles_if_exist(original_video: &str, target_video: &str) {
    let orig_path = Path::new(original_video);
    if let Some(parent) = orig_path.parent() {
        if let Some(stem) = orig_path.file_stem().and_then(|s| s.to_str()) {
            if let Ok(entries) = std::fs::read_dir(parent) {
                for entry in entries.flatten() {
                    if let Some(filename) = entry.file_name().to_str() {
                        if filename.starts_with(&format!("{}.", stem)) && filename.ends_with(".srt") {
                            let orig_sub = entry.path();
                            let sub_extension = filename.strip_prefix(stem).unwrap(); // e.g. ".en.srt"
                            let target_sub_path = format!(
                                "{}{}",
                                target_video.strip_suffix(".mp4").unwrap(),
                                sub_extension
                            );
                            let _ = std::fs::hard_link(orig_sub, target_sub_path);
                        }
                    }
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        println!("Shibari Study Scraper & Downloader Pipeline");
        println!("Usage:");
        println!("  cargo run -- print-programs          # Print all available programs from the site");
        println!("  cargo run -- scrape-programs         # Scrape program lists/metadata into programs_records.jsonl");
        println!("  cargo run -- scrape-videos           # Scrape individual video pages for fresh .m3u8 streams");
        println!("  cargo run -- download [LIMIT]        # Concurrently download videos (default limit: 3)");
        return Ok(());
    }

    let command = &args[1];

    // Commands that require launching the browser instance
    match command.as_str() {
        "print-programs" | "scrape-programs" | "scrape-videos" => {
            println!("Launching browser for task: '{}'...", command);
            
            let (browser, mut handler) = Browser::launch(
                BrowserConfig::builder()
                    .with_head()
                    .build()
                    .expect("Failed to build browser")
            ).await?;

            tokio::spawn(async move {
                while let Some(_h) = handler.next().await {}
            });

            let page = browser.new_page("about:blank").await?;
            page.enable_stealth_mode().await?;

            load_cookies(&page).await?;

            match command.as_str() {
                "print-programs" => {
                    print_prgramms(&page).await?;
                }
                "scrape-videos" => {
                    let concurrency_limit = if args.len() > 2 {
                        args[2].parse::<usize>().unwrap_or(3)
                    } else {
                        3
                    };

                    scrape_videos(&page, concurrency_limit).await?;
                }
                _ => unreachable!(),
            }
        }
        "jellyfin" => {
            create_jellyfin_hardlinks().await?;
        }
        _ => {
            eprintln!(
                "Unknown command: '{}'. Use 'print-programs', 'scrape-programs', 'scrape-videos', or 'download'.",
                command
            );
        }
    }

    Ok(())
}
