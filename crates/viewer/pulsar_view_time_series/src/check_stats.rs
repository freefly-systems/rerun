//! Pulsar's check stat cards: the bar under a check chart. A card shows a
//! label, a value, a limit and a pass/warn/fail status. Clicking a card with
//! violations strokes the violating stretches of its series in red; clicking a
//! card tied to a threshold line turns that line red and thick.
//!
//! Stored as a Pulsar archetype, `pulsar.archetypes.CheckStats`, built from
//! Rerun components and usually logged static:
//! * per card, Text batches: `labels`, `values`, `thresholds`, `statuses`
//!   ("pass" | "warning" | "fail" | ""), `hline_refs` (a threshold series
//!   name, or "");
//! * per violation, Scalar batches: `violation_cards` (card index),
//!   `violation_series`, `violation_times` (seconds on the flight timeline),
//!   `violation_values`;
//! * one Scalar, `sample_period`, in seconds: violations closer than about one
//!   period apart are one stretch.

use re_sdk_types::components::{Scalar, Text};
use re_sdk_types::{Component as _, ComponentDescriptor};
use re_view::{DataResultQuery as _, VisualizerInstructionQueryResults};
use re_viewer_context::{
    AppOptions, IdentifiedViewSystem, SingleRequiredComponentConstraint, ViewContext,
    ViewContextCollection, ViewQuery, ViewSystemExecutionError, ViewSystemIdentifier,
    VisualizerExecutionOutput, VisualizerQueryInfo, VisualizerSystem,
};

pub const ARCHETYPE: &str = "pulsar.archetypes.CheckStats";

fn descriptor(field: &str, component_type: re_sdk_types::ComponentType) -> ComponentDescriptor {
    ComponentDescriptor {
        archetype: Some(ARCHETYPE.into()),
        component: format!("{ARCHETYPE}:{field}").into(),
        component_type: Some(component_type),
    }
}

const TEXT_FIELDS: [&str; 5] = ["labels", "values", "thresholds", "statuses", "hline_refs"];
const SCALAR_FIELDS: [&str; 5] = [
    "violation_cards",
    "violation_series",
    "violation_times",
    "violation_values",
    "sample_period",
];

pub fn descriptor_labels() -> ComponentDescriptor {
    descriptor("labels", Text::name())
}

fn all_descriptors() -> Vec<ComponentDescriptor> {
    TEXT_FIELDS
        .iter()
        .map(|f| descriptor(f, Text::name()))
        .chain(SCALAR_FIELDS.iter().map(|f| descriptor(f, Scalar::name())))
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardStatus {
    Pass,
    Warning,
    Fail,
    None,
}

/// One violation: time in timeline nanoseconds, value, and the series index.
#[derive(Clone, Copy, Debug)]
pub struct Violation {
    pub time: i64,
    pub value: f64,
    pub series: u32,
}

#[derive(Clone, Debug)]
pub struct StatCard {
    pub label: String,
    pub value: String,
    pub threshold: String,
    pub status: CardStatus,
    pub hline_ref: String,
    pub violations: Vec<Violation>,
    /// Violations closer than this, in nanoseconds, join into one stretch.
    pub join_gap: i64,
}

impl StatCard {
    pub fn is_clickable(&self) -> bool {
        !self.violations.is_empty() || !self.hline_ref.is_empty()
    }
}

/// Visualizer output: every card of every check entity in the view, in order.
#[derive(Clone, Debug, Default)]
pub struct CheckStatsData {
    pub cards: Vec<StatCard>,
}

#[derive(Default)]
pub struct CheckStatsSystem;

impl IdentifiedViewSystem for CheckStatsSystem {
    fn identifier() -> ViewSystemIdentifier {
        re_viewer_context::external::re_string_interner::intern_static!(
            ViewSystemIdentifier,
            "PulsarCheckStats"
        )
    }
}

impl VisualizerSystem for CheckStatsSystem {
    fn visualizer_query_info(&self, _app_options: &AppOptions) -> VisualizerQueryInfo {
        VisualizerQueryInfo {
            relevant_archetype: descriptor_labels().archetype,
            constraints: SingleRequiredComponentConstraint::new::<Text>(&descriptor_labels())
                .with_allow_static_data(true)
                .into(),
            queried: all_descriptors().into_iter().collect(),
        }
    }

    fn execute(
        &self,
        ctx: &ViewContext<'_>,
        query: &ViewQuery<'_>,
        _context_systems: &ViewContextCollection,
    ) -> Result<VisualizerExecutionOutput, ViewSystemExecutionError> {
        let output = VisualizerExecutionOutput::default();
        let mut cards = Vec::new();
        let descriptors = all_descriptors();
        let components: Vec<_> = descriptors.iter().map(|d| d.component).collect();

        for (data_result, instruction) in query.iter_visualizer_instruction_for(Self::identifier())
        {
            let results = data_result.query_components_with_history(
                ctx,
                query,
                components.iter().copied(),
                instruction,
            );
            let results = VisualizerInstructionQueryResults::new(instruction, &results, &output);

            let text = |i: usize| -> Vec<String> {
                results
                    .iter_optional(components[i])
                    .slice::<String>()
                    .last()
                    .map(|(_, s)| s.iter().map(|v| v.to_string()).collect())
                    .unwrap_or_default()
            };
            let scalars = |i: usize| -> Vec<f64> {
                results
                    .iter_optional(components[TEXT_FIELDS.len() + i])
                    .slice::<f64>()
                    .last()
                    .map(|(_, s)| s.to_vec())
                    .unwrap_or_default()
            };
            let (labels, values, thresholds, statuses, hline_refs) =
                (text(0), text(1), text(2), text(3), text(4));
            let (v_cards, v_series, v_times, v_values) =
                (scalars(0), scalars(1), scalars(2), scalars(3));
            let period = scalars(4).first().copied().unwrap_or(0.0);
            let join_gap = (period * 1.5 * 1e9) as i64;

            let first = cards.len();
            for (i, label) in labels.into_iter().enumerate() {
                let get = |v: &Vec<String>| v.get(i).cloned().unwrap_or_default();
                cards.push(StatCard {
                    label,
                    value: get(&values),
                    threshold: get(&thresholds),
                    status: match get(&statuses).as_str() {
                        "pass" => CardStatus::Pass,
                        "warning" => CardStatus::Warning,
                        "fail" => CardStatus::Fail,
                        _ => CardStatus::None,
                    },
                    hline_ref: get(&hline_refs),
                    violations: Vec::new(),
                    join_gap,
                });
            }
            for j in 0..v_cards.len().min(v_times.len()).min(v_values.len()) {
                if let Some(card) = cards.get_mut(first + v_cards[j] as usize) {
                    card.violations.push(Violation {
                        time: (v_times[j] * 1e9) as i64,
                        value: v_values[j],
                        series: v_series.get(j).copied().unwrap_or(0.0) as u32,
                    });
                }
            }
        }

        Ok(output.with_visualizer_data(CheckStatsData { cards }))
    }
}

const RED: egui::Color32 = egui::Color32::from_rgb(239, 68, 68);
const AMBER: egui::Color32 = egui::Color32::from_rgb(245, 158, 11);

/// Height reserved under the plot for the stat bar.
pub const BAR_HEIGHT: f32 = 20.0;

/// The stat bar, in Pulsar's layout: LABEL value limit, with a dot on
/// clickable cards. Clicking toggles `active`.
pub fn stat_bar_ui(ui: &mut egui::Ui, cards: &[StatCard], active: &mut Option<usize>) {
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        ui.spacing_mut().item_spacing.x = 10.0;
        let weak = ui.visuals().weak_text_color();
        let strong = ui.visuals().strong_text_color();
        for (i, card) in cards.iter().enumerate() {
            let is_active = *active == Some(i);
            let clickable = card.is_clickable();
            let mut job = egui::text::LayoutJob::default();
            let fmt = |size: f32, color| egui::TextFormat::simple(egui::FontId::monospace(size), color);
            job.append(&card.label.to_uppercase(), 0.0, fmt(9.0, weak));
            let value_color = match card.status {
                CardStatus::Fail => RED,
                CardStatus::Warning => AMBER,
                _ => strong,
            };
            job.append(&card.value, 5.0, fmt(10.0, value_color));
            if !card.threshold.is_empty() {
                job.append(&card.threshold, 5.0, fmt(9.0, weak.gamma_multiply(0.7)));
            }
            if clickable {
                job.append("●", 5.0, fmt(8.0, if is_active { RED } else { weak }));
            }
            let frame = egui::Frame::new()
                .fill(if is_active { RED.gamma_multiply(0.12) } else { egui::Color32::TRANSPARENT })
                .corner_radius(3.0)
                .inner_margin(egui::Margin::symmetric(4, 1));
            let response = frame
                .show(ui, |ui| ui.add(egui::Label::new(job).selectable(false)))
                .response;
            if clickable {
                let response = response.interact(egui::Sense::click());
                if response.clicked() {
                    *active = if is_active { None } else { Some(i) };
                }
                response.on_hover_cursor(egui::CursorIcon::PointingHand);
            }
        }
    });
}

/// The active card's violations, on top of the lines: red strokes along runs
/// of violations one sample period apart, red dots for single ones.
pub fn paint_violations(
    ui: &egui::Ui,
    plot: egui::Rect,
    to_screen: impl Fn(i64, f64) -> egui::Pos2,
    card: &StatCard,
) {
    let painter = ui.painter().with_clip_rect(plot);
    let mut by_series: std::collections::BTreeMap<u32, Vec<&Violation>> = Default::default();
    for v in &card.violations {
        by_series.entry(v.series).or_default().push(v);
    }
    let stroke = egui::Stroke::new(2.0, RED);
    for (_, mut points) in by_series {
        points.sort_by_key(|v| v.time);
        let mut run: Vec<egui::Pos2> = Vec::new();
        let flush = |run: &mut Vec<egui::Pos2>| {
            match run.len() {
                0 => {}
                1 => {
                    painter.circle_filled(run[0], 3.0, RED);
                }
                _ => {
                    painter.add(egui::Shape::line(std::mem::take(run), stroke));
                }
            }
            run.clear();
        };
        let mut last: Option<i64> = None;
        for v in points {
            if last.is_some_and(|t| v.time - t > card.join_gap) {
                flush(&mut run);
            }
            run.push(to_screen(v.time, v.value));
            last = Some(v.time);
        }
        flush(&mut run);
    }
}

/// A threshold line, highlighted: thick red with a soft glow.
pub fn paint_highlighted_hline(ui: &egui::Ui, plot: egui::Rect, y: f32) {
    if !plot.y_range().contains(y) {
        return;
    }
    let painter = ui.painter().with_clip_rect(plot);
    painter.hline(plot.x_range(), y, egui::Stroke::new(7.0, RED.gamma_multiply(0.3)));
    painter.hline(plot.x_range(), y, egui::Stroke::new(3.0, RED));
}
