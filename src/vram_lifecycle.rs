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
    /// Off-schedule eviction already ran; avoid heartbeat/pong spam.
    pub off_schedule_evicted: bool,
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
        let transition = ScheduleTransition {
            entered_earning: !was_earning && now_earning,
            left_earning: was_earning && !now_earning,
        };
        if transition.entered_earning {
            self.off_schedule_evicted = false;
            self.post_job_evicted = false;
        }
        transition
    }

    pub fn on_job_started(&mut self) {
        self.post_job_evicted = false;
        self.off_schedule_evicted = false;
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
        self.off_schedule_evicted = false;
    }

    pub fn should_preload(&self, config: &VramLifecycleConfig) -> bool {
        self.schedule.earning_soon(config)
    }

    pub fn tick(&mut self, job_state: JobState, config: &VramLifecycleConfig) -> VramTickAction {
        if job_state == JobState::Busy {
            return VramTickAction::None;
        }

        if !self.schedule.earning_soon(config) {
            // Off-schedule: free VRAM once, but keep a post-job grace so debug /
            // probes aren't immediately cold-started again on the next request.
            if let Some(finished) = self.last_job_finished_at {
                let now = Instant::now();
                if now.duration_since(finished) < Duration::from_secs(config.post_job_idle_secs) {
                    return VramTickAction::None;
                }
            }
            if self.off_schedule_evicted {
                return VramTickAction::None;
            }
            self.off_schedule_evicted = true;
            return VramTickAction::EvictVram;
        }

        // On-schedule / earning soon: server warm plan owns residency. Do not
        // idle-evict while accepting jobs or in the preload lead window.
        let _ = config.vram_idle_secs;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> VramLifecycleConfig {
        VramLifecycleConfig {
            vram_idle_secs: 600,
            post_job_idle_secs: 120,
            preload_lead_minutes: 15,
        }
    }

    fn accepting() -> AgentSchedule {
        AgentSchedule {
            accepting_jobs: true,
            minutes_until_earning: None,
        }
    }

    fn offline() -> AgentSchedule {
        AgentSchedule {
            accepting_jobs: false,
            minutes_until_earning: None,
        }
    }

    fn preload_soon() -> AgentSchedule {
        AgentSchedule {
            accepting_jobs: false,
            minutes_until_earning: Some(10),
        }
    }

    #[test]
    fn accepting_never_idle_evicts_even_after_long_idle() {
        let mut state = VramLifecycleState::default();
        state.schedule = accepting();
        state.had_schedule = true;
        state.last_vram_activity = Some(Instant::now() - Duration::from_secs(10_000));
        assert_eq!(
            state.tick(JobState::Idle, &cfg()),
            VramTickAction::None
        );
    }

    #[test]
    fn preload_lead_never_idle_evicts() {
        let mut state = VramLifecycleState::default();
        state.schedule = preload_soon();
        state.had_schedule = true;
        state.last_vram_activity = Some(Instant::now() - Duration::from_secs(10_000));
        assert_eq!(
            state.tick(JobState::Idle, &cfg()),
            VramTickAction::None
        );
    }

    #[test]
    fn off_schedule_evicts_once_then_silent() {
        let mut state = VramLifecycleState::default();
        state.schedule = offline();
        state.had_schedule = true;
        state.last_job_finished_at = Some(Instant::now() - Duration::from_secs(200));
        assert_eq!(
            state.tick(JobState::Idle, &cfg()),
            VramTickAction::EvictVram
        );
        assert!(state.off_schedule_evicted);
        assert_eq!(
            state.tick(JobState::Idle, &cfg()),
            VramTickAction::None
        );
    }

    #[test]
    fn off_schedule_respects_post_job_grace() {
        let mut state = VramLifecycleState::default();
        state.schedule = offline();
        state.had_schedule = true;
        state.last_job_finished_at = Some(Instant::now() - Duration::from_secs(30));
        assert_eq!(
            state.tick(JobState::Idle, &cfg()),
            VramTickAction::None
        );
        assert!(!state.off_schedule_evicted);
    }

    #[test]
    fn entered_earning_resets_off_schedule_latch() {
        let mut state = VramLifecycleState::default();
        state.schedule = offline();
        state.had_schedule = true;
        state.off_schedule_evicted = true;
        let transition = state.apply_schedule(accepting());
        assert!(transition.entered_earning);
        assert!(!state.off_schedule_evicted);
    }

    #[test]
    fn busy_never_evicts() {
        let mut state = VramLifecycleState::default();
        state.schedule = offline();
        state.had_schedule = true;
        assert_eq!(
            state.tick(JobState::Busy, &cfg()),
            VramTickAction::None
        );
    }
}
