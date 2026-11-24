use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;
use std::env;

use crate::{common::load_cookies, courses::{fetch_courses, print_pending_videos, scrape_videos}, harness_guides::scrape_harness_guides, items::fetch_items, jellyfin::{create_jellyfin_hardlinks, create_thumbnails}};

pub mod common;
pub mod items;
pub mod old_programms;
pub mod courses;
pub mod harness_guides;
pub mod jellyfin;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        println!("Shibari Study Scraper & Downloader Pipeline");
        return Ok(());
    }

    let command = &args[1];

    // Commands that require launching the browser instance
    match command.as_str() {
        "fetch-items" | "fetch-courses" | "scrape-harness-guides" | "scrape-videos" => {
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
                "fetch-items" => { fetch_items(&page).await?; }
                "fetch-courses" => { fetch_courses(&page).await?; }
                "scrape-harness-guides" => { scrape_harness_guides(&page).await?; }
                "scrape-videos" => {
                    let concurrency_limit = if args.len() > 2 {
                        args[2].parse::<usize>().unwrap_or(3)
                    } else {
                        3
                    };
                    scrape_videos(&page, concurrency_limit).await?;
                }                _ => unreachable!(),
            }
        }
        "scrape-videos-dry-run" => {
            print_pending_videos(5)?;
        }
        "jellyfin" => {
            create_jellyfin_hardlinks().await?;
        }
        "thumbnails" => {
            create_thumbnails().await?;
        }
        _ => {
            eprintln!(
                "Unknown command: '{}'.",
                command
            );
        }
    }

    Ok(())
}
