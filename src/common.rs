use std::{fs, path::Path, time::Duration};
use chromiumoxide::{Page, cdp::browser_protocol::network::{SetCookieParams, TimeSinceEpoch}};
use tokio::process::Command;

pub async fn load_cookies(page: &Page) -> Result<(), Box<dyn std::error::Error>> {

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

pub fn is_target_navigated_or_closed(err: &(dyn std::error::Error + 'static)) -> bool {
    let msg = err.to_string();
    msg.contains("-32000") || msg.contains("Inspected target navigated or closed")
}

pub async fn wait_for_page_settled(page: &chromiumoxide::Page) -> Result<(), Box<dyn std::error::Error>> {
    let js_wait = r#"
        () => {
            return new Promise((resolve) => {
                const quietMs = 500;
                const maxWaitMs = 10000;
                const startTime = Date.now();
                let lastMutationTime = Date.now();

                const observer = new MutationObserver(() => {
                    lastMutationTime = Date.now();
                });

                const target = document.body || document.documentElement;
                if (target) {
                    observer.observe(target, {
                        childList: true,
                        subtree: true,
                        attributes: true
                    });
                }

                const interval = setInterval(() => {
                    const now = Date.now();
                    const quietEnough = (now - lastMutationTime) > quietMs && document.readyState === 'complete';
                    const timedOut = (now - startTime) > maxWaitMs;

                    if (quietEnough || timedOut) {
                        observer.disconnect();
                        clearInterval(interval);
                        resolve(true);
                    }
                }, 100);
            });
        }
    "#;

    page.evaluate(js_wait).await?;
    //tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(())
}

pub async fn goto_settled(page: &Page, href: &str) -> Result<(), Box<dyn std::error::Error>> {
    let url = if href.starts_with("http") {
        href.to_string()
    } else {
        format!("https://shibaristudy.com{}", href)
    };

    page.goto(&url).await?;

    const MAX_ATTEMPTS: usize = 5;
    for attempt in 1..=MAX_ATTEMPTS {
        match wait_for_page_settled(page).await {
            Ok(()) => return Ok(()),
            Err(e) if is_target_navigated_or_closed(e.as_ref()) && attempt < MAX_ATTEMPTS => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                println!("Navigated while wait_for_page_settled");
            }
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

pub async fn eval_with_retry<T>(
    page: &Page,
    js: &str,
) -> Result<T, Box<dyn std::error::Error>>
where
    T: serde::de::DeserializeOwned,
{
    const MAX_ATTEMPTS: usize = 5;
    for attempt in 1..=MAX_ATTEMPTS {
        match page.evaluate(js).await {
            Ok(val) => return Ok(val.into_value()?),
            Err(e) if is_target_navigated_or_closed(&e) && attempt < MAX_ATTEMPTS => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = wait_for_page_settled(page).await;
                println!("Navigated while evaluate");
            }
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!()
}

pub async fn download_file_with_curl(url: &str, dest_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let status = Command::new("curl")
        .args([
            "-fsSL",
            "-A",
            "Mozilla/5.0",
            "--cookie",
            "cookies.txt",
            url,
            "-o",
            dest_path.to_str().ok_or("Invalid destination path")?,
        ])
        .status()
        .await?;

    if !status.success() {
        return Err(format!("curl failed with status {} for {}", status, url).into());
    }

    Ok(())
}

pub fn sanitize_filename(name: &str) -> String {
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
