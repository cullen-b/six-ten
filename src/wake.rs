//! Sleeping until a set time or a provider's usage reset, so agents start work in a fresh usage window.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

use crate::store::now;

/// Waits longer than this are refused: harness tool timeouts (Claude Code's is about 28h) end them first.
const MAX_WAIT: u64 = 24 * 60 * 60;
/// Margin after a reset so the first request lands in the new window.
const SLACK: u64 = 60;

/// One rate-limit window: percent used and when it resets (Unix seconds).
pub struct Window {
    pub name: String,
    pub used: f64,
    pub resets_at: u64,
    pub minutes: u64,
}

pub struct Usage {
    pub windows: Vec<Window>,
    /// When the provider last reported these numbers.
    pub as_of: u64,
}

pub const PROVIDERS: [&str; 2] = ["claude", "codex"];

/// Latest usage the provider's own CLI cached on this machine; never makes a network call.
pub fn usage(provider: &str) -> Option<Usage> {
    match provider {
        "claude" => claude_usage(),
        "codex" => codex_usage(),
        _ => None,
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Claude Code keeps `cachedUsageUtilization` in `~/.claude.json`, refreshed as it makes requests.
fn claude_usage() -> Option<Usage> {
    let path = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(home)?
        .join(".claude.json");
    let doc: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    let cached = &doc["cachedUsageUtilization"];
    let windows = cached["utilization"]
        .as_object()?
        .iter()
        .filter_map(|(name, w)| {
            let minutes = match name.as_str() {
                "five_hour" => 300,
                n if n.starts_with("seven_day") => 7 * 24 * 60,
                _ => return None,
            };
            Some(Window {
                name: match name.strip_prefix("seven_day_") {
                    Some(model) => format!("7d {model}"),
                    None => span_name(minutes),
                },
                used: w["utilization"].as_f64()?,
                resets_at: parse_rfc3339(w["resets_at"].as_str()?)?,
                minutes,
            })
        })
        .collect();
    Some(Usage {
        windows,
        as_of: cached["fetchedAtMs"].as_u64()? / 1000,
    })
}

/// Codex logs `rate_limits` in its session rollouts (`~/.codex/sessions/YYYY/MM/DD/*.jsonl`).
fn codex_usage() -> Option<Usage> {
    let sessions = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".codex")))?
        .join("sessions");
    let sorted = |dir: PathBuf| -> Vec<PathBuf> {
        let mut entries: Vec<PathBuf> = fs::read_dir(dir)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        entries.sort();
        entries.reverse();
        entries
    };
    let days = sorted(sessions)
        .into_iter()
        .flat_map(sorted)
        .flat_map(sorted)
        .take(7);
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = days
        .flat_map(sorted)
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .filter_map(|p| Some((fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    files.sort();
    files.into_iter().rev().take(20).find_map(|(_, path)| {
        let text = fs::read_to_string(path).ok()?;
        text.lines()
            .rev()
            .filter(|l| l.contains("\"rate_limits\""))
            .find_map(codex_line)
    })
}

fn codex_line(line: &str) -> Option<Usage> {
    let event: Value = serde_json::from_str(line).ok()?;
    let limits = &event["payload"]["rate_limits"];
    let windows: Vec<Window> = ["primary", "secondary"]
        .iter()
        .filter_map(|k| {
            let w = &limits[*k];
            let minutes = w["window_minutes"].as_u64()?;
            Some(Window {
                name: span_name(minutes),
                used: w["used_percent"].as_f64()?,
                resets_at: w["resets_at"].as_u64()?,
                minutes,
            })
        })
        .collect();
    (!windows.is_empty()).then(|| Usage {
        windows,
        as_of: event["timestamp"]
            .as_str()
            .and_then(parse_rfc3339)
            .unwrap_or(0),
    })
}

/// When work can start again: the latest reset of any exhausted window, else the shortest window's
/// reset. `None` when every window has already reset.
pub fn reset_time(usage: &Usage, now: u64) -> Option<u64> {
    let live: Vec<&Window> = usage.windows.iter().filter(|w| w.resets_at > now).collect();
    live.iter()
        .filter(|w| w.used >= 100.0)
        .map(|w| w.resets_at)
        .max()
        .or_else(|| live.iter().min_by_key(|w| w.minutes).map(|w| w.resets_at))
}

pub struct Plan {
    pub until: u64,
    /// Why the agent is woken, completing "it is 2:20am; …".
    pub why: String,
}

/// Works out when to wake from `at` (clock time, RFC 3339 or Unix seconds), `minutes`, or else the
/// usage reset of `provider` (default: the agent's harness).
pub fn plan(
    harness: &str,
    at: Option<&str>,
    minutes: Option<f64>,
    provider: Option<&str>,
) -> Result<Plan, String> {
    let now = now();
    let plan = if let Some(at) = at {
        Plan {
            until: parse_at(at, now, local_offset())?,
            why: "this is the time you asked to be woken".into(),
        }
    } else if let Some(m) = minutes {
        if !m.is_finite() || m < 0.0 {
            return Err("minutes must be zero or more".into());
        }
        Plan {
            until: now + (m * 60.0).round() as u64,
            why: "this is the time you asked to be woken".into(),
        }
    } else {
        let provider = provider.unwrap_or(harness);
        let usage = usage(provider).ok_or_else(|| {
            format!(
                "six-ten has no usage data for {provider} (it reads what Claude Code and Codex cache locally). \
                 Ask the user when their usage limit resets, then call again with `at` (e.g. \"2:30am\")."
            )
        })?;
        let until = reset_time(&usage, now).map_or(now, |t| t + SLACK);
        Plan {
            until,
            why: format!("your {provider} usage window has reset"),
        }
    };
    if plan.until > now + MAX_WAIT {
        return Err(format!(
            "{} is more than 24h away; pick a time within a day",
            clock(plan.until, now, local_offset())
        ));
    }
    Ok(plan)
}

pub fn reminder(plan: &Plan, task: Option<&str>) -> String {
    let task = task
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map_or(String::new(), |t| format!(": {t}"));
    format!(
        "six-ten wake-up: it is {}; {}. Start working now{task}",
        clock(now(), now(), local_offset()),
        plan.why
    )
}

/// `six-ten usage`: what each provider last reported.
pub fn usage_text() -> String {
    let now = now();
    let offset = local_offset();
    let mut out = String::new();
    for p in PROVIDERS {
        match usage(p) {
            None => out.push_str(&format!("{p:<7} no local usage data\n")),
            Some(u) => {
                let windows: Vec<String> = u
                    .windows
                    .iter()
                    .map(|w| {
                        if w.resets_at > now {
                            format!(
                                "{} {:.0}% used, resets {}",
                                w.name,
                                w.used,
                                clock(w.resets_at, now, offset)
                            )
                        } else {
                            format!("{} reset at {}", w.name, clock(w.resets_at, now, offset))
                        }
                    })
                    .collect();
                out.push_str(&format!(
                    "{p:<7} {}  (reported {} ago)\n",
                    windows.join(" · "),
                    span(now.saturating_sub(u.as_of))
                ));
            }
        }
    }
    out
}

/// Window length as a label: "5h", "7d".
fn span_name(minutes: u64) -> String {
    match minutes {
        m if m % 1440 == 0 => format!("{}d", m / 1440),
        m if m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

/// "2h04m", "45m", "30s".
pub fn span(secs: u64) -> String {
    match secs {
        s if s >= 3600 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// Local UTC offset in seconds, from `date +%z` (no timezone database needed).
pub fn local_offset() -> i64 {
    Command::new("date")
        .arg("+%z")
        .output()
        .ok()
        .and_then(|o| parse_offset(String::from_utf8_lossy(&o.stdout).trim()))
        .unwrap_or(0)
}

/// "-0500" or "+05:30" → seconds east of UTC.
fn parse_offset(s: &str) -> Option<i64> {
    let sign = match s.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits: String = s[1..].chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 {
        return None;
    }
    let h: i64 = digits[..2].parse().ok()?;
    let m: i64 = digits[2..].parse().ok()?;
    Some(sign * (h * 3600 + m * 60))
}

/// "2am", "2:30 am", "14:05" (next occurrence, local time), RFC 3339, or Unix seconds.
pub fn parse_at(s: &str, now: u64, offset: i64) -> Result<u64, String> {
    let s = s.trim();
    let bad = || {
        format!(
            "can't read the time `{s}`; use e.g. \"2:30am\", \"14:05\", RFC 3339 or Unix seconds"
        )
    };
    if s.len() >= 9 && s.bytes().all(|b| b.is_ascii_digit()) {
        return s.parse().map_err(|_| bad());
    }
    if s.contains('T') {
        return parse_rfc3339(s).ok_or_else(bad);
    }
    let lower = s.to_ascii_lowercase().replace(' ', "");
    let (body, pm) = match lower.strip_suffix("pm") {
        Some(b) => (b, Some(true)),
        None => match lower.strip_suffix("am") {
            Some(b) => (b, Some(false)),
            None => (lower.as_str(), None),
        },
    };
    let (h, m) = body.split_once(':').unwrap_or((body, "0"));
    let (mut h, m): (u64, u64) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    match pm {
        Some(_) if !(1..=12).contains(&h) => return Err(bad()),
        Some(pm) => h = h % 12 + if pm { 12 } else { 0 },
        None if h > 23 => return Err(bad()),
        None => {}
    }
    if m > 59 {
        return Err(bad());
    }
    let local_now = now as i64 + offset;
    let midnight = local_now - local_now.rem_euclid(86400);
    let mut at = midnight + (h * 3600 + m * 60) as i64 - offset;
    if at <= now as i64 {
        at += 86400;
    }
    Ok(at as u64)
}

/// "2026-10-16T07:19:59.611904+00:00" or "…Z" → Unix seconds.
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let (date, rest) = s.split_once(['T', 't', ' '])?;
    let mut d = date.splitn(3, '-').map(str::parse::<i64>);
    let (y, mo, da) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let tz_at = rest.find(['Z', 'z', '+', '-']).unwrap_or(rest.len());
    let (time, tz) = rest.split_at(tz_at);
    let mut t = time
        .split('.')
        .next()?
        .splitn(3, ':')
        .map(str::parse::<i64>);
    let (h, mi) = (t.next()?.ok()?, t.next()?.ok()?);
    let sec = t.next().transpose().ok()?.unwrap_or(0);
    let offset = match tz {
        "" | "Z" | "z" => 0,
        tz => parse_offset(tz)?,
    };
    let secs = days_from_civil(y, mo, da) * 86400 + h * 3600 + mi * 60 + sec - offset;
    u64::try_from(secs).ok()
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

/// Local clock time, "2:19am"; with the date when it isn't within the next or last 20 hours.
pub fn clock(at: u64, now: u64, offset: i64) -> String {
    let local = at as i64 + offset;
    let secs = local.rem_euclid(86400);
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    let time = format!(
        "{}:{m:02}{}",
        if h % 12 == 0 { 12 } else { h % 12 },
        if h < 12 { "am" } else { "pm" }
    );
    if at.abs_diff(now) > 20 * 3600 {
        let (y, mo, d) = civil_from_days(local.div_euclid(86400));
        format!("{y}-{mo:02}-{d:02} {time}")
    } else {
        time
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-10-16T05:15:00Z, i.e. 12:15am in UTC-5.
    const NOW: u64 = 1792127700;
    const CDT: i64 = -5 * 3600;

    #[test]
    fn rfc3339() {
        assert_eq!(parse_rfc3339("2026-10-16T05:15:00Z"), Some(NOW));
        assert_eq!(parse_rfc3339("2026-10-16T00:15:00.5-05:00"), Some(NOW));
        assert_eq!(
            parse_rfc3339("2026-10-16T07:19:59.611904+00:00"),
            Some(NOW + 2 * 3600 + 4 * 60 + 59)
        );
        assert_eq!(parse_rfc3339("yesterday"), None);
    }

    #[test]
    fn clock_times_are_the_next_occurrence() {
        assert_eq!(parse_at("2am", NOW, CDT), Ok(NOW + 105 * 60));
        assert_eq!(parse_at("2:30 AM", NOW, CDT), Ok(NOW + 135 * 60));
        assert_eq!(parse_at("00:10", NOW, CDT), Ok(NOW + 86400 - 5 * 60));
        assert_eq!(parse_at("12am", NOW, CDT), Ok(NOW + 86400 - 15 * 60));
        assert_eq!(parse_at("1pm", NOW, CDT), Ok(NOW + 765 * 60));
        assert_eq!(parse_at("1792127760", NOW, CDT), Ok(NOW + 60));
        assert!(parse_at("13pm", NOW, CDT).is_err());
        assert!(parse_at("soon", NOW, CDT).is_err());
    }

    #[test]
    fn clock_text() {
        assert_eq!(clock(NOW, NOW, CDT), "12:15am");
        assert_eq!(clock(NOW + 13 * 3600, NOW, CDT), "1:15pm");
        assert_eq!(clock(NOW + 30 * 3600, NOW, CDT), "2026-10-17 6:15am");
        assert_eq!(parse_offset("+05:30"), Some(19800));
    }

    fn window(minutes: u64, used: f64, resets_at: u64) -> Window {
        Window {
            name: String::new(),
            used,
            resets_at,
            minutes,
        }
    }

    #[test]
    fn reset_prefers_exhausted_windows() {
        let usage = |windows| Usage {
            windows,
            as_of: NOW,
        };
        // Within limits: the 5-hour window decides.
        let u = usage(vec![
            window(10080, 40.0, NOW + 9000),
            window(300, 84.0, NOW + 600),
        ]);
        assert_eq!(reset_time(&u, NOW), Some(NOW + 600));
        // Weekly limit spent: the 5-hour reset doesn't help.
        let u = usage(vec![
            window(10080, 100.0, NOW + 9000),
            window(300, 84.0, NOW + 600),
        ]);
        assert_eq!(reset_time(&u, NOW), Some(NOW + 9000));
        // Everything already reset.
        let u = usage(vec![window(300, 100.0, NOW - 1)]);
        assert_eq!(reset_time(&u, NOW), None);
    }

    #[test]
    fn codex_rollout_line() {
        let line = r#"{"timestamp":"2026-10-16T05:15:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":12.0,"window_minutes":300,"resets_at":1792130000},"secondary":{"used_percent":3.0,"window_minutes":10080,"resets_at":1792600000}}}}"#;
        let u = codex_line(line).unwrap();
        assert_eq!(u.as_of, NOW);
        assert_eq!(reset_time(&u, NOW), Some(1792130000));
        assert_eq!(u.windows[1].name, "7d");
    }
}
