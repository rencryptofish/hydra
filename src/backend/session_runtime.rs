use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::agent::{provider_for, StatusStrategy};
use crate::backend::state::{OutputDetector, TaskTimers};
use crate::logs::SessionStats;
use crate::session::{Session, SessionStatus};

pub(crate) struct SessionRuntime {
    output_detector: OutputDetector,
    timers: TaskTimers,
    dead_ticks: HashMap<String, u8>,
}

impl SessionRuntime {
    const DEAD_TICK_THRESHOLD: u8 = 3;
    const DEAD_TICK_SUBAGENT_THRESHOLD: u8 = 15;

    pub(crate) fn new() -> Self {
        Self {
            output_detector: OutputDetector::new(),
            timers: TaskTimers::new(),
            dead_ticks: HashMap::new(),
        }
    }

    pub(crate) fn record_output(&mut self, tmux_name: &str) {
        self.output_detector.record_output(tmux_name);
    }

    pub(crate) fn apply_statuses(
        &mut self,
        sessions: &mut [Session],
        prev_statuses: &HashMap<String, SessionStatus>,
        session_stats: &HashMap<String, SessionStats>,
        pane_status: Option<&HashMap<String, (bool, u64)>>,
        use_output_events: bool,
        now: Instant,
    ) {
        for session in sessions.iter_mut() {
            let tmux_name = session.tmux_name.clone();
            let is_dead = pane_status
                .and_then(|map| map.get(&tmux_name))
                .map(|&(dead, _)| dead)
                .unwrap_or(false);

            if is_dead {
                session.status =
                    self.apply_exited_debounce(&tmux_name, prev_statuses, session_stats);
                continue;
            }

            self.dead_ticks.insert(tmux_name.clone(), 0);

            let log_running = session_stats
                .get(&tmux_name)
                .and_then(|stats| stats.task_elapsed())
                .is_some();
            let recent_output = self.output_detector.status(&tmux_name) == SessionStatus::Running;
            let has_log_stats = session_stats.contains_key(&tmux_name);
            let strategy = provider_for(&session.agent_type).preferred_status_strategy();

            let running = match strategy {
                StatusStrategy::JsonlActivity => {
                    // Prefer durable log events when available, but allow output
                    // events as a startup fallback until logs are discovered.
                    log_running || (!has_log_stats && recent_output)
                }
                StatusStrategy::OutputEvent => {
                    if use_output_events {
                        recent_output || log_running
                    } else {
                        log_running || recent_output
                    }
                }
            };

            session.status = if running {
                SessionStatus::Running
            } else {
                SessionStatus::Idle
            };
        }

        self.timers.update(sessions, session_stats, now);
    }

    pub(crate) fn prune(&mut self, live_keys: &HashSet<&String>) {
        self.output_detector.prune(live_keys);
        self.timers.prune(live_keys);
        self.dead_ticks.retain(|k, _| live_keys.contains(k));
    }

    fn apply_exited_debounce(
        &mut self,
        tmux_name: &str,
        prev_statuses: &HashMap<String, SessionStatus>,
        session_stats: &HashMap<String, SessionStats>,
    ) -> SessionStatus {
        let has_active_subagents = session_stats
            .get(tmux_name)
            .map(|stats| stats.active_subagents > 0)
            .unwrap_or(false);

        let threshold = if has_active_subagents {
            Self::DEAD_TICK_SUBAGENT_THRESHOLD
        } else {
            Self::DEAD_TICK_THRESHOLD
        };

        let count = self.dead_ticks.entry(tmux_name.to_string()).or_insert(0);
        *count = count.saturating_add(1);

        if *count < threshold {
            prev_statuses
                .get(tmux_name)
                .filter(|status| **status != SessionStatus::Exited)
                .cloned()
                .unwrap_or(SessionStatus::Idle)
        } else {
            SessionStatus::Exited
        }
    }
}
