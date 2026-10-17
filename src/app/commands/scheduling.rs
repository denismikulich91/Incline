//! Schedules of optimization scenarios: add, edit, save, delete, duplicate,
//! rename, and move or copy one to another scenario. Every change is handed
//! to the project with the scenarios (`write_optimization_scenarios`), so the
//! project reads unsaved until it is saved. See
//! `.claude/specs/scheduling/stage-1-ui.md`.

use anyhow::Result;

use crate::{
    app::App,
    i18n::tr,
    model::{
        optimization::{OptimizationScenario, copy_name, unique_name},
        scheduling::Schedule,
    },
    ui::state::{ScheduleDraft, ScheduleStatus},
    userspace_log, userspace_warn,
};

/// A name no other schedule of `scenario` has (other than `id` itself).
fn free_name(scenario: &OptimizationScenario, id: u64, wanted: &str) -> String {
    unique_name(
        scenario.schedules.iter().filter(|schedule| schedule.id != id).map(|schedule| schedule.name.as_str()),
        wanted,
    )
}

fn copy_of(scenario: &OptimizationScenario, name: &str) -> String {
    copy_name(scenario.schedules.iter().map(|schedule| schedule.name.as_str()), name, " ", &tr!("opt-copy-suffix"))
}

impl<'a> App<'a> {
    fn scenario_mut(&mut self, id: u64) -> Option<&mut OptimizationScenario> {
        self.editor.optimization.scenarios.iter_mut().find(|scenario| scenario.id == id)
    }

    /// Start a new schedule of `parent` in the schedule editor, hiding the list.
    pub(crate) fn add_schedule(&mut self, parent: u64) {
        let id = self.editor.optimization.fresh_id();
        let Some(scenario) = self.editor.optimization.scenarios.iter().find(|scenario| scenario.id == parent) else {
            return;
        };
        let name = free_name(scenario, id, &tr!("sched-default-name"));
        let mut schedule = Schedule::new(id, name);
        // One row per method and sold element, so the limits start complete.
        schedule.add_all_limits(scenario);
        self.editor.optimization.schedule_draft = Some(ScheduleDraft::new(parent, schedule, true));
        self.editor.optimization.list_open = false;
    }

    pub(crate) fn edit_schedule(&mut self, parent: u64, id: u64) {
        let state = &mut self.editor.optimization;
        let Some(schedule) = state
            .scenarios
            .iter()
            .find(|scenario| scenario.id == parent)
            .and_then(|scenario| scenario.schedules.iter().find(|schedule| schedule.id == id))
            .cloned()
        else {
            return;
        };
        state.schedule_draft = Some(ScheduleDraft::new(parent, schedule, false));
        state.list_open = false;
    }

    /// Put the edited schedule in its scenario (replacing its earlier copy, or
    /// added at the end) and hand the scenarios to the project. The editor
    /// stays open, holding it as saved, unless closing.
    pub(crate) fn save_schedule(&mut self, parent: u64, mut schedule: Schedule, then_close: bool) -> Result<()> {
        let Some(scenario) = self.scenario_mut(parent) else {
            anyhow::bail!("{}", tr!("sched-parent-gone"));
        };
        schedule.name = free_name(scenario, schedule.id, schedule.name.trim());
        match scenario.schedules.iter_mut().find(|existing| existing.id == schedule.id) {
            // The name is changed in the list only; the editor's copy never wins over it.
            Some(existing) => {
                schedule.name = existing.name.clone();
                *existing = schedule.clone();
            }
            None => scenario.schedules.push(schedule.clone()),
        }
        self.editor.optimization.expanded.insert(parent);
        self.write_optimization_scenarios()?;
        let state = &mut self.editor.optimization;
        if then_close {
            state.schedule_draft = None;
            state.list_open = true;
        } else if let Some(draft) = state.schedule_draft.as_mut().filter(|draft| draft.schedule.id == schedule.id) {
            draft.schedule.name = schedule.name.clone();
            draft.saved = schedule;
            draft.is_new = false;
            draft.confirm_close = false;
        }
        Ok(())
    }

    pub(crate) fn delete_schedule(&mut self, parent: u64, id: u64) -> Result<()> {
        if let Some(scenario) = self.scenario_mut(parent) {
            scenario.schedules.retain(|schedule| schedule.id != id);
        }
        let state = &mut self.editor.optimization;
        state.schedule_runs.remove(&id);
        state.schedule_summaries.remove(&id);
        self.write_optimization_scenarios()
    }

    /// Copy a schedule to a new one right after it. The copy has not been run.
    pub(crate) fn duplicate_schedule(&mut self, parent: u64, id: u64) -> Result<()> {
        let fresh = self.editor.optimization.fresh_id();
        let Some(scenario) = self.scenario_mut(parent) else {
            return Ok(());
        };
        let Some(position) = scenario.schedules.iter().position(|schedule| schedule.id == id) else {
            return Ok(());
        };
        let mut copy = scenario.schedules[position].clone();
        copy.id = fresh;
        copy.name = copy_of(scenario, &copy.name);
        scenario.schedules.insert(position + 1, copy);
        self.write_optimization_scenarios()
    }

    pub(crate) fn rename_schedule(&mut self, parent: u64, id: u64, name: String) -> Result<()> {
        let name = name.trim().to_owned();
        let Some(scenario) = self.scenario_mut(parent) else {
            return Ok(());
        };
        let name = free_name(scenario, id, &name);
        match scenario.schedules.iter_mut().find(|schedule| schedule.id == id) {
            Some(schedule) if !name.is_empty() && schedule.name != name => schedule.name = name.clone(),
            _ => return Ok(()),
        }
        // The open editor shows the new name in its title.
        if let Some(draft) = self.editor.optimization.schedule_draft.as_mut().filter(|draft| draft.schedule.id == id) {
            draft.schedule.name = name.clone();
            draft.saved.name = name;
        }
        self.write_optimization_scenarios()
    }

    /// Move a schedule to scenario `to`, or (`keep`) copy it there. It keeps
    /// every setting, including those `to` has no counterpart for: they show
    /// red there rather than being dropped.
    pub(crate) fn move_schedule(&mut self, id: u64, from: u64, to: u64, keep: bool) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let fresh = self.editor.optimization.fresh_id();
        let Some(source) = self.scenario_mut(from) else {
            return Ok(());
        };
        let Some(position) = source.schedules.iter().position(|schedule| schedule.id == id) else {
            return Ok(());
        };
        let mut schedule = if keep { source.schedules[position].clone() } else { source.schedules.remove(position) };
        let Some(target) = self.scenario_mut(to) else {
            // Put back what was taken: there is nowhere to move it.
            if !keep && let Some(source) = self.scenario_mut(from) {
                source.schedules.insert(position, schedule);
            }
            return Ok(());
        };
        if keep {
            schedule.id = fresh;
        }
        let taken = target.schedules.iter().any(|existing| existing.name.eq_ignore_ascii_case(&schedule.name));
        if taken {
            schedule.name = copy_of(target, &schedule.name);
        }
        let issues = schedule.issues(target).len();
        let (name, scenario) = (schedule.name.clone(), target.name.clone());
        target.schedules.push(schedule);
        if !keep {
            // Its run was of the old scenario's shells.
            let state = &mut self.editor.optimization;
            state.schedule_runs.remove(&id);
            state.schedule_summaries.remove(&id);
        }
        let message = if keep {
            tr!("sched-copied-to", name = name, scenario = scenario)
        } else {
            tr!("sched-moved-to", name = name, scenario = scenario)
        };
        userspace_log!("{message}");
        if issues > 0 {
            userspace_warn!("{}", tr!("sched-moved-issues", count = issues.to_string()));
        }
        self.write_optimization_scenarios()
    }

    /// Run a schedule on its scenario's run of this session. The engine is
    /// stage 2; until then this says so.
    pub(crate) fn run_schedule(&mut self, parent: u64, id: u64) {
        let state = &self.editor.optimization;
        let Some(scenario) = state.scenarios.iter().find(|scenario| scenario.id == parent) else {
            return;
        };
        let Some(schedule) = scenario.schedules.iter().find(|schedule| schedule.id == id) else {
            return;
        };
        if state.schedule_status(scenario, schedule) == ScheduleStatus::Blocked {
            userspace_warn!("{}", tr!("sched-blocked"));
            return;
        }
        let issues = schedule.issues(scenario);
        if !issues.is_empty() {
            userspace_warn!("{}", tr!("sched-run-issues", name = schedule.name.clone(), issues = issues.join("; ")));
            return;
        }
        userspace_log!("{}", tr!("sched-engine-pending", name = schedule.name.clone()));
    }
}
