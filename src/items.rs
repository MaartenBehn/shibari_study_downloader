use chromiumoxide::{ Page};
use std::fs::OpenOptions;
use std::io::{Write};
use std::time::Duration;

use crate::common::goto_settled;

pub async fn fetch_items(page: &Page) -> Result<(), Box<dyn std::error::Error>> {
    // Navigate to the library browse page first so session/cookies are active
    println!("Navigating to library browse page...");
    goto_settled(page, "library/browse?view=all").await?;

    let output_path = "library_items.jsonl";
    let mut out_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(output_path)?;

    let mut total_items = 0usize;
    let mut page_num = 1usize;

    println!("Fetching all library pages from API...");

    loop {
        let url = format!(
            "https://shibaristudy.com/next/api/library?view=all&page={}",
            page_num
        );

        // Navigate directly to the API URL — the browser sends all cookies
        // automatically, so auth just works. The response body is raw JSON.
        page.goto(&url).await?;
        page.wait_for_navigation().await?;

        let body_text: String = page
            .evaluate("document.body.innerText")
            .await?
            .into_value()?;

        let data: serde_json::Value = match serde_json::from_str(&body_text) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Page {}: failed to parse JSON — {}", page_num, e);
                eprintln!("Preview: {}", &body_text[..body_text.len().min(500)]);
                break;
            }
        };

        let items = match data.get("items").and_then(|v| v.as_array()) {
            Some(arr) => arr.clone(),
            None => {
                eprintln!("Page {}: no 'items' key. Response:\n{:#}", page_num, data);
                break;
            }
        };

        println!("Page {:>3}: {} items", page_num, items.len());

        for item in &items {
            writeln!(out_file, "{}", serde_json::to_string(item)?)?;
            total_items += 1;
        }

        let has_next = data
            .get("hasNext")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !has_next {
            println!("Last page reached (hasNext = false).");
            break;
        }

        page_num += 1;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    println!(
        "\nDone! Saved {} items across {} pages to '{}'.",
        total_items, page_num, output_path
    );
    Ok(())
}

