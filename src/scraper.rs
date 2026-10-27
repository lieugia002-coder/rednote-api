use regex::Regex;
use reqwest::{header::HeaderMap, Client};
use serde_json::Value;

#[derive(Debug, Clone, serde::Serialize)]
pub struct RednoteEngagement {
    pub likes: String,
    pub comments: String,
    pub collects: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RednoteDownload {
    pub quality: String,
    pub url: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RednoteResult {
    #[serde(rename = "noteId")]
    pub note_id: String,
    pub nickname: String,
    pub title: String,
    pub desc: String,
    pub keywords: String,
    pub duration: String,
    pub engagement: RednoteEngagement,
    pub images: Vec<String>,
    pub downloads: Vec<RednoteDownload>,
}

const UA_DESKTOP: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/132.0.0.0 Safari/537.36";
const UA_MOBILE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";

// ---------------------------------------------------------------- helpers

fn capture_single(html: &str, pattern: &str) -> String {
    Regex::new(pattern)
        .unwrap()
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|v| v.as_str().trim().to_string())
        .unwrap_or_default()
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// Lấy content của <meta name|property="key" content="...">, chấp nhận cả 2 thứ tự thuộc tính.
fn meta_all(html: &str, key: &str) -> Vec<String> {
    let k = regex::escape(key);
    let p1 = format!(
        r#"(?is)<meta[^>]*?(?:name|property)\s*=\s*["']{k}["'][^>]*?content\s*=\s*["'](.*?)["']"#
    );
    let p2 = format!(
        r#"(?is)<meta[^>]*?content\s*=\s*["'](.*?)["'][^>]*?(?:name|property)\s*=\s*["']{k}["']"#
    );
    let mut out: Vec<String> = Vec::new();
    for p in [p1, p2] {
        for c in Regex::new(&p).unwrap().captures_iter(html) {
            if let Some(m) = c.get(1) {
                let v = html_unescape(m.as_str().trim());
                if !v.is_empty() && !out.contains(&v) {
                    out.push(v);
                }
            }
        }
    }
    out
}

fn meta_first(html: &str, key: &str) -> String {
    meta_all(html, key).into_iter().next().unwrap_or_default()
}

fn s(v: &Value) -> String {
    match v {
        Value::String(x) => x.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn fix_url(u: &str) -> String {
    let u = u.trim().replace("\\u002F", "/").replace("&amp;", "&");
    if let Some(rest) = u.strip_prefix("http://") {
        format!("https://{rest}")
    } else if u.starts_with("//") {
        format!("https:{u}")
    } else {
        u
    }
}

/// Tách và parse `window.__INITIAL_STATE__={...}` (JS object, có thể chứa `undefined`).
fn extract_initial_state(html: &str) -> Option<Value> {
    let marker = "window.__INITIAL_STATE__";
    let start = html.find(marker)? + marker.len();
    let rest = html[start..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let end = rest.find("</script>").unwrap_or(rest.len());
    let body = &rest[..end];

    // `undefined` không phải JSON hợp lệ -> null
    let re = Regex::new(r"([:\[,])\s*undefined\b").unwrap();
    let body = re.replace_all(body, "${1}null");

    serde_json::Deserializer::from_str(body.trim())
        .into_iter::<Value>()
        .next()?
        .ok()
}

/// Tìm object "note" trong state (bản web và bản mobile share).
fn find_note(state: &Value) -> Option<&Value> {
    // Web: note.noteDetailMap[id].note
    if let Some(map) = state.pointer("/note/noteDetailMap").and_then(|v| v.as_object()) {
        let first_id = s(&state["note"]["firstNoteId"]);
        let pick = map
            .get(&first_id)
            .and_then(|e| e.get("note"))
            .filter(|n| n.as_object().map_or(false, |o| !o.is_empty()))
            .or_else(|| {
                map.values()
                    .filter_map(|e| e.get("note"))
                    .find(|n| n.as_object().map_or(false, |o| !o.is_empty()))
            });
        if pick.is_some() {
            return pick;
        }
    }
    // Mobile: noteData.data.noteData
    if let Some(n) = state.pointer("/noteData/data/noteData") {
        if n.is_object() {
            return Some(n);
        }
    }
    None
}

fn collect_downloads(note: &Value) -> Vec<RednoteDownload> {
    let mut out: Vec<RednoteDownload> = Vec::new();
    let mut push = |q: String, u: String| {
        let u = fix_url(&u);
        if !u.is_empty() && !out.iter().any(|d| d.url == u) {
            out.push(RednoteDownload { quality: q, url: u });
        }
    };

    // Bản gốc không watermark
    let origin = s(&note["video"]["consumer"]["originVideoKey"]);
    if !origin.is_empty() {
        push(
            "Original".to_string(),
            format!("https://sns-video-bd.xhscdn.com/{origin}"),
        );
    }

    // Các stream: h264 / h265 / av1 / h266
    if let Some(stream) = note.pointer("/video/media/stream").and_then(|v| v.as_object()) {
        for (codec, list) in stream {
            let Some(arr) = list.as_array() else { continue };
            for item in arr {
                let h = s(&item["height"]);
                let label = if h.is_empty() {
                    codec.to_uppercase()
                } else {
                    format!("{h}p {}", codec.to_uppercase())
                };
                push(label.clone(), s(&item["masterUrl"]));
                if let Some(b) = item["backupUrls"].as_array() {
                    for (i, u) in b.iter().enumerate() {
                        push(format!("{label} (backup {})", i + 1), s(u));
                    }
                }
            }
        }
    }
    out
}

fn collect_images(note: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(arr) = note["imageList"].as_array() {
        for img in arr {
            let mut u = s(&img["urlDefault"]);
            if u.is_empty() {
                u = s(&img["infoList"][0]["url"]);
            }
            if u.is_empty() {
                u = s(&img["url"]);
            }
            let u = fix_url(&u);
            if !u.is_empty() && !out.contains(&u) {
                out.push(u);
            }
        }
    }
    out
}

// ---------------------------------------------------------------- fetching

fn build_client(ua: &str, mobile: bool) -> Result<Client, String> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "Accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
            .parse()
            .unwrap(),
    );
    headers.insert(
        "Accept-Language",
        "zh-CN,zh;q=0.9,en;q=0.8".parse().unwrap(),
    );
    headers.insert("Referer", "https://www.xiaohongshu.com/".parse().unwrap());
    if !mobile {
        headers.insert(
            "sec-ch-ua",
            r#""Chromium";v="132", "Not_A Brand";v="99""#.parse().unwrap(),
        );
        headers.insert("sec-ch-ua-mobile", "?0".parse().unwrap());
        headers.insert("sec-ch-ua-platform", "\"Windows\"".parse().unwrap());
    }

    Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent(ua)
        .brotli(true)
        .gzip(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Build client error: {}", e))
}

async fn fetch_html(client: &Client, url: &str) -> Result<(String, String), String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", e))?;
    let final_url = resp.url().to_string();
    let html = resp
        .text()
        .await
        .map_err(|e| format!("Read HTML failed: {}", e))?;
    Ok((html, final_url))
}

fn parse_page(html: &str, final_url: &str) -> RednoteResult {
    let state = extract_initial_state(html);
    let note = state.as_ref().and_then(find_note);

    if state.is_none() {
        eprintln!("[scraper] __INITIAL_STATE__ not found (url: {final_url})");
    } else if note.is_none() {
        eprintln!("[scraper] note object not found in state (url: {final_url})");
    }

    // ---- text fields
    let title_tag = {
        let t = capture_single(html, r#"(?is)<title>(.*?)</title>"#);
        html_unescape(&t)
    };
    let mut title = note.map(|n| s(&n["title"])).unwrap_or_default();
    if title.is_empty() {
        title = meta_first(html, "og:title");
    }
    if title.is_empty() {
        title = title_tag;
    }

    let mut desc = note.map(|n| s(&n["desc"])).unwrap_or_default();
    if desc.is_empty() {
        desc = meta_first(html, "description");
    }
    if desc.is_empty() {
        desc = meta_first(html, "og:description");
    }

    let keywords = meta_first(html, "keywords");

    let mut nickname = note.map(|n| s(&n["user"]["nickname"])).unwrap_or_default();
    if nickname.is_empty() {
        nickname = meta_first(html, "og:title")
            .split(" - ")
            .next()
            .unwrap_or("")
            .to_string();
    }

    let mut note_id = note.map(|n| s(&n["noteId"])).unwrap_or_default();
    if note_id.is_empty() {
        let og_url = meta_first(html, "og:url");
        let src = if og_url.is_empty() { final_url.to_string() } else { og_url };
        note_id = src
            .split('?')
            .next()
            .unwrap_or("")
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string();
    }

    // ---- duration
    let mut duration = note
        .map(|n| s(&n["video"]["capa"]["duration"]))
        .unwrap_or_default();
    if duration.is_empty() {
        duration = meta_first(html, "og:videotime");
    }

    // ---- engagement
    let (mut likes, mut comments, mut collects) = match note {
        Some(n) => (
            s(&n["interactInfo"]["likedCount"]),
            s(&n["interactInfo"]["commentCount"]),
            s(&n["interactInfo"]["collectedCount"]),
        ),
        None => Default::default(),
    };
    if likes.is_empty() {
        likes = meta_first(html, "og:xhs:note_like");
    }
    if comments.is_empty() {
        comments = meta_first(html, "og:xhs:note_comment");
    }
    if collects.is_empty() {
        collects = meta_first(html, "og:xhs:note_collect");
    }

    // ---- images
    let mut images = note.map(collect_images).unwrap_or_default();
    if images.is_empty() {
        images = meta_all(html, "og:image").iter().map(|u| fix_url(u)).collect();
    }

    // ---- downloads
    let mut downloads = note.map(collect_downloads).unwrap_or_default();

    // Fallback 1: thẻ meta og:video
    if downloads.is_empty() {
        for (i, u) in meta_all(html, "og:video").iter().enumerate() {
            let u = fix_url(u);
            if !downloads.iter().any(|d| d.url == u) {
                downloads.push(RednoteDownload {
                    quality: if i == 0 { "Original".into() } else { format!("Alt {}", i + 1) },
                    url: u,
                });
            }
        }
    }

    // Fallback 2: quét thô masterUrl / originVideoKey trong HTML
    if downloads.is_empty() {
        let re = Regex::new(r#""masterUrl"\s*:\s*"(.*?)""#).unwrap();
        for c in re.captures_iter(html) {
            let u = fix_url(&c[1]);
            if !u.is_empty() && !downloads.iter().any(|d| d.url == u) {
                downloads.push(RednoteDownload { quality: "Video".into(), url: u });
            }
        }
        let key = capture_single(html, r#""originVideoKey"\s*:\s*"(.*?)""#);
        if !key.is_empty() {
            downloads.insert(
                0,
                RednoteDownload {
                    quality: "Original".into(),
                    url: format!("https://sns-video-bd.xhscdn.com/{key}"),
                },
            );
        }
    }

    RednoteResult {
        note_id,
        nickname,
        title,
        desc,
        keywords,
        duration,
        engagement: RednoteEngagement { likes, comments, collects },
        images,
        downloads,
    }
}

pub async fn scrape(url: String) -> Result<RednoteResult, String> {
    let attempts = [(UA_DESKTOP, false), (UA_MOBILE, true)];
    let mut best: Option<RednoteResult> = None;
    let mut last_err = String::new();

    for (ua, mobile) in attempts {
        let client = match build_client(ua, mobile) {
            Ok(c) => c,
            Err(e) => {
                last_err = e;
                continue;
            }
        };
        match fetch_html(&client, &url).await {
            Ok((html, final_url)) => {
                let res = parse_page(&html, &final_url);
                if !res.downloads.is_empty() {
                    return Ok(res);
                }
                // giữ kết quả giàu thông tin nhất để trả về nếu mọi lần thử đều không có video
                let score = |r: &RednoteResult| {
                    r.images.len() + usize::from(!r.desc.is_empty()) + usize::from(!r.nickname.is_empty())
                };
                if best.as_ref().map_or(true, |b| score(&res) > score(b)) {
                    best = Some(res);
                }
            }
            Err(e) => last_err = e,
        }
    }

    best.ok_or(last_err)
}

pub fn is_valid_rednote_url(url: &str) -> bool {
    Regex::new(r"(?i)^https?://(.*?)?(xhslink\.com|xiaohongshu\.com)/[^\s]+$")
        .unwrap()
        .is_match(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_web_state() {
        let html = r#"<html><head><title>x - 小红书</title></head><body><script>window.__INITIAL_STATE__={"note":{"firstNoteId":"abc","noteDetailMap":{"abc":{"note":{"noteId":"abc","title":"T","desc":"D #tag","user":{"nickname":"nick"},"interactInfo":{"likedCount":"10","commentCount":"2","collectedCount":"3"},"imageList":[{"urlDefault":"http://img/1.jpg"}],"video":{"capa":{"duration":33},"consumer":{"originVideoKey":"pre_post/xyz"},"media":{"stream":{"h264":[{"height":1080,"masterUrl":"http:\u002F\u002Fsns-video-bd.xhscdn.com\u002Fa.mp4","backupUrls":["http://b/a.mp4"]}],"h265":[]}}}}}},"x":undefined}};</script></body></html>"#;
        let r = parse_page(html, "https://www.xiaohongshu.com/discovery/item/abc");
        assert_eq!(r.note_id, "abc");
        assert_eq!(r.nickname, "nick");
        assert_eq!(r.engagement.likes, "10");
        assert_eq!(r.duration, "33");
        assert_eq!(r.downloads[0].url, "https://sns-video-bd.xhscdn.com/pre_post/xyz");
        assert!(r.downloads.iter().any(|d| d.url == "https://sns-video-bd.xhscdn.com/a.mp4"));
    }

    #[test]
    fn falls_back_to_meta() {
        let html = r#"<meta name="og:video" content="http://v/x.mp4?a=1&amp;b=2"><meta name="description" content="hi">"#;
        let r = parse_page(html, "https://www.xiaohongshu.com/explore/zzz");
        assert_eq!(r.downloads[0].url, "https://v/x.mp4?a=1&b=2");
        assert_eq!(r.note_id, "zzz");
    }
}
