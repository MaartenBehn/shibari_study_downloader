use chromiumoxide::Page;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

use crate::common::{eval_with_retry, goto_settled, sanitize_filename};

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct BlogRecord {
    pub id: String,
    pub loc: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub read_time: Option<String>,
    pub author: Option<String>,
    pub author_href: Option<String>,
    pub hero_image_candidates: Vec<String>,
    pub hero_image_alt: Option<String>,
    pub markdown_path: String,
    pub downloaded_images: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BodyBlock {
    Heading {
        level: u8,
        text: String,
    },
    Paragraph {
        markdown: String,
    },
    Blockquote {
        markdown: String,
    },
    Figure {
        #[serde(default)]
        candidates: Vec<String>,
        alt: String,
    },
    List {
        ordered: bool,
        items: Vec<String>,
    },
    #[default]
    Empty,
}

#[derive(Debug, Deserialize, Default)]
struct ExtractedBlogPage {
    title: String,
    subtitle: Option<String>,
    read_time: Option<String>,
    author: Option<String>,
    author_href: Option<String>,
    #[serde(default)]
    hero_image_candidates: Vec<String>,
    hero_image_alt: Option<String>,
    #[serde(default)]
    blocks: Vec<BodyBlock>,
}

const BLOG_PAGE_JS: &str = r#"
(() => {
    const cleanText = (el) => el ? el.textContent.replace(/\u00a0/g, ' ').replace(/\s+/g, ' ').trim() : null;

    const resolveImgCandidates = (img) => {
        if (!img) return [];
        const candidates = [];
        const pushUrl = (raw) => {
            if (!raw) return;
            const trimmed = raw.trim();
            if (!trimmed) return;
            if (!candidates.includes(trimmed)) candidates.push(trimmed);
            try {
                const u = new URL(trimmed, window.location.origin);
                if (u.pathname === '/_next/image') {
                    const inner = u.searchParams.get('url');
                    if (inner && !candidates.includes(inner)) {
                        candidates.unshift(inner);
                    }
                }
            } catch {}
        };

        pushUrl(img.getAttribute('src'));
        pushUrl(img.currentSrc);

        const srcset = img.getAttribute('srcset') || '';
        if (srcset) {
            let bestUrl = null;
            let bestW = -1;
            for (const part of srcset.split(',')) {
                const tokens = part.trim().split(/\s+/);
                if (!tokens[0]) continue;
                const wMatch = (tokens[1] || '').match(/^(\d+)w$/);
                const w = wMatch ? parseInt(wMatch[1], 10) : 0;
                if (w >= bestW) {
                    bestW = w;
                    bestUrl = tokens[0];
                }
            }
            pushUrl(bestUrl);
        }

        return candidates;
    };

    const header = document.querySelector('header');
    const h1 = header?.querySelector('h1') || document.querySelector('h1');
    const title = cleanText(h1) || '';

    const subtitleEl = h1?.nextElementSibling?.tagName === 'P' ? h1.nextElementSibling : null;
    const subtitle = cleanText(subtitleEl);

    const authorLink = header?.querySelector('a[href^="/blog/authors/"]');
    const author = cleanText(authorLink);
    const author_href = authorLink?.getAttribute('href') || null;

    let read_time = null;
    const metaRow = authorLink?.parentElement;
    if (metaRow) {
        const spans = Array.from(metaRow.querySelectorAll('span'));
        const rtSpan = spans.find(s => /min\s+read/i.test(s.textContent || ''));
        if (rtSpan) read_time = cleanText(rtSpan);
    }

    const heroImg = header?.querySelector('img');
    const hero_image_candidates = resolveImgCandidates(heroImg);
    const hero_image_alt = heroImg?.getAttribute('alt')?.trim() || null;

    const inlineMd = (node) => {
        if (!node) return '';
        if (node.nodeType === Node.TEXT_NODE) {
            return node.textContent.replace(/\u00a0/g, ' ');
        }
        if (node.nodeType !== Node.ELEMENT_NODE) return '';

        const tag = node.tagName.toUpperCase();
        if (tag === 'BR') return '  \n';

        const inner = Array.from(node.childNodes).map(inlineMd).join('');
        if (!inner) return '';

        if (tag === 'STRONG' || tag === 'B') {
            return `**${inner.trim()}**`;
        }
        if (tag === 'EM' || tag === 'I') {
            return `*${inner.trim()}*`;
        }
        if (tag === 'CODE') {
            return `\`${inner.trim()}\``;
        }
        if (tag === 'A') {
            const href = node.getAttribute('href') || '';
            const fullHref = href.startsWith('/') ? `https://shibaristudy.com${href}` : href;
            return fullHref ? `[${inner.trim()}](${fullHref})` : inner;
        }
        return inner;
    };

    const bodyRoot = document.querySelector('[data-article-body="true"]') || document.querySelector('article');
    const blocks = [];

    if (bodyRoot) {
        for (const el of Array.from(bodyRoot.children)) {
            const tag = el.tagName.toUpperCase();
            if (/^H[1-6]$/.test(tag)) {
                const level = parseInt(tag.slice(1), 10);
                const text = cleanText(el);
                if (text) blocks.push({ kind: 'heading', level, text });
            } else if (tag === 'P') {
                const md = inlineMd(el).replace(/\s+/g, ' ').trim();
                if (md) blocks.push({ kind: 'paragraph', markdown: md });
            } else if (tag === 'BLOCKQUOTE') {
                const md = inlineMd(el).replace(/\s+/g, ' ').trim();
                if (md) blocks.push({ kind: 'blockquote', markdown: md });
            } else if (tag === 'FIGURE') {
                const img = el.querySelector('img');
                const candidates = resolveImgCandidates(img);
                const alt = img?.getAttribute('alt')?.trim() || '';
                if (candidates.length > 0) blocks.push({ kind: 'figure', candidates, alt });
            } else if (tag === 'UL' || tag === 'OL') {
                const items = Array.from(el.querySelectorAll(':scope > li'))
                    .map(li => inlineMd(li).replace(/\s+/g, ' ').trim())
                    .filter(Boolean);
                if (items.length > 0) {
                    blocks.push({ kind: 'list', ordered: tag === 'OL', items });
                }
            }
        }
    }

    return {
        title,
        subtitle,
        read_time,
        author,
        author_href,
        hero_image_candidates,
        hero_image_alt,
        blocks,
    };
})()
"#;

fn resolve_asset_url(raw: &str) -> String {
    if raw.starts_with("http") {
        raw.to_string()
    } else if raw.starts_with('/') {
        format!("https://shibaristudy.com{}", raw)
    } else {
        format!("https://shibaristudy.com/{}", raw)
    }
}

fn guess_image_ext(url: &str) -> &str {
    url.split('?')
        .next()
        .and_then(|p| Path::new(p).extension())
        .and_then(|e| e.to_str())
        .filter(|e| matches!(e.to_lowercase().as_str(), "webp" | "png" | "jpg" | "jpeg" | "gif"))
        .unwrap_or("webp")
}

fn escape_yaml_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

async fn download_first_available_image(
    candidates: &[String],
    referer_url: &str,
    images_dir: &Path,
    base_name_no_ext: &str,
    downloaded_images: &mut Vec<String>,
) -> Option<String> {
    let mut last_err_url: Option<String> = None;

    for raw in candidates {
        let full_url = resolve_asset_url(raw);
        let ext = guess_image_ext(&full_url);
        let file_name = format!("{}.{}", base_name_no_ext, ext);
        let img_dest = images_dir.join(&file_name);

        if let Ok(meta) = fs::metadata(&img_dest) {
            if meta.len() > 0 {
                let full_str = img_dest.to_string_lossy().to_string();
                if !downloaded_images.contains(&full_str) {
                    downloaded_images.push(full_str);
                }
                return Some(format!("images/{}", file_name));
            }
        }

        let Some(dest_str) = img_dest.to_str() else {
            continue;
        };

        let mut args = vec![
            "-fsSL",
            "-A",
            "Mozilla/5.0",
            "-e",
            referer_url,
        ];
        if Path::new("cookies.txt").exists() {
            args.extend(["--cookie", "cookies.txt"]);
        }
        args.extend([&full_url, "-o", dest_str]);

        match Command::new("curl").args(&args).status().await {
            Ok(status) if status.success() => {
                if let Ok(meta) = fs::metadata(&img_dest) {
                    if meta.len() > 0 {
                        let full_str = img_dest.to_string_lossy().to_string();
                        if !downloaded_images.contains(&full_str) {
                            downloaded_images.push(full_str);
                        }
                        println!("  ✔ Downloaded image: {}", img_dest.display());
                        return Some(format!("images/{}", file_name));
                    }
                }
                let _ = fs::remove_file(&img_dest);
            }
            _ => {
                let _ = fs::remove_file(&img_dest);
                last_err_url = Some(full_url);
            }
        }
    }

    if let Some(url) = last_err_url {
        eprintln!("  ✖ Failed to download image candidates (last: {})", url);
    }

    None
}

async fn collect_blog_items(page: &Page) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
    let mut items = Vec::new();

    if let Ok(file) = File::open("library_items.jsonl") {
        for line in BufReader::new(file).lines().flatten() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
                let typ = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let href = val.get("href").and_then(|v| v.as_str()).unwrap_or("");
                if typ == "blog" || typ == "article" || href.starts_with("/blog/") {
                    items.push(val);
                }
            }
        }
    }

    if !items.is_empty() {
        println!("Found {} blog entries in library_items.jsonl.", items.len());
        return Ok(items);
    }

    println!("No blog items in library_items.jsonl; querying /next/api/library?view=blog...");
    goto_settled(page, "blog").await?;

    let mut page_num = 1usize;
    loop {
        let api_url = format!(
            "https://shibaristudy.com/next/api/library?view=blog&page={}",
            page_num
        );
        page.goto(&api_url).await?;
        let _ = page.wait_for_navigation().await;

        let body_text: String = page
            .evaluate("document.body.innerText")
            .await?
            .into_value()?;

        let Ok(data) = serde_json::from_str::<serde_json::Value>(&body_text) else {
            break;
        };

        let Some(arr) = data.get("items").and_then(|v| v.as_array()) else {
            break;
        };

        for item in arr {
            items.push(item.clone());
        }

        if !data.get("hasNext").and_then(|v| v.as_bool()).unwrap_or(false) {
            break;
        }
        page_num += 1;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    println!("Fetched {} blog entries from API.", items.len());
    Ok(items)
}

pub async fn scrape_blogs(page: &Page) -> Result<(), Box<dyn std::error::Error>> {
    let index_out_path = "blogs_records.jsonl";
    let base_blogs_dir = Path::new("./blogs");
    let images_dir = base_blogs_dir.join("images");
    fs::create_dir_all(&images_dir)?;

    let mut done_locs: HashSet<String> = HashSet::new();
    if let Ok(f) = File::open(index_out_path) {
        for line in BufReader::new(f).lines().flatten() {
            if let Ok(rec) = serde_json::from_str::<BlogRecord>(&line) {
                done_locs.insert(rec.loc);
            }
        }
        println!("Resuming: {} blogs already downloaded.", done_locs.len());
    }

    let blog_items = collect_blog_items(page).await?;
    let mut out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(index_out_path)?;

    for item in blog_items {
        let id = item
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let loc = match item.get("href").and_then(|v| v.as_str()) {
            Some(h) if !h.trim().is_empty() => h.to_string(),
            _ => continue,
        };

        if done_locs.contains(&loc) {
            continue;
        }

        let fallback_title = item
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("untitled_blog")
            .to_string();

        let source_url = resolve_asset_url(&loc);
        println!("Scraping blog: {} ({})", fallback_title, loc);
        goto_settled(page, &loc).await?;

        let extracted: ExtractedBlogPage = eval_with_retry(page, BLOG_PAGE_JS).await?;
        let final_title = if extracted.title.trim().is_empty() {
            fallback_title
        } else {
            extracted.title.clone()
        };

        let slug = sanitize_filename(&final_title);
        let md_filename = format!("{}.md", slug);
        let md_path = base_blogs_dir.join(&md_filename);

        let mut downloaded_images = Vec::new();
        let mut md = String::new();

        // YAML frontmatter
        md.push_str("---\n");
        md.push_str(&format!("title: \"{}\"\n", escape_yaml_str(&final_title)));
        if let Some(sub) = &extracted.subtitle {
            md.push_str(&format!("subtitle: \"{}\"\n", escape_yaml_str(sub)));
        }
        if let Some(author) = &extracted.author {
            md.push_str(&format!("author: \"{}\"\n", escape_yaml_str(author)));
        }
        if let Some(rt) = &extracted.read_time {
            md.push_str(&format!("read_time: \"{}\"\n", escape_yaml_str(rt)));
        }
        md.push_str(&format!(
            "source_url: \"{}\"\n",
            escape_yaml_str(&source_url)
        ));
        md.push_str("---\n\n");

        // Header block
        md.push_str(&format!("# {}\n\n", final_title));
        if let Some(sub) = &extracted.subtitle {
            md.push_str(&format!("*{}*\n\n", sub));
        }

        let mut meta_parts = Vec::new();
        if let Some(author) = &extracted.author {
            if let Some(ahref) = &extracted.author_href {
                meta_parts.push(format!("By [{}]({})", author, resolve_asset_url(ahref)));
            } else {
                meta_parts.push(format!("By {}", author));
            }
        }
        if let Some(rt) = &extracted.read_time {
            meta_parts.push(rt.clone());
        }
        if !meta_parts.is_empty() {
            md.push_str(&format!("{}\n\n", meta_parts.join(" • ")));
        }

        // Hero image
        if !extracted.hero_image_candidates.is_empty() {
            let alt = extracted.hero_image_alt.as_deref().unwrap_or("");
            let hero_base = format!("{}_hero", slug);
            if let Some(local_rel) = download_first_available_image(
                &extracted.hero_image_candidates,
                &source_url,
                &images_dir,
                &hero_base,
                &mut downloaded_images,
            )
            .await
            {
                md.push_str(&format!("![{}]({})\n\n", alt, local_rel));
            } else {
                let fallback_url = resolve_asset_url(&extracted.hero_image_candidates[0]);
                md.push_str(&format!("![{}]({})\n\n", alt, fallback_url));
            }
        }

        // Article body blocks
        let mut fig_idx = 0usize;
        for block in extracted.blocks {
            match block {
                BodyBlock::Heading { level, text } => {
                    let hashes = "#".repeat(level.clamp(1, 6) as usize);
                    md.push_str(&format!("{} {}\n\n", hashes, text));
                }
                BodyBlock::Paragraph { markdown } => {
                    md.push_str(&format!("{}\n\n", markdown));
                }
                BodyBlock::Blockquote { markdown } => {
                    for line in markdown.lines() {
                        md.push_str(&format!("> {}\n", line));
                    }
                    md.push('\n');
                }
                BodyBlock::Figure { candidates, alt } => {
                    fig_idx += 1;
                    let fig_base = format!("{}_{:0>2}", slug, fig_idx);
                    if let Some(local_rel) = download_first_available_image(
                        &candidates,
                        &source_url,
                        &images_dir,
                        &fig_base,
                        &mut downloaded_images,
                    )
                    .await
                    {
                        md.push_str(&format!("![{}]({})\n\n", alt, local_rel));
                    } else if let Some(first) = candidates.first() {
                        md.push_str(&format!("![{}]({})\n\n", alt, resolve_asset_url(first)));
                    }
                }
                BodyBlock::List { ordered, items } => {
                    for (idx, item_md) in items.iter().enumerate() {
                        if ordered {
                            md.push_str(&format!("{}. {}\n", idx + 1, item_md));
                        } else {
                            md.push_str(&format!("- {}\n", item_md));
                        }
                    }
                    md.push('\n');
                }
                BodyBlock::Empty => {}
            }
        }

        fs::write(&md_path, md)?;
        println!("  ✔ Saved Markdown: {}", md_path.display());

        let record = BlogRecord {
            id,
            loc: loc.clone(),
            title: final_title,
            subtitle: extracted.subtitle,
            read_time: extracted.read_time,
            author: extracted.author,
            author_href: extracted.author_href,
            hero_image_candidates: extracted.hero_image_candidates,
            hero_image_alt: extracted.hero_image_alt,
            markdown_path: md_path.to_string_lossy().to_string(),
            downloaded_images,
        };

        writeln!(out, "{}", serde_json::to_string(&record)?)?;
        out.flush()?;
        done_locs.insert(loc);

        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    println!("Blogs done.");
    Ok(())
}
