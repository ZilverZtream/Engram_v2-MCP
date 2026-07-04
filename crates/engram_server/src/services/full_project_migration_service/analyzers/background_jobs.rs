//! Extracted analyzer: background jobs.
//!
//! Part of the Phase 2 refactor that split the 13k-line
//! `full_project_migration_service.rs` into focused submodules.
//! No behaviour was changed during the move; every function lives
//! here exactly as before, just under a narrower module boundary.

#![allow(unused_imports)]

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use engram_graph::{EdgeKind, GraphStore};
use regex::Regex;

use super::super::model::*;
// Wildcard catches parent-module `pub(super) static` / `type` /
// `pub(crate) fn` helpers that were left in the grandparent during
// the Phase 2 extraction.
use super::super::super::auth_config_service::AuthConfigMap;
use super::super::super::db_strategy_service::{self, FileDataAccessProfile};
use super::super::super::dossier_service::{self, MigrationDossier};
use super::super::super::migration_order_service::{self, MigrationOrderPlan};
use super::super::super::pattern_detection_service;
use super::super::super::state_migration_service::{self, StateMigrationReport};
use super::super::*;

pub(crate) fn detect_background_job_patterns(
    code_files: &[(&str, &str)],
    global_asax_content: Option<&str>,
) -> BackgroundJobReport {
    static THREAD_POOL_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\bThreadPool\.QueueUserWorkItem\s*\(").expect("valid regex")
    });
    static BG_WORKER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:New\s+)?BackgroundWorker\b").expect("valid regex")
    });
    static TASK_RUN_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)\bTask\.Run\s*\(").expect("valid regex"));
    static TIMER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:New\s+)?(?:System\.(?:Timers|Threading)\.)?Timer\s*\(")
            .expect("valid regex")
    });
    static THREAD_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:New\s+)?Thread\s*\(\s*(?:AddressOf|New\s+ThreadStart|New\s+ParameterizedThreadStart)\s").expect("valid regex")
    });
    static HANGFIRE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(
            r"(?i)\bBackgroundJob\.(?:Enqueue|Schedule|ContinueWith|ContinueJobWith)\s*[\(<]",
        )
        .expect("valid regex")
    });
    static QUARTZ_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:IScheduler|JobBuilder\.Create|TriggerBuilder\.Create)\b")
            .expect("valid regex")
    });
    static SELF_CALL_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)WebClient\s*\(\s*\)\.Download(?:String|Data)\s*\(\s*"(?:http|~/)"#)
            .expect("valid regex")
    });

    struct BgDef {
        re: &'static std::sync::LazyLock<Regex>,
        pattern_type: &'static str,
        modern: &'static str,
        risk: &'static str,
    }

    let bg_defs: Vec<BgDef> = vec![
        BgDef {
            re: &THREAD_POOL_RE,
            pattern_type: "ThreadPool.QueueUserWorkItem",
            modern: "BackgroundService + Channel<T>",
            risk: "high",
        },
        BgDef {
            re: &BG_WORKER_RE,
            pattern_type: "BackgroundWorker",
            modern: "BackgroundService",
            risk: "medium",
        },
        BgDef {
            re: &TASK_RUN_RE,
            pattern_type: "Task.Run (fire-and-forget)",
            modern: "Hangfire BackgroundJob.Enqueue or IHostedService",
            risk: "high",
        },
        BgDef {
            re: &TIMER_RE,
            pattern_type: "Timer",
            modern: "IHostedService with PeriodicTimer",
            risk: "medium",
        },
        BgDef {
            re: &THREAD_RE,
            pattern_type: "Thread creation",
            modern: "BackgroundService or Task.Run with lifetime management",
            risk: "high",
        },
        BgDef {
            re: &HANGFIRE_RE,
            pattern_type: "Hangfire",
            modern: "Hangfire (already compatible)",
            risk: "low",
        },
        BgDef {
            re: &QUARTZ_RE,
            pattern_type: "Quartz.NET",
            modern: "Quartz.NET (already compatible)",
            risk: "low",
        },
        BgDef {
            re: &SELF_CALL_RE,
            pattern_type: "Self-call timer (WebClient)",
            modern: "IHostedService + HttpClientFactory",
            risk: "high",
        },
    ];

    let mut patterns: Vec<BackgroundJobPattern> = Vec::new();
    let mut bg_files: Vec<String> = Vec::new();
    let mut uses_thread_pool = false;
    let mut uses_timers = false;
    let mut uses_task_run = false;
    let mut uses_bg_worker = false;
    let mut uses_hangfire = false;
    let mut uses_quartz = false;
    let mut fire_and_forget = 0usize;

    let all_code: Vec<(&str, &str)> = code_files
        .iter()
        .copied()
        .chain(global_asax_content.map(|c| ("Global.asax", c)))
        .collect();

    for (path, content) in &all_code {
        let mut file_has_bg = false;
        for def in &bg_defs {
            let count = def.re.find_iter(content).count();
            if count > 0 {
                patterns.push(BackgroundJobPattern {
                    file_path: path.to_string(),
                    pattern_type: def.pattern_type.to_string(),
                    count,
                    modern_equivalent: def.modern.to_string(),
                    risk_level: def.risk.to_string(),
                });
                file_has_bg = true;

                match def.pattern_type {
                    "ThreadPool.QueueUserWorkItem" => {
                        uses_thread_pool = true;
                        fire_and_forget += count;
                    }
                    "BackgroundWorker" => uses_bg_worker = true,
                    "Task.Run (fire-and-forget)" => {
                        uses_task_run = true;
                        fire_and_forget += count;
                    }
                    "Timer" => uses_timers = true,
                    "Thread creation" => fire_and_forget += count,
                    "Hangfire" => uses_hangfire = true,
                    "Quartz.NET" => uses_quartz = true,
                    _ => {}
                }
            }
        }
        if file_has_bg {
            bg_files.push(path.to_string());
        }
    }
    bg_files.sort();
    bg_files.dedup();

    BackgroundJobReport {
        has_background_jobs: !patterns.is_empty(),
        total_background_files: bg_files.len(),
        uses_thread_pool,
        uses_timers,
        uses_task_run,
        uses_bg_worker,
        uses_hangfire,
        uses_quartz,
        fire_and_forget_count: fire_and_forget,
        patterns,
    }
}
