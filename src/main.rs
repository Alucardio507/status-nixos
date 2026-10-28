//! status-nixos: show how fresh your NixOS channel(s) are, using
//! the public NixOS Prometheus instance. Channel status comes from Prometheus;
//! no build jobs are queried.
//!
//! Flags: --json, --no-color, --flake PATH, -h/--help, -V/--version
//! Env:   NO_COLOR is respected.

use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PROMETHEUS_API: &str = "https://prometheus.nixos.org/api/v1/query";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const DAY: f64 = 86_400.0;
const FRESH_DAYS: f64 = 2.0;
const AGING_DAYS: f64 = 5.0;
const LABEL_WIDTH: usize = 14;
const AGE_WIDTH: usize = 18;

// ───────────────────────────── data types ─────────────────────────────

#[derive(Debug, Clone)]
struct Channel {
    name: String,
    revision: String,
    status: String,
    /// Unix timestamp of the last channel update, if known.
    updated: Option<f64>,
}

impl Channel {
    fn age(&self, now: f64) -> Option<f64> {
        self.updated.map(|ts| now - ts)
    }
    fn freshness(&self, now: f64) -> Freshness {
        Freshness::from_age(self.updated.filter(|ts| *ts > 0.0).map(|ts| now - ts))
    }
    fn status_order(&self) -> u8 {
        match self.status.as_str() {
            "stable" => 0,
            "rolling" => 1,
            "beta" => 2,
            _ => 9,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Freshness {
    Fresh,
    Aging,
    Stale,
}

impl Freshness {
    fn from_age(age_secs: Option<f64>) -> Self {
        match age_secs {
            Some(a) if a < FRESH_DAYS * DAY => Self::Fresh,
            Some(a) if a < AGING_DAYS * DAY => Self::Aging,
            _ => Self::Stale,
        }
    }
    /// Plain ANSI color code (no bold).
    fn color(self) -> &'static str {
        match self {
            Self::Fresh => "32",
            Self::Aging => "33",
            Self::Stale => "31",
        }
    }
}

/// A possible channel found on this machine, before matching it against the
/// channels the status page knows about.
#[derive(Debug, Clone)]
struct Candidate {
    /// Channel label or flake input name.
    label: String,
    /// Channel name when the source states it (`nixos-25.05`).
    name: Option<String>,
    /// Pinned revision (flake.lock / registry), used when no name is given.
    rev: Option<String>,
    /// Where it was found, shown to the user.
    source: String,
}

/// A candidate resolved to a channel name.
#[derive(Debug, Clone)]
struct Detected {
    label: String,
    name: String,
    source: String,
    rev: Option<String>,
}

#[derive(Debug, Default)]
struct LocalInfo {
    candidates: Vec<Candidate>,
    revision: Option<String>,
    version: Option<String>,
    generation: Option<String>,
}

#[derive(Debug, Default)]
struct Options {
    json: bool,
    color: bool,
    /// `--flake PATH`: a flake.lock file or a directory containing one.
    flake: Option<String>,
}

// ───────────────────────────── styling ─────────────────────────────

#[derive(Clone, Copy)]
struct Style {
    enabled: bool,
}

impl Style {
    fn paint(self, code: &str, text: &str) -> String {
        if self.enabled && !code.is_empty() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
}

/// A line that tracks its plain text (for width) and styled text (for output)
/// together, so the two can never drift apart.
#[derive(Default)]
struct Line {
    plain: String,
    styled: String,
}

impl Line {
    fn push(&mut self, style: Style, code: &str, text: &str) -> &mut Self {
        self.plain.push_str(text);
        self.styled.push_str(&style.paint(code, text));
        self
    }
}

fn kv(style: Style, label: &str, value: &str, value_code: &str) -> Line {
    let mut line = Line::default();
    line.push(style, "2", &format!("{label:<LABEL_WIDTH$}"))
        .push(style, value_code, value);
    line
}

fn box_lines(lines: &[Line]) -> String {
    // NOTE: chars().count() is fine for the glyphs used here; use the
    // `unicode-width` crate if you ever print CJK or emoji.
    let width = lines
        .iter()
        .map(|l| l.plain.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = Vec::with_capacity(lines.len() + 2);
    out.push(format!("╭{}╮", "─".repeat(width + 2)));
    for l in lines {
        let pad = width.saturating_sub(l.plain.chars().count());
        out.push(format!("│ {}{} │", l.styled, " ".repeat(pad)));
    }
    out.push(format!("╰{}╯", "─".repeat(width + 2)));
    out.join("\n")
}

// ───────────────────────────── helpers ─────────────────────────────

fn age_string(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let (days, hours, minutes, secs) = (
        total / 86_400,
        (total % 86_400) / 3_600,
        (total % 3_600) / 60,
        total % 60,
    );
    if days > 0 {
        format!("{days}d {hours}h {minutes}m {secs}s")
    } else if hours > 0 {
        format!("{hours}h {minutes}m {secs}s")
    } else if minutes > 0 {
        format!("{minutes}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

/// Unix timestamp -> `YYYY-MM-DD HH:MM:SS UTC` (civil-from-days algorithm,
/// so no date crate is needed).
fn utc_string(ts: f64) -> String {
    let total = ts.max(0.0) as i64;
    let (days, rem) = (total.div_euclid(86_400), total.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

fn short(rev: &str, n: usize) -> String {
    rev.chars().take(n).collect()
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn run(command: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(command).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn label(row: &Value, name: &str) -> Option<String> {
    row.get("metric")?.get(name)?.as_str().map(str::to_owned)
}

// ───────────────────────────── network ─────────────────────────────

/// Query the public Prometheus API and return its `data.result` array.
fn query(agent: &ureq::Agent, metric: &str) -> Result<Vec<Value>, String> {
    let mut response = agent
        .get(PROMETHEUS_API)
        .query("query", metric) // let ureq do the escaping
        .call()
        .map_err(|e| format!("network request failed for {metric}: {e}"))?;
    let status = response.status();
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("could not read Prometheus response for {metric}: {e}"))?;

    if !status.is_success() {
        let detail = body.trim();
        return Err(if detail.is_empty() {
            format!("Prometheus returned HTTP {status} for {metric}")
        } else {
            format!("Prometheus returned HTTP {status} for {metric}: {detail}")
        });
    }

    let payload: Value = serde_json::from_str(&body)
        .map_err(|e| format!("invalid JSON from Prometheus for {metric}: {e}"))?;

    if payload.get("status").and_then(Value::as_str) != Some("success") {
        let detail = payload
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown Prometheus API error");
        return Err(format!("Prometheus query failed for {metric}: {detail}"));
    }

    payload
        .pointer("/data/result")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| format!("malformed Prometheus response for {metric}: missing data.result"))
}

/// Parse the combined result from the two channel metrics.
fn parse_channels(rows: &[Value]) -> Result<Vec<Channel>, String> {
    let updates: HashMap<String, f64> = rows
        .iter()
        .filter(|row| label(row, "__name__").as_deref() == Some("channel_update_time"))
        .filter_map(|row| {
            let name = label(row, "channel")?;
            let timestamp = row.pointer("/value/1")?.as_str()?.parse::<f64>().ok()?;
            Some((name, timestamp))
        })
        .collect();

    let mut channels: Vec<Channel> = rows
        .iter()
        .filter(|row| label(row, "__name__").as_deref() == Some("channel_revision"))
        .filter_map(|row| {
            let name = label(row, "channel")?;
            Some(Channel {
                updated: updates.get(&name).copied(),
                revision: label(row, "revision").unwrap_or_else(|| "?".into()),
                status: label(row, "status").unwrap_or_else(|| "?".into()),
                name,
            })
        })
        .collect();

    if channels.is_empty() {
        return Err("No channel data returned by Prometheus.".into());
    }

    channels.sort_by(|a, b| (a.status_order(), &a.name).cmp(&(b.status_order(), &b.name)));
    channels.dedup_by(|a, b| a.name == b.name);
    Ok(channels)
}

/// Fetch revisions, statuses, and update times in one bounded-time request.
fn fetch_channels(agent: &ureq::Agent) -> Result<Vec<Channel>, String> {
    let rows = query(
        agent,
        r#"{__name__=~"channel_revision|channel_update_time"}"#,
    )?;
    parse_channels(&rows)
}

// ───────────────────────────── local system ─────────────────────────────

fn is_rev(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Channel name from a channel URL such as
/// `https://channels.nixos.org/nixos-25.05/nixexprs.tar.xz` or
/// `https://nixos.org/channels/nixos-unstable`.
fn channel_from_url(url: &str) -> Option<String> {
    ["channels.nixos.org/", "nixos.org/channels/"]
        .iter()
        .find_map(|marker| url.split_once(marker).map(|(_, rest)| rest))
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
}

/// `nix-channel --list` prints `name url`; `~/.nix-channels` stores `url name`.
fn nix_channel_candidates(output: &str, url_first: bool, source: &str) -> Vec<Candidate> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let (a, b) = (parts.next()?, parts.next()?);
            if parts.next().is_some() {
                return None;
            }
            let (label, url) = if url_first { (b, a) } else { (a, b) };
            let name = channel_from_url(url).or_else(|| {
                url.trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .map(str::to_owned)
            })?;
            Some(Candidate {
                label: label.to_owned(),
                name: Some(name),
                rev: None,
                source: source.to_owned(),
            })
        })
        .collect()
}

/// Turn a flake.lock node into a candidate if it is a nixpkgs input.
fn flake_node_candidate(label: &str, node: &Value, source: &str) -> Option<Candidate> {
    let original = node.get("original")?;
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
    let locked_rev = node.get("locked").and_then(|l| text(l, "rev"));

    let mut name = None;
    let mut rev = text(original, "rev");
    match original.get("type")?.as_str()? {
        "github" => {
            let owner = text(original, "owner")?.to_ascii_lowercase();
            if owner != "nixos" || !text(original, "repo")?.eq_ignore_ascii_case("nixpkgs") {
                return None;
            }
            match text(original, "ref") {
                Some(r) if is_rev(&r) => rev = Some(r),
                Some(r) => name = Some(r),
                None => {}
            }
        }
        "git" => {
            let url = text(original, "url")?.to_ascii_lowercase();
            if !url.contains("nixos/nixpkgs") {
                return None;
            }
            name = text(original, "ref").map(|r| r.trim_start_matches("refs/heads/").to_owned());
        }
        "tarball" | "file" => name = channel_from_url(&text(original, "url")?),
        _ => return None,
    }
    let rev = locked_rev.or(rev);
    if name.is_none() && rev.is_none() {
        return None;
    }
    Some(Candidate {
        label: label.to_owned(),
        name,
        rev,
        source: source.to_owned(),
    })
}

/// Resolve a flake.lock `inputs` target: either a node key or a `follows` path.
fn follow_input(
    nodes: &serde_json::Map<String, Value>,
    root: &str,
    target: &Value,
    depth: u8,
) -> Option<String> {
    if depth > 16 {
        return None;
    }
    match target {
        Value::String(key) => Some(key.clone()),
        Value::Array(path) => {
            let mut current = root.to_owned();
            for segment in path {
                let next = nodes.get(&current)?.get("inputs")?.get(segment.as_str()?)?;
                current = follow_input(nodes, root, next, depth + 1)?;
            }
            Some(current)
        }
        _ => None,
    }
}

/// nixpkgs inputs the flake itself declares (the root's direct inputs, with
/// `follows` resolved). Nixpkgs copies pulled in only by other inputs are
/// ignored: they say nothing about which channel *your* system tracks.
fn flake_candidates(text: &str, source: &str) -> Vec<Candidate> {
    let Ok(lock) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let root = lock.get("root").and_then(Value::as_str).unwrap_or("root");
    let (Some(nodes), Some(inputs)) = (
        lock.get("nodes").and_then(Value::as_object),
        lock.pointer(&format!("/nodes/{root}/inputs"))
            .and_then(Value::as_object),
    ) else {
        return Vec::new();
    };
    let mut out: Vec<Candidate> = inputs
        .iter()
        .filter_map(|(label, target)| {
            let key = follow_input(nodes, root, target, 0)?;
            flake_node_candidate(label, nodes.get(&key)?, source)
        })
        .collect();
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

fn flake_lock_paths(explicit: Option<&str>) -> Vec<PathBuf> {
    let normalize = |p: PathBuf| if p.is_dir() { p.join("flake.lock") } else { p };
    if let Some(path) = explicit {
        return vec![normalize(PathBuf::from(path))];
    }
    let mut paths = vec![PathBuf::from("/etc/nixos/flake.lock")];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        paths.push(home.join(".config/nixos/flake.lock"));
        paths.push(home.join(".config/home-manager/flake.lock"));
    }
    paths.into_iter().map(normalize).collect()
}

/// `nix registry list` lines like `system flake:nixpkgs github:NixOS/nixpkgs/<ref-or-rev>`.
/// The `global` registry is skipped: it is not specific to this machine.
fn registry_candidates(output: &str) -> Vec<Candidate> {
    output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            let [scope, id, target] = parts[..] else {
                return None;
            };
            if !matches!(scope, "system" | "user") || id != "flake:nixpkgs" {
                return None;
            }
            let mut segs = target.strip_prefix("github:")?.split('/');
            let (owner, repo, reference) = (segs.next()?, segs.next()?, segs.next()?);
            if !owner.eq_ignore_ascii_case("nixos") || !repo.eq_ignore_ascii_case("nixpkgs") {
                return None;
            }
            let reference = reference.split(['?', '#']).next()?.to_owned();
            let (name, rev) = if is_rev(&reference) {
                (None, Some(reference))
            } else {
                (Some(reference), None)
            };
            Some(Candidate {
                label: "nixpkgs".into(),
                name,
                rev,
                source: format!("nix registry ({scope})"),
            })
        })
        .collect()
}

/// Read the active NixOS system generation from the system profile link
/// (for example, `system-42-link` -> generation `42`).
fn current_system_generation() -> Option<String> {
    let target = std::fs::read_link("/nix/var/nix/profiles/system").ok()?;
    let name = target.file_name()?.to_str()?;
    let generation = name.strip_prefix("system-")?.strip_suffix("-link")?;
    (!generation.is_empty() && generation.bytes().all(|b| b.is_ascii_digit()))
        .then(|| generation.to_owned())
}

fn local_info(opts: &Options) -> LocalInfo {
    let mut candidates = Vec::new();

    // 1. Classic channels: the command only sees *this* user's channels, so
    //    also read the channel files directly (root's is often unreadable).
    let listed = run("nix-channel", &["--list"]).unwrap_or_default();
    candidates.extend(nix_channel_candidates(&listed, false, "nix-channel"));
    if candidates.is_empty() {
        let mut files = vec![PathBuf::from("/root/.nix-channels")];
        if let Some(home) = std::env::var_os("HOME") {
            files.insert(0, PathBuf::from(home).join(".nix-channels"));
        }
        for file in files {
            if let Ok(text) = std::fs::read_to_string(&file) {
                let source = file.display().to_string();
                candidates.extend(nix_channel_candidates(&text, true, &source));
            }
        }
    }

    // 2. Flakes: nixpkgs inputs declared in flake.lock.
    for path in flake_lock_paths(opts.flake.as_deref()) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            candidates.extend(flake_candidates(&text, &path.display().to_string()));
        }
    }

    // 3. Last resort before guessing: the nix registry's nixpkgs entry.
    if candidates.is_empty() {
        let listed = run("nix", &["registry", "list"]).unwrap_or_default();
        candidates.extend(registry_candidates(&listed));
    }

    let parsed =
        run("nixos-version", &["--json"]).and_then(|s| serde_json::from_str::<Value>(&s).ok());
    let field = |key: &str| {
        parsed
            .as_ref()?
            .get(key)?
            .as_str()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };

    let version = field("nixosVersion");
    let generation = version.as_ref().and_then(|_| current_system_generation());

    LocalInfo {
        candidates,
        revision: field("nixpkgsRevision"),
        version,
        generation,
    }
}

/// `25.05.812436.abc (Warbler)` -> `nixos-25.05`; `26.05pre123.abc` -> `nixos-unstable`.
fn channel_from_version(version: &str) -> Option<String> {
    let head = version.split_whitespace().next()?;
    let (year, rest) = head.split_once('.')?;
    let month = rest.get(..2)?;
    if year.len() != 2 || !year.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !month.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if rest[2..].starts_with("pre") {
        Some("nixos-unstable".into())
    } else {
        Some(format!("nixos-{year}.{month}"))
    }
}

fn add_detected(
    found: &mut Vec<Detected>,
    label: &str,
    name: &str,
    source: &str,
    rev: Option<&String>,
) {
    if !found.iter().any(|d| d.name == name) {
        found.push(Detected {
            label: label.to_owned(),
            name: name.to_owned(),
            source: source.to_owned(),
            rev: rev.cloned(),
        });
    }
}

/// Match candidates against the status page's channels and fall back to
/// guessing from the running system when nothing was configured.
fn resolve_channels(channels: &[Channel], local: &LocalInfo) -> Vec<Detected> {
    let mut found: Vec<Detected> = Vec::new();

    for c in &local.candidates {
        if let Some(name) = &c.name {
            add_detected(&mut found, &c.label, name, &c.source, c.rev.as_ref());
        } else if let Some(rev) = &c.rev {
            // Pinned to a commit: every channel currently at that commit is a match.
            for ch in channels.iter().filter(|ch| &ch.revision == rev) {
                add_detected(&mut found, &c.label, &ch.name, &c.source, Some(rev));
            }
        }
    }

    let guess_rev = local.revision.as_ref().filter(|_| found.is_empty());
    if let Some(sys) = guess_rev {
        for ch in channels.iter().filter(|ch| &ch.revision == sys) {
            add_detected(
                &mut found,
                "guessed",
                &ch.name,
                "running system revision",
                Some(sys),
            );
        }
    }
    let guess_name = local
        .version
        .as_deref()
        .filter(|_| found.is_empty())
        .and_then(channel_from_version)
        .filter(|name| channels.iter().any(|ch| &ch.name == name));
    if let Some(name) = guess_name {
        add_detected(&mut found, "guessed", &name, "NixOS version", None);
    }
    found
}

// ───────────────────────────── rendering ─────────────────────────────

fn render_summary(
    style: Style,
    now: f64,
    channels: &[Channel],
    local: &LocalInfo,
    detected: &[Detected],
) -> String {
    let by_name: HashMap<&str, &Channel> = channels.iter().map(|c| (c.name.as_str(), c)).collect();

    let mut lines = Vec::new();
    let mut title = Line::default();
    title.push(style, "1", "Your NixOS channel");
    lines.push(title);

    if detected.is_empty() {
        let mut l = Line::default();
        l.push(
            style,
            "2",
            "unknown (no nix-channel entries or flake.lock found; try --flake PATH)",
        );
        lines.push(l);
    }

    for d in detected {
        lines.push(Line::default());

        let mut head = Line::default();
        head.push(style, "1;36", &format!("▶ {}", d.name));
        head.push(style, "2", &format!("  ({} · {})", d.label, d.source));
        lines.push(head);

        let Some(channel) = by_name.get(d.name.as_str()) else {
            lines.push(kv(style, "Status", "not listed on status page", "33"));
            continue;
        };

        let updated = channel
            .age(now)
            .map(|a| format!("{} ago", age_string(a)))
            .unwrap_or_else(|| "?".into());
        let color = format!("1;{}", channel.freshness(now).color());
        lines.push(kv(style, "Last updated", &updated, &color));
        if let Some(ts) = channel.updated.filter(|ts| *ts > 0.0) {
            lines.push(kv(style, "Updated at", &utc_string(ts), "0"));
        }
        lines.push(kv(style, "Commit", &short(&channel.revision, 12), "1"));
        lines.push(kv(style, "Status", &channel.status, "1"));

        let pinned = d
            .rev
            .as_deref()
            .filter(|_| d.source != "running system revision");
        if let Some(pinned) = pinned {
            let at_head = pinned == channel.revision;
            let text = format!(
                "{} ({})",
                short(pinned, 12),
                if at_head {
                    "at channel head"
                } else {
                    "differs from head"
                }
            );
            let code = if at_head { "1;32" } else { "1;33" };
            lines.push(kv(style, "Locked rev", &text, code));
        }

        if let Some(sys) = local.revision.as_deref() {
            if sys == channel.revision {
                lines.push(kv(style, "Your system", "at channel head", "1;32"));
            } else {
                let text = format!("on {} (differs)", short(sys, 12));
                lines.push(kv(style, "Your system", &text, "1;33"));
            }
        }
    }

    lines.push(Line::default());
    if let Some(v) = local.version.as_deref() {
        lines.push(kv(style, "NixOS version", v, "1"));
    }
    if let Some(generation) = local.generation.as_deref() {
        lines.push(kv(style, "Generation", generation, "1"));
    }
    lines.push(kv(style, "status-nixos", env!("CARGO_PKG_VERSION"), "1"));
    box_lines(&lines)
}

fn render_table(style: Style, now: f64, channels: &[Channel], used: &HashSet<String>) -> String {
    let mut out = vec![style.paint(
        "1",
        &format!(
            "  {:<26}{:<AGE_WIDTH$}{:<14}STATUS",
            "CHANNEL", "LAST UPDATED", "COMMIT"
        ),
    )];
    for c in channels {
        let in_use = used.contains(&c.name);
        let updated = c.age(now).map(age_string).unwrap_or_else(|| "?".into());
        let marker = if in_use {
            style.paint("1;36", "▶ ")
        } else {
            "  ".into()
        };
        let name = format!("{:<26}", c.name);
        let name = if in_use {
            style.paint("1", &name)
        } else {
            name
        };
        let status = c.status.clone();
        out.push(format!(
            "{marker}{name}{}{:<14}{status}",
            style.paint(c.freshness(now).color(), &format!("{updated:<AGE_WIDTH$}")),
            short(&c.revision, 12),
        ));
    }
    out.join("\n")
}

fn render_json(now: f64, channels: &[Channel], detected: &[Detected], local: &LocalInfo) -> String {
    let used: HashSet<&str> = detected.iter().map(|d| d.name.as_str()).collect();
    let items: Vec<Value> = channels
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "status": c.status,
                "revision": c.revision,
                "updated_unix": c.updated,
                "updated_utc": c.updated.filter(|ts| *ts > 0.0).map(utc_string),
                "age_seconds": c.age(now),
                "in_use": used.contains(c.name.as_str()),
                "system_at_head": local.revision.as_deref().map(|r| r == c.revision),
            })
        })
        .collect();
    let selected: Vec<Value> = detected
        .iter()
        .map(|d| {
            json!({
                "name": d.name,
                "label": d.label,
                "source": d.source,
                "locked_revision": d.rev,
            })
        })
        .collect();
    serde_json::to_string_pretty(&json!({
        "nixos_version": local.version,
        "nixos_generation": local.generation,
        "system_revision": local.revision,
        "selected": selected,
        "channels": items,
    }))
    .unwrap_or_default()
}

// ───────────────────────────── main ─────────────────────────────

fn parse_args() -> Result<Options, String> {
    let mut opts = Options {
        json: false,
        color: io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        flake: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => opts.json = true,
            "--no-color" => opts.color = false,
            "--flake" => {
                opts.flake = Some(
                    args.next()
                        .ok_or("--flake needs a path (flake.lock or its directory)")?,
                );
            }
            "-V" | "--version" => {
                println!("status-nixos {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!("Usage: status-nixos [--json] [--no-color] [--flake PATH] [--version]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other} (try --help)")),
        }
    }
    Ok(opts)
}

fn main_result() -> Result<(), String> {
    let opts = parse_args()?;
    let style = Style {
        enabled: opts.color,
    };

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        // Keep HTTP error responses so we can show Prometheus's error detail.
        .http_status_as_error(false)
        .build()
        .into();

    let channels = fetch_channels(&agent)?;
    let local = local_info(&opts);
    let now = now_seconds();

    // Which channels is this machine on?
    let detected = resolve_channels(&channels, &local);
    let used: HashSet<String> = detected.iter().map(|d| d.name.clone()).collect();

    if opts.json {
        println!("{}", render_json(now, &channels, &detected, &local));
    } else {
        println!(
            "{}",
            render_summary(style, now, &channels, &local, &detected)
        );
        println!("{}", render_table(style, now, &channels, &used));
    }
    Ok(())
}

fn main() {
    if let Err(error) = main_result() {
        eprintln!("status-nixos: {error}");
        std::process::exit(1);
    }
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_ages() {
        assert_eq!(age_string(7.0), "7s");
        assert_eq!(age_string(59.0), "59s");
        assert_eq!(age_string(125.0), "2m 5s");
        assert_eq!(age_string(3_700.0), "1h 1m 40s");
        assert_eq!(age_string(90_061.0), "1d 1h 1m 1s");
        assert_eq!(age_string(-5.0), "0s");
    }

    #[test]
    fn formats_utc() {
        assert_eq!(utc_string(0.0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc_string(951_782_400.0), "2000-02-29 00:00:00 UTC");
        assert_eq!(utc_string(1_791_000_005.0), "2026-10-03 04:00:05 UTC");
    }

    #[test]
    fn channel_urls() {
        assert_eq!(
            channel_from_url("https://channels.nixos.org/nixos-25.05/nixexprs.tar.xz").as_deref(),
            Some("nixos-25.05")
        );
        assert_eq!(
            channel_from_url("https://nixos.org/channels/nixos-unstable").as_deref(),
            Some("nixos-unstable")
        );
        assert_eq!(channel_from_url("https://example.com/x"), None);
    }

    #[test]
    fn parses_system_generation_link_name() {
        let parse = |name: &str| {
            let name = name.strip_prefix("system-")?.strip_suffix("-link")?;
            (!name.is_empty() && name.bytes().all(|b| b.is_ascii_digit())).then(|| name.to_owned())
        };
        assert_eq!(parse("system-42-link").as_deref(), Some("42"));
        assert_eq!(parse("system-current-link"), None);
        assert_eq!(parse("system-42"), None);
    }

    #[test]
    fn version_to_channel() {
        assert_eq!(
            channel_from_version("25.05.812436.abc (Warbler)").as_deref(),
            Some("nixos-25.05")
        );
        assert_eq!(
            channel_from_version("26.05pre123.abc (Yarara)").as_deref(),
            Some("nixos-unstable")
        );
        assert_eq!(channel_from_version("garbage"), None);
    }

    #[test]
    fn detects_flake_inputs_and_follows() {
        let lock = r#"{
          "root": "root", "version": 7,
          "nodes": {
            "root": { "inputs": { "nixpkgs": "nixpkgs", "hm": "hm", "nixpkgs-pinned": "pinned", "nixpkgs-alias": ["hm", "nixpkgs"] } },
            "nixpkgs": { "original": { "owner": "NixOS", "repo": "nixpkgs", "ref": "nixos-25.05", "type": "github" },
                         "locked": { "rev": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "type": "github" } },
            "hm": { "inputs": { "nixpkgs": "nixpkgs_2" }, "original": { "owner": "nix-community", "repo": "home-manager", "type": "github" } },
            "nixpkgs_2": { "original": { "owner": "NixOS", "repo": "nixpkgs", "ref": "nixos-unstable", "type": "github" },
                           "locked": { "rev": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "type": "github" } },
            "pinned": { "original": { "owner": "NixOS", "repo": "nixpkgs", "type": "github" },
                        "locked": { "rev": "cccccccccccccccccccccccccccccccccccccccc", "type": "github" } }
          }
        }"#;
        let found = flake_candidates(lock, "test");
        let by = |l: &str| found.iter().find(|c| c.label == l).cloned();
        assert_eq!(by("nixpkgs").unwrap().name.as_deref(), Some("nixos-25.05"));
        // `follows` path resolves to the transitive node.
        assert_eq!(
            by("nixpkgs-alias").unwrap().name.as_deref(),
            Some("nixos-unstable")
        );
        // No ref: matched later by revision.
        let pinned = by("nixpkgs-pinned").unwrap();
        assert!(pinned.name.is_none() && pinned.rev.is_some());
        // Non-nixpkgs inputs are ignored.
        assert!(by("hm").is_none());
    }

    #[test]
    fn pinned_rev_resolves_to_channel() {
        let ch = |name: &str, rev: &str| Channel {
            name: name.into(),
            revision: rev.into(),
            status: "stable".into(),
            updated: None,
        };
        let channels = vec![ch("nixos-25.05", "abc"), ch("nixos-unstable", "def")];
        let local = LocalInfo {
            candidates: vec![Candidate {
                label: "nixpkgs".into(),
                name: None,
                rev: Some("def".into()),
                source: "flake.lock".into(),
            }],
            ..Default::default()
        };
        let d = resolve_channels(&channels, &local);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].name, "nixos-unstable");
    }

    #[test]
    fn registry_skips_global() {
        let out = "global flake:nixpkgs github:NixOS/nixpkgs/nixpkgs-unstable\nsystem flake:nixpkgs github:NixOS/nixpkgs/nixos-25.05\n";
        let c = registry_candidates(out);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name.as_deref(), Some("nixos-25.05"));
    }

    #[test]
    fn parses_statuses_and_update_times_from_combined_metrics() {
        let rows = vec![
            json!({
                "metric": { "__name__": "channel_revision", "channel": "nixos-unstable", "revision": "aaa", "status": "rolling" },
                "value": ["1", "1"]
            }),
            json!({
                "metric": { "__name__": "channel_revision", "channel": "nixos-25.05", "revision": "bbb", "status": "stable" },
                "value": ["1", "1"]
            }),
            json!({
                "metric": { "__name__": "channel_revision", "channel": "nixos-24.11", "revision": "ccc", "status": "unmaintained" },
                "value": ["1", "1"]
            }),
            json!({
                "metric": { "__name__": "channel_update_time", "channel": "nixos-25.05" },
                "value": ["1", "1735689600"]
            }),
        ];

        let channels = parse_channels(&rows).unwrap();
        assert_eq!(
            channels
                .iter()
                .map(|c| c.status.as_str())
                .collect::<Vec<_>>(),
            vec!["stable", "rolling", "unmaintained"]
        );
        assert_eq!(channels[0].updated, Some(1_735_689_600.0));
        assert_eq!(channels[1].updated, None);
    }

    #[test]
    fn classifies_freshness() {
        assert_eq!(Freshness::from_age(Some(DAY)), Freshness::Fresh);
        assert_eq!(Freshness::from_age(Some(3.0 * DAY)), Freshness::Aging);
        assert_eq!(Freshness::from_age(Some(9.0 * DAY)), Freshness::Stale);
        assert_eq!(Freshness::from_age(None), Freshness::Stale);
    }

    #[test]
    fn box_width_ignores_ansi() {
        let style = Style { enabled: true };
        let mut a = Line::default();
        a.push(style, "1", "abc");
        let mut b = Line::default();
        b.push(style, "", "abcdef");
        let boxed = box_lines(&[a, b]);
        let widths: Vec<usize> = boxed
            .lines()
            .map(|l| {
                // strip escape sequences for measuring
                let mut n = 0;
                let mut in_esc = false;
                for ch in l.chars() {
                    match (in_esc, ch) {
                        (false, '\x1b') => in_esc = true,
                        (true, 'm') => in_esc = false,
                        (false, _) => n += 1,
                        _ => {}
                    }
                }
                n
            })
            .collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]));
    }
}
