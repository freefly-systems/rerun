//! Pulsar's y-bands: a filled range of y values behind a chart's lines, such
//! as a check's accepted range (`hBands` in Pulsar's `CheckVisualizerData`).
//!
//! Rerun has no archetype for this, so Pulsar logs a small one of its own,
//! `pulsar.archetypes.YBands`, built from Rerun components: one `Range1D` per
//! band, plus a fill `Color` and an optional border `Color` (alpha 0: none).
//! It is usually logged static, so it shows at every time.

use re_sdk_types::components::{Color, Range1D};
use re_sdk_types::{Component as _, ComponentDescriptor};
use re_view::{DataResultQuery as _, VisualizerInstructionQueryResults};
use re_viewer_context::{
    AppOptions, IdentifiedViewSystem, SingleRequiredComponentConstraint, ViewContext,
    ViewContextCollection, ViewQuery, ViewSystemExecutionError, ViewSystemIdentifier,
    VisualizerExecutionOutput, VisualizerQueryInfo, VisualizerSystem,
};

pub const ARCHETYPE: &str = "pulsar.archetypes.YBands";

pub fn descriptor_ranges() -> ComponentDescriptor {
    ComponentDescriptor {
        archetype: Some(ARCHETYPE.into()),
        component: "pulsar.archetypes.YBands:ranges".into(),
        component_type: Some(Range1D::name()),
    }
}

pub fn descriptor_fills() -> ComponentDescriptor {
    ComponentDescriptor {
        archetype: Some(ARCHETYPE.into()),
        component: "pulsar.archetypes.YBands:fills".into(),
        component_type: Some(Color::name()),
    }
}

pub fn descriptor_borders() -> ComponentDescriptor {
    ComponentDescriptor {
        archetype: Some(ARCHETYPE.into()),
        component: "pulsar.archetypes.YBands:borders".into(),
        component_type: Some(Color::name()),
    }
}

/// One band, ready to paint.
#[derive(Clone, Debug)]
pub struct YBand {
    pub min: f64,
    pub max: f64,
    pub fill: egui::Color32,
    pub border: Option<egui::Color32>,
}

/// Visualizer output: every band of every entity in the view.
#[derive(Clone, Debug, Default)]
pub struct YBandsData {
    pub bands: Vec<YBand>,
}

#[derive(Default)]
pub struct YBandsSystem;

impl IdentifiedViewSystem for YBandsSystem {
    fn identifier() -> ViewSystemIdentifier {
        re_viewer_context::external::re_string_interner::intern_static!(
            ViewSystemIdentifier,
            "PulsarYBands"
        )
    }
}

impl VisualizerSystem for YBandsSystem {
    fn visualizer_query_info(&self, _app_options: &AppOptions) -> VisualizerQueryInfo {
        VisualizerQueryInfo {
            relevant_archetype: descriptor_ranges().archetype,
            constraints: SingleRequiredComponentConstraint::new::<Range1D>(&descriptor_ranges())
                .with_allow_static_data(true)
                .into(),
            annotation_context: None,
            queried: [descriptor_ranges(), descriptor_fills(), descriptor_borders()]
                .into_iter()
                .collect(),
        }
    }

    fn execute(
        &self,
        ctx: &ViewContext<'_>,
        query: &ViewQuery<'_>,
        _context_systems: &ViewContextCollection,
    ) -> Result<VisualizerExecutionOutput, ViewSystemExecutionError> {
        let output = VisualizerExecutionOutput::default();
        let mut bands = Vec::new();

        let ranges_c = descriptor_ranges().component;
        let fills_c = descriptor_fills().component;
        let borders_c = descriptor_borders().component;

        for (data_result, instruction) in query.iter_visualizer_instruction_for(Self::identifier())
        {
            let results = data_result.query_components_with_history(
                ctx,
                query,
                [ranges_c, fills_c, borders_c],
                instruction,
                None,
            );
            let results = VisualizerInstructionQueryResults::new(instruction, &results, &output);

            // Latest row of each component; a static band has exactly one.
            let ranges_iter = results.iter_required(ranges_c);
            let fills_iter = results.iter_optional(fills_c);
            let borders_iter = results.iter_optional(borders_c);
            let Some((_, ranges)) = ranges_iter.slice::<[f64; 2]>().last() else {
                continue;
            };
            let fills = fills_iter.slice::<u32>().last().map(|(_, s)| s).unwrap_or_default();
            let borders = borders_iter.slice::<u32>().last().map(|(_, s)| s).unwrap_or_default();

            for (i, [min, max]) in ranges.iter().copied().enumerate() {
                let color = |raw: Option<&u32>| raw.map(|&c| egui::Color32::from(Color::from_u32(c)));
                bands.push(YBand {
                    min: min.min(max),
                    max: min.max(max),
                    // Pulsar's default region fill: a neutral, mostly transparent grey.
                    fill: color(fills.get(i)).unwrap_or(egui::Color32::from_white_alpha(14)),
                    border: color(borders.get(i)).filter(|c| c.a() > 0),
                });
            }
        }

        Ok(output.with_visualizer_data(YBandsData { bands }))
    }
}
