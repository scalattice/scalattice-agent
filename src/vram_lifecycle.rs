use std::time::{Duration, Instant};

use crate::protocol::AgentSchedule;
use crate::runtime::JobState;

#[derive(Debug, Clone)]
pub struct VramLifecycleConfig {
    pub vram_idle_secs: u64,
    pub post_job_idle_secs: u64,
    pub preload_lead_minutes: u32,
}

impl VramLifecycleConfig {
    pub fn from_env() -> Self {
        Self {
            vram_idle_secs: env_u64("SCALATTICE_VRAM_IDLE_SECS", 600),
            post_job_idle_secs: env_u64("SCALATTICE_POST_JOB_IDLE_SECS", 120),
            preload_lead_minutes: env_u32("SCALATTICE_PRELOAD_LEAD_MINUTES", 15),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct VramLifecycleState {
    pub schedule: AgentSchedule,
    pub last_vram_activity: Option<Instant>,
    pub last_job_finished_at: Option<Instant>,
    pub post_job_evicted: bool,
    had_schedule: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleTransition {
    pub entered_earning: bool,
    pub left_earning: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VramTickAction {
    None,
    EvictVram,
}

impl VramLifecycleState {
    pub fn apply_schedule(&mut self, schedule: AgentSchedule) -> ScheduleTransition {
        let config = VramLifecycleConfig::from_env();
        let was_earning = self.had_schedule && self.schedule.earning_soon(&config);
        self.schedule = schedule;
        self.had_schedule = true;
        let now_earning = self.schedule.earning_soon(&config);
        ScheduleTransition {
            entered_earning: !was_earning && now_earning,
            left_earning: was_earning && !now_earning,
        }
    }

    pub fn on_job_started(&mut self) {
        self.post_job_evicted = false;
        self.last_vram_activity = Some(Instant::now());
        self.last_job_finished_at = None;
    }

    pub fn on_job_finished(&mut self) {
        let now = Instant::now();
        self.last_job_finished_at = Some(now);
        self.last_vram_activity = Some(now);
    }

    pub fn on_vram_loaded(&mut self) {
        self.last_vram_activity = Some(Instant::now());
        self.post_job_evicted = false;
    }

    pub fn should_preload(&self, config: &VramLifecycleConfig) -> bool {
        self.schedule.earning_soon(config)
    }

    pub fn tick(&mut self, job_state: JobState, config: &VramLifecycleConfig) -> VramTickAction {
        if job_state == JobState::Busy {
            return VramTickAction::None;
        }

        if !self.schedule.earning_soon(config) {
            return VramTickAction::EvictVram;
        }

        let now = Instant::now();
        if let Some(finished) = self.last_job_finished_at {
            if !self.post_job_evicted
                && now.duration_since(finished) >= Duration::from_secs(config.post_job_idle_secs)
            {
                self.post_job_evicted = true;
                return VramTickAction::EvictVram;
            }
        }

        if let Some(activity) = self.last_vram_activity {
            if now.duration_since(activity) >= Duration::from_secs(config.vram_idle_secs) {
                return VramTickAction::EvictVram;
            }
        }

        VramTickAction::None
    }
}

impl AgentSchedule {
    pub fn earning_soon(&self, config: &VramLifecycleConfig) -> bool {
        if self.accepting_jobs {
            return true;
        }
        self.minutes_until_earning
            .is_some_and(|minutes| minutes <= config.preload_lead_minutes)
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .unwrap_or(default)
}

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .unwrap_or(default)
}
