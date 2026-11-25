//! Daily traffic figures for a site, read from its nginx access log.

use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

/// Most that is read of each log file, from its end.
const MAX_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Serialize, Debug, PartialEq)]
pub struct Day {
    /// `YYYY-MM-DD`, in the time zone nginx logs in.
    pub date: String,
    pub requests: u64,
    /// Response body bytes, as nginx logs them. Headers aren't included.
    pub bytes: u64,
    /// Distinct client addresses. Behind a proxy that hides them, these are the proxy's.
    pub visitors: u64,
    pub client_errors: u64,
    pub server_errors: u64,
}

#[derive(Serialize, Debug)]
pub struct Traffic {
    /// Newest first.
    pub days: Vec<Day>,
    /// True if a log was too large to read whole, so the oldest day shown is undercounted.
    pub truncated: bool,
}

#[derive(Default)]
struct Tally {
    requests: u64,
    bytes: u64,
    visitors: HashSet<String>,
    client_errors: u64,
    server_errors: u64,
}

struct Hit<'a> {
    client: &'a str,
    date: String,
    status: u16,
    bytes: u64,
}

/// `25/Nov/2026:09:11:54 +0530` as `2026-11-25`.
fn iso_date(stamp: &str) -> Option<String> {
    let date = stamp.split(':').next()?;
    chrono::NaiveDate::parse_from_str(date, "%d/%b/%Y")
        .ok()
        .map(|d| d.format("%Y-%m-%d").to_string())
}

/// One line of nginx's `combined` format. nginx escapes quotes inside values, so the quotes in a
/// line are only the ones around them.
fn parse_line(line: &str) -> Option<Hit<'_>> {
    let mut quoted = line.split('"');
    let head = quoted.next()?;
    let _request = quoted.next()?;
    let mut numbers = quoted.next()?.split_whitespace();
    let status = numbers.next()?.parse().ok()?;
    let bytes = numbers.next()?.parse().ok()?;
    let client = head.split_whitespace().next()?;
    // The last bracket: a basic-auth user name comes before it and may hold one too.
    let stamp = &head[head.rfind('[')? + 1..];
    Some(Hit {
        client,
        date: iso_date(stamp)?,
        status,
        bytes,
    })
}

/// Count the end of one log into `days`. A missing file counts as empty.
fn add_file(path: &Path, max: u64, days: &mut BTreeMap<String, Tally>) -> std::io::Result<bool> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let start = file.metadata()?.len().saturating_sub(max);
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    if start > 0 {
        // Most likely the middle of a line.
        reader.read_until(b'\n', &mut line)?;
    }
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        let Some(hit) = parse_line(&text) else { continue };
        let day = days.entry(hit.date).or_default();
        day.requests += 1;
        day.bytes = day.bytes.saturating_add(hit.bytes);
        if !day.visitors.contains(hit.client) {
            day.visitors.insert(hit.client.to_string());
        }
        match hit.status {
            400..=499 => day.client_errors += 1,
            500..=599 => day.server_errors += 1,
            _ => {}
        }
    }
    Ok(start > 0)
}

/// Figures from `<domain>.access.log` and, if it isn't compressed yet, the one rotated before it.
pub fn summarize(log_dir: &Path, domain: &str) -> std::io::Result<Traffic> {
    summarize_up_to(log_dir, domain, MAX_BYTES)
}

fn summarize_up_to(log_dir: &Path, domain: &str, max: u64) -> std::io::Result<Traffic> {
    let mut days = BTreeMap::new();
    let mut truncated = false;
    for name in [format!("{domain}.access.log.1"), format!("{domain}.access.log")] {
        truncated |= add_file(&log_dir.join(name), max, &mut days)?;
    }
    let days = days
        .into_iter()
        .rev()
        .map(|(date, t)| Day {
            date,
            requests: t.requests,
            bytes: t.bytes,
            visitors: t.visitors.len() as u64,
            client_errors: t.client_errors,
            server_errors: t.server_errors,
        })
        .collect();
    Ok(Traffic { days, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(ip: &str, day: u32, status: u16, bytes: u64) -> String {
        format!("{ip} - - [{day:02}/Nov/2026:09:11:54 +0530] \"GET / HTTP/1.1\" {status} {bytes} \"-\" \"curl\"\n")
    }

    #[test]
    fn adds_up_days_across_both_logs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.com.access.log.1"), line("1.1.1.1", 24, 200, 10)).unwrap();
        let today = [
            line("1.1.1.1", 25, 200, 100),
            line("1.1.1.1", 25, 404, 5),
            line("2001:db8::1", 25, 502, 7),
            "not a log line\n".to_string(),
        ]
        .concat();
        std::fs::write(dir.path().join("a.com.access.log"), today).unwrap();
        let t = summarize(dir.path(), "a.com").unwrap();
        assert!(!t.truncated);
        assert_eq!(
            t.days[0],
            Day {
                date: "2026-11-25".into(),
                requests: 3,
                bytes: 112,
                visitors: 2,
                client_errors: 1,
                server_errors: 1,
            }
        );
        assert_eq!((t.days[1].date.as_str(), t.days[1].requests), ("2026-11-24", 1));
    }

    #[test]
    fn missing_logs_are_empty_and_long_ones_are_cut() {
        let dir = tempfile::tempdir().unwrap();
        assert!(summarize(dir.path(), "a.com").unwrap().days.is_empty());
        let text = line("1.1.1.1", 25, 200, 1).repeat(10);
        std::fs::write(dir.path().join("a.com.access.log"), &text).unwrap();
        let t = summarize_up_to(dir.path(), "a.com", 200).unwrap();
        assert!(t.truncated);
        assert!(t.days[0].requests < 10);
    }

    #[test]
    fn brackets_in_a_user_name_do_not_confuse_the_date() {
        let l = "1.1.1.1 - a[b [25/Nov/2026:09:11:54 +0530] \"GET /[x] HTTP/1.1\" 200 4 \"-\" \"-\"";
        assert_eq!(parse_line(l).unwrap().date, "2026-11-25");
    }
}
