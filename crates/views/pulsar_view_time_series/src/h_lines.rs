//! Pulsar's threshold lines: a check's horizontal limits (`hLines` in Pulsar's
//! `CheckVisualizerData`). Drawn by the view on top of the series, solid or
//! dashed, kept out of the legend and the readout, and listed as chips in the
//! stat bar, as Pulsar's check charts do.
//!
//! Stored as a Pulsar archetype, `pulsar.archetypes.HLines`, built from Rerun
//! components and usually logged static; one instance per line:
//! `values` (Scalar), `colors` (Color), `labels` (Text), `widths` (Scalar, UI
//! points), `dash_on` and `dash_off` (Scalar, UI points; 0 = solid).

use re_sdk_types::components::{Color, Scalar, Text};
use re_sdk_types::{Component as _, ComponentDescriptor};
use re_view::{DataResultQuery as _, VisualizerInstructionQueryResults};
use re_viewer_context::{
    AppOptions, IdentifiedViewSystem, SingleRequiredComponentConstraint, ViewContext,
    ViewContextCollection, ViewQuery, ViewSystemExecutionError, ViewSystemIdentifier,
    VisualizerExecutionOutput, VisualizerQueryInfo, VisualizerSystem,
};

pub const ARCHETYPE: &str = "pulsar.archetypes.HLines";

fn descriptor(field: &str, component_type: re_sdk_types::ComponentType) -> ComponentDescriptor {
    ComponentDescriptor {
        archetype: Some(ARCHETYPE.into()),
        component: re_sdk_types::ComponentIdentifier::from_archetype_field(ARCHETYPE.into(), field),
        component_type: Some(component_type),
    }
}

fn descriptors() -> [ComponentDescriptor; 6] {
    [
        descriptor("values", Scalar::name()),
        descriptor("colors", Color::name()),
        descriptor("labels", Text::name()),
        descriptor("widths", Scalar::name()),
        descriptor("dash_on", Scalar::name()),
        descriptor("dash_off", Scalar::name()),
    ]
}

#[derive(Clone, Debug)]
pub struct HLine {
    pub y: f64,
    pub color: egui::Color32,
    pub label: String,
    pub width: f32,
    /// (dash, gap) in UI points; `None` for a solid line.
    pub dash: Option<(f32, f32)>,
}

#[derive(Clone, Debug, Default)]
pub struct HLinesData {
    pub lines: Vec<HLine>,
}

#[derive(Default)]
pub struct HLinesSystem;

impl IdentifiedViewSystem for HLinesSystem {
    fn identifier() -> ViewSystemIdentifier {
        re_viewer_context::external::re_string_interner::intern_static!(
            ViewSystemIdentifier,
            "PulsarHLines"
        )
    }
}

impl VisualizerSystem for HLinesSystem {
    fn visualizer_query_info(&self, _app_options: &AppOptions) -> VisualizerQueryInfo {
        let [values, ..] = descriptors();
        VisualizerQueryInfo {
            relevant_archetype: values.archetype,
            constraints: SingleRequiredComponentConstraint::new::<Scalar>(&values)
                .with_allow_static_data(true)
                .into(),
            annotation_context: None,
            queried: descriptors().into_iter().collect(),
        }
    }

    fn execute(
        &self,
        ctx: &ViewContext<'_>,
        query: &ViewQuery<'_>,
        _context_systems: &ViewContextCollection,
    ) -> Result<VisualizerExecutionOutput, ViewSystemExecutionError> {
        let output = VisualizerExecutionOutput::default();
        let mut lines = Vec::new();
        let components = descriptors().map(|d| d.component);

        for (data_result, instruction) in query.iter_visualizer_instruction_for(Self::identifier())
        {
            let results = data_result.query_components_with_history(
                ctx,
                query,
                components,
                instruction,
                None,
            );
            let results = VisualizerInstructionQueryResults::new(instruction, &results, &output);
            let scalars = |i: usize| -> Vec<f64> {
                results
                    .iter_optional(components[i])
                    .slice::<f64>()
                    .last()
                    .map(|(_, s)| s.to_vec())
                    .unwrap_or_default()
            };
            let colors: Vec<u32> = results
                .iter_optional(components[1])
                .slice::<u32>()
                .last()
                .map(|(_, s)| s.to_vec())
                .unwrap_or_default();
            let labels: Vec<String> = results
                .iter_optional(components[2])
                .slice::<String>()
                .last()
                .map(|(_, s)| s.iter().map(|v| v.to_string()).collect())
                .unwrap_or_default();
            let (values, widths, dash_on, dash_off) = (scalars(0), scalars(3), scalars(4), scalars(5));

            for (i, &y) in values.iter().enumerate() {
                let on = dash_on.get(i).copied().unwrap_or(0.0) as f32;
                let off = dash_off.get(i).copied().unwrap_or(0.0) as f32;
                lines.push(HLine {
                    y,
                    color: colors
                        .get(i)
                        .map_or(egui::Color32::GRAY, |&c| Color::from_u32(c).into()),
                    label: labels.get(i).cloned().unwrap_or_default(),
                    width: widths.get(i).copied().unwrap_or(1.5) as f32,
                    dash: (on > 0.0).then_some((on, off.max(1.0))),
                });
            }
        }

        Ok(output.with_visualizer_data(HLinesData { lines }))
    }
}

/// Draw the lines across the plot, on top of the series.
pub fn paint_h_lines(ui: &egui::Ui, plot: egui::Rect, y_of: impl Fn(f64) -> f32, lines: &[HLine]) {
    let painter = ui.painter().with_clip_rect(plot);
    for line in lines {
        let y = y_of(line.y);
        if !plot.y_range().contains(y) {
            continue;
        }
        let stroke = egui::Stroke::new(line.width, line.color);
        let path = [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)];
        match line.dash {
            Some((dash, gap)) => {
                painter.extend(egui::Shape::dashed_line(&path, stroke, dash, gap));
            }
            None => {
                painter.line_segment(path, stroke);
            }
        }
    }
}

/// A chip for the stat bar: a short swatch of the line's style and its label.
pub fn chip_ui(ui: &mut egui::Ui, line: &HLine, highlighted: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 10.0), egui::Sense::hover());
    let path = [rect.left_center(), rect.right_center()];
    let stroke = egui::Stroke::new(2.0, line.color);
    match line.dash {
        Some(_) => {
            ui.painter().extend(egui::Shape::dashed_line(&path, stroke, 3.0, 2.0));
        }
        None => {
            ui.painter().line_segment(path, stroke);
        }
    }
    let color = if highlighted {
        egui::Color32::from_rgb(239, 68, 68)
    } else {
        ui.visuals().weak_text_color()
    };
    ui.add(
        egui::Label::new(egui::RichText::new(&line.label).monospace().size(9.0).color(color))
            .selectable(false),
    );
}
