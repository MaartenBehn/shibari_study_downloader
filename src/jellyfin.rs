use serde::Deserialize;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use tokio::process::Command;

use crate::common::sanitize_filename;
use crate::courses::{CourseRecord, CourseVideo};
use crate::old_programms::{norm_title, ProgramRecord, VideoItem};

#[derive(Deserialize, Default, Clone)]
struct LegacyVideoEntry {
    course_title: String,
    video_title: String,
    description: Option<String>,
    duration: Option<String>,
    video_path: Option<PathBuf>,
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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

fn find_video_file(
    base_videos_dir: &Path,
    course_title: &str,
    video_title: &str,
    expected_path: &Path,
    fallback_path: Option<&Path>,
) -> Option<PathBuf> {
    if expected_path.exists() {
        return Some(expected_path.to_path_buf());
    }
    if let Some(fb) = fallback_path {
        if fb.exists() {
            return Some(fb.to_path_buf());
        }
    }

    let target_video_norm = norm_title(video_title);
    if target_video_norm.is_empty() {
        return None;
    }

    let target_course_norm = norm_title(course_title);
    let mut candidate_dirs: Vec<PathBuf> = Vec::new();

    let exact_dir = base_videos_dir.join(sanitize_filename(course_title));
    if exact_dir.is_dir() {
        candidate_dirs.push(exact_dir);
    }

    if let Ok(entries) = fs::read_dir(base_videos_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    if name != "temp"
                        && norm_title(name) == target_course_norm
                        && !candidate_dirs.contains(&p)
                    {
                        candidate_dirs.push(p);
                    }
                }
            }
        }
    }

    for dir in candidate_dirs {
        if let Ok(files) = fs::read_dir(dir) {
            for file in files.flatten() {
                let fp = file.path();
                if fp.extension().and_then(|e| e.to_str()) != Some("mp4") {
                    continue;
                }
                if let Some(stem) = fp.file_stem().and_then(|s| s.to_str()) {
                    let cleaned = strip_leading_index(stem);
                    if norm_title(cleaned) == target_video_norm {
                        return Some(fp);
                    }
                }
            }
        }
    }

    None
}

fn load_old_programs(
    base_videos_dir: &Path,
) -> (
    HashMap<String, ProgramRecord>,
    HashMap<String, Vec<LegacyVideoEntry>>,
) {
    let mut by_course_norm: HashMap<String, ProgramRecord> = HashMap::new();
    let mut by_video_norm: HashMap<String, Vec<LegacyVideoEntry>> = HashMap::new();

    let Ok(f) = File::open("programs_records.jsonl") else {
        return (by_course_norm, by_video_norm);
    };

    for line in BufReader::new(f).lines().flatten() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<ProgramRecord>(trimmed) else {
            continue;
        };

        let prog_norm = norm_title(&rec.title);
        let prog_dir = base_videos_dir.join(sanitize_filename(&rec.title));

        for (idx, v) in rec.videos.iter().enumerate() {
            let v_norm = norm_title(&v.title);
            if v_norm.is_empty() {
                continue;
            }
            let expected = prog_dir.join(format!("{:0>2}_{}.mp4", idx + 1, sanitize_filename(&v.title)));
            let found_path = find_video_file(base_videos_dir, &rec.title, &v.title, &expected, None);

            by_video_norm.entry(v_norm).or_default().push(LegacyVideoEntry {
                course_title: rec.title.clone(),
                video_title: v.title.clone(),
                description: v.description.clone(),
                duration: v.duration.clone(),
                video_path: found_path,
            });
        }

        if !prog_norm.is_empty() {
            by_course_norm.entry(prog_norm).or_insert(rec);
        }
    }

    (by_course_norm, by_video_norm)
}

fn match_legacy_video<'a>(
    entries: &'a [LegacyVideoEntry],
    course_norm: &str,
) -> Option<&'a LegacyVideoEntry> {
    if entries.is_empty() {
        return None;
    }
    entries
        .iter()
        .find(|e| norm_title(&e.course_title) == course_norm)
        .or_else(|| entries.first())
}

fn link_subtitles_if_exist(original_video: &Path, target_video: &Path) {
    let Some(parent) = original_video.parent() else {
        return;
    };
    let Some(stem) = original_video.file_stem().and_then(|s| s.to_str()) else {
        return;
    };
    let Some(target_stem) = target_video.file_stem().and_then(|s| s.to_str()) else {
        return;
    };
    let Some(target_parent) = target_video.parent() else {
        return;
    };

    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(filename) = file_name.to_str() else {
            continue;
        };
        let prefix = format!("{}.", stem);
        if filename.starts_with(&prefix) && filename.ends_with(".srt") {
            let suffix = &filename[stem.len()..]; // e.g. ".en.srt"
            let target_sub = target_parent.join(format!("{}{}", target_stem, suffix));
            if target_sub.exists() {
                let _ = fs::remove_file(&target_sub);
            }
            let _ = fs::hard_link(entry.path(), target_sub);
        }
    }
}

fn resolve_course_records_path() -> &'static str {
    if Path::new("courses_records.jsonl").exists() {
        "courses_records.jsonl"
    } else if Path::new("scrape_videos.jsonl").exists() {
        "scrape_videos.jsonl"
    } else {
        "courses_records.jsonl"
    }
}

pub async fn create_jellyfin_hardlinks() -> Result<(), Box<dyn std::error::Error>> {
    let base_videos_dir = Path::new("./videos");
    let jellyfin_root = Path::new("./jellyfin_library");
    let records_path = resolve_course_records_path();

    let (old_by_course, old_by_video) = load_old_programs(base_videos_dir);
    let mut matched_old_courses: Vec<String> = Vec::new();

    println!(
        "Starting Jellyfin hardlink restructuring from '{}' into '{}'...",
        records_path,
        jellyfin_root.display()
    );

    let file = File::open(records_path)?;
    let reader = BufReader::new(file);

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

        let course_norm = norm_title(&record.title);
        let old_course = old_by_course.get(&course_norm);
        if let Some(oc) = old_course {
            matched_old_courses.push(norm_title(&oc.title));
        }

        let program_folder_name = sanitize_filename(&record.title);
        let program_dest_dir = jellyfin_root.join(&program_folder_name);
        let season_dest_dir = program_dest_dir.join("Season 01");
        fs::create_dir_all(&season_dest_dir)?;

        // Program-level tvshow.nfo: prefer course fields, fall back to programs_records.jsonl metadata
        let about_text = record
            .about
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| record.description.as_deref().filter(|s| !s.trim().is_empty()))
            .or_else(|| {
                old_course
                    .and_then(|oc| oc.about.as_deref())
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or("No program description available.");

        let genres: Vec<String> = if !record.categories.is_empty() {
            record.categories.clone()
        } else if let Some(oc) = old_course {
            oc.categories.clone()
        } else {
            Vec::new()
        };

        let genre_xml = if genres.is_empty() {
            String::new()
        } else {
            format!(
                "    <genre>{}</genre>\n",
                genres
                    .iter()
                    .map(|g| html_escape(g))
                    .collect::<Vec<_>>()
                    .join("</genre>\n    <genre>")
            )
        };

        let tvshow_xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<tvshow>
    <title>{}</title>
    <plot>{}</plot>
{}</tvshow>
"#,
            html_escape(&record.title),
            html_escape(about_text),
            genre_xml
        );
        fs::write(program_dest_dir.join("tvshow.nfo"), tvshow_xml)?;

        let course_dir = base_videos_dir.join(&program_folder_name);
        let mut linked_thumbs: Vec<PathBuf> = Vec::new();

        for (index, video) in record.videos.iter().enumerate() {
            let episode_num = index + 1;
            let sanitized_title = sanitize_filename(&video.title);
            let structured_filename = format!("{} - S01E{:0>2}.mp4", sanitized_title, episode_num);

            let expected_path = course_dir.join(format!("{:0>2}_{}.mp4", episode_num, sanitized_title));
            let v_norm = norm_title(&video.title);
            let legacy_match = old_by_video
                .get(&v_norm)
                .and_then(|list| match_legacy_video(list, &course_norm));

            let fallback_file = legacy_match.and_then(|m| m.video_path.as_deref());
            let Some(original_path) =
                find_video_file(base_videos_dir, &record.title, &video.title, &expected_path, fallback_file)
            else {
                println!(
                    "Skipping hardlink (video not found): {} — {}",
                    record.title, video.title
                );
                continue;
            };

            let target_video_path = season_dest_dir.join(&structured_filename);
            if target_video_path.exists() {
                let _ = fs::remove_file(&target_video_path);
            }

            match fs::hard_link(&original_path, &target_video_path) {
                Ok(_) => println!("✔ Hardlinked video: {}", structured_filename),
                Err(e) => {
                    eprintln!("✖ Failed to hardlink video {}: {}", video.title, e);
                    continue;
                }
            }

            // Episode .nfo: prefer new course video metadata, fall back to programs_records.jsonl
            let video_desc = video
                .in_this_video
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    legacy_match
                        .and_then(|m| m.description.as_deref())
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or("No description available.");

            let video_duration = video
                .duration
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    legacy_match
                        .and_then(|m| m.duration.as_deref())
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or("");

            let target_nfo_path = target_video_path.with_extension("nfo");
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
                html_escape(&video.title),
                episode_num,
                html_escape(video_desc),
                html_escape(video_duration)
            );
            fs::write(target_nfo_path, episode_xml)?;

            // Subtitles
            link_subtitles_if_exist(&original_path, &target_video_path);

            // Thumbnail (check original video path first, then expected_path)
            let orig_thumb = original_path.with_extension("jpg");
            let thumb_source = if orig_thumb.exists() {
                Some(orig_thumb)
            } else if expected_path.with_extension("jpg").exists() {
                Some(expected_path.with_extension("jpg"))
            } else {
                None
            };

            if let Some(src_thumb) = thumb_source {
                let target_thumb = target_video_path.with_extension("jpg");
                if target_thumb.exists() {
                    let _ = fs::remove_file(&target_thumb);
                }
                if fs::hard_link(&src_thumb, &target_thumb).is_ok() {
                    linked_thumbs.push(target_thumb);
                }
            }
        }

        if !linked_thumbs.is_empty() {
            let last_thumb = &linked_thumbs[linked_thumbs.len() - 1];
            let second_last_thumb = if linked_thumbs.len() > 1 {
                &linked_thumbs[linked_thumbs.len() - 2]
            } else {
                last_thumb
            };

            let show_poster = program_dest_dir.join("poster.jpg");
            if show_poster.exists() {
                let _ = fs::remove_file(&show_poster);
            }
            let _ = fs::copy(last_thumb, &show_poster);

            let season_poster = season_dest_dir.join("poster.jpg");
            if season_poster.exists() {
                let _ = fs::remove_file(&season_poster);
            }
            let _ = fs::copy(second_last_thumb, &season_poster);
        }
    }

    // Fallback pass: any programs in programs_records.jsonl that could not be matched to a course
    if let Ok(old_file) = File::open("programs_records.jsonl") {
        for line in BufReader::new(old_file).lines().flatten() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(old_rec) = serde_json::from_str::<ProgramRecord>(trimmed) else {
                continue;
            };
            let p_norm = norm_title(&old_rec.title);
            if matched_old_courses.contains(&p_norm) {
                continue;
            }

            let folder_name = sanitize_filename(&old_rec.title);
            let prog_dest = jellyfin_root.join(&folder_name);
            let season_dest = prog_dest.join("Season 01");
            fs::create_dir_all(&season_dest)?;

            let about_text = old_rec
                .about
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("No program description available.");

            let genre_xml = if old_rec.categories.is_empty() {
                String::new()
            } else {
                format!(
                    "    <genre>{}</genre>\n",
                    old_rec
                        .categories
                        .iter()
                        .map(|g| html_escape(g))
                        .collect::<Vec<_>>()
                        .join("</genre>\n    <genre>")
                )
            };

            let tvshow_xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<tvshow>
    <title>{}</title>
    <plot>{}</plot>
{}</tvshow>
"#,
                html_escape(&old_rec.title),
                html_escape(about_text),
                genre_xml
            );
            fs::write(prog_dest.join("tvshow.nfo"), tvshow_xml)?;

            let old_prog_dir = base_videos_dir.join(&folder_name);
            for (idx, v) in old_rec.videos.iter().enumerate() {
                let ep_num = idx + 1;
                let safe_v = sanitize_filename(&v.title);
                let expected = old_prog_dir.join(format!("{:0>2}_{}.mp4", ep_num, safe_v));
                let Some(orig_path) =
                    find_video_file(base_videos_dir, &old_rec.title, &v.title, &expected, None)
                else {
                    continue;
                };

                let structured = format!("{} - S01E{:0>2}.mp4", safe_v, ep_num);
                let target_video = season_dest.join(&structured);
                if target_video.exists() {
                    let _ = fs::remove_file(&target_video);
                }
                if fs::hard_link(&orig_path, &target_video).is_ok() {
                    println!("✔ Hardlinked fallback video: {}", structured);
                    link_subtitles_if_exist(&orig_path, &target_video);

                    let ep_xml = format!(
                        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<episodedetails>
    <title>{}</title>
    <season>1</season>
    <episode>{}</episode>
    <plot>{}</plot>
    <duration>{}</duration>
</episodedetails>
"#,
                        html_escape(&v.title),
                        ep_num,
                        html_escape(v.description.as_deref().unwrap_or("No description available.")),
                        html_escape(v.duration.as_deref().unwrap_or(""))
                    );
                    let _ = fs::write(target_video.with_extension("nfo"), ep_xml);
                }
            }
        }
    }

    println!(
        "\nJellyfin hardlink library successfully built at '{}'!",
        jellyfin_root.display()
    );
    Ok(())
}

pub async fn create_thumbnails() -> Result<(), Box<dyn std::error::Error>> {
    let base_videos_dir = Path::new("./videos");
    let records_path = resolve_course_records_path();
    let (_, old_by_video) = load_old_programs(base_videos_dir);

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

        let course_norm = norm_title(&record.title);
        let course_dir = base_videos_dir.join(sanitize_filename(&record.title));

        for (index, video) in record.videos.iter().enumerate() {
            let ep_num = index + 1;
            let safe_v = sanitize_filename(&video.title);
            let expected = course_dir.join(format!("{:0>2}_{}.mp4", ep_num, safe_v));
            let legacy_file = old_by_video
                .get(&norm_title(&video.title))
                .and_then(|list| match_legacy_video(list, &course_norm))
                .and_then(|m| m.video_path.as_deref());

            if let Some(found) =
                find_video_file(base_videos_dir, &record.title, &video.title, &expected, legacy_file)
            {
                if let Err(e) = extract_video_thumbnail(&found).await {
                    eprintln!("Thumbnail error for {}: {}", found.display(), e);
                }
            }
        }
    }

    Ok(())
}

async fn extract_video_thumbnail(video_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if !video_path.exists() {
        return Err("Video file does not exist".into());
    }

    let thumb_path = video_path.with_extension("jpg");
    if thumb_path.exists() {
        return Ok(());
    }

    let video_str = video_path.to_str().ok_or("Invalid video path")?;
    let thumb_str = thumb_path.to_str().ok_or("Invalid thumbnail path")?;

    let probe_output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            video_str,
        ])
        .output()
        .await?;

    let duration_str = String::from_utf8(probe_output.stdout)?;
    let duration: f64 = duration_str.trim().parse().unwrap_or(10.0);
    let seek_time = if duration > 10.0 { duration / 2.0 } else { 2.0 };

    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-ss",
            &seek_time.to_string(),
            "-i",
            video_str,
            "-vframes",
            "1",
            "-q:v",
            "2",
            thumb_str,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await?;

    if status.success() {
        if let Some(file_name) = video_path.file_name() {
            println!("✔ Extracted thumbnail for: {}", file_name.to_string_lossy());
        }
    }

    Ok(())
}
