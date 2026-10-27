use ahash::HashMap;
use egui::{NumExt as _, Vec2, Vec2b, emath::fast_midpoint};
use egui_plot::{ItemId, Plot, PlotPoint};
use itertools::{Either, Itertools as _, chain};
use nohash_hasher::{IntMap, IntSet};
use re_chunk_store::TimeType;
use re_format::time::next_grid_tick_magnitude_nanos;
use re_log_types::external::arrow::datatypes::DataType;
use re_log_types::{AbsoluteTimeRange, ComponentPath, EntityPath};
use re_sdk_types::archetypes::{Measurements, Scalars, SeriesLines, SeriesPoints};
use re_sdk_types::blueprint::archetypes::{
    PlotBackground, PlotInteraction, PlotLegend, ScalarAxis, TimeAxis,
};
use re_sdk_types::blueprint::components::{
    Corner2D, Enabled, LinkAxis, LockRangeDuringZoom, PointsDisplay, TooltipMode,
    VisualizerInstructionId,
};
use re_sdk_types::components::{Color, Range1D, Visible};
use re_sdk_types::encodings::TimeRange;
use re_sdk_types::{ComponentBatch as _, ComponentIdentifier, ViewClassIdentifier};
use re_ui::{Help, IconText, MouseButtonText, UiExt as _, icons, list_item};
use re_view::controls::{MOVE_TIME_CURSOR_BUTTON, SELECTION_RECT_ZOOM_BUTTON};
use re_view::view_property_ui;
use re_viewer_context::{
    BlueprintContext as _, DataResultInteractionAddress, DatatypeMatch, DragAndDropFeedback,
    IdentifiedViewSystem as _, IndicatedEntities, PerVisualizerType, QueryRange,
    RecommendedMappings, RecommendedView, RecommendedVisualizers, SingleRequiredComponentMatch,
    SystemExecutionOutput, ViewClass, ViewClassExt as _, ViewClassRegistryError, ViewId, ViewQuery,
    ViewSpawnHeuristics, ViewState, ViewStateExt as _, ViewSystemExecutionError,
    ViewSystemIdentifier, ViewerContext, VisualizableReason, VisualizerComponentSource,
};
use re_viewport_blueprint::ViewProperty;
use smallvec::SmallVec;
use vec1::Vec1;

use crate::line_visualizer_system::{SeriesLinesOutput, SeriesLinesSystem};
use crate::measurements_visualizer_system::{MeasurementsSeriesOutput, MeasurementsSeriesSystem};
use crate::naming::{SeriesInfo, SeriesNamesContext};
use crate::point_visualizer_system::{SeriesPointsOutput, SeriesPointsSystem};
use crate::util::data_result_time_range;
use crate::{MAX_NUM_NON_INDICATED_RECOMMENDED_VISUALIZERS_PER_ENTITY, PlotSeriesKind};

// ---

#[derive(Clone, re_byte_size::SizeBytes)]
pub struct TimeSeriesViewState {
    /// The range of the scalar values currently on screen.
    ///
    /// None if no values are on screen right now.
    pub scalar_range: Option<Range1D>,

    /// The combined query range of all entities in this view.
    ///
    /// Only entities that are currently _visible_ are considered,
    /// but for these the entire data range in the store is calculated
    /// (not just what we're currently zoomed in on).
    pub full_data_time_range: AbsoluteTimeRange,

    /// We offset the time values of the plot so that unix timestamps don't run out of precision.
    ///
    /// Other parts of the system, such as query clamping, need to be aware of that offset in order
    /// to work properly.
    pub time_offset: i64,

    /// Cached disambiguated names for visualizers, used when no label is provided.
    pub default_series_name_formats: HashMap<VisualizerInstructionId, String>,

    /// The number of time series rendered by each visualizer instruction last frame.
    ///
    /// We track plot item ids here because the number of "series" passed to egui can actually be much higher
    /// since every color change, every discontinuity, etc. creates a new series, sharing the same item id.
    pub num_time_series_last_frame_per_instruction:
        HashMap<VisualizerInstructionId, IntSet<ItemId>>,

    /// The plot transform from the previous frame, used by visualizers to map
    /// data-space points to screen-space for `re_renderer` primitives.
    ///
    /// `None` on the first frame (before `plot.show()` has run).
    // `egui_plot::PlotTransform` doesn't impl `SizeBytes`; it's POD with no heap.
    #[size_bytes(ignore)]
    pub plot_transform: Option<egui_plot::PlotTransform>,

    /// How many time units correspond to a single physical pixel on the plot.
    ///
    /// Computed from the plot's transform after each frame and used by visualizers
    /// on the next frame to determine aggregation levels.
    /// This avoids relying on `egui_plot::PlotMemory` keyed by view id, which breaks
    /// when the same blueprint view is shown multiple times.
    pub time_per_pixel: f64,

    /// Whether line-series data point markers are always displayed.
    pub always_show_line_data_markers: bool,

    /// Pulsar: the stat card the user clicked, if any.
    pub pulsar_active_card: Option<usize>,
}

impl Default for TimeSeriesViewState {
    fn default() -> Self {
        Self {
            scalar_range: None,
            full_data_time_range: AbsoluteTimeRange::EMPTY,
            time_offset: 0,
            default_series_name_formats: Default::default(),
            num_time_series_last_frame_per_instruction: Default::default(),
            plot_transform: None,
            time_per_pixel: 1.0,
            always_show_line_data_markers: false,
            pulsar_active_card: None,
        }
    }
}

impl ViewState for TimeSeriesViewState {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn heap_size_bytes(&self) -> u64 {
        re_byte_size::SizeBytes::heap_size_bytes(self)
    }
}

#[derive(Default)]
pub struct TimeSeriesView;


impl ViewClass for TimeSeriesView {
    fn identifier() -> ViewClassIdentifier {
        "PulsarTimeSeries".into()
    }

    fn display_name(&self) -> &'static str {
        "Pulsar time series"
    }

    fn icon(&self) -> &'static re_ui::Icon {
        &re_ui::icons::VIEW_TIMESERIES
    }

    fn help(&self, os: egui::os::OperatingSystem) -> Help {
        let egui::InputOptions {
            zoom_modifier,
            horizontal_scroll_modifier,
            vertical_scroll_modifier,
            ..
        } = egui::InputOptions::default(); // This is OK, since we don't allow the user to change these modifiers.

        Help::new("Time series view")
            .docs_link("https://rerun.io/docs/reference/types/views/time_series_view")
            .control("Pan", (icons::LEFT_MOUSE_CLICK, "+", "drag"))
            .control(
                "Horizontal pan",
                IconText::from_modifiers_and(os, horizontal_scroll_modifier, icons::SCROLL),
            )
            .control(
                "Zoom",
                IconText::from_modifiers_and(os, zoom_modifier, icons::SCROLL),
            )
            .control(
                "Zoom X-axis",
                IconText::from_modifiers_and(
                    os,
                    zoom_modifier | horizontal_scroll_modifier,
                    icons::SCROLL,
                ),
            )
            .control(
                "Zoom Y-axis",
                IconText::from_modifiers_and(
                    os,
                    zoom_modifier | vertical_scroll_modifier,
                    icons::SCROLL,
                ),
            )
            .control(
                "Zoom to selection",
                (MouseButtonText(SELECTION_RECT_ZOOM_BUTTON), "+", "drag"),
            )
            .control("Move time cursor", MouseButtonText(MOVE_TIME_CURSOR_BUTTON))
            .control("Reset view", ("double", icons::LEFT_MOUSE_CLICK))
            .control_separator()
            .control("Hide/show series", (icons::LEFT_MOUSE_CLICK, "legend"))
            .control(
                "Hide/show other series",
                (
                    IconText::from_modifiers_and(os, egui::Modifiers::ALT, icons::LEFT_MOUSE_CLICK),
                    "legend",
                ),
            )
    }

    fn on_register(
        &self,
        system_registry: &mut re_viewer_context::ViewSystemRegistrator<'_>,
    ) -> Result<(), ViewClassRegistryError> {
        crate::fallbacks::register_fallbacks(system_registry);

        system_registry.register_visualizer::<SeriesLinesSystem>()?;
        system_registry.register_visualizer::<SeriesPointsSystem>()?;
        system_registry.register_visualizer::<MeasurementsSeriesSystem>()?;
        // Pulsar: StateChange entities in a chart paint as labelled time regions
        // behind it. Same visualizer type as the state timeline view.
        system_registry.register_visualizer::<re_view_state_timeline::StateVisualizer>()?;
        // Pulsar: y-ranges (a check's accepted band) painted behind the lines.
        system_registry.register_visualizer::<crate::y_bands::YBandsSystem>()?;
        // Pulsar: a check's stat cards, in a bar under the chart.
        system_registry.register_visualizer::<crate::check_stats::CheckStatsSystem>()?;
        // Pulsar: a check's threshold lines, drawn by the view.
        system_registry.register_visualizer::<crate::h_lines::HLinesSystem>()?;
        Ok(())
    }

    fn new_state(&self) -> Box<dyn ViewState> {
        Box::<TimeSeriesViewState>::default()
    }

    fn preferred_tile_aspect_ratio(&self, _state: &dyn ViewState) -> Option<f32> {
        None
    }

    fn layout_priority(&self) -> re_viewer_context::ViewClassLayoutPriority {
        re_viewer_context::ViewClassLayoutPriority::Low
    }

    fn supports_visible_time_range(&self) -> bool {
        true
    }

    fn default_query_range(&self, _view_state: &dyn ViewState) -> QueryRange {
        QueryRange::TimeRange(TimeRange::EVERYTHING)
    }

    fn selection_ui<'a>(
        &'a self,
        _view_ctx: &re_viewer_context::ViewContext<'_>,
    ) -> re_viewer_context::ViewSelectionUi<'a> {
        re_viewer_context::ViewSelectionUi {
            visualizers: Some(time_series_visualizers_section(_view_ctx)),
            blueprint_properties: Some(Box::new(move |ui, ctx| {
                list_item::list_item_scope(ui, "time_series_selection_ui", |ui| {
                    view_property_ui::<PlotBackground>(ctx, ui);
                    view_property_ui::<PlotLegend>(ctx, ui);
                    view_property_ui::<PlotInteraction>(ctx, ui);

                    let link_x_axis = ViewProperty::from_archetype::<TimeAxis>(ctx)
                        .component_or_fallback::<LinkAxis>(
                            ctx,
                            TimeAxis::descriptor_link().component,
                        )?;

                    match link_x_axis {
                        LinkAxis::Independent => {
                            view_property_ui::<TimeAxis>(ctx, ui);
                        }
                        LinkAxis::LinkToGlobal => {
                            re_view::view_property_ui_with_redirect::<TimeAxis>(
                                ctx,
                                ui,
                                TimeAxis::descriptor_view_range().component,
                                re_viewer_context::GLOBAL_VIEW_ID,
                            );
                        }
                    }

                    view_property_ui::<ScalarAxis>(ctx, ui);

                    Ok::<(), ViewSystemExecutionError>(())
                })
                .inner?;

                Ok(())
            })),
            ..Default::default()
        }
    }

    fn spawn_heuristics(
        &self,
        ctx: &ViewerContext<'_>,
        include_entity: &dyn Fn(&EntityPath) -> bool,
    ) -> ViewSpawnHeuristics {
        re_tracing::profile_function!();

        // Note that point series has the same visualizable conditions, it doesn't matter which one we look at here.
        let Some(visualizable_entities) = ctx
            .visualizable_entities_per_visualizer
            .get(&SeriesLinesSystem::identifier())
        else {
            return ViewSpawnHeuristics::empty();
        };

        let entities_with_exact_match =
            visualizable_entities
                .iter()
                .filter_map(|(entity_path, reason)| {
                    if !include_entity(entity_path) {
                        return None;
                    }
                    should_auto_spawn_time_series(reason).then_some(entity_path)
                });

        ViewSpawnHeuristics::new_with_order_preserved(
            entities_with_exact_match
                .into_iter()
                .map(|entity_path| RecommendedView::new_single_entity(entity_path.clone())),
        )
    }

    /// Auto picked visualizers for an entity if there was not explicit selection.
    fn recommended_visualizers_for_entity(
        &self,
        entity_path: &EntityPath,
        visualizers_with_reason: &[(ViewSystemIdentifier, &VisualizableReason)],
        indicated_entities_per_visualizer: &PerVisualizerType<&IndicatedEntities>,
    ) -> RecommendedVisualizers {
        let available_visualizers: HashMap<ViewSystemIdentifier, &VisualizableReason> =
            visualizers_with_reason
                .iter()
                .map(|(visualizer, reason)| (*visualizer, *reason))
                .collect();

        let mut recommended = RecommendedVisualizers::new(
            available_visualizers
                .iter()
                .filter_map(|(visualizer, reason)| {
                    // Pulsar: one instruction per scalar source is right for the
                    // series visualizers only. For Pulsar's own (threshold lines
                    // have several Scalar fields) it would duplicate the entity.
                    if *visualizer != SeriesLinesSystem::identifier()
                        && *visualizer != SeriesPointsSystem::identifier()
                        && *visualizer != MeasurementsSeriesSystem::identifier()
                    {
                        return None;
                    }
                    // Filter out entities that weren't indicated.
                    // We later fall back on to line visualizers for those.
                    if indicated_entities_per_visualizer
                        .get(visualizer)?
                        .contains(entity_path)
                    {
                        let all_mappings: Vec<RecommendedMappings> = all_scalar_mappings(reason)
                            .map(|(component, source)| RecommendedMappings::new(component, source))
                            .collect();
                        Vec1::try_from_vec(all_mappings)
                            .ok()
                            .map(|mappings| (*visualizer, mappings))
                    } else {
                        None
                    }
                })
                .collect(),
        );

        // Pulsar: a StateChange entity in the view becomes background regions.
        let state_visualizer = re_view_state_timeline::StateVisualizer::identifier();
        if available_visualizers.contains_key(&state_visualizer)
            && indicated_entities_per_visualizer
                .get(&state_visualizer)
                .is_some_and(|entities| entities.contains(entity_path))
        {
            recommended.insert(state_visualizer, Default::default(), true);
        }
        let y_bands = crate::y_bands::YBandsSystem::identifier();
        if available_visualizers.contains_key(&y_bands) {
            recommended.insert(y_bands, Default::default(), true);
        }
        let h_lines = crate::h_lines::HLinesSystem::identifier();
        if available_visualizers.contains_key(&h_lines)
            && indicated_entities_per_visualizer
                .get(&h_lines)
                .is_some_and(|entities| entities.contains(entity_path))
        {
            recommended.insert(h_lines, Default::default(), true);
        }
        let check_stats = crate::check_stats::CheckStatsSystem::identifier();
        if available_visualizers.contains_key(&check_stats)
            && indicated_entities_per_visualizer
                .get(&check_stats)
                .is_some_and(|entities| entities.contains(entity_path))
        {
            recommended.insert(check_stats, Default::default(), true);
        }

        // If there were no other visualizers, but the SeriesLineSystem is available, use it.
        if recommended.all_recommendations().is_empty()
            && let Some(series_line_visualizable_reason) =
                available_visualizers.get(&SeriesLinesSystem::identifier())
        {
            let mut mappings_with_auto_spawn: Vec<RecommendedMappings> =
                all_scalar_mappings(series_line_visualizable_reason)
                    .map(|(component, source)| RecommendedMappings::new(component, source))
                    .collect();

            // Not all automatic recommendations should be spawned by default, since that can lead to
            // a huge number of visualizers being spawned for a single entity.
            let recommendation_only = if mappings_with_auto_spawn.len()
                > MAX_NUM_NON_INDICATED_RECOMMENDED_VISUALIZERS_PER_ENTITY
            {
                mappings_with_auto_spawn
                    .split_off(MAX_NUM_NON_INDICATED_RECOMMENDED_VISUALIZERS_PER_ENTITY)
            } else {
                Vec::new()
            };

            // First add mappings with auto spawn since they should show up higher in the list.
            if let Ok(mappings) = Vec1::try_from_vec(mappings_with_auto_spawn) {
                recommended.insert(SeriesLinesSystem::identifier(), mappings, true);
            }
            if let Ok(mappings) = Vec1::try_from_vec(recommendation_only) {
                recommended.insert(SeriesLinesSystem::identifier(), mappings, false);
            }
        }

        recommended
    }

    /// Accept drops of scalar components onto the time series view. For each dropped component, a
    /// new `SeriesLines` visualizer is added that remaps `Scalars.scalars` from it.
    fn handle_component_drop(
        &self,
        ctx: &ViewerContext<'_>,
        view_id: ViewId,
        component_paths: &[ComponentPath],
        released: bool,
    ) -> DragAndDropFeedback {
        match re_view::handle_component_drop(
            ctx,
            view_id,
            component_paths,
            released,
            SeriesLinesSystem::identifier(),
            Scalars::descriptor_scalars().component,
        ) {
            re_view::ComponentDropResult::Accept => DragAndDropFeedback::Accept,
            re_view::ComponentDropResult::CompatibleButAlreadyVisualized => {
                DragAndDropFeedback::Reject(Some("Already visualized"))
            }
            re_view::ComponentDropResult::Incompatible => {
                DragAndDropFeedback::Reject(Some("Not a scalar component"))
            }
        }
    }

    fn ui(
        &self,
        ctx: &ViewerContext<'_>,
        _missing_chunk_reporter: &re_viewer_context::MissingChunkReporter,
        ui: &mut egui::Ui,
        state: &mut dyn ViewState,
        query: &ViewQuery<'_>,
        mut system_output: SystemExecutionOutput,
    ) -> Result<re_viewer_context::ViewClassUiOutput, ViewSystemExecutionError> {
        re_tracing::profile_function!();

        let state = state.downcast_mut::<TimeSeriesViewState>()?;

        // Drain draw data from visualizer outputs (accessing field directly to avoid
        // borrow conflict with view_systems which is borrowed via all_plot_series).
        let re_renderer_draw_data: Vec<_> = system_output.drain_draw_data().collect();

        let line_series = system_output
            .visualizer_data_or_default::<SeriesLinesOutput>(SeriesLinesSystem::identifier())?;
        let point_series = system_output
            .visualizer_data_or_default::<SeriesPointsOutput>(SeriesPointsSystem::identifier())?;
        let measurements_series = system_output
            .visualizer_data_or_default::<MeasurementsSeriesOutput>(
                MeasurementsSeriesSystem::identifier(),
            )?;

        let all_plot_series: Vec<_> = chain!(
            &line_series.all_series,
            &point_series.all_series,
            &measurements_series.all_series
        )
        .collect();

        let region_lanes: Vec<re_view_state_timeline::StateLane> = system_output
            .iter_visualizer_data::<re_view_state_timeline::StateLanesOutput>()
            .flat_map(|data| data.groups.iter().flat_map(|group| group.lanes.iter().cloned()))
            .collect();

        let y_bands: Vec<crate::y_bands::YBand> = system_output
            .iter_visualizer_data::<crate::y_bands::YBandsData>()
            .flat_map(|data| data.bands.iter().cloned())
            .collect();

        let h_lines: Vec<crate::h_lines::HLine> = system_output
            .iter_visualizer_data::<crate::h_lines::HLinesData>()
            .flat_map(|data| data.lines.iter().cloned())
            .collect();
        let check_cards: Vec<crate::check_stats::StatCard> = system_output
            .iter_visualizer_data::<crate::check_stats::CheckStatsData>()
            .flat_map(|data| data.cards.iter().cloned())
            .collect();
        if state.pulsar_active_card.is_some_and(|i| i >= check_cards.len()) {
            state.pulsar_active_card = None;
        }

        // Pulsar: each series' source component, for the readout's raw-sample query.
        let mut series_sources: HashMap<_, (ComponentIdentifier, bool)> = HashMap::default();

        state.num_time_series_last_frame_per_instruction.clear();

        let view_query_result = ctx.lookup_query_result(query.view_id);

        let mut series_names = SeriesNamesContext::default();

        {
            re_tracing::profile_scope!("iterate all_plot_series");

            for series in &all_plot_series {
                let instruction_id = series.visualizer_instruction_id;

                if let Some(data_result) = view_query_result
                    .tree
                    .lookup_result_by_visualizer_instruction(instruction_id)
                    && let Some(instruction) = data_result
                        .visualizer_instructions
                        .iter()
                        .find(|instr| instr.id == instruction_id)
                {
                    // Each visualizer maps into its own value component; look up the one this
                    // instruction actually keys its mapping on.
                    let value_component =
                        if instruction.visualizer_type == MeasurementsSeriesSystem::identifier() {
                            Measurements::descriptor_values().component
                        } else {
                            Scalars::descriptor_scalars().component
                        };

                    let (component, selector) = instruction
                        .component_mappings
                        .get(&value_component)
                        .and_then(|mapping| match mapping {
                            re_viewer_context::VisualizerComponentSource::SourceComponent {
                                source_component,
                                selector,
                            } => Some((*source_component, selector.clone())),
                            _ => None,
                        })
                        .unwrap_or((value_component, String::new()));

                    series_sources.insert(instruction_id, (component, selector.is_empty()));
                    series_names.insert(
                        instruction_id,
                        SeriesInfo::new(
                            data_result.entity_path.clone(),
                            component,
                            &selector,
                            value_component,
                        ),
                    );
                }

                state
                    .num_time_series_last_frame_per_instruction
                    .entry(instruction_id)
                    .or_default()
                    .insert(series.id());
            }
        }

        // Compute disambiguated names after all series are collected
        state.default_series_name_formats = series_names.dissambiguated_names();

        // Note that a several plot items can point to the same entity path and in some cases even to the same instance path!
        // (e.g. when plotting both lines & points with the same entity/instance path)
        let plot_item_id_to_data_result_address: HashMap<ItemId, DataResultInteractionAddress> =
            all_plot_series
                .iter()
                .map(|series| {
                    (
                        series.id(),
                        DataResultInteractionAddress {
                            view_id: query.view_id,
                            instance_path: series.instance_path.clone(),
                            visualizer: Some(series.visualizer_instruction_id),
                        },
                    )
                })
                .collect();

        let current_time = ctx.time_ctrl.time_i64();
        let Some(timeline) = ctx.time_ctrl.timeline() else {
            return Ok(Default::default());
        };
        let time_type = timeline.typ();

        let timeline_name = timeline.name().to_string();

        // Get the minimum time/X value for the entire plot…
        let min_time = all_plot_series
            .iter()
            .map(|line| line.min_time)
            .min()
            .unwrap_or(0);

        // …then use that as an offset to avoid nasty precision issues with
        // large times (nanos since epoch does not fit into a f64).
        let time_offset = match timeline.typ() {
            TimeType::Sequence => min_time,
            TimeType::TimestampNs | TimeType::DurationNs => {
                // In order to make the tick-marks on the time axis fall on whole days, hours, minutes etc,
                // we need to round to a whole day:
                round_nanos_to_start_of_day(min_time)
            }
        };
        state.time_offset = time_offset;

        let recording = ctx.recording();

        let timeline_range = recording
            .time_range_for(timeline.name())
            .unwrap_or(AbsoluteTimeRange::EVERYTHING);

        {
            // Get the full time range of data in the store for all visible entity paths.
            // This queries the store directly rather than looking at loaded data,
            // so it works even before any chunks are loaded.
            state.full_data_time_range = AbsoluteTimeRange::EMPTY;

            for data_result in view_query_result.tree.iter_data_results() {
                if data_result.tree_prefix_only || !data_result.is_visible() {
                    continue;
                }
                state.full_data_time_range = state
                    .full_data_time_range
                    .union(data_result_time_range(ctx, data_result, query.timeline));
            }
        }

        let view_id = query.view_id;

        let (always_show_line_data_markers, tooltip_mode) = {
            let view_ctx = self.view_context(ctx, view_id, state, query.space_origin);
            let plot_interaction = ViewProperty::from_archetype::<PlotInteraction>(&view_ctx);

            let always_show_line_data_markers =
                plot_interaction.component_or_fallback::<PointsDisplay>(
                    &view_ctx,
                    PlotInteraction::descriptor_points_display().component,
                )? == PointsDisplay::Always;

            let tooltip_mode = plot_interaction.component_or_fallback::<TooltipMode>(
                &view_ctx,
                PlotInteraction::descriptor_tooltip_mode().component,
            )?;

            (always_show_line_data_markers, tooltip_mode)
        };
        state.always_show_line_data_markers = always_show_line_data_markers;

        let view_ctx = self.view_context(ctx, view_id, state, query.space_origin);
        let background = ViewProperty::from_archetype::<PlotBackground>(&view_ctx);
        let background_color = background.component_or_fallback::<Color>(
            &view_ctx,
            PlotBackground::descriptor_color().component,
        )?;
        let show_grid = background.component_or_fallback::<Enabled>(
            &view_ctx,
            PlotBackground::descriptor_show_grid().component,
        )?;

        let plot_legend = ViewProperty::from_archetype::<PlotLegend>(&view_ctx);
        let legend_visible = plot_legend.component_or_fallback::<Visible>(
            &view_ctx,
            PlotLegend::descriptor_visible().component,
        )?;
        let legend_corner = plot_legend.component_or_fallback::<Corner2D>(
            &view_ctx,
            PlotLegend::descriptor_corner().component,
        )?;

        let time_axis = ViewProperty::from_archetype::<TimeAxis>(&view_ctx);

        let view_current_time = re_sdk_types::encodings::TimeInt(
            current_time
                .unwrap_or_default()
                .at_least(timeline_range.min.as_i64()),
        );

        let (time_range_property, view_time_range) = re_view::time_axis_view_range(&view_ctx)?;

        let resolve_time_range =
            |view_time_range: &re_sdk_types::blueprint::components::TimeRange| {
                let range = re_view::resolve_time_axis_range(
                    view_time_range,
                    timeline_range,
                    view_current_time,
                );

                // Into plot space, where `f64` coordinates still have enough precision.
                make_range_sane(Range1D::new(
                    range.min.as_i64().saturating_sub(time_offset) as f64,
                    range.max.as_i64().saturating_sub(time_offset) as f64,
                ))
            };

        let x_range = resolve_time_range(&view_time_range);

        let scalar_axis = ViewProperty::from_archetype::<ScalarAxis>(&view_ctx);
        let y_range = scalar_axis.component_or_fallback::<Range1D>(
            &view_ctx,
            ScalarAxis::descriptor_range().component,
        )?;
        let y_range = make_range_sane(y_range);

        let zoom_lock = Vec2b::new(
            **time_axis.component_or_fallback::<LockRangeDuringZoom>(
                &view_ctx,
                TimeAxis::descriptor_zoom_lock().component,
            )?,
            **scalar_axis.component_or_fallback::<LockRangeDuringZoom>(
                &view_ctx,
                ScalarAxis::descriptor_zoom_lock().component,
            )?,
        );

        // TODO(#5075): Boxed-zoom should be fixed to accommodate the locked range.
        let timestamp_format = ctx.app_options().timestamp_format;

        let min_axis_thickness = ui.tokens().small_icon_size.y;

        let legend_id = ui.make_persistent_id("plot_legend");
        let legend_hovered = ui
            .ctx()
            .read_response(re_plot::legend::legend_frame_id(legend_id))
            .is_some_and(|r| r.contains_pointer());

        ui.scope(|ui| {
            // Use timeline_name as part of id, so that egui stores different pan/zoom for different timelines.
            // Derived from `ui.scope_id()` so it's unique per UI location (i.e. works when the same view is shown multiple times).
            let plot_id_src = ("plot", &timeline_name);
            let plot_id = ui.make_persistent_id(plot_id_src);

            ui.style_mut().visuals.extreme_bg_color = background_color.into();
            let tokens = re_ui::design_tokens_of_visuals(ui.visuals());

            let mut plot = Plot::new(plot_id_src)
                .id(plot_id)
                .show_grid(**show_grid)
                .grid_color(tokens.plot_grid_color)
                .grid_fade(tokens.plot_grid_fade)
                .auto_bounds(false)
                .allow_zoom(!zoom_lock)
                .allow_scroll(!legend_hovered)
                .custom_x_axes(vec![
                    egui_plot::AxisHints::new_x()
                        .min_thickness(min_axis_thickness)
                        .formatter(move |time, _| {
                            re_log_types::TimeCell::new(
                                time_type,
                                (time.value as i64).saturating_add(time_offset),
                            )
                            .format_compact(timestamp_format)
                        }),
                ])
                .custom_y_axes(vec![
                    egui_plot::AxisHints::new_y()
                        .min_thickness(min_axis_thickness)
                        .formatter(move |mark, _| format_y_axis(mark)),
                ])
                // Suppress egui_plot's built-in tooltip so we can render our own.
                .show_x(false)
                .show_y(false);

            // Sharing the same cursor is always nice:
            plot = plot.link_cursor(egui::Id::unique(timeline.name()), [true; 2]);

            // Pulsar: leave room under the plot for the check's stat bar.
            let has_stat_bar = !check_cards.is_empty() || h_lines.iter().any(|l| !l.label.is_empty());
            if has_stat_bar {
                plot = plot.height(
                    (ui.available_height() - crate::check_stats::BAR_HEIGHT).max(40.0),
                );
            }

            // Legend is rendered separately after plot.show() using our LegendWidget.

            match timeline.typ() {
                TimeType::Sequence => {}
                TimeType::DurationNs | TimeType::TimestampNs => {
                    let canvas_size = ui.available_size();
                    plot =
                        plot.x_grid_spacer(move |spacer| nanos_grid_spacer(canvas_size, &spacer));
                }
            }

            let mut plot_double_clicked = false;
            let mut new_view_time_range = None;
            let egui_plot::PlotResponse {
                inner: (),
                response,
                transform,
                hovered_plot_item: _,
            } = plot.show(ui, |plot_ui| {
                if plot_ui.response().secondary_clicked()
                    && let Some(pointer) = plot_ui.pointer_coordinate()
                {
                    let time = re_log_types::TimeReal::from(pointer.x as i64 + time_offset);

                    new_view_time_range =
                        re_view::set_time_cursor(ctx, current_time, Some(&view_time_range.0), time);
                }

                plot_double_clicked = plot_ui.response().double_clicked();

                // Let the user pick x and y ranges from the blueprint:
                plot_ui.set_plot_bounds_y(y_range);
                plot_ui.set_plot_bounds_x(x_range);
            });

            // Merge per-series finite ranges from visible series (used as the
            // fallback for `ScalarAxis::descriptor_range()`).
            state.scalar_range = None;
            for series in &all_plot_series {
                if !series.visible {
                    continue;
                }
                let Some(series_range) = series.value_range else {
                    continue;
                };
                if let Some(range) = state.scalar_range.as_mut() {
                    if series_range.start() < range.start() {
                        *range.start_mut() = series_range.start();
                    }
                    if series_range.end() > range.end() {
                        *range.end_mut() = series_range.end();
                    }
                } else {
                    state.scalar_range = Some(series_range);
                }
            }
            // Pulsar: like a check chart, the y range always shows its bands and
            // threshold lines whole.
            let line_ranges = h_lines.iter().map(|l| Range1D::new(l.y, l.y));
            for band_range in y_bands.iter().map(|b| Range1D::new(b.min, b.max)).chain(line_ranges) {
                state.scalar_range = Some(match state.scalar_range {
                    Some(range) => Range1D::new(
                        range.start().min(band_range.start()),
                        range.end().max(band_range.end()),
                    ),
                    None => band_range,
                });
            }

            // Store the transform for next frame's visualizers to use.
            // They drew this frame with the old one, so if it changed we need another frame.
            let transform_changed = state.plot_transform.is_none_or(|prev| {
                prev.frame() != transform.frame() || prev.bounds() != transform.bounds()
            });
            if transform_changed {
                ui.request_repaint();
            }
            state.plot_transform = Some(transform);

            // Update time_per_pixel from the plot transform for use by visualizers next frame.
            {
                let points_per_time = transform.dpos_dvalue_x();
                let pixels_per_time = ui.ctx().pixels_per_point() as f64 * points_per_time;
                state.time_per_pixel = 1.0 / pixels_per_time.max(f64::EPSILON);
            }

            // Cross-view time-range highlight (e.g. hovered state phase). Only paint
            // StateTimeline-kind highlights on the current timeline that carry a color.
            if let Some(highlight) = ctx.time_ctrl.highlighted_range()
                && highlight.timeline == *timeline.name()
                && highlight.kind == re_viewer_context::TimeRangeHighlightKind::StateTimeline
                && let Some(color) = highlight.color
            {
                paint_time_range_highlight(
                    ui,
                    &response,
                    &transform,
                    time_offset,
                    highlight.range,
                    color,
                );
            }

            paint_y_bands(ui, &response, &transform, &y_bands);
            paint_region_lanes(ui, &response, &transform, time_offset, &region_lanes);

            // Render re_renderer draw data (already in screen space) via ViewBuilder.
            render_re_renderer_draw_data(
                ctx,
                ui,
                &response,
                query.view_id.render_view_id(),
                re_renderer_draw_data,
            )?;

            crate::h_lines::paint_h_lines(
                ui,
                response.rect,
                |y| transform.position_from_point(&PlotPoint::new(0.0, y)).y,
                &h_lines,
            );

            // Pulsar: the active stat card's violations or threshold line, on top.
            if let Some(card) = state.pulsar_active_card.and_then(|i| check_cards.get(i)) {
                let to_screen = |t: i64, v: f64| {
                    transform.position_from_point(&PlotPoint::new(
                        t.saturating_sub(time_offset) as f64,
                        v,
                    ))
                };
                crate::check_stats::paint_violations(ui, response.rect, to_screen, card);
                // The referenced threshold: a Pulsar line, else a two-point series.
                let referenced_y = (!card.hline_ref.is_empty())
                    .then(|| {
                        h_lines.iter().find(|l| l.label == card.hline_ref).map(|l| l.y).or_else(|| {
                            all_plot_series
                                .iter()
                                .find(|s| s.label == card.hline_ref)
                                .and_then(|s| s.points.first().map(|&(_, y)| y))
                        })
                    })
                    .flatten();
                if let Some(y) = referenced_y {
                    crate::check_stats::paint_highlighted_hline(
                        ui,
                        response.rect,
                        transform.position_from_point(&PlotPoint::new(0.0, y)).y,
                    );
                }
            }

            // Render the configured tooltip and use its closest series for interaction.
            let hovered_data_result = (!legend_hovered)
                .then(|| {
                    show_time_series_tooltip(
                        ui,
                        &response,
                        &transform,
                        &all_plot_series,
                        time_offset,
                        time_type,
                        timestamp_format,
                        &plot_item_id_to_data_result_address,
                        tooltip_mode,
                    )
                })
                .flatten();

            if let Some(hovered) = hovered_data_result.clone().or_else(|| {
                if response.hovered() {
                    Some(re_viewer_context::Item::View(query.view_id))
                } else {
                    None
                }
            }) {
                ctx.handle_select_hover_drag_interactions(&response, hovered, false);
            }

            paint_time_cursor(
                ctx,
                ui,
                &response,
                &transform,
                time_offset,
                current_time,
                &view_time_range,
                &mut new_view_time_range,
            );

            // Pulsar: every chart reads out the value of each series at one shared
            // instant, the hovered time in any Pulsar chart, else the time cursor.
            update_shared_hover(ui, &response, &transform, time_offset);
            scrub_time_cursor(ctx, ui, &response, &transform, time_offset, current_time, view_id);
            paint_pulsar_readout(
                ctx,
                ui,
                &response,
                &transform,
                &all_plot_series,
                &series_sources,
                *timeline.name(),
                state.time_per_pixel,
                time_offset,
                time_type,
                timestamp_format,
                current_time,
            );

            // Can determine whether we're resetting only now since we need to know whether there's a plot item hovered.
            let is_resetting = plot_double_clicked && hovered_data_result.is_none();

            if is_resetting {
                reset_view(ctx, &time_range_property, &scalar_axis);

                ui.request_repaint(); // Make sure we get another frame with the view reset.
            } else {
                let unchanged_bounds = egui_plot::PlotBounds::from_min_max(
                    [x_range.start(), y_range.start()],
                    [x_range.end(), y_range.end()],
                );

                if unchanged_bounds != *transform.bounds() || new_view_time_range.is_some() {
                    if let Some(new_view_time_range) = new_view_time_range
                        .or_else(|| {
                            let new_x_range = transform_axis_range(transform, 0);

                            if new_x_range == x_range {
                                return None;
                            }

                            Some(re_view::recover_relative_boundaries_after_zoom_or_pan(
                                re_view::time_axis_time_from_plot(
                                    new_x_range.start().into(),
                                    time_offset,
                                ),
                                re_view::time_axis_time_from_plot(
                                    new_x_range.end().into(),
                                    time_offset,
                                ),
                                &view_time_range,
                                view_current_time,
                            ))
                        })
                        .map(re_sdk_types::blueprint::components::TimeRange)
                        && view_time_range != new_view_time_range
                    {
                        time_range_property.save_blueprint_component(
                            ctx,
                            &TimeAxis::descriptor_view_range(),
                            &new_view_time_range,
                        );
                        ui.request_repaint(); // Make sure we get another frame with this new range applied.
                    }

                    let new_y_range = transform_axis_range(transform, 1);

                    // Write new y_range if it has changed.
                    if new_y_range != y_range {
                        scalar_axis.save_blueprint_component(
                            ctx,
                            &ScalarAxis::descriptor_range(),
                            &new_y_range,
                        );
                        ui.request_repaint(); // Make sure we get another frame with this new range applied.
                    }
                }
            }

            // Render our legend overlay and sync visibility with blueprint.
            if *legend_visible.0 {
                // Build a map from label → whether any series with that label is globally hovered.
                // Multiple series can share a label (e.g. /spiral[0] and /spiral[1]), so we OR
                // across all of them.
                let prev_hovered_items = ctx.selection_state().hovered_items();
                let mut label_hovered: ahash::HashMap<&str, bool> = ahash::HashMap::default();
                for series in all_plot_series
                    .iter()
                    .filter(|s| !matches!(s.kind, PlotSeriesKind::Clear))
                {
                    let address = plot_item_id_to_data_result_address[&series.id()].clone();
                    let is_hovered = prev_hovered_items
                        .contains_item(&re_viewer_context::Item::DataResult(address.clone()))
                        || prev_hovered_items.contains_item(&re_viewer_context::Item::DataResult(
                            address.as_entity_all(),
                        ));
                    if is_hovered {
                        label_hovered.insert(series.label.as_str(), true);
                    } else {
                        label_hovered.entry(series.label.as_str()).or_insert(false);
                    }
                }

                let legend_widget =
                    re_plot::legend::LegendWidget::new(re_plot::legend::LegendConfig {
                        position: legend_corner.into(),
                        id: legend_id,
                    });

                // Render the legend overlaid on the plot rect.
                let plot_rect = response.rect;
                let mut legend_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(plot_rect)
                        .layout(egui::Layout::left_to_right(egui::Align::Min)),
                );

                let legend_output = legend_widget.show_entries(
                    &mut legend_ui,
                    all_plot_series
                        .iter()
                        .filter(|s| !matches!(s.kind, PlotSeriesKind::Clear))
                        .map(|s| re_plot::legend::LegendEntry {
                            id: s.id(),
                            label: match &s.unit {
                                Some(unit) if !s.label.is_empty() => {
                                    let label = &s.label;
                                    format!("{label} [{unit}]")
                                }
                                _ => s.label.clone(),
                            },
                            color: s.color,
                            visible: s.visible,
                            hovered: label_hovered
                                .get(s.label.as_str())
                                .copied()
                                .unwrap_or(false),
                        }),
                );

                // Legend hover → global item hover (only when no plot item is hovered).
                if hovered_data_result.is_none()
                    && let Some(hovered_id) = legend_output.hovered_id
                    && let Some(address) = plot_item_id_to_data_result_address.get(&hovered_id)
                {
                    ctx.selection_state()
                        .set_hovered(re_viewer_context::Item::DataResult(address.clone()));
                }

                let hidden_items = legend_output.hidden_ids;
                update_series_visibility_from_legend(ctx, query, &all_plot_series, &hidden_items);
            }

            // Pulsar: the check's stat bar, under the plot.
            if has_stat_bar {
                crate::check_stats::stat_bar_ui(
                    ui,
                    &check_cards,
                    &h_lines,
                    &mut state.pulsar_active_card,
                );
            }

            Ok::<(), ViewSystemExecutionError>(())
        })
        .inner?;

        Ok(Default::default())
    }
}

fn all_scalar_mappings_for(
    matches: &IntMap<ComponentIdentifier, DatatypeMatch>,
) -> Vec<RecommendedMappings> {
    let target = Scalars::descriptor_scalars().component;

    matches
        .iter()
        .sorted_by_key(|(k, _)| **k)
        .flat_map(|(source_component, match_info)| match match_info {
            DatatypeMatch::NativeSemantics { .. } => {
                Either::Left(std::iter::once(RecommendedMappings::new(
                    target,
                    VisualizerComponentSource::simple_map(*source_component),
                )))
            }

            DatatypeMatch::PhysicalDatatypeOnly { selectors, .. } => {
                if selectors.is_empty() {
                    Either::Left(std::iter::once(RecommendedMappings::new(
                        target,
                        VisualizerComponentSource::simple_map(*source_component),
                    )))
                } else {
                    Either::Right(selectors.iter().map(|(selector, _datatype)| {
                        RecommendedMappings::new(
                            target,
                            VisualizerComponentSource::SourceComponent {
                                source_component: *source_component,
                                selector: selector.to_string(),
                            },
                        )
                    }))
                }
            }
        })
        .collect()
}

/// Renders the active visualizer pills for the "Visualizers" section in the selection panel.
fn visualizers_section_ui(ui: &mut egui::Ui, ctx: &re_viewer_context::ViewContext<'_>) {
    list_item::list_item_scope(ui, "time_series_visualizers_ui", |ui| {
        re_tracing::profile_function!();
        let query_result = ctx.query_result;

        let handles = query_result.tree.data_results_by_path.values().sorted();
        for handle in handles {
            let Some(node) = query_result.tree.data_results.get(*handle) else {
                continue;
            };
            let pill_margin = egui::Margin::symmetric(8, 6);
            for instruction in &node.data_result.visualizer_instructions {
                if !node.data_result.visible {
                    continue;
                }

                ui.add_space(10.0);

                crate::visualizer_ui::visualizer_ui_element(
                    ui,
                    ctx,
                    node,
                    pill_margin,
                    instruction,
                );
            }
        }
    });
}

/// Returns a priority score for a given Arrow datatype.
/// Lower scores are preferred.
fn scalar_datatype_priority(datatype: &re_log_types::external::arrow::datatypes::DataType) -> u32 {
    use re_log_types::external::arrow::datatypes::DataType;
    match datatype {
        DataType::Float64 => 0,
        DataType::Float32 => 1,
        DataType::Float16 => 2,
        // Note: We can visualize the following datatype but don't recommend them.
        DataType::Int64 => 3,
        DataType::Int32 => 5,
        DataType::Int16 => 7,
        DataType::Int8 => 9,
        DataType::UInt64 => 4,
        DataType::UInt32 => 6,
        DataType::UInt16 => 8,
        DataType::UInt8 => 10,
        DataType::Boolean => 11,
        _ => 100, // Any other type gets lowest priority
    }
}

const RECOMMENDED_DATATYPES: &[DataType] =
    &[DataType::Float64, DataType::Float32, DataType::Float16];

fn should_auto_spawn_time_series(reason: &VisualizableReason) -> bool {
    has_native_scalar_semantics(reason) && all_scalar_mappings(reason).next().is_some()
}

fn has_native_scalar_semantics(reason: &VisualizableReason) -> bool {
    // This is always going to be `Some`, but nicer than writing `expect`.
    let Some(scalar_type) = Scalars::descriptor_scalars().component_type else {
        return false;
    };

    let VisualizableReason::SingleRequiredComponentMatch(m) = reason else {
        return reason.full_native_match(Scalars::descriptor_scalars().component);
    };

    m.matches.values().any(|match_info| {
        matches!(
            match_info,
            DatatypeMatch::NativeSemantics { component_type, .. }
            | DatatypeMatch::PhysicalDatatypeOnly { component_type, .. }
                if component_type.as_ref() == Some(&scalar_type)
        )
    })
}

fn all_scalar_mappings(
    reason: &VisualizableReason,
) -> impl Iterator<Item = (ComponentIdentifier, VisualizerComponentSource)> {
    let re_viewer_context::VisualizableReason::SingleRequiredComponentMatch(m) = reason else {
        return Either::Left(std::iter::empty());
    };
    let matches = &m.matches;

    // Every scalar-valued target shares the rerun-native `Scalar` semantic type, so the type check
    // below can use `Scalars.scalars` for any visualizer.
    let target_component = m.target_component;
    let target_component_type = Scalars::descriptor_scalars().component_type;

    // Flatten all (component, selector) pairs into a single comparable list
    // to find the globally best match across all components.
    let candidates = matches.iter().flat_map(|(source_component, match_info)| {
        let is_rerun_native_type = match_info.component_type() == target_component_type.as_ref();

        // If it's not the exact semantic type that we're looking for,
        // but it is a Rerun-builtin semantic type then we don't consider it at all.
        if !is_rerun_native_type
            && match_info
                .component_type()
                .is_some_and(|t| t.is_rerun_type())
        {
            return Either::Left(Either::Right(std::iter::empty()));
        }

        let primary_match_order = match match_info {
            DatatypeMatch::NativeSemantics { .. } => {
                i32::from(*source_component != target_component)
            }
            DatatypeMatch::PhysicalDatatypeOnly { .. } => {
                if *source_component == target_component {
                    0
                } else {
                    2
                }
            }
        };

        match match_info {
            DatatypeMatch::NativeSemantics {
                arrow_data_type, ..
            } => Either::Left(Either::Left(std::iter::once((
                primary_match_order,
                is_rerun_native_type,
                scalar_datatype_priority(arrow_data_type),
                *source_component,
                0usize,
                String::new(),
            )))),
            DatatypeMatch::PhysicalDatatypeOnly {
                arrow_data_type,
                selectors,
                ..
            } => {
                if selectors.is_empty() {
                    if is_rerun_native_type
                        || RECOMMENDED_DATATYPES.contains(match_info.arrow_data_type())
                    {
                        Either::Left(Either::Left(std::iter::once((
                            primary_match_order,
                            is_rerun_native_type,
                            scalar_datatype_priority(arrow_data_type),
                            *source_component,
                            0usize,
                            String::new(),
                        ))))
                    } else {
                        Either::Left(Either::Right(std::iter::empty()))
                    }
                } else {
                    // Nested field access: selector_index preserves field definition order.
                    Either::Right(selectors.iter().enumerate().filter_map(
                        move |(selector_index, (selector, datatype))| {
                            (is_rerun_native_type || RECOMMENDED_DATATYPES.contains(datatype))
                                .then_some((
                                    primary_match_order,
                                    is_rerun_native_type,
                                    scalar_datatype_priority(datatype),
                                    *source_component,
                                    selector_index,
                                    selector.to_string(),
                                ))
                        },
                    ))
                }
            }
        }
    });

    // Priority key (lower = better):
    // 1. primary_match_order (0=exact match, 1=semantic match, 2=physical only)
    // 2. is_rerun_native_type (false < true, prefer custom types)
    // 3. scalar_datatype_priority (Float64=0, Float32=1, etc.)
    // 4. source_component (deterministic component selection)
    // 5. selector_index (field definition order within component)
    Either::Right(
        candidates
            .into_iter()
            .sorted_by_key(
                |(match_order, is_rerun, dt_priority, component, field_order, _)| {
                    (
                        *match_order,
                        *is_rerun,
                        *dt_priority,
                        *component,
                        *field_order,
                    )
                },
            )
            .map(move |(_, _, _, source_component, _, selector)| {
                (
                    target_component,
                    VisualizerComponentSource::SourceComponent {
                        source_component,
                        selector,
                    },
                )
            }),
    )
}

/// Shows the configured time-series tooltip and returns the closest data result for interaction.
///
/// Returns the hovered data result item for selection/highlighting, or `None` if
/// no data point is close enough.
#[expect(clippy::too_many_arguments)]
fn show_time_series_tooltip(
    ui: &egui::Ui,
    response: &egui::Response,
    transform: &egui_plot::PlotTransform,
    all_plot_series: &[&crate::PlotSeries],
    time_offset: i64,
    time_type: TimeType,
    timestamp_format: re_log_types::TimestampFormat,
    plot_item_id_to_data_result_address: &ahash::HashMap<ItemId, DataResultInteractionAddress>,
    tooltip_mode: TooltipMode,
) -> Option<re_viewer_context::Item> {
    match tooltip_mode {
        TooltipMode::Nearest => nearest_data_point_tooltip(
            ui,
            response,
            transform,
            all_plot_series,
            time_offset,
            time_type,
            timestamp_format,
            plot_item_id_to_data_result_address,
        ),
        TooltipMode::All => all_series_tooltip(
            ui,
            response,
            transform,
            all_plot_series,
            time_offset,
            time_type,
            timestamp_format,
            plot_item_id_to_data_result_address,
        ),
    }
}

#[expect(clippy::too_many_arguments)]
fn nearest_data_point_tooltip(
    ui: &egui::Ui,
    response: &egui::Response,
    transform: &egui_plot::PlotTransform,
    all_plot_series: &[&crate::PlotSeries],
    time_offset: i64,
    time_type: TimeType,
    timestamp_format: re_log_types::TimestampFormat,
    plot_item_id_to_data_result_address: &ahash::HashMap<ItemId, DataResultInteractionAddress>,
) -> Option<re_viewer_context::Item> {
    let hover_pos = match response.hover_pos() {
        Some(pos) if response.rect.contains(pos) => pos,
        _ => return None,
    };

    // Use a larger radius than the default egui interact_radius (5px) for better UX.
    // This makes it easier to hover data points without needing pixel-perfect precision.
    const TOOLTIP_INTERACT_RADIUS: f32 = 16.0;
    let interact_radius_sq = TOOLTIP_INTERACT_RADIUS.powi(2);

    // Find the closest actual data point across all visible series.
    let hover_value = transform.value_from_position(hover_pos);
    let hover_time = (hover_value.x as i64).saturating_add(time_offset);

    let mut closest: Option<(f32, usize)> = None; // (dist_sq, series_index)
    let mut closest_time: i64 = 0;
    let mut closest_value: f64 = 0.0;

    for (series_idx, series) in all_plot_series.iter().enumerate() {
        if !series.visible
            || series.points.is_empty()
            || matches!(series.kind, PlotSeriesKind::Clear)
        {
            continue;
        }

        let idx = series
            .points
            .binary_search_by_key(&hover_time, |&(t, _)| t)
            .unwrap_or_else(|i| i);

        // Check the point at `idx` and the one before it (the two nearest candidates).
        let candidates = [idx.saturating_sub(1), idx.min(series.points.len() - 1)];
        for &candidate_idx in &candidates {
            if candidate_idx >= series.points.len()
                || (candidate_idx == 0 && series.first_point_is_continuity_bridge)
            {
                continue;
            }
            let (t, v) = series.points[candidate_idx];
            let plot_x = (t.saturating_sub(time_offset)) as f64;
            let screen_pos = transform.position_from_point(&PlotPoint::new(plot_x, v));
            let dist_sq = hover_pos.distance_sq(screen_pos);

            if dist_sq <= interact_radius_sq && closest.is_none_or(|(best, _)| dist_sq < best) {
                closest = Some((dist_sq, series_idx));
                closest_time = t;
                closest_value = v;
            }
        }
    }

    let (_, series_idx) = closest?;
    let series = all_plot_series[series_idx];

    // Draw a small dot at the hovered data point (always shown, no delay).
    let plot_x = (closest_time.saturating_sub(time_offset)) as f64;
    let point_screen_pos = transform.position_from_point(&PlotPoint::new(plot_x, closest_value));
    let painter = ui.painter();
    let series_color = series.color;
    painter.circle_filled(point_screen_pos, 3.0, series_color);

    // Strip the date to keep the tooltip compact — most recordings don't span midnight.
    // Formatting still auto-trims trailing zeros (ms → 3 digits, µs → 6, etc.).
    let time_label = {
        let tooltip_format = if time_type == TimeType::TimestampNs {
            timestamp_format.with_date_visibility(re_log_types::DateVisibility::HideDate)
        } else {
            timestamp_format
        };
        time_type.format(
            re_log_types::TimeInt::new_temporal(closest_time),
            tooltip_format,
        )
    };
    let y_value_text = tooltip_series_value(series, closest_value);
    let display_label = tooltip_series_label(series);

    re_plot::tooltip::show_plot_tooltip(
        ui,
        response,
        series.id(),
        &time_label,
        &display_label,
        &y_value_text,
        series_color,
    );

    // Map the hovered series to a data result for selection/highlighting.
    plot_item_id_to_data_result_address
        .get(&series.id())
        .map(|address| re_viewer_context::Item::DataResult(address.clone()))
}

/// Interpolates a series value at `time` using the rendered line semantics.
fn interpolated_series_value(
    kind: PlotSeriesKind,
    time: i64,
    time0: i64,
    value0: f64,
    time1: i64,
    value1: f64,
) -> f64 {
    match kind {
        PlotSeriesKind::Continuous => {
            if time1 == time0 {
                value0
            } else {
                let elapsed = (time as i128 - time0 as i128) as f64;
                let duration = (time1 as i128 - time0 as i128) as f64;
                egui::lerp(value0..=value1, elapsed / duration)
            }
        }
        PlotSeriesKind::Stepped(crate::StepMode::After) => value0,
        PlotSeriesKind::Stepped(crate::StepMode::Before) => value1,
        PlotSeriesKind::Stepped(crate::StepMode::Mid) => {
            if 2 * (time as i128) < time0 as i128 + time1 as i128 {
                value0
            } else {
                value1
            }
        }
        PlotSeriesKind::Scatter(_) | PlotSeriesKind::Clear => {
            if time.abs_diff(time0) <= time.abs_diff(time1) {
                value0
            } else {
                value1
            }
        }
    }
}

struct TooltipSample {
    marker_point: egui::Pos2,
    value: f64,
    distance_sq: f32,
}

fn tooltip_value_at_hover(
    points: &[(i64, f64)],
    kind: PlotSeriesKind,
    hover_time: i64,
) -> Option<f64> {
    let index = match points.binary_search_by_key(&hover_time, |&(time, _)| time) {
        Ok(index) => return Some(points[index].1),
        Err(index) => index,
    };

    match kind {
        PlotSeriesKind::Continuous | PlotSeriesKind::Stepped(_) => {
            if index == 0 {
                points.first().map(|(_, value)| *value)
            } else if index == points.len() {
                points.last().map(|(_, value)| *value)
            } else {
                let (time0, value0) = points[index - 1];
                let (time1, value1) = points[index];
                Some(interpolated_series_value(
                    kind, hover_time, time0, value0, time1, value1,
                ))
            }
        }
        PlotSeriesKind::Scatter(_) | PlotSeriesKind::Clear => None,
    }
}

fn tooltip_sample_at_hover(
    series: &crate::PlotSeries,
    hover_time: i64,
    hover_pos: egui::Pos2,
    transform: &egui_plot::PlotTransform,
    time_offset: i64,
) -> Option<TooltipSample> {
    let index = series
        .points
        .binary_search_by_key(&hover_time, |&(time, _)| time)
        .unwrap_or_else(|index| index);

    let sample = match series.kind {
        PlotSeriesKind::Continuous | PlotSeriesKind::Stepped(_) => {
            let value = tooltip_value_at_hover(&series.points, series.kind, hover_time)?;
            let marker_point = transform.position_from_point(&PlotPoint::new(
                (hover_time.saturating_sub(time_offset)) as f64,
                value,
            ));
            TooltipSample {
                marker_point,
                value,
                distance_sq: (hover_pos.y - marker_point.y).powi(2),
            }
        }

        PlotSeriesKind::Scatter(_) => {
            struct ScatterCandidate {
                horizontal_distance: f32,
                sample: TooltipSample,
            }

            const SAMPLE_PROXIMITY: f32 = 16.0;

            let candidate_indices = [index.saturating_sub(1), index.min(series.points.len() - 1)];
            let mut closest_sample = None;
            for candidate_index in candidate_indices {
                if candidate_index == 0 && series.first_point_is_continuity_bridge {
                    continue;
                }
                let (time, value) = series.points[candidate_index];
                if !value.is_finite() {
                    continue;
                }
                let marker_point = transform.position_from_point(&PlotPoint::new(
                    (time.saturating_sub(time_offset)) as f64,
                    value,
                ));
                let horizontal_distance = (hover_pos.x - marker_point.x).abs();
                if closest_sample
                    .as_ref()
                    .is_none_or(|best: &ScatterCandidate| {
                        horizontal_distance < best.horizontal_distance
                    })
                {
                    closest_sample = Some(ScatterCandidate {
                        horizontal_distance,
                        sample: TooltipSample {
                            marker_point,
                            value,
                            distance_sq: hover_pos.distance_sq(marker_point),
                        },
                    });
                }
            }
            let closest_sample = closest_sample?;
            if closest_sample.horizontal_distance > SAMPLE_PROXIMITY {
                return None;
            }
            closest_sample.sample
        }
        PlotSeriesKind::Clear => return None,
    };

    sample.value.is_finite().then_some(sample)
}

fn tooltip_series_label(series: &crate::PlotSeries) -> String {
    let label = if series.label.is_empty() {
        "y".to_owned()
    } else {
        series.label.clone()
    };
    if series.is_aggregated() {
        format!("{label}, {}", series.aggregator)
    } else {
        label
    }
}

fn tooltip_series_value(series: &crate::PlotSeries, value: f64) -> String {
    let value = re_format::format_f64(value);
    series
        .unit
        .as_ref()
        .map_or_else(|| value.clone(), |unit| format!("{value} {unit}"))
}

#[expect(clippy::too_many_arguments)]
fn all_series_tooltip(
    ui: &egui::Ui,
    response: &egui::Response,
    transform: &egui_plot::PlotTransform,
    all_plot_series: &[&crate::PlotSeries],
    time_offset: i64,
    time_type: TimeType,
    timestamp_format: re_log_types::TimestampFormat,
    plot_item_id_to_data_result_address: &ahash::HashMap<ItemId, DataResultInteractionAddress>,
) -> Option<re_viewer_context::Item> {
    re_tracing::profile_function!();

    let hover_pos = response
        .hover_pos()
        .filter(|position| response.rect.contains(*position))?;
    let hover_time =
        (transform.value_from_position(hover_pos).x as i64).saturating_add(time_offset);

    const MAX_TOOLTIP_ROWS: usize = 10;

    let mut rows = Vec::new();
    let mut num_displayed_series = 0_usize;
    let mut closest_series = None;
    let mut displayed_series = ahash::HashSet::default();
    for (series_idx, series) in all_plot_series.iter().enumerate() {
        let Some(&(first_time, _)) = series.points.first() else {
            continue;
        };
        let Some(&(last_time, _)) = series.points.last() else {
            continue;
        };
        if !series.visible || hover_time < first_time || hover_time > last_time {
            continue;
        }

        let Some(sample) =
            tooltip_sample_at_hover(series, hover_time, hover_pos, transform, time_offset)
        else {
            continue;
        };
        if !displayed_series.insert(series.id()) {
            continue;
        }

        ui.painter()
            .circle_filled(sample.marker_point, 3.0, series.color);

        if closest_series.is_none_or(|(closest, _)| sample.distance_sq < closest) {
            closest_series = Some((sample.distance_sq, series_idx));
        }

        num_displayed_series += 1;
        if rows.len() < MAX_TOOLTIP_ROWS {
            rows.push((
                tooltip_series_label(series),
                tooltip_series_value(series, sample.value),
                series.color,
            ));
        }
    }

    if rows.is_empty() {
        return None;
    }

    let time_label = format_time_for_tooltip(hover_time, time_type, timestamp_format);
    let num_omitted_rows = num_displayed_series.saturating_sub(rows.len());
    re_plot::tooltip::show_plot_tooltip_rows(
        ui,
        response,
        response.id.with("all_series"),
        &time_label,
        rows.iter()
            .map(|(label, value, color)| (label.as_str(), value.as_str(), *color)),
        num_omitted_rows,
    );

    const LINE_PROXIMITY_RADIUS_SQ: f32 = 48.0 * 48.0;
    let (_, series_idx) =
        closest_series.filter(|(distance_sq, _)| *distance_sq <= LINE_PROXIMITY_RADIUS_SQ)?;
    plot_item_id_to_data_result_address
        .get(&all_plot_series[series_idx].id())
        .map(|address| re_viewer_context::Item::DataResult(address.clone()))
}

fn format_time_for_tooltip(
    time: i64,
    time_type: TimeType,
    timestamp_format: re_log_types::TimestampFormat,
) -> String {
    let tooltip_format = if time_type == TimeType::TimestampNs {
        timestamp_format.with_date_visibility(re_log_types::DateVisibility::HideDate)
    } else {
        timestamp_format
    };
    time_type.format(re_log_types::TimeInt::new_temporal(time), tooltip_format)
}

fn paint_time_range_highlight(
    ui: &egui::Ui,
    response: &egui::Response,
    transform: &egui_plot::PlotTransform,
    time_offset: i64,
    range: re_log_types::AbsoluteTimeRange,
    color: egui::Color32,
) {
    let plot_rect = response.rect;
    let bounds = transform.bounds();
    let start_plot_x = (range.min.as_i64().saturating_sub(time_offset)) as f64;
    let end_plot_x = (range.max.as_i64().saturating_sub(time_offset)) as f64;

    // Clip to the visible plot bounds before converting to screen.
    let start_plot_x = start_plot_x.max(bounds.min()[0]);
    let end_plot_x = end_plot_x.min(bounds.max()[0]);
    if end_plot_x <= start_plot_x {
        return;
    }

    let x_start = transform
        .position_from_point(&PlotPoint::new(start_plot_x, 0.0))
        .x;
    let x_end = transform
        .position_from_point(&PlotPoint::new(end_plot_x, 0.0))
        .x;

    ui.painter().with_clip_rect(plot_rect).rect_filled(
        egui::Rect::from_x_y_ranges(x_start..=x_end, plot_rect.y_range()),
        0.0,
        color,
    );
}

// ── Pulsar cursor readout ─────────────────────────────────────────────────

/// The hover time shared by every Pulsar time-series view, so all charts read
/// out the same instant, like Pulsar's synced crosshair.
#[derive(Clone, Copy)]
struct SharedHover {
    time: i64,
    pass: u64,
}

/// Pulsar's cursor: hovering a Pulsar chart moves the time cursor, so every
/// other view (3D, map, text log) shows that instant; leaving the charts puts
/// the cursor back where it was parked. A click parks it at the hovered time.
/// While playback runs, the playhead owns the cursor.
#[derive(Clone, Default)]
struct Scrub {
    /// Each Pulsar chart's plot rect and the pass it was last drawn in.
    charts: Vec<(re_viewer_context::ViewId, egui::Rect, u64)>,
    /// Where the cursor goes back to when the pointer leaves the charts;
    /// `Some` while a hover is scrubbing.
    parked: Option<i64>,
}

fn scrub_id() -> egui::Id {
    egui::Id::NULL.with("pulsar_scrub")
}

fn scrub_time_cursor(
    ctx: &ViewerContext<'_>,
    ui: &egui::Ui,
    response: &egui::Response,
    transform: &egui_plot::PlotTransform,
    time_offset: i64,
    current_time: Option<i64>,
    view_id: re_viewer_context::ViewId,
) {
    use re_viewer_context::TimeControlCommand;
    let pass = ui.ctx().cumulative_pass_nr();
    let mut scrub = ui.ctx().data(|d| d.get_temp::<Scrub>(scrub_id())).unwrap_or_default();
    scrub.charts.retain(|(id, _, seen)| *id != view_id && seen + 1 >= pass);
    scrub.charts.push((view_id, response.rect, pass));

    let playing = ctx.time_ctrl.play_state() != re_sdk_types::blueprint::components::PlayState::Paused;
    let pointer = ui.ctx().pointer_hover_pos();
    let hovered = pointer.filter(|p| response.rect.contains(*p) && response.hovered());
    if playing {
        scrub.parked = None;
    } else if let Some(pos) = hovered {
        let time = time_offset.saturating_add(transform.value_from_position(pos).x.round() as i64);
        if scrub.parked.is_none() {
            scrub.parked = current_time;
        }
        if response.clicked() || response.secondary_clicked() {
            scrub.parked = Some(time);
        }
        if current_time != Some(time) {
            ctx.send_time_commands([TimeControlCommand::SetTimeClamped(re_log_types::TimeReal::from(time))]);
        }
    } else if let Some(parked) = scrub.parked
        && !pointer.is_some_and(|p| scrub.charts.iter().any(|(_, rect, _)| rect.contains(p)))
    {
        // The pointer left every chart: back to the parked cursor.
        scrub.parked = None;
        if current_time != Some(parked) {
            ctx.send_time_commands([TimeControlCommand::SetTimeClamped(re_log_types::TimeReal::from(parked))]);
        }
    }
    ui.ctx().data_mut(|d| d.insert_temp(scrub_id(), scrub));
}

fn shared_hover_id() -> egui::Id {
    egui::Id::NULL.with("pulsar_shared_hover_time")
}

/// The hovered view writes the time under the pointer; views drawn earlier in
/// the same frame pick it up on the next one.
fn update_shared_hover(
    ui: &egui::Ui,
    response: &egui::Response,
    transform: &egui_plot::PlotTransform,
    time_offset: i64,
) {
    if let Some(pos) = response.hover_pos()
        && response.rect.contains(pos)
    {
        let time = time_offset.saturating_add(transform.value_from_position(pos).x.round() as i64);
        let pass = ui.ctx().cumulative_pass_nr();
        let previous = ui.ctx().data(|d| d.get_temp::<SharedHover>(shared_hover_id()));
        ui.ctx()
            .data_mut(|d| d.insert_temp(shared_hover_id(), SharedHover { time, pass }));
        // One more frame lets views drawn earlier in this one catch up. Only when the hover
        // time moved, or was stale: an unconditional repaint keeps the viewer redrawing at
        // the display rate for as long as the pointer rests on a chart.
        if previous.is_none_or(|p| p.time != time || p.pass + 1 < pass) {
            ui.ctx().request_repaint();
        }
    }
}

/// The shared hover time, if some Pulsar view was hovered this frame or the last.
fn shared_hover_time(ui: &egui::Ui) -> Option<i64> {
    let pass = ui.ctx().cumulative_pass_nr();
    ui.ctx()
        .data(|d| d.get_temp::<SharedHover>(shared_hover_id()))
        .filter(|h| h.pass + 1 >= pass)
        .map(|h| h.time)
}

/// Paint a crosshair at the readout time and, top left, each visible series'
/// value nearest that time with the sample's own timestamp. A `~` marks a value
/// taken from aggregated points, which happens when zoomed out.
#[expect(clippy::too_many_arguments)]
fn paint_pulsar_readout(
    ctx: &ViewerContext<'_>,
    ui: &egui::Ui,
    response: &egui::Response,
    transform: &egui_plot::PlotTransform,
    all_plot_series: &[&crate::PlotSeries],
    series_sources: &HashMap<re_sdk_types::blueprint::components::VisualizerInstructionId, (ComponentIdentifier, bool)>,
    timeline: re_log_types::TimelineName,
    time_per_pixel: f64,
    time_offset: i64,
    time_type: TimeType,
    timestamp_format: re_log_types::TimestampFormat,
    current_time: Option<i64>,
) {
    let hover = shared_hover_time(ui);
    let Some(time) = hover.or(current_time) else {
        return;
    };
    let painter = ui.painter().with_clip_rect(response.rect);

    if hover.is_some() {
        let x = transform
            .position_from_point(&PlotPoint::new(time.saturating_sub(time_offset) as f64, 0.0))
            .x;
        if response.rect.x_range().contains(x) {
            painter.vline(
                x,
                response.rect.y_range(),
                egui::Stroke::new(1.0, ui.visuals().weak_text_color()),
            );
        }
    }

    let format_time = |t: i64| {
        let format = if time_type == TimeType::TimestampNs {
            timestamp_format.with_date_visibility(re_log_types::DateVisibility::HideDate)
        } else {
            timestamp_format
        };
        time_type.format(re_log_types::TimeInt::new_temporal(t), format)
    };

    let font = egui::FontId::monospace(11.0);
    let small = egui::FontId::monospace(10.0);
    let strong = ui.visuals().strong_text_color();
    let weak = ui.visuals().weak_text_color();
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &format_time(time),
        0.0,
        egui::TextFormat::simple(font.clone(), egui::Color32::from_rgb(242, 204, 96)),
    );

    for series in all_plot_series
        .iter()
        .filter(|s| s.visible && !matches!(s.kind, PlotSeriesKind::Clear) && !s.points.is_empty())
    {
        let points = &series.points;
        let i = points.partition_point(|(t, _)| *t < time);
        let i = if i == points.len()
            || (i > 0 && time.abs_diff(points[i - 1].0) <= time.abs_diff(points[i].0))
        {
            i - 1
        } else {
            i
        };
        let plotted = points[i];
        let downsampled =
            series.aggregator != re_sdk_types::components::AggregationPolicy::Off
                && series.aggregation_factor > 1.0;

        // Plotted points are raw samples unless the series is downsampled. Then
        // the nearest raw sample sits within a few pixels of the readout time.
        let raw = downsampled
            .then(|| series_sources.get(&series.visualizer_instruction_id))
            .flatten()
            .filter(|(component, no_selector)| {
                *no_selector && *component == Scalars::descriptor_scalars().component
            })
            .and_then(|(component, _)| {
                let window = (time_per_pixel * 3.0).ceil() as i64 + 1;
                nearest_raw_sample(ctx, timeline, series, *component, time, window)
            });
        let aggregated = downsampled && raw.is_none();
        let (sample_time, value) = raw.unwrap_or(plotted);

        job.append("   ", 0.0, egui::TextFormat::simple(font.clone(), weak));
        job.append(&series.label, 0.0, egui::TextFormat::simple(font.clone(), series.color));
        job.append(
            &format!(" {}{}", if aggregated { "~" } else { "" }, compact_value(value)),
            0.0,
            egui::TextFormat::simple(font.clone(), strong),
        );
        job.append(
            &format!(" @{}", format_time(sample_time)),
            0.0,
            egui::TextFormat::simple(small.clone(), weak),
        );
    }

    job.wrap = egui::text::TextWrapping {
        max_width: (response.rect.width() - 16.0).max(40.0),
        max_rows: 2,
        break_anywhere: false,
        overflow_character: Some('…'),
    };
    let galley = painter.layout_job(job);
    let pos = response.rect.left_top() + egui::vec2(8.0, 6.0);
    let bg = egui::Rect::from_min_size(pos, galley.size()).expand(3.0);
    painter.rect_filled(bg, 3.0, ui.visuals().extreme_bg_color.gamma_multiply(0.85));
    painter.galley(pos, galley, strong);
}

/// The raw sample of `series` nearest `time`, from a range query of `window`
/// either side. `None` when no sample falls in the window.
fn nearest_raw_sample(
    ctx: &ViewerContext<'_>,
    timeline: re_log_types::TimelineName,
    series: &crate::PlotSeries,
    component: ComponentIdentifier,
    time: i64,
    window: i64,
) -> Option<(i64, f64)> {
    let query = re_chunk_store::RangeQuery::new(
        timeline,
        AbsoluteTimeRange::new(time.saturating_sub(window), time.saturating_add(window)),
    );
    let engine = ctx.recording_engine();
    let results = engine.cache().range(
        re_chunk_store::ChunkTrackingMode::Ignore,
        &query,
        &series.instance_path.entity_path,
        [component],
    );
    let instance = series.instance_path.instance;
    let index = if instance.is_specific() { instance.get() as usize } else { 0 };

    let mut best: Option<(i64, f64)> = None;
    for chunk in results.get(component)? {
        for ((t, _row), values) in std::iter::zip(
            chunk.iter_component_indices(timeline, component),
            chunk.iter_slices::<f64>(component),
        ) {
            let (t, Some(&value)) = (t.as_i64(), values.get(index)) else {
                continue;
            };
            if best.is_none_or(|(bt, _)| t.abs_diff(time) < bt.abs_diff(time)) {
                best = Some((t, value));
            }
        }
    }
    best
}

/// Paint each y-band across the plot's full width, with its border lines if any.
fn paint_y_bands(
    ui: &egui::Ui,
    response: &egui::Response,
    transform: &egui_plot::PlotTransform,
    bands: &[crate::y_bands::YBand],
) {
    let plot = response.rect;
    let painter = ui.painter().with_clip_rect(plot);
    let y_of = |v: f64| transform.position_from_point(&PlotPoint::new(0.0, v)).y;
    for band in bands {
        let (y_top, y_bottom) = (y_of(band.max), y_of(band.min));
        let rect = egui::Rect::from_x_y_ranges(plot.x_range(), y_top..=y_bottom).intersect(plot);
        if rect.height() > 0.0 {
            painter.rect_filled(rect, 0.0, band.fill);
        }
        if let Some(border) = band.border {
            for y in [y_top, y_bottom] {
                painter.hline(plot.x_range(), y, egui::Stroke::new(1.0, border));
            }
        }
    }
}

/// Paint each drawn phase of the given state lanes as a translucent band over
/// the plot's full height: Pulsar's labelled time regions on check charts.
fn paint_region_lanes(
    ui: &egui::Ui,
    response: &egui::Response,
    transform: &egui_plot::PlotTransform,
    time_offset: i64,
    lanes: &[re_view_state_timeline::StateLane],
) {
    let plot = response.rect;
    let painter = ui.painter().with_clip_rect(plot);
    let pointer = response.hover_pos();
    let mut hovered_label: Option<&str> = None;
    let x_of = |t: i64| {
        transform
            .position_from_point(&PlotPoint::new(t.saturating_sub(time_offset) as f64, 0.0))
            .x
    };
    for lane in lanes {
        for (i, phase) in lane.phases.iter().enumerate() {
            let Some(content) = &phase.content else {
                continue;
            };
            let x0 = x_of(phase.start_time).max(plot.left());
            let x1 = lane
                .phases
                .get(i + 1)
                .map_or(plot.right(), |next| x_of(next.start_time))
                .min(plot.right());
            if x1 <= x0 {
                continue;
            }
            let band = egui::Rect::from_x_y_ranges(x0..=x1, plot.y_range());
            painter.rect_filled(band, 0.0, content.color.gamma_multiply(0.16));
            if band.contains(pointer.unwrap_or(egui::Pos2::NAN)) {
                hovered_label = Some(content.label.as_str());
            }
        }
    }

    // As in Pulsar, the region's label shows only on Ctrl/Cmd-hover, as a tooltip.
    if let (Some(label), Some(pos)) = (hovered_label, pointer)
        && ui.input(|i| i.modifiers.command)
    {
        let galley = painter.layout_no_wrap(
            label.to_owned(),
            egui::FontId::monospace(10.0),
            ui.visuals().strong_text_color(),
        );
        let at = pos + egui::vec2(12.0, -galley.size().y - 8.0);
        let bg = egui::Rect::from_min_size(at, galley.size()).expand2(egui::vec2(6.0, 2.0));
        painter.rect(
            bg,
            4.0,
            ui.visuals().window_fill,
            ui.visuals().window_stroke,
            egui::StrokeKind::Outside,
        );
        painter.galley(at, galley, ui.visuals().strong_text_color());
    }
}

/// Four significant digits, like Pulsar's legend; scientific outside 1e-3..1e5.
fn compact_value(v: f64) -> String {
    if !v.is_finite() {
        return format!("{v}");
    }
    if v == 0.0 {
        return "0".to_owned();
    }
    let a = v.abs();
    if !(1e-3..1e5).contains(&a) {
        return format!("{v:.3e}");
    }
    let decimals = (3 - a.log10().floor() as i32).max(0) as usize;
    format!("{v:.decimals$}")
}

fn paint_time_cursor(
    ctx: &ViewerContext<'_>,
    ui: &egui::Ui,
    response: &egui::Response,
    transform: &egui_plot::PlotTransform,
    time_offset: i64,
    current_time: Option<i64>,
    view_time_range: &re_sdk_types::blueprint::components::TimeRange,
    new_view_time_range: &mut Option<TimeRange>,
) {
    // Decide if the time cursor should be displayed, and if so where:
    let time_x = current_time
        .map(|current_time| (current_time.saturating_sub(time_offset)) as f64)
        .filter(|&x| {
            // only display the time cursor when it's actually above the plot area
            transform.bounds().min()[0] <= x && x <= transform.bounds().max()[0]
        })
        .map(|x| transform.position_from_point(&PlotPoint::new(x, 0.0)).x);

    let Some(mut time_x) = time_x else {
        return;
    };

    let interact_radius = ui.style().interaction.resize_grab_radius_side;
    let line_rect = egui::Rect::from_x_y_ranges(time_x..=time_x, response.rect.y_range())
        .expand(interact_radius);

    let time_drag_id = ui.make_persistent_id("time_drag");
    let pointer_pos = ui.input(|i| i.pointer.hover_pos());
    let is_near = ui.rect_contains_pointer(line_rect);
    let is_being_dragged = ui.is_being_dragged(time_drag_id);

    if is_near || is_being_dragged {
        ui.ctx()
            .set_cursor_icon(re_viewer_context::MOVE_TIME_CURSOR_ICON);
    }

    if is_near
        && !is_being_dragged
        && ui.input(|i| i.pointer.button_pressed(egui::PointerButton::Primary))
    {
        ui.set_dragged_id(time_drag_id);
    }

    if is_being_dragged && let Some(pointer_pos) = pointer_pos {
        let aim_radius = ui.input(|i| i.aim_radius());

        let new_offset_time = egui::emath::smart_aim::best_in_range_f64(
            transform
                .value_from_position(pointer_pos - aim_radius * Vec2::X)
                .x,
            transform
                .value_from_position(pointer_pos + aim_radius * Vec2::X)
                .x,
        );
        let new_time = time_offset + new_offset_time.round() as i64;

        // Avoid frame-delay:
        time_x = pointer_pos.x;

        *new_view_time_range =
            re_view::set_time_cursor(ctx, current_time, Some(&view_time_range.0), new_time.into());
    }

    let highlighted = is_near || is_being_dragged;
    let style = if is_being_dragged {
        &ui.visuals().widgets.active
    } else if highlighted {
        &ui.visuals().widgets.hovered
    } else {
        &ui.visuals().widgets.inactive
    };

    ui.paint_time_cursor_with_style(ui.painter(), style, time_x, response.rect.y_range());
}

fn reset_view(ctx: &ViewerContext<'_>, time_axis: &ViewProperty, scalar_axis: &ViewProperty) {
    scalar_axis.reset_blueprint_component(ctx, ScalarAxis::descriptor_range());
    time_axis.reset_blueprint_component(ctx, TimeAxis::descriptor_view_range());
}

/// axis = 0 is the x axis
///
/// axis = 1 is the y axis
fn transform_axis_range(transform: egui_plot::PlotTransform, axis: usize) -> Range1D {
    Range1D::new(
        transform.bounds().min()[axis],
        transform.bounds().max()[axis],
    )
}

fn update_series_visibility_from_legend(
    ctx: &ViewerContext<'_>,
    query: &ViewQuery<'_>,
    all_plot_series: &[&crate::PlotSeries],
    hidden_items: &egui_plot::ItemIdSet,
) {
    let Some(query_results) = ctx.query_results.get(&query.view_id) else {
        return;
    };

    // Determine which series have changed visibility state.
    let mut per_visualizer_inst_id_series_new_visibility_state: HashMap<
        VisualizerInstructionId,
        SmallVec<[bool; 1]>,
    > = HashMap::default();
    let mut series_to_update = Vec::new();
    for series in all_plot_series {
        let entity_visibility_flags = per_visualizer_inst_id_series_new_visibility_state
            .entry(series.visualizer_instruction_id)
            .or_default();

        let visible_new = !hidden_items.contains(&series.id());

        let instance = series.instance_path.instance;
        let index = instance.specific_index().map_or(0, |i| i.get() as usize);
        if entity_visibility_flags.len() <= index {
            entity_visibility_flags.resize(index + 1, false);
        }
        entity_visibility_flags[index] = visible_new;

        if series.visible != visible_new {
            series_to_update.push(series);
        }
    }

    for series in series_to_update {
        let Some(visibility_state) = per_visualizer_inst_id_series_new_visibility_state
            .remove(&series.visualizer_instruction_id)
        else {
            continue;
        };
        let Some(result) = query_results.result_for_entity(&series.instance_path.entity_path)
        else {
            continue;
        };

        if let Some(visualizer_instruction) = result
            .visualizer_instructions
            .iter()
            .find(|instr| instr.id == series.visualizer_instruction_id)
        {
            let visualizer = visualizer_instruction.visualizer_type;
            let descriptor = if visualizer == SeriesLinesSystem::identifier() {
                Some(SeriesLines::descriptor_visible_series())
            } else if visualizer == SeriesPointsSystem::identifier() {
                Some(SeriesPoints::descriptor_visible_series())
            } else if visualizer == MeasurementsSeriesSystem::identifier() {
                Some(Measurements::descriptor_visible_series())
            } else {
                re_log::debug_panic!("Unexpected time series visualizer `{visualizer}`");
                None
            };

            let override_path = &visualizer_instruction.override_path;

            let component_array = visibility_state
                .into_iter()
                .map(Visible::from)
                .collect::<Vec<_>>();

            if let Some(serialized_component_batch) =
                descriptor.and_then(|descriptor| component_array.serialized(descriptor))
            {
                ctx.save_serialized_blueprint_component(
                    override_path.clone(),
                    serialized_component_batch,
                );
            }
        } else {
            re_log::warn_once!(
                "Could not find visualizer instruction for series at instance path `{}`",
                series.instance_path
            );
        }
    }
}

pub(crate) fn to_stepped_points(points: &[[f64; 2]], mode: crate::StepMode) -> Vec<[f64; 2]> {
    if points.len() < 2 {
        return points.to_vec();
    }
    let capacity = match mode {
        crate::StepMode::After | crate::StepMode::Before => points.len() * 2 - 1,
        crate::StepMode::Mid => points.len() * 3 - 2,
    };
    let mut stepped = Vec::with_capacity(capacity);
    match mode {
        crate::StepMode::After => {
            for [a, b] in points.array_windows() {
                stepped.push(*a);
                stepped.push([b[0], a[1]]);
            }
        }
        crate::StepMode::Before => {
            for [a, b] in points.array_windows() {
                stepped.push(*a);
                stepped.push([a[0], b[1]]);
            }
        }
        crate::StepMode::Mid => {
            for [a, b] in points.array_windows() {
                let mid_t = fast_midpoint(a[0], b[0]);
                stepped.push(*a);
                stepped.push([mid_t, a[1]]);
                stepped.push([mid_t, b[1]]);
            }
        }
    }
    if let Some(last) = points.last() {
        stepped.push(*last);
    }
    stepped
}

fn format_y_axis(mark: egui_plot::GridMark) -> String {
    // Example: If the step to the next tick is `0.01`, we should use 2 decimals of precision:
    let num_decimals = -mark.step_size.log10().round() as usize;

    re_format::FloatFormatOptions::DEFAULT_f64
        .with_decimals(num_decimals)
        .format(mark.value)
}

fn nanos_grid_spacer(
    canvas_size: egui::Vec2,
    input: &egui_plot::GridInput,
) -> Vec<egui_plot::GridMark> {
    let minimum_medium_line_spacing = 150.0; // ≈min size of a label
    let max_medium_lines = canvas_size.x as f64 / minimum_medium_line_spacing;

    let (min_nanos, max_nanos) = input.bounds;
    let width_nanos = max_nanos - min_nanos;

    let mut small_spacing_nanos = 1;
    while width_nanos / (next_grid_tick_magnitude_nanos(small_spacing_nanos) as f64)
        > max_medium_lines
    {
        let next_nanos = next_grid_tick_magnitude_nanos(small_spacing_nanos);
        if small_spacing_nanos < next_nanos {
            small_spacing_nanos = next_nanos;
        } else {
            break; // we've reached the max
        }
    }
    let medium_spacing_nanos = next_grid_tick_magnitude_nanos(small_spacing_nanos);
    let big_spacing_nanos = next_grid_tick_magnitude_nanos(medium_spacing_nanos);

    let mut current_nanos = (min_nanos.floor() as i64) / small_spacing_nanos * small_spacing_nanos;
    let mut marks = vec![];

    while current_nanos <= max_nanos.ceil() as i64 {
        let is_big_line = current_nanos % big_spacing_nanos == 0;
        let is_medium_line = current_nanos % medium_spacing_nanos == 0;

        let step_size = if is_big_line {
            big_spacing_nanos
        } else if is_medium_line {
            medium_spacing_nanos
        } else {
            small_spacing_nanos
        };

        marks.push(egui_plot::GridMark {
            value: current_nanos as f64,
            step_size: step_size as f64,
        });

        if let Some(new_nanos) = current_nanos.checked_add(small_spacing_nanos) {
            current_nanos = new_nanos;
        } else {
            break;
        }
    }

    marks
}

fn round_nanos_to_start_of_day(ns: i64) -> i64 {
    let nanos_per_day = 24 * 60 * 60 * 1_000_000_000;
    (ns.saturating_add(nanos_per_day / 2)) / nanos_per_day * nanos_per_day
}

/// Add a relative margin to a range so the data doesn't touch the plot edges.
///
/// `fraction` is the fraction of the range to add on each side (e.g. 0.05 = 5%).
pub fn add_margin_to_range(range: Range1D, fraction: f64) -> Range1D {
    let span = range.end() - range.start();
    let margin = span * fraction;
    Range1D::new(range.start() - margin, range.end() + margin)
}

/// Make sure the range is finite and positive, or `egui_plot` might be buggy.
pub fn make_range_sane(y_range: Range1D) -> Range1D {
    let (mut start, mut end) = (y_range.start(), y_range.end());

    if !start.is_finite() {
        start = -1.0;
    }
    if !end.is_finite() {
        end = 1.0;
    }

    if end < start {
        (start, end) = (end, start);
    }

    if end <= start {
        let center = fast_midpoint(start, end);
        let margin = f64::max(1.0, center.abs() * 0.01);
        Range1D::new(center - margin, center + margin)
    } else {
        Range1D::new(start, end)
    }
}

/// Render `re_renderer` draw data (lines, points) produced by visualizers in screen space.
///
/// The visualizers have already transformed points to screen coordinates using `PlotTransform`,
/// so we use a simple identity orthographic projection matching the screen rect.
fn render_re_renderer_draw_data(
    ctx: &ViewerContext<'_>,
    ui: &egui::Ui,
    response: &egui::Response,
    view_id: re_renderer::ViewBuilderId,
    draw_data: Vec<re_renderer::QueueableDrawData>,
) -> Result<(), re_renderer::view_builder::ViewBuilderError> {
    if draw_data.is_empty() {
        return Ok(());
    }

    let render_ctx = ctx.render_ctx();
    let pixels_per_point = ui.ctx().pixels_per_point();

    // The plot area in screen coordinates.
    let plot_rect = response.rect;

    // Resolution in physical pixels for the render target.
    let resolution_in_pixel =
        re_viewer_context::gpu_bridge::viewport_resolution_in_pixels(plot_rect, pixels_per_point);

    if resolution_in_pixel[0] == 0 || resolution_in_pixel[1] == 0 {
        return Ok(());
    }

    let height = plot_rect.height();

    // Points are already in screen (UI) coordinates from PlotTransform.
    // Translate so that the plot rect's top-left maps to the origin.
    let view_from_world = re_math::IsoTransform::from_rotation_translation(
        glam::Quat::IDENTITY,
        glam::vec3(-plot_rect.left(), -plot_rect.top(), 0.0),
    );

    let target_config = re_renderer::view_builder::TargetConfiguration {
        name: "time_series_plot".into(),
        resolution_in_pixel,
        view_from_world,
        projection_from_view: re_renderer::view_builder::Projection::Orthographic {
            camera_mode: re_renderer::view_builder::OrthographicCameraMode::TopLeftCornerAndExtendZ,
            vertical_world_size: height,
            far_plane_distance: 1000.0,
        },
        viewport_transformation: re_renderer::RectTransform::IDENTITY,
        pixels_per_point,
        blend_with_background: re_renderer::BlendWithBackground::Premultiplied,
        ..Default::default()
    };

    let mut view_builder = re_renderer::ViewBuilder::new(render_ctx, target_config, view_id)?;

    for dd in draw_data {
        view_builder.queue_draw(render_ctx, dd)?;
    }

    let painter = ui.painter_at(plot_rect);
    painter.add(re_viewer_context::gpu_bridge::new_renderer_callback(
        view_builder,
        plot_rect,
        re_renderer::Rgba::TRANSPARENT,
    ));

    Ok(())
}

fn time_series_visualizers_section(
    ctx: &re_viewer_context::ViewContext<'_>,
) -> re_viewer_context::VisualizersSectionOutput<'static> {
    let series_line_id = SeriesLinesSystem::identifier();
    let visualizable_entities = ctx
        .viewer_ctx
        .iter_visualizable_entities_for_view_class(TimeSeriesView::identifier())
        .find(|(viz, _)| *viz == series_line_id)
        .map(|(_, ents)| ents);

    let data_results = ctx.query_result.tree.iter_data_results();
    let add_options = data_results
        .filter_map(|data_result| {
            if data_result.tree_prefix_only {
                return None;
            }

            // For the "add visualizer" menu, offer a SeriesLine for every possible scalar mapping.
            // Unlike `recommended_visualizers_for_entity` / `all_scalar_mappings`, we don't filter
            // by indication or recommended datatypes — we show everything that could be visualized.
            let VisualizableReason::SingleRequiredComponentMatch(SingleRequiredComponentMatch {
                target_component: _,
                matches,
            }) = visualizable_entities?.get(&data_result.entity_path)?
            else {
                return None;
            };

            let recommended = if let Ok(mappings) =
                vec1::Vec1::try_from_vec(all_scalar_mappings_for(matches))
            {
                RecommendedVisualizers::new(std::iter::once((series_line_id, mappings)).collect())
            } else {
                return None;
            };

            Some((data_result.entity_path.clone(), recommended))
        })
        .collect();

    re_viewer_context::VisualizersSectionOutput {
        ui: Box::new(move |ui, ctx| {
            visualizers_section_ui(ui, ctx);
        }),
        add_options,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StepMode;
    use re_viewer_context::SingleRequiredComponentMatch;

    #[test]
    fn test_interpolated_series_value_respects_line_modes() {
        assert_eq!(
            interpolated_series_value(PlotSeriesKind::Continuous, 5, 0, 0.0, 10, 10.0),
            5.0
        );
        assert_eq!(
            interpolated_series_value(PlotSeriesKind::Continuous, 5, 5, 1.0, 5, 2.0),
            1.0
        );
        assert_eq!(
            interpolated_series_value(PlotSeriesKind::Stepped(StepMode::After), 5, 0, 1.0, 10, 2.0),
            1.0
        );
        assert_eq!(
            interpolated_series_value(
                PlotSeriesKind::Stepped(StepMode::Before),
                5,
                0,
                1.0,
                10,
                2.0
            ),
            2.0
        );
        assert_eq!(
            interpolated_series_value(PlotSeriesKind::Stepped(StepMode::Mid), 4, 0, 1.0, 10, 2.0),
            1.0
        );
        assert_eq!(
            interpolated_series_value(PlotSeriesKind::Stepped(StepMode::Mid), 5, 0, 1.0, 10, 2.0),
            2.0
        );
        assert_eq!(
            interpolated_series_value(
                PlotSeriesKind::Scatter(Default::default()),
                8,
                0,
                1.0,
                10,
                2.0
            ),
            2.0
        );

        let timestamp = 1_700_000_000_000_000_000;
        assert_eq!(
            interpolated_series_value(
                PlotSeriesKind::Continuous,
                timestamp + 50,
                timestamp,
                0.0,
                timestamp + 100,
                10.0,
            ),
            5.0
        );
        assert_eq!(
            interpolated_series_value(
                PlotSeriesKind::Scatter(Default::default()),
                i64::MAX,
                i64::MIN,
                1.0,
                i64::MAX - 1,
                2.0,
            ),
            2.0
        );
        assert_eq!(
            interpolated_series_value(PlotSeriesKind::Continuous, 0, i64::MIN, 0.0, i64::MAX, 10.0,),
            5.0
        );
        assert_eq!(
            interpolated_series_value(PlotSeriesKind::Stepped(StepMode::Mid), 1, 0, 1.0, 3, 2.0),
            1.0
        );
        assert_eq!(
            interpolated_series_value(PlotSeriesKind::Stepped(StepMode::Mid), 2, 0, 1.0, 3, 2.0),
            2.0
        );
        assert_eq!(
            interpolated_series_value(
                PlotSeriesKind::Stepped(StepMode::Mid),
                -1,
                i64::MIN,
                1.0,
                i64::MAX,
                2.0,
            ),
            1.0
        );
        assert_eq!(
            interpolated_series_value(
                PlotSeriesKind::Stepped(StepMode::Mid),
                0,
                i64::MIN,
                1.0,
                i64::MAX,
                2.0,
            ),
            2.0
        );
    }

    #[test]
    fn test_tooltip_value_at_hover_uses_exact_finite_sample() {
        let points = [(0, f64::NAN), (1, 5.0), (2, f64::NAN)];

        for kind in [
            PlotSeriesKind::Continuous,
            PlotSeriesKind::Stepped(StepMode::After),
            PlotSeriesKind::Stepped(StepMode::Before),
            PlotSeriesKind::Stepped(StepMode::Mid),
        ] {
            assert_eq!(tooltip_value_at_hover(&points, kind, 1), Some(5.0));
        }
    }

    #[test]
    fn test_help_view() {
        re_test_context::TestContext::test_help_view(|ctx| TimeSeriesView.help(ctx));
    }

    /// Regression: non-recommended physical datatype (`Int32`) must not cause
    /// `SeriesLinesSystem` to be recommended with an empty mapping.
    #[test]
    fn test_no_recommendation_for_non_recommended_datatype() {
        let entity_path = EntityPath::from("sensor/data");
        let reason =
            VisualizableReason::SingleRequiredComponentMatch(SingleRequiredComponentMatch {
                target_component: Scalars::descriptor_scalars().component,
                matches: std::iter::once((
                    Scalars::descriptor_scalars().component,
                    DatatypeMatch::PhysicalDatatypeOnly {
                        arrow_data_type: DataType::Int32,
                        component_type: None,
                        selectors: vec![],
                    },
                ))
                .collect(),
            });
        let visualizers = [(SeriesLinesSystem::identifier(), &reason)];
        let indicated = PerVisualizerType::default();
        let result = TimeSeriesView.recommended_visualizers_for_entity(
            &entity_path,
            &visualizers,
            &indicated,
        );

        assert!(result.all_recommendations().is_empty());
    }

    /// `SeriesLinesSystem` should be recommended when the datatype is a recommended one, even if not indicated.
    #[test]
    fn test_recommendation_for_recommended_datatype() {
        let entity_path = EntityPath::from("sensor/data");
        let reason =
            VisualizableReason::SingleRequiredComponentMatch(SingleRequiredComponentMatch {
                target_component: Scalars::descriptor_scalars().component,
                matches: std::iter::once((
                    Scalars::descriptor_scalars().component,
                    DatatypeMatch::NativeSemantics {
                        arrow_data_type: DataType::Float64,
                        component_type: None,
                    },
                ))
                .collect(),
            });
        let visualizers = [(SeriesLinesSystem::identifier(), &reason)];
        let indicated = PerVisualizerType::default();
        let result = TimeSeriesView.recommended_visualizers_for_entity(
            &entity_path,
            &visualizers,
            &indicated,
        );

        assert!(
            result
                .all_recommendations()
                .contains_key(&SeriesLinesSystem::identifier())
        );
        let mappings = &result.all_recommendations()[&SeriesLinesSystem::identifier()];
        assert_eq!(mappings.len(), 1);
        assert!(
            mappings[0].contains_mapping_for_component(&Scalars::descriptor_scalars().component)
        );
    }
}
