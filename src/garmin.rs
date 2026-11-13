//! Garmin Connect through Garmin's Personal API: the athlete creates a personal
//! API key in Garmin Connect (it expires after 30 to 365 days), which is
//! exchanged for a 24 h bearer token. Activities are listed with the search
//! endpoint, and each one is downloaded as its original `.fit` file.
//!
//! Garmin has no cheap "after this date" filter on big accounts, so the search
//! runs over date windows that are split in two whenever a page comes back full
//! or Garmin answers "too large".

use chrono::{Duration, NaiveDate, NaiveDateTime, Utc};
use cyclai_plugin_sdk::{ListedActivity, PluginError};
use serde::Deserialize;
use std::io::Read;
use std::sync::Mutex;
use std::time::Instant;

const API_BASE: &str = "https://personal.connectapi.garmin.com";
const TOKEN_URL: &str = "https://personal.connectapi.garmin.com/personalapitoken-service/personal-api/personal-api-key/token";
const PAGE_LIMIT: usize = 20;
/// A full-history walk stops after about two years without any activity.
const EMPTY_WINDOWS_BEFORE_STOP: u32 = 24;
const WINDOW_DAYS: i64 = 30;
const MAX_SPLIT_STEPS: u32 = 500;
const MAX_DAY_OFFSET: usize = 400;
/// Garmin tokens last 24 h; renewed a bit before.
const TOKEN_TTL_SECONDS: u64 = 23 * 3600;

pub struct Garmin {
    http: reqwest::Client,
    api_key: String,
    token: Mutex<Option<(String, Instant)>>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct RawActivity {
    activity_id: i64,
    activity_name: Option<String>,
    #[serde(rename = "startTimeGMT")]
    start_time_gmt: Option<String>,
    activity_type: Option<RawType>,
    #[serde(default)]
    manual_activity: bool,
    duration: Option<f64>,
    elapsed_duration: Option<f64>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct RawType {
    type_key: String,
}

enum PageError {
    TooLarge,
    Other(PluginError),
}

/// CyclAI's sports from Garmin's activity type keys; everything else
/// (gravel, mountain biking, swimming...) is left out.
pub fn map_sport(type_key: &str) -> Option<&'static str> {
    match type_key {
        "cycling" | "road_biking" => Some("Ride"),
        "gravel_cycling" => Some("GravelRide"),
        "indoor_cycling" | "virtual_ride" => Some("VirtualRide"),
        "running" | "treadmill_running" | "track_running" => Some("Run"),
        "trail_running" | "ultra_run" => Some("TrailRun"),
        _ => None,
    }
}

/// Garmin's `startTimeGMT` ("2026-11-10 08:00:00" or ISO, optionally with
/// fractional seconds), as UTC epoch seconds.
pub fn parse_gmt(s: &str) -> Option<i64> {
    ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(s.trim().trim_end_matches('Z'), f).ok())
        .map(|d| d.and_utc().timestamp())
}

fn rfc3339(epoch: i64) -> String {
    chrono::DateTime::from_timestamp(epoch, 0).unwrap_or_default().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn unreachable(e: reqwest::Error) -> PluginError {
    PluginError::upstream(format!("Garmin injoignable : {}", e.without_url()))
}

impl Garmin {
    pub fn new(api_key: String) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(60)).build()?;
        Ok(Self { http, api_key: api_key.trim().to_string(), token: Mutex::new(None) })
    }

    pub fn is_configured(&self) -> bool {
        !self.api_key.is_empty()
    }

    async fn authenticate(&self) -> Result<String, PluginError> {
        let res = self
            .http
            .post(TOKEN_URL)
            .header("Authorization", format!("ApiKey {}", self.api_key))
            .header("Accept", "*/*")
            .send()
            .await
            .map_err(unreachable)?;
        let status = res.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(PluginError::upstream("clé API Garmin refusée (expirée ou révoquée)"));
        }
        if !status.is_success() {
            return Err(PluginError::upstream(format!("authentification Garmin impossible (HTTP {status})")));
        }
        // The body is a raw JWT, not JSON.
        let body = res.text().await.map_err(unreachable)?;
        let token = body.trim().trim_matches('"').to_string();
        if token.split('.').count() != 3 {
            return Err(PluginError::upstream("réponse d'authentification Garmin inattendue"));
        }
        Ok(token)
    }

    async fn token(&self, force: bool) -> Result<String, PluginError> {
        if !force && let Some((t, at)) = self.token.lock().unwrap().as_ref() && at.elapsed().as_secs() < TOKEN_TTL_SECONDS {
            return Ok(t.clone());
        }
        let t = self.authenticate().await?;
        *self.token.lock().unwrap() = Some((t.clone(), Instant::now()));
        Ok(t)
    }

    /// GET with a bearer token: renews it once if Garmin refuses it, and waits
    /// out the rate limit (HTTP 429) a few times before giving up.
    async fn send_get(&self, url: &str, query: &[(&str, String)], accept: &str) -> Result<reqwest::Response, PluginError> {
        let mut wait = 2u64;
        let mut renewed = false;
        let mut retry_after = None;
        for attempt in 0..6 {
            let token = self.token(false).await?;
            let res = self.http.get(url).bearer_auth(token).header("Accept", accept).query(query).send().await.map_err(unreachable)?;
            match res.status() {
                reqwest::StatusCode::UNAUTHORIZED if !renewed => {
                    renewed = true;
                    self.token(true).await?;
                }
                reqwest::StatusCode::TOO_MANY_REQUESTS => {
                    retry_after = res.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
                    if attempt == 5 {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(retry_after.unwrap_or(wait).min(60))).await;
                    wait = (wait * 2).min(60);
                }
                _ => return Ok(res),
            }
        }
        Err(PluginError::rate_limited("Garmin limite le nombre de requêtes", retry_after))
    }

    async fn search_page(&self, from: NaiveDate, to: NaiveDate, offset: usize) -> Result<Vec<RawActivity>, PageError> {
        let query = [("start", offset.to_string()), ("limit", PAGE_LIMIT.to_string()), ("startDate", from.to_string()), ("endDate", to.to_string())];
        let res = self
            .send_get(&format!("{API_BASE}/activitylist-service/activities/search/activities"), &query, "application/json")
            .await
            .map_err(PageError::Other)?;
        let status = res.status();
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(Vec::new());
        }
        if status == reqwest::StatusCode::INTERNAL_SERVER_ERROR {
            return Err(PageError::TooLarge);
        }
        let body = res.text().await.map_err(|e| PageError::Other(unreachable(e)))?;
        if !status.is_success() {
            let excerpt: String = body.chars().take(180).collect();
            return Err(PageError::Other(PluginError::upstream(format!("Garmin a répondu HTTP {status} : {excerpt}"))));
        }
        if body.trim().is_empty() {
            return Ok(Vec::new());
        }
        serde_json::from_str(&body).map_err(|e| PageError::Other(PluginError::upstream(format!("réponse Garmin illisible : {e}"))))
    }

    /// Every activity between two dates (inclusive): a window is split whenever
    /// it holds more than a page.
    async fn search_range(&self, from: NaiveDate, to: NaiveDate) -> Result<Vec<RawActivity>, PluginError> {
        let mut out = Vec::new();
        let mut stack = vec![(from, to)];
        let mut steps = 0;
        while let Some((f, t)) = stack.pop() {
            steps += 1;
            if steps > MAX_SPLIT_STEPS {
                return Err(PluginError::upstream("trop de requêtes Garmin pour cette période"));
            }
            match self.search_page(f, t, 0).await {
                Ok(page) if page.len() < PAGE_LIMIT => out.extend(page),
                Ok(page) if f == t => {
                    out.extend(page);
                    out.extend(self.search_day_paged(f).await?);
                }
                Err(PageError::TooLarge) if f == t => {
                    return Err(PluginError::upstream(format!("Garmin refuse de lister le {f} (réponse trop volumineuse)")));
                }
                Ok(_) | Err(PageError::TooLarge) => {
                    let mid = f + Duration::days((t - f).num_days() / 2);
                    stack.push((mid + Duration::days(1), t));
                    stack.push((f, mid));
                }
                Err(PageError::Other(e)) => return Err(e),
            }
        }
        Ok(out)
    }

    /// A single day holding more than a page: the rest, page by page.
    async fn search_day_paged(&self, day: NaiveDate) -> Result<Vec<RawActivity>, PluginError> {
        let mut out = Vec::new();
        let mut offset = PAGE_LIMIT;
        while offset <= MAX_DAY_OFFSET {
            let page = match self.search_page(day, day, offset).await {
                Ok(p) => p,
                Err(PageError::TooLarge) => break,
                Err(PageError::Other(e)) => return Err(e),
            };
            let full = page.len() == PAGE_LIMIT;
            out.extend(page);
            if !full {
                break;
            }
            offset += PAGE_LIMIT;
        }
        Ok(out)
    }

    /// Activities started after `after_epoch` (0 = the whole history).
    pub async fn list(&self, after_epoch: i64) -> Result<Vec<ListedActivity>, PluginError> {
        let today = Utc::now().date_naive();
        let mut raw: Vec<RawActivity> = Vec::new();
        if after_epoch > 0 {
            let mut from = chrono::DateTime::from_timestamp(after_epoch, 0).map(|d| d.date_naive()).unwrap_or(today);
            while from <= today {
                let to = (from + Duration::days(WINDOW_DAYS - 1)).min(today);
                raw.extend(self.search_range(from, to).await?);
                from = to + Duration::days(1);
            }
        } else {
            let oldest = NaiveDate::from_ymd_opt(2007, 1, 1).unwrap();
            let (mut to, mut empty) = (today, 0u32);
            while to >= oldest && empty < EMPTY_WINDOWS_BEFORE_STOP {
                let from = (to - Duration::days(WINDOW_DAYS - 1)).max(oldest);
                let found = self.search_range(from, to).await?;
                empty = if found.is_empty() { empty + 1 } else { 0 };
                raw.extend(found);
                to = from - Duration::days(1);
            }
        }
        Ok(to_listed(raw, after_epoch))
    }

    /// The original `.fit` of an activity; `None` when Garmin has no file for it.
    pub async fn download_fit(&self, activity_id: &str) -> Result<Option<Vec<u8>>, PluginError> {
        if activity_id.is_empty() || !activity_id.chars().all(|c| c.is_ascii_digit()) {
            return Err(PluginError::new(400, "identifiant d'activité invalide"));
        }
        let res = self
            .send_get(&format!("{API_BASE}/download-service/files/activity/{activity_id}"), &[], "*/*")
            .await?;
        let status = res.status();
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(PluginError::upstream(format!("téléchargement Garmin refusé (HTTP {status})")));
        }
        let bytes = res.bytes().await.map_err(unreachable)?.to_vec();
        extract_fit(bytes).map_err(|e| PluginError::upstream(format!("{e:#}")))
    }
}

fn to_listed(raw: Vec<RawActivity>, after_epoch: i64) -> Vec<ListedActivity> {
    let mut listed: Vec<ListedActivity> = raw
        .into_iter()
        .filter(|a| !a.manual_activity)
        .filter_map(|a| {
            let sport = map_sport(&a.activity_type.as_ref()?.type_key)?;
            let epoch = a.start_time_gmt.as_deref().and_then(parse_gmt)?;
            let duration_s = a.duration.or(a.elapsed_duration).map(|d| d.round() as i64).filter(|d| *d > 0);
            Some(ListedActivity {
                external_id: a.activity_id.to_string(),
                title: a.activity_name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Sortie".to_string()),
                started_at: rfc3339(epoch),
                start_epoch: epoch,
                duration_s,
                sport_type: sport.to_string(),
            })
        })
        .filter(|l| after_epoch <= 0 || l.start_epoch >= after_epoch)
        .collect();
    listed.sort_by(|a, b| a.start_epoch.cmp(&b.start_epoch).then_with(|| a.external_id.cmp(&b.external_id)));
    listed.dedup_by(|a, b| a.external_id == b.external_id);
    listed
}

/// The ZIP Garmin returns holds the original `.fit`; a bare FIT is accepted too.
/// A file counts as FIT by its name or by its header (`..FIT` at offset 8), and
/// `None` means the archive has no FIT file at all (an activity without a
/// device file, e.g. entered by hand or imported from elsewhere).
fn extract_fit(bytes: Vec<u8>) -> anyhow::Result<Option<Vec<u8>>> {
    let is_fit = |b: &[u8]| b.len() > 12 && &b[8..12] == b".FIT";
    if !bytes.starts_with(b"PK") {
        return Ok(if is_fit(&bytes) { Some(bytes) } else { None });
    }
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let mut names = Vec::new();
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let name = file.name().to_string();
        let mut out = Vec::new();
        file.read_to_end(&mut out)?;
        if name.to_lowercase().ends_with(".fit") || is_fit(&out) {
            return Ok(Some(out));
        }
        names.push(name);
    }
    eprintln!("no FIT file in the Garmin archive (entries: {names:?})");
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sports_are_mapped_and_others_left_out() {
        assert_eq!(map_sport("road_biking"), Some("Ride"));
        assert_eq!(map_sport("indoor_cycling"), Some("VirtualRide"));
        assert_eq!(map_sport("treadmill_running"), Some("Run"));
        assert_eq!(map_sport("trail_running"), Some("TrailRun"));
        assert_eq!(map_sport("gravel_cycling"), Some("GravelRide"));
        assert_eq!(map_sport("mountain_biking"), None);
        assert_eq!(map_sport("swimming"), None);
    }

    #[test]
    fn garmin_timestamps_are_read_as_utc() {
        let want = parse_gmt("2026-11-10 08:00:00").unwrap();
        assert_eq!(parse_gmt("2026-11-10T08:00:00"), Some(want));
        assert_eq!(parse_gmt("2026-11-10 08:00:00.0"), Some(want));
        assert_eq!(parse_gmt("nope"), None);
        assert_eq!(rfc3339(want), "2026-11-10T08:00:00Z");
    }

    #[test]
    fn the_listing_keeps_supported_sports_sorted_and_without_duplicates() {
        let raw: Vec<RawActivity> = serde_json::from_str(
            r#"[
              {"activityId": 2, "activityName": "B", "startTimeGMT": "2026-11-10 09:00:00", "activityType": {"typeKey": "running"}, "duration": 1800.4},
              {"activityId": 1, "activityName": "", "startTimeGMT": "2026-11-10 08:00:00", "activityType": {"typeKey": "road_biking"}, "elapsedDuration": 3600.0},
              {"activityId": 1, "activityName": "dup", "startTimeGMT": "2026-11-10 08:00:00", "activityType": {"typeKey": "road_biking"}},
              {"activityId": 3, "startTimeGMT": "2026-11-10 10:00:00", "activityType": {"typeKey": "swimming"}},
              {"activityId": 4, "startTimeGMT": "2026-11-10 11:00:00", "activityType": {"typeKey": "cycling"}, "manualActivity": true},
              {"activityId": 5, "startTimeGMT": "bad", "activityType": {"typeKey": "cycling"}}
            ]"#,
        )
        .unwrap();
        let listed = to_listed(raw, 0);
        assert_eq!(listed.iter().map(|l| l.external_id.as_str()).collect::<Vec<_>>(), ["1", "2"]);
        assert_eq!(listed[0].title, "Sortie", "a blank name gets a default");
        assert_eq!(listed[0].duration_s, Some(3600), "elapsed duration when there is no moving duration");
        assert_eq!(listed[1].duration_s, Some(1800));
        assert_eq!(listed[1].sport_type, "Run");
        // Only what started after the marker.
        let after = parse_gmt("2026-11-10 08:30:00").unwrap();
        let raw2: Vec<RawActivity> = serde_json::from_str(
            r#"[{"activityId": 1, "startTimeGMT": "2026-11-10 08:00:00", "activityType": {"typeKey": "cycling"}},
                {"activityId": 2, "startTimeGMT": "2026-11-10 09:00:00", "activityType": {"typeKey": "cycling"}}]"#,
        )
        .unwrap();
        assert_eq!(to_listed(raw2, after).len(), 1);
    }

    #[test]
    fn the_fit_is_taken_from_the_archive_or_passed_through() {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("readme.txt", opts).unwrap();
            zip.write_all(b"hello").unwrap();
            zip.start_file("12345_ACTIVITY.FIT", opts).unwrap();
            zip.write_all(b"FITDATA").unwrap();
            zip.finish().unwrap();
        }
        assert_eq!(extract_fit(buf.into_inner()).unwrap().unwrap(), b"FITDATA");
        let bare = b"      .FITxxxx".to_vec();
        assert_eq!(extract_fit(bare.clone()).unwrap(), Some(bare));
        assert_eq!(extract_fit(b"<html>".to_vec()).unwrap(), None);
        let mut empty = std::io::Cursor::new(Vec::new());
        zip::ZipWriter::new(&mut empty).finish().unwrap();
        assert_eq!(extract_fit(empty.into_inner()).unwrap(), None);
    }
}
