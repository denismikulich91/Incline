//! Schedules of optimization scenarios: add, edit, save, delete, duplicate,
//! rename, and move or copy one to another scenario. Every change is handed
//! to the project with the scenarios (`write_optimization_scenarios`), so the
//! project reads unsaved until it is saved. See
//! `.claude/specs/scheduling/stage-1-ui.md`. Running one is stage 2
//! (`stage-2-engine.md`): the job is [`schedule_run::run`].

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::Arc,
};

use anyhow::Result;

use crate::{
    app::{App, jobs::JobKey},
    i18n::tr,
    model::{
        block_model::BlockModelId,
        optimization::{BlockModelFields, OptimizationScenario, copy_name, format_factor, unique_name},
        schedule_run::{self, ScheduleInput, ScheduleOutcome, simulate::Held},
        scheduling::{PeriodCase, Schedule, format_money},
    },
    ui::state::{LoadThen, RunState, ScenarioStatus, ScheduleDraft, ScheduleStatus, ScheduleSummary},
    userspace_error, userspace_log, userspace_warn,
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
        self.cancel_jobs(|key| *key == JobKey::ScheduleRun(id));
        if let Some(scenario) = self.scenario_mut(parent) {
            scenario.schedules.retain(|schedule| schedule.id != id);
        }
        let state = &mut self.editor.optimization;
        state.schedule_runs.remove(&id);
        state.schedule_summaries.remove(&id);
        state.schedule_results.remove(&id);
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
        if !keep {
            self.cancel_jobs(|key| *key == JobKey::ScheduleRun(id));
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
            state.schedule_results.remove(&id);
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

    /// Run a schedule on its scenario's run of this session, or stop it while
    /// it runs (the row's play button does both).
    pub(crate) fn run_schedule(&mut self, parent: u64, id: u64) {
        let state = &self.editor.optimization;
        let Some(scenario) = state.scenarios.iter().find(|scenario| scenario.id == parent).cloned() else {
            return;
        };
        let Some(schedule) = scenario.schedules.iter().find(|schedule| schedule.id == id).cloned() else {
            return;
        };
        let status = state.schedule_status(&scenario, &schedule);
        if matches!(status, ScheduleStatus::Running(_) | ScheduleStatus::Queued) {
            self.cancel_schedule_run(id, &schedule.name);
            return;
        }
        let issues = schedule.issues(&scenario);
        if !issues.is_empty() {
            userspace_warn!("{}", tr!("sched-run-issues", name = schedule.name.clone(), issues = issues.join("; ")));
            return;
        }
        // No up-to-date run of the scenario: run it first, then this.
        if status == ScheduleStatus::Blocked {
            let waiting = self.editor.optimization.schedules_after_run.entry(parent).or_default();
            if !waiting.contains(&id) {
                waiting.push(id);
            }
            userspace_log!("{}", tr!("sched-run-parent-first", name = schedule.name.clone(), scenario = scenario.name.clone()));
            let state = &self.editor.optimization;
            if !matches!(state.status(&scenario), ScenarioStatus::Running(_) | ScenarioStatus::Queued) {
                self.run_optimization_scenario(parent);
            }
            return;
        }
        // The block model and topography are read back first, if the user agrees.
        let unloaded = self.unloaded_optimization_inputs(&scenario);
        if !unloaded.is_empty() {
            self.ask_to_load_for_optimization(unloaded, LoadThen::Schedule { parent, id });
            return;
        }
        self.launch_schedule_run(scenario, schedule);
    }

    fn cancel_schedule_run(&mut self, id: u64, name: &str) {
        self.cancel_jobs(|key| *key == JobKey::ScheduleRun(id));
        let state = &mut self.editor.optimization;
        state.awaiting_restore.remove(&id);
        let mut waited = false;
        for waiting in state.schedules_after_run.values_mut() {
            let before = waiting.len();
            waiting.retain(|other| *other != id);
            waited |= waiting.len() != before;
        }
        state.schedules_after_run.retain(|_, waiting| !waiting.is_empty());
        if state.schedule_runs.remove(&id).is_some() || waited {
            userspace_log!("{}", tr!("sched-run-cancelled", name = name.to_owned()));
        }
    }

    /// The scenario's run is over: run the schedules that waited for it, or,
    /// when it did not finish, say they will not run.
    pub(crate) fn after_scenario_run(&mut self, parent: u64, finished: bool) {
        let Some(waiting) = self.editor.optimization.schedules_after_run.remove(&parent) else {
            return;
        };
        if !finished {
            userspace_warn!("{}", tr!("sched-run-parent-failed", count = waiting.len().to_string()));
            return;
        }
        for id in waiting {
            self.run_schedule(parent, id);
        }
    }

    fn fail_schedule_run(&mut self, id: u64, name: &str, issues: &[String]) {
        userspace_error!("{}", tr!("sched-run-cannot-start", name = name.to_owned()));
        for issue in issues {
            userspace_error!("{}", tr!("opt-run-issue", issue = issue.clone()));
        }
        self.editor.optimization.schedule_runs.remove(&id);
        self.editor.optimization.awaiting_restore.remove(&id);
    }

    /// Everything the schedule needs is in memory: prepare its scenario again
    /// and spawn the job.
    fn launch_schedule_run(&mut self, scenario: OptimizationScenario, schedule: Schedule) {
        let id = schedule.id;
        self.editor.optimization.awaiting_restore.remove(&id);
        let Some(run) = self.editor.optimization.results.get(&scenario.id).cloned() else {
            self.fail_schedule_run(id, &schedule.name, &[tr!("sched-blocked")]);
            return;
        };
        let (prepared, layout, model_id) = match self.prepare_scenario_inputs(&scenario) {
            Ok(inputs) => inputs,
            Err(issues) => {
                self.fail_schedule_run(id, &schedule.name, &issues);
                return;
            }
        };
        // Checked before the run, so a field that cannot be written does not
        // cost the run's time first.
        let fields = self
            .block_models
            .iter()
            .find(|model| model.id == model_id)
            .map(BlockModelFields::of_open)
            .unwrap_or_default();
        let period_field = match schedule.period_field_target(&scenario, &fields) {
            Ok(field) => field,
            Err(issue) => {
                self.fail_schedule_run(id, &schedule.name, &[issue]);
                return;
            }
        };
        let input = ScheduleInput {
            prepared,
            layout,
            run,
            settings: schedule.resolve(&scenario),
            case: schedule.period_case,
            write_field: period_field.is_some(),
        };
        userspace_log!(
            "{}",
            tr!("sched-run-started", name = schedule.name.clone(), pits = input.run.shells.summaries.len().to_string())
        );
        let fingerprint = self.editor.optimization.schedule_fingerprint(&scenario, &schedule);
        let started = web_time::Instant::now();
        let progress = self.spawn_job_sharing_progress(
            tr!("sched-run-label", name = schedule.name.clone()),
            vec![JobKey::ScheduleRun(id), JobKey::BlockModel(model_id)],
            move |cancel, progress| schedule_run::run(input, cancel, progress),
            move |app, result| app.finish_schedule_run(&schedule, fingerprint, started, model_id, period_field, result),
        );
        self.editor.optimization.schedule_runs.insert(id, RunState::Running { progress });
    }

    fn finish_schedule_run(
        &mut self,
        schedule: &Schedule,
        fingerprint: u64,
        started: web_time::Instant,
        model_id: BlockModelId,
        period_field: Option<String>,
        result: Result<ScheduleOutcome>,
    ) {
        let id = schedule.id;
        let name = schedule.name.clone();
        // Deleted, moved or cancelled meanwhile: the result is of nothing shown.
        if !matches!(self.editor.optimization.schedule_runs.get(&id), Some(RunState::Running { .. })) {
            return;
        }
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                self.editor.optimization.schedule_runs.remove(&id);
                if format!("{error:#}").contains("Cancelled") {
                    userspace_log!("{}", tr!("sched-run-cancelled", name = name));
                } else {
                    userspace_error!("{}", tr!("sched-run-failed", name = name, error = format!("{error:#}")));
                }
                return;
            }
        };
        let ScheduleOutcome { result, field } = outcome;
        let state = &mut self.editor.optimization;
        state.schedule_runs.insert(id, RunState::Finished { fingerprint });
        userspace_log!(
            "{}",
            tr!("sched-run-finished", name = name.clone(), seconds = format!("{:.1}", started.elapsed().as_secs_f64()))
        );
        let unfinished = format!(" {}", tr!("sched-run-unfinished"));
        let mark = |unfinished_pit: bool| if unfinished_pit { unfinished.clone() } else { String::new() };
        for (shell, pit) in result.shells.iter().zip(&result.pits) {
            userspace_log!(
                "{}",
                tr!(
                    "sched-run-pit",
                    shell = shell.number.to_string(),
                    factor = match shell.distance {
                        None => format_factor(shell.factor),
                        Some(share) => format!("{}, {:.0}%", format_factor(shell.factor), share * 100.0),
                    },
                    worst = format!("{}{}", format_money(pit.worst.npv), mark(pit.worst.unfinished)),
                    lead = format!("{}{}", format_money(pit.lead.npv), mark(pit.lead.unfinished)),
                    best = format!("{}{}", format_money(pit.best.npv), mark(pit.best.unfinished)),
                    periods = pit.lead.periods.len().to_string(),
                    held = pit.lead.periods.iter().filter(|period| period.held.is_some()).count().to_string()
                )
            );
        }
        match result.peak(PeriodCase::FixedLead) {
            Some(peak) => {
                let pit = &result.pits[peak];
                let summary = ScheduleSummary {
                    peak_shell: peak,
                    npv_best: pit.best.npv,
                    npv_worst: pit.worst.npv,
                    npv_lead: pit.lead.npv,
                };
                let (shell, lead, best, worst) = (
                    (peak + 1).to_string(),
                    format_money(summary.npv_lead),
                    format_money(summary.npv_best),
                    format_money(summary.npv_worst),
                );
                userspace_log!(
                    "{}",
                    tr!(
                        "sched-run-peak",
                        name = name.clone(),
                        shell = shell.clone(),
                        npv = lead.clone(),
                        best = best.clone(),
                        worst = worst.clone()
                    )
                );
                // Fixed lead should fall between the bounds.
                let (low, high) = (summary.npv_worst.min(summary.npv_best), summary.npv_worst.max(summary.npv_best));
                let slack = 1e-6 * high.abs().max(1.0);
                if summary.npv_lead < low - slack || summary.npv_lead > high + slack {
                    userspace_warn!(
                        "{}",
                        tr!(
                            "sched-run-outside-bounds",
                            name = name.clone(),
                            shell = shell.clone(),
                            lead = lead,
                            best = best,
                            worst = worst
                        )
                    );
                }
                // Where mining waited at the peak because ore had nowhere to go.
                for reason in [Held::NoStockpile, Held::StockpilesFull] {
                    let periods: Vec<String> = pit
                        .lead
                        .periods
                        .iter()
                        .enumerate()
                        .filter(|(_, period)| period.held == Some(reason))
                        .map(|(index, _)| (index + 1).to_string())
                        .collect();
                    if !periods.is_empty() {
                        let periods = periods.join(", ");
                        let message = match reason {
                            Held::NoStockpile => tr!("sched-run-held-no-stockpile", shell = shell.clone(), periods = periods),
                            Held::StockpilesFull => tr!("sched-run-held-stockpiles-full", shell = shell.clone(), periods = periods),
                        };
                        userspace_warn!("{message}");
                    }
                }
                self.editor.optimization.schedule_summaries.insert(id, summary);
            }
            None => {
                userspace_warn!("{}", tr!("sched-run-no-peak", name = name.clone(), max = schedule.max_periods.to_string()));
                self.editor.optimization.schedule_summaries.remove(&id);
            }
        }
        if let Some(order) = result.order.as_ref().filter(|_| period_field.is_some()) {
            userspace_log!("{}", tr!("sched-run-field-pit", shell = (order.pit + 1).to_string(), case = order.case.label()));
        } else if period_field.is_some() {
            userspace_warn!("{}", tr!("sched-run-field-none", case = schedule.period_case.label()));
        }
        self.editor.optimization.schedule_results.insert(id, Arc::new(result));
        if let (Some(field), Some(values)) = (period_field, field) {
            let seed = {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                (id, started.elapsed().as_nanos()).hash(&mut hasher);
                hasher.finish()
            };
            self.write_shell_field(model_id, field, values, HashMap::new(), seed);
        }
    }
}
