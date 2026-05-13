//! Persistence and polling for Glass's scheduled prompts.
//!
//! `CronStore` owns `$GLASS_SYSTEM_DATA/cron.jsonl`: an append-only-ish log
//! of pending scheduled prompts. The orchestrator socket calls
//! [`CronStore::append`] when the agent invokes the `schedule` tool; the
//! cron poller task calls [`CronStore::poll_due`] every ~30s to find entries
//! that have come due and fire them through the dispatcher.
//!
//! Entry shape (one JSON object per line):
//!
//!   one-shot:   `{"id": "...", "what": "...", "fire_at": "<rfc3339-local>"}`
//!   recurring:  `{"id": "...", "what": "...", "cron": "0 9 * * *",
//!                "last_fired_at": "<rfc3339-local>"}`
//!
//! Both forms also carry an optional `last_fired_at`. For one-shots we
//! never set it (the entry is removed on fire). For recurring entries it
//! tracks the most recent fire time; on creation it's seeded to "now" so
//! the first fire is the next upcoming slot, not all the missed slots from
//! the epoch.
//!
//! Catch-up policy: if a recurring entry's bot was offline through many
//! slots, the poller fires it ONCE on resume and sets `last_fired_at = now`,
//! skipping the rest. Mikayla's stated preference: "fire once, no spamming."
//!
//! Concurrency: every read-modify-write goes through `lock`. The
//! schedule-tool append path and the poller share the same store; the
//! mutex prevents lost updates between them.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Local, NaiveDateTime, NaiveTime, TimeZone};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use tokio::fs;
use tokio::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronEntry {
    pub id: String,
    pub what: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fire_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<String>,
}

#[derive(Clone)]
pub struct CronStore {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl CronStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append a new schedule entry. Validates that exactly one of `when` or
    /// `cron` is set, parses `when` against `now` (so HH:MM rolls correctly
    /// across day boundaries), and seeds `last_fired_at = now` for recurring
    /// entries (so the first fire is the next upcoming slot, not every past
    /// slot since the epoch).
    pub async fn append(
        &self,
        what: &str,
        when: Option<&str>,
        cron_expr: Option<&str>,
        now: DateTime<Local>,
    ) -> Result<String> {
        let what = what.trim();
        if what.is_empty() {
            anyhow::bail!("'what' is required");
        }
        let (fire_at, cron_field, last_fired_at) = match (when, cron_expr) {
            (None, None) => anyhow::bail!("must specify either 'when' or 'cron'"),
            (Some(_), Some(_)) => anyhow::bail!("'when' and 'cron' are mutually exclusive"),
            (Some(w), None) => {
                let dt = parse_when(w, now)?;
                (Some(dt.to_rfc3339()), None, None)
            }
            (None, Some(c)) => {
                // Validate the expression up-front so a bad cron is rejected
                // at schedule-time rather than failing silently in the poller.
                parse_cron(c).with_context(|| format!("invalid cron expression: {c}"))?;
                (None, Some(c.to_string()), Some(now.to_rfc3339()))
            }
        };

        let entry = CronEntry {
            id: short_id(),
            what: what.to_string(),
            fire_at,
            cron: cron_field,
            last_fired_at,
        };
        let id = entry.id.clone();

        let _guard = self.lock.lock().await;
        let mut all = read_entries(&self.path).await?;
        all.push(entry);
        write_entries(&self.path, &all).await?;
        Ok(id)
    }

    /// All scheduled entries currently on disk, in file order. Useful for
    /// CLI inspection (`glass cron list`). The mutex is shared with the
    /// append + poll paths so reads don't race with concurrent writes.
    pub async fn list(&self) -> Result<Vec<CronEntry>> {
        let _guard = self.lock.lock().await;
        read_entries(&self.path).await
    }

    /// Remove an entry by id or unique id-prefix. Returns the removed
    /// entry on success, [`RemoveResult::NotFound`] if nothing matches,
    /// or [`RemoveResult::Ambiguous`] when a partial prefix matches more
    /// than one entry (no deletion happens in that case; caller is
    /// expected to disambiguate).
    pub async fn remove(&self, id_or_prefix: &str) -> Result<RemoveResult> {
        let needle = id_or_prefix.trim();
        if needle.is_empty() {
            anyhow::bail!("id is required");
        }
        let _guard = self.lock.lock().await;
        let entries = read_entries(&self.path).await?;
        let matches: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.id == needle || e.id.starts_with(needle))
            .map(|(i, _)| i)
            .collect();
        match matches.as_slice() {
            [] => Ok(RemoveResult::NotFound),
            [idx] => {
                let mut kept = entries;
                let removed = kept.remove(*idx);
                write_entries(&self.path, &kept).await?;
                Ok(RemoveResult::Removed(removed))
            }
            many => {
                let ids: Vec<String> = many.iter().map(|i| entries[*i].id.clone()).collect();
                Ok(RemoveResult::Ambiguous(ids))
            }
        }
    }

    /// Poll once: read the file, find entries due to fire at `now`, write
    /// back the survivors (one-shots fired are removed; recurring entries
    /// fired have `last_fired_at` updated to `now`), and return the fired
    /// entries for the caller to dispatch.
    pub async fn poll_due(&self, now: DateTime<Local>) -> Result<Vec<CronEntry>> {
        let _guard = self.lock.lock().await;
        let all = read_entries(&self.path).await?;
        let PollResult { fire, keep } = poll_due_pure(all, now);
        if fire.is_empty() {
            return Ok(Vec::new());
        }
        write_entries(&self.path, &keep).await?;
        Ok(fire)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct PollResult {
    fire: Vec<CronEntry>,
    keep: Vec<CronEntry>,
}

/// Outcome of a [`CronStore::remove`] call.
#[derive(Debug, PartialEq, Eq)]
pub enum RemoveResult {
    /// The matching entry was removed from disk.
    Removed(CronEntry),
    /// No entry's id matched the input (exact or prefix).
    NotFound,
    /// A prefix matched multiple entries; nothing was removed. The
    /// caller should disambiguate by supplying a longer prefix or the
    /// full id.
    Ambiguous(Vec<String>),
}

/// Pure decision function: given the current set of entries and the current
/// time, decide which fire and what the rewritten file should contain.
/// Separated so unit tests can drive it without touching the filesystem.
fn poll_due_pure(entries: Vec<CronEntry>, now: DateTime<Local>) -> PollResult {
    let mut fire = Vec::new();
    let mut keep = Vec::new();
    for entry in entries {
        match (entry.fire_at.as_deref(), entry.cron.as_deref()) {
            (Some(fire_at), None) => match DateTime::parse_from_rfc3339(fire_at) {
                Ok(t) if t.with_timezone(&Local) <= now => {
                    fire.push(entry);
                }
                Ok(_) => keep.push(entry),
                Err(e) => {
                    tracing::warn!(
                        id = %entry.id,
                        "cron: dropping entry with unparseable fire_at ({e}); was {fire_at}"
                    );
                    // Drop corrupted entries rather than retrying every poll.
                }
            },
            (None, Some(c)) => {
                let schedule = match parse_cron(c) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            id = %entry.id,
                            "cron: dropping entry with unparseable cron ({e}); was {c}"
                        );
                        continue;
                    }
                };
                let last = entry
                    .last_fired_at
                    .as_deref()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Local))
                    // Fallback: treat as never-fired = "just created."
                    // Schedule::after below will use this as the lower bound;
                    // setting it to a far-past instant would fire missed
                    // slots, which violates the no-spam policy.
                    .unwrap_or(now);
                let next = schedule.after(&last).next();
                match next {
                    Some(t) if t <= now => {
                        // Skip any other missed slots: set last_fired_at to
                        // now, so the next computed slot is in the future.
                        let mut updated = entry;
                        updated.last_fired_at = Some(now.to_rfc3339());
                        fire.push(updated.clone());
                        keep.push(updated);
                    }
                    _ => keep.push(entry),
                }
            }
            // Malformed entry (neither fire_at nor cron, or both). Drop.
            _ => {
                tracing::warn!(
                    id = %entry.id,
                    "cron: dropping malformed entry (need exactly one of fire_at, cron)"
                );
            }
        }
    }
    PollResult { fire, keep }
}

/// Read the cron file, parsing one JSON entry per line. Missing file = empty
/// list (first-run case). Malformed lines are skipped with a warning so one
/// bad row doesn't lose the whole file.
async fn read_entries(path: &Path) -> Result<Vec<CronEntry>> {
    let raw = match fs::read_to_string(path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut entries = Vec::new();
    for (n, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<CronEntry>(line) {
            Ok(e) => entries.push(e),
            Err(err) => tracing::warn!(line = n + 1, "cron: dropping unparseable line: {err}"),
        }
    }
    Ok(entries)
}

/// Atomic-rename write: serialize entries to a `.tmp` sibling, then rename
/// over the real file. Crash-safety: either the old file or the new file is
/// visible to readers, never a partial write.
async fn write_entries(path: &Path, entries: &[CronEntry]) -> Result<()> {
    let mut body = String::new();
    for entry in entries {
        body.push_str(&serde_json::to_string(entry)?);
        body.push('\n');
    }
    let tmp = path.with_extension("jsonl.tmp");
    fs::write(&tmp, body.as_bytes())
        .await
        .with_context(|| format!("writing temp file {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .await
        .with_context(|| format!("renaming temp into {}", path.display()))?;
    Ok(())
}

/// Parse a 5-field cron expression (`min hour dom month dow`, local timezone
/// assumed) into a `cron::Schedule`. The `cron` crate uses 7 fields
/// internally — `sec min hour dom month dow year` — so we prepend `0`
/// (seconds = 0) and append `*` (year = any).
pub fn parse_cron(expr: &str) -> Result<cron::Schedule> {
    let seven = format!("0 {} *", expr.trim());
    cron::Schedule::from_str(&seven)
        .with_context(|| format!("invalid 5-field cron expression: {expr}"))
}

/// Parse the `when` field into a concrete local-time `DateTime`.
///
///   `HH:MM`               next occurrence within 24h in local timezone.
///                         If already past today, rolls to tomorrow.
///   `YYYY-MM-DD HH:MM`    that specific local-time moment. Must be in
///                         the future.
pub fn parse_when(input: &str, now: DateTime<Local>) -> Result<DateTime<Local>> {
    let input = input.trim();

    if let Ok(t) = NaiveTime::parse_from_str(input, "%H:%M") {
        let today = now.date_naive().and_time(t);
        let today_local = local_from_naive(&today)?;
        if today_local > now {
            return Ok(today_local);
        }
        let tomorrow = (now.date_naive() + ChronoDuration::days(1)).and_time(t);
        return local_from_naive(&tomorrow);
    }

    if let Ok(dt) = NaiveDateTime::parse_from_str(input, "%Y-%m-%d %H:%M") {
        let resolved = local_from_naive(&dt)?;
        if resolved <= now {
            anyhow::bail!(
                "when: {} is in the past (now is {})",
                resolved.to_rfc3339(),
                now.to_rfc3339()
            );
        }
        return Ok(resolved);
    }

    anyhow::bail!(
        "when: expected 'HH:MM' (next 24h, local) or 'YYYY-MM-DD HH:MM' (local), got {input:?}"
    )
}

fn local_from_naive(dt: &NaiveDateTime) -> Result<DateTime<Local>> {
    match Local.from_local_datetime(dt).single() {
        Some(d) => Ok(d),
        None => anyhow::bail!(
            "when: {} is ambiguous or invalid in local time (likely a DST transition); \
             try a different minute",
            dt
        ),
    }
}

/// 12-char base32-ish ID. Not cryptographically unique — just
/// distinguishable when scrolling `cron.jsonl` by eye.
fn short_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut n = nanos;
    let mut out = String::with_capacity(12);
    for _ in 0..12 {
        out.push(ALPHABET[(n & 0x1f) as usize] as char);
        n >>= 5;
    }
    out
}

// ─── CLI rendering ──────────────────────────────────────────────────────────────

/// Multi-line, human-friendly rendering of a cron entry for the
/// `glass cron list` CLI. Includes id, kind (one-shot vs recurring), the
/// next/last fire times in local time with a relative delta, and the
/// prompt body (truncated to keep lists scannable).
pub fn format_entry_line(entry: &CronEntry, now: DateTime<Local>) -> String {
    use std::fmt::Write as _;
    const PROMPT_PREVIEW_CHARS: usize = 120;
    let mut out = String::new();

    match (entry.fire_at.as_deref(), entry.cron.as_deref()) {
        (Some(fire_at), None) => {
            let _ = writeln!(out, "  {}   one-shot", entry.id);
            match DateTime::parse_from_rfc3339(fire_at) {
                Ok(t) => {
                    let local = t.with_timezone(&Local);
                    let _ = writeln!(
                        out,
                        "    fires at: {} ({})",
                        local.format("%Y-%m-%d %H:%M"),
                        relative_delta(local - now)
                    );
                }
                Err(_) => {
                    let _ = writeln!(out, "    fires at: <unparseable: {fire_at}>");
                }
            }
        }
        (None, Some(expr)) => {
            let _ = writeln!(out, "  {}   recurring ({})", entry.id, expr);
            if let Some(lfa) = entry.last_fired_at.as_deref() {
                if let Ok(t) = DateTime::parse_from_rfc3339(lfa) {
                    let _ = writeln!(
                        out,
                        "    last fired: {}",
                        t.with_timezone(&Local).format("%Y-%m-%d %H:%M")
                    );
                }
            }
            // Next fire is computed from the schedule and last_fired_at.
            if let Ok(schedule) = parse_cron(expr) {
                let base = entry
                    .last_fired_at
                    .as_deref()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Local))
                    .unwrap_or(now);
                if let Some(next) = schedule.after(&base).next() {
                    let _ = writeln!(
                        out,
                        "    next fire:  {} ({})",
                        next.format("%Y-%m-%d %H:%M"),
                        relative_delta(next - now)
                    );
                }
            }
        }
        _ => {
            let _ = writeln!(out, "  {}   <malformed entry>", entry.id);
        }
    }

    let preview = truncate_chars(entry.what.trim(), PROMPT_PREVIEW_CHARS);
    let _ = writeln!(out, "    prompt: {preview}");
    out
}

fn relative_delta(d: ChronoDuration) -> String {
    let total = d.num_seconds();
    if total < 0 {
        return "overdue".to_string();
    }
    let days = total / 86_400;
    let hours = (total % 86_400) / 3_600;
    let mins = (total % 3_600) / 60;
    if days > 0 {
        format!("in {days}d {hours}h")
    } else if hours > 0 {
        format!("in {hours}h {mins}m")
    } else if mins > 0 {
        format!("in {mins}m")
    } else {
        "in <1m".to_string()
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    let collapsed: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if collapsed.chars().count() <= max {
        return collapsed;
    }
    let take = max.saturating_sub(3);
    let mut out: String = collapsed.chars().take(take).collect();
    out.push_str("...");
    out
}

// ─── Poller task ────────────────────────────────────────────────────────────

use crate::dispatcher::Dispatcher;
use crate::invocation_log::{InvocationContext, InvocationLog, InvocationStatus, Trigger};
use std::time::Duration;
use tokio::sync::mpsc;

pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Spawn the cron poller. Every `interval`, it asks the store for due
/// entries; for each one, it dispatches the cron manifest with the entry's
/// `what` as the prompt. The dispatcher serializes with the bus turn lock
/// — Glass is never running in two places at once. Streamed output from
/// the cron agent is dropped (cron is silent unless `send_dm` is called).
pub fn spawn_poller(
    store: CronStore,
    dispatcher: Arc<Dispatcher>,
    cron_manifest: PathBuf,
    invocations_dir: PathBuf,
    interval: Duration,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        // Fire on the very first tick (no skip), so a fresh start catches up
        // on any entries that came due while the orchestrator was offline.
        // `poll_due_pure` handles the no-spam policy: missed slots collapse
        // into a single fire with `last_fired_at` advanced to `now`.
        loop {
            tick.tick().await;
            let now = Local::now();
            let due = match store.poll_due(now).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!("cron: poll failed: {e:#}");
                    continue;
                }
            };
            for entry in due {
                let id = entry.id.clone();
                tracing::info!(
                    id = %id,
                    manifest = %cron_manifest.display(),
                    what = %entry.what,
                    "cron: firing"
                );

                // Open an invocation log so the full preamble +
                // SessionUpdate transcript lands in
                // `$GLASS_SYSTEM_DATA/invocations/`. Best-effort: if the
                // log can't be opened we still try the dispatch.
                let mut inv_log = match InvocationLog::create(
                    &invocations_dir,
                    InvocationContext {
                        trigger: Trigger::Cron,
                        manifest: cron_manifest.clone(),
                        prompt: entry.what.clone(),
                        cron_id: Some(id.clone()),
                        channel: None,
                    },
                )
                .await
                {
                    Ok(log) => Some(log),
                    Err(e) => {
                        tracing::warn!(
                            id = %id,
                            "invocation_log: failed to open for cron fire: {e:#}"
                        );
                        None
                    }
                };

                let (tx, mut rx) = mpsc::channel::<String>(16);
                // Drain the streamed output and log each rendered message
                // at info level. Cron's user-visible output is silent
                // (only `send_dm` reaches Mikayla), but we still want
                // server-side visibility into what the agent said/did when
                // diagnosing failures — the per-invocation log captures
                // the full transcript; this tracing line is for real-time
                // operator visibility.
                let drain_id = id.clone();
                let drain = tokio::spawn(async move {
                    while let Some(msg) = rx.recv().await {
                        tracing::info!(id = %drain_id, event = %msg, "cron: agent event");
                    }
                });
                let result = dispatcher
                    .dispatch(&cron_manifest, &entry.what, tx, inv_log.as_mut())
                    .await;
                // Wait for the drain to finish so any buffered messages
                // hit the log before we report success or failure.
                let _ = drain.await;

                let status = match &result {
                    Ok(()) => {
                        tracing::info!(id = %id, "cron: turn complete");
                        InvocationStatus::Ok
                    }
                    Err(e) => {
                        tracing::error!(
                            id = %id,
                            manifest = %cron_manifest.display(),
                            "cron: dispatch failed: {e:#}"
                        );
                        InvocationStatus::Err(format!("{e:#}"))
                    }
                };
                if let Some(log) = inv_log.take() {
                    let path = log.path().to_path_buf();
                    if let Err(e) = log.complete(status).await {
                        tracing::warn!(
                            path = %path.display(),
                            "invocation_log: failed to complete: {e:#}"
                        );
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn local_at(y: i32, mo: u32, d: u32, h: u32, m: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, m, 0).unwrap()
    }

    // ─── parse_when ─────────────────────────────────────────────────────

    #[test]
    fn parse_when_hh_mm_in_future_today() {
        let now = local_at(2026, 5, 13, 14, 0);
        let got = parse_when("15:30", now).unwrap();
        assert_eq!(got, local_at(2026, 5, 13, 15, 30));
    }

    #[test]
    fn parse_when_hh_mm_in_past_rolls_to_tomorrow() {
        let now = local_at(2026, 5, 13, 18, 0);
        let got = parse_when("09:00", now).unwrap();
        assert_eq!(got, local_at(2026, 5, 14, 9, 0));
    }

    #[test]
    fn parse_when_exact_local_datetime() {
        let now = local_at(2026, 5, 13, 14, 0);
        let got = parse_when("2026-05-15 09:00", now).unwrap();
        assert_eq!(got, local_at(2026, 5, 15, 9, 0));
    }

    #[test]
    fn parse_when_rejects_past_specific_datetime() {
        let now = local_at(2026, 5, 13, 14, 0);
        let err = parse_when("2026-05-12 09:00", now).unwrap_err();
        assert!(err.to_string().contains("in the past"));
    }

    #[test]
    fn parse_when_rejects_bad_format() {
        let now = local_at(2026, 5, 13, 14, 0);
        let err = parse_when("tomorrow at 3pm", now).unwrap_err();
        assert!(err.to_string().contains("expected"));
    }

    // ─── parse_cron ─────────────────────────────────────────────────────

    #[test]
    fn parse_cron_5_field_daily_at_9() {
        let schedule = parse_cron("0 9 * * *").unwrap();
        let now = local_at(2026, 5, 13, 14, 0);
        let next = schedule.after(&now).next().unwrap();
        assert_eq!(next, local_at(2026, 5, 14, 9, 0));
    }

    #[test]
    fn parse_cron_5_field_every_15_minutes() {
        let schedule = parse_cron("*/15 * * * *").unwrap();
        let now = local_at(2026, 5, 13, 14, 7);
        let next = schedule.after(&now).next().unwrap();
        assert_eq!(next, local_at(2026, 5, 13, 14, 15));
    }

    #[test]
    fn parse_cron_rejects_garbage() {
        let err = parse_cron("not a cron").unwrap_err();
        assert!(err.to_string().contains("invalid"));
    }

    // ─── append + read round-trip ───────────────────────────────────────

    #[tokio::test]
    async fn append_one_shot_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CronStore::new(dir.path().join("cron.jsonl"));
        let now = local_at(2026, 5, 13, 14, 0);

        let id = store
            .append("dance reminder", Some("15:30"), None, now)
            .await
            .unwrap();
        assert_eq!(id.len(), 12);

        let raw = tokio::fs::read_to_string(store.path()).await.unwrap();
        let entry: CronEntry = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(entry.id, id);
        assert_eq!(entry.what, "dance reminder");
        assert!(entry.cron.is_none());
        let fire_at = entry.fire_at.unwrap();
        let parsed = DateTime::parse_from_rfc3339(&fire_at).unwrap();
        assert_eq!(parsed.with_timezone(&Local), local_at(2026, 5, 13, 15, 30));
        assert!(entry.last_fired_at.is_none());
    }

    #[tokio::test]
    async fn append_recurring_seeds_last_fired_at_now() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CronStore::new(dir.path().join("cron.jsonl"));
        let now = local_at(2026, 5, 13, 14, 0);

        store
            .append("morning routine", None, Some("0 9 * * *"), now)
            .await
            .unwrap();

        let raw = tokio::fs::read_to_string(store.path()).await.unwrap();
        let entry: CronEntry = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(entry.cron.as_deref(), Some("0 9 * * *"));
        // Seeded to `now` so the first fire is tomorrow 9am, not every past 9am.
        let lfa = entry.last_fired_at.unwrap();
        let parsed = DateTime::parse_from_rfc3339(&lfa).unwrap();
        assert_eq!(parsed.with_timezone(&Local), now);
    }

    #[tokio::test]
    async fn append_rejects_unparseable_cron() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CronStore::new(dir.path().join("cron.jsonl"));
        let now = local_at(2026, 5, 13, 14, 0);
        let err = store
            .append("x", None, Some("garbage"), now)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid cron"));
        assert!(!store.path().exists(), "no file should be created");
    }

    // ─── poll_due_pure ──────────────────────────────────────────────────

    fn entry_one_shot(id: &str, fire_at: DateTime<Local>) -> CronEntry {
        CronEntry {
            id: id.into(),
            what: format!("do-{id}"),
            fire_at: Some(fire_at.to_rfc3339()),
            cron: None,
            last_fired_at: None,
        }
    }

    fn entry_recurring(id: &str, expr: &str, last_fired_at: DateTime<Local>) -> CronEntry {
        CronEntry {
            id: id.into(),
            what: format!("do-{id}"),
            fire_at: None,
            cron: Some(expr.into()),
            last_fired_at: Some(last_fired_at.to_rfc3339()),
        }
    }

    #[test]
    fn poll_one_shot_fires_and_is_removed() {
        let now = local_at(2026, 5, 13, 14, 0);
        let due = entry_one_shot("a", local_at(2026, 5, 13, 13, 0));
        let not_due = entry_one_shot("b", local_at(2026, 5, 13, 15, 0));

        let result = poll_due_pure(vec![due.clone(), not_due.clone()], now);
        assert_eq!(result.fire, vec![due]);
        assert_eq!(result.keep, vec![not_due]);
    }

    #[test]
    fn poll_recurring_fires_when_slot_elapsed_updates_last_fired_at() {
        let now = local_at(2026, 5, 13, 14, 0);
        // Daily 9am; last fired yesterday 9am. Today 9am has elapsed → fire.
        let entry = entry_recurring("a", "0 9 * * *", local_at(2026, 5, 12, 9, 0));

        let result = poll_due_pure(vec![entry.clone()], now);
        assert_eq!(result.fire.len(), 1);
        assert_eq!(result.fire[0].id, "a");
        // last_fired_at advanced to `now` (not to the missed-slot time)
        // so the no-spam policy holds even for long offline gaps.
        let lfa = result.fire[0].last_fired_at.as_deref().unwrap();
        let parsed = DateTime::parse_from_rfc3339(lfa).unwrap();
        assert_eq!(parsed.with_timezone(&Local), now);
        // Keep the entry too (recurring stays).
        assert_eq!(result.keep.len(), 1);
        assert_eq!(result.keep[0].id, "a");
    }

    #[test]
    fn poll_recurring_skips_when_no_slot_elapsed() {
        // Last fired today 9am; daily 9am; now is today 2pm. Next slot is
        // tomorrow 9am — not yet elapsed. Don't fire.
        let now = local_at(2026, 5, 13, 14, 0);
        let entry = entry_recurring("a", "0 9 * * *", local_at(2026, 5, 13, 9, 0));

        let result = poll_due_pure(vec![entry.clone()], now);
        assert_eq!(result.fire, vec![]);
        assert_eq!(result.keep, vec![entry]);
    }

    #[test]
    fn poll_recurring_long_offline_fires_once_not_per_missed_slot() {
        // Daily 9am cron, last fired 10 days ago. The poll should fire ONE
        // time and update last_fired_at to now — not enqueue 10 fires.
        let now = local_at(2026, 5, 13, 14, 0);
        let entry = entry_recurring("a", "0 9 * * *", local_at(2026, 5, 3, 9, 0));

        let result = poll_due_pure(vec![entry], now);
        assert_eq!(result.fire.len(), 1);
        let lfa = result.fire[0].last_fired_at.as_deref().unwrap();
        let parsed = DateTime::parse_from_rfc3339(lfa).unwrap();
        assert_eq!(parsed.with_timezone(&Local), now);
    }

    #[test]
    fn poll_drops_malformed_entries() {
        let now = local_at(2026, 5, 13, 14, 0);
        let bad = CronEntry {
            id: "broken".into(),
            what: "x".into(),
            fire_at: None,
            cron: None,
            last_fired_at: None,
        };
        let bad_cron = CronEntry {
            id: "bad_cron".into(),
            what: "x".into(),
            fire_at: None,
            cron: Some("not-a-cron".into()),
            last_fired_at: None,
        };
        let result = poll_due_pure(vec![bad, bad_cron], now);
        assert_eq!(result.fire, vec![]);
        assert_eq!(result.keep, vec![]);
    }

    // ─── Concurrency through CronStore::append + CronStore::poll_due ────

    #[tokio::test]
    async fn append_and_poll_round_trip_through_store() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CronStore::new(dir.path().join("cron.jsonl"));
        let now = local_at(2026, 5, 13, 14, 0);

        // Append two one-shots, one already-due, one in the future.
        store
            .append("past", Some("13:00"), None, local_at(2026, 5, 13, 12, 0))
            .await
            .unwrap();
        store
            .append("future", Some("15:00"), None, now)
            .await
            .unwrap();

        // At 14:00, only "past" (resolved to 13:00 in the morning) has fired.
        // Wait — "13:00" with morning-now resolves to 13:00 today. But we
        // called append with `now = 12:00`, so 13:00 today is in the future
        // at append time. By the time we poll at 14:00 it's elapsed.
        let due = store.poll_due(now).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].what, "past");

        // The remaining entry is the future one.
        let raw = tokio::fs::read_to_string(store.path()).await.unwrap();
        let lines: Vec<_> = raw.lines().collect();
        assert_eq!(lines.len(), 1);
        let entry: CronEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(entry.what, "future");
    }

    // ─── format_entry_line ───────────────────────────────────────────────────

    #[test]
    fn format_entry_line_one_shot_shows_relative_delta_and_prompt() {
        let now = local_at(2026, 5, 13, 14, 0);
        let entry = entry_one_shot("abc123def456", local_at(2026, 5, 13, 20, 0));
        let out = format_entry_line(&entry, now);
        assert!(out.contains("abc123def456   one-shot"));
        assert!(out.contains("fires at: 2026-05-13 20:00 (in 6h 0m)"));
        assert!(out.contains("prompt: do-abc123def456"));
    }

    #[test]
    fn format_entry_line_recurring_shows_last_and_next() {
        let now = local_at(2026, 5, 13, 14, 0);
        let entry = entry_recurring("recurr12abcd", "0 9 * * *", local_at(2026, 5, 13, 9, 0));
        let out = format_entry_line(&entry, now);
        assert!(out.contains("recurr12abcd   recurring (0 9 * * *)"));
        assert!(out.contains("last fired: 2026-05-13 09:00"));
        assert!(out.contains("next fire:  2026-05-14 09:00 (in 19h 0m)"));
    }

    #[test]
    fn format_entry_line_overdue_one_shot_is_labeled() {
        let now = local_at(2026, 5, 13, 14, 0);
        let entry = entry_one_shot("latelatelate", local_at(2026, 5, 13, 9, 0));
        let out = format_entry_line(&entry, now);
        assert!(out.contains("(overdue)"));
    }

    #[test]
    fn format_entry_line_truncates_long_prompts() {
        let now = local_at(2026, 5, 13, 14, 0);
        let mut entry = entry_one_shot("longprompt12", local_at(2026, 5, 13, 20, 0));
        entry.what = "x".repeat(500);
        let out = format_entry_line(&entry, now);
        let prompt_line = out
            .lines()
            .find(|l| l.trim_start().starts_with("prompt:"))
            .unwrap();
        // "    prompt: " + 120-char body. The trailing "..." is in the 120.
        assert!(prompt_line.ends_with("..."));
        let body = prompt_line.trim_start().trim_start_matches("prompt: ");
        assert_eq!(body.chars().count(), 120);
    }

    // ─── CronStore::list ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_returns_entries_in_file_order() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CronStore::new(dir.path().join("cron.jsonl"));
        let now = local_at(2026, 5, 13, 14, 0);

        store
            .append("first", Some("15:00"), None, now)
            .await
            .unwrap();
        store
            .append("second", Some("16:00"), None, now)
            .await
            .unwrap();
        store
            .append("third", None, Some("0 9 * * *"), now)
            .await
            .unwrap();

        let entries = store.list().await.unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].what, "first");
        assert_eq!(entries[1].what, "second");
        assert_eq!(entries[2].what, "third");
    }

    #[tokio::test]
    async fn list_on_missing_file_returns_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CronStore::new(dir.path().join("cron.jsonl"));
        assert_eq!(store.list().await.unwrap(), vec![]);
    }

    // ─── CronStore::remove ──────────────────────────────────────────────────

    async fn populated_store() -> (tempfile::TempDir, CronStore) {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CronStore::new(dir.path().join("cron.jsonl"));
        let now = local_at(2026, 5, 13, 14, 0);
        store
            .append("first", Some("15:00"), None, now)
            .await
            .unwrap();
        store
            .append("second", Some("16:00"), None, now)
            .await
            .unwrap();
        store
            .append("third", None, Some("0 9 * * *"), now)
            .await
            .unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn remove_by_exact_id_drops_one_entry() {
        let (_dir, store) = populated_store().await;
        let target = store.list().await.unwrap()[1].clone();

        let result = store.remove(&target.id).await.unwrap();
        assert_eq!(result, RemoveResult::Removed(target.clone()));

        let remaining = store.list().await.unwrap();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.iter().all(|e| e.id != target.id));
    }

    #[tokio::test]
    async fn remove_by_unique_prefix_drops_the_match() {
        let (_dir, store) = populated_store().await;
        let target = store.list().await.unwrap()[0].clone();
        // First 6 chars of a 12-char base32 id are very likely unique
        // across 3 entries; assert that before relying on it.
        let prefix = &target.id[..6];
        let others = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.id != target.id && e.id.starts_with(prefix))
            .count();
        assert_eq!(others, 0, "prefix collision in test fixture");

        let result = store.remove(prefix).await.unwrap();
        assert_eq!(result, RemoveResult::Removed(target));
    }

    #[tokio::test]
    async fn remove_ambiguous_prefix_returns_match_list_without_deleting() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CronStore::new(dir.path().join("cron.jsonl"));
        let now = local_at(2026, 5, 13, 14, 0);
        // Synthesize two entries with the same id prefix to force ambiguity.
        let entries = vec![
            CronEntry {
                id: "aaa111bbb222".into(),
                what: "one".into(),
                fire_at: Some(now.to_rfc3339()),
                cron: None,
                last_fired_at: None,
            },
            CronEntry {
                id: "aaa333ccc444".into(),
                what: "two".into(),
                fire_at: Some(now.to_rfc3339()),
                cron: None,
                last_fired_at: None,
            },
        ];
        write_entries(store.path(), &entries).await.unwrap();

        let result = store.remove("aaa").await.unwrap();
        match result {
            RemoveResult::Ambiguous(ids) => {
                assert_eq!(ids, vec!["aaa111bbb222", "aaa333ccc444"]);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
        // Nothing was deleted.
        assert_eq!(store.list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn remove_no_match_returns_not_found() {
        let (_dir, store) = populated_store().await;
        let before = store.list().await.unwrap();

        let result = store.remove("zzzzzzzzzzzz").await.unwrap();
        assert_eq!(result, RemoveResult::NotFound);

        // File untouched.
        assert_eq!(store.list().await.unwrap(), before);
    }

    #[tokio::test]
    async fn remove_empty_input_errors() {
        let (_dir, store) = populated_store().await;
        let err = store.remove("   ").await.unwrap_err();
        assert!(err.to_string().contains("id is required"));
    }

    #[tokio::test]
    async fn poll_due_on_missing_file_returns_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CronStore::new(dir.path().join("cron.jsonl"));
        let now = local_at(2026, 5, 13, 14, 0);
        let due = store.poll_due(now).await.unwrap();
        assert!(due.is_empty());
        assert!(!store.path().exists());
    }
}
