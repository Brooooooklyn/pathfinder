// pathfinder/renderer/src/tiles.rs
//
// Copyright © 2019 The Pathfinder Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::builder::SceneBuilder;
use crate::gpu::renderer::MASK_TILES_ACROSS;
use crate::gpu_data::{AlphaTile, AlphaTileVertex, BuiltObject, RenderStage, TileObjectPrimitive};
use crate::paint::PaintMetadata;
use crate::scene::PathId;
use pathfinder_content::outline::{Contour, Outline, PointIndex};
use pathfinder_content::segment::Segment;
use pathfinder_content::sorted_vector::SortedVector;
use pathfinder_geometry::line_segment::LineSegment2F;
use pathfinder_geometry::rect::{RectF, RectI};
use pathfinder_geometry::vector::{Vector2F, Vector2I};
use std::cmp::Ordering;
use std::mem;
use std::u16;

// TODO(pcwalton): Make this configurable.
const FLATTENING_TOLERANCE: f32 = 0.1;

pub const TILE_WIDTH: u32 = 16;
pub const TILE_HEIGHT: u32 = 16;

pub(crate) struct Tiler<'a> {
    builder: &'a SceneBuilder<'a>,
    pub built_object: BuiltObject,
    path_info: TilingPathInfo<'a>,

    point_queue: SortedVector<QueuedEndpoint>,
    active_edges: SortedVector<ActiveEdge>,
    old_active_edges: Vec<ActiveEdge>,
}

#[derive(Clone, Copy)]
pub(crate) struct TilingPathInfo<'a> {
    pub(crate) outline: &'a Outline,
    pub(crate) id: PathId,
    pub(crate) paint_metadata: Option<&'a PaintMetadata>,
    pub(crate) render_stage: RenderStage,
}

impl<'a> Tiler<'a> {
    #[allow(clippy::or_fun_call)]
    pub(crate) fn new(
        builder: &'a SceneBuilder<'a>,
        view_box: RectF,
        path_info: TilingPathInfo<'a>,
    ) -> Tiler<'a> {
        let bounds = path_info.outline.bounds().intersection(view_box).unwrap_or(RectF::default());
        let built_object = BuiltObject::new(bounds, path_info.render_stage);

        Tiler {
            builder,
            built_object,
            path_info,

            point_queue: SortedVector::new(),
            active_edges: SortedVector::new(),
            old_active_edges: vec![],
        }
    }

    pub(crate) fn generate_tiles(&mut self) {
        // Initialize the point queue.
        self.init_point_queue();

        // Reset active edges.
        self.active_edges.clear();
        self.old_active_edges.clear();

        // Generate strips.
        let tile_rect = self.built_object.tile_rect();
        for strip_origin_y in tile_rect.min_y()..tile_rect.max_y() {
            self.generate_strip(strip_origin_y);
        }

        // Pack and cull.
        self.pack_and_cull_if_necessary();

        // Done!
        debug!("{:#?}", self.built_object);
    }

    fn generate_strip(&mut self, strip_origin_y: i32) {
        // Process old active edges.
        self.process_old_active_edges(strip_origin_y);

        // Add new active edges.
        let strip_max_y = ((i32::from(strip_origin_y) + 1) * TILE_HEIGHT as i32) as f32;
        while let Some(queued_endpoint) = self.point_queue.peek() {
            // We're done when we see an endpoint that belongs to the next tile strip.
            //
            // Note that this test must be `>`, not `>=`, in order to make sure we don't miss
            // active edges that lie precisely on the tile strip boundary.
            if queued_endpoint.y > strip_max_y {
                break;
            }

            self.add_new_active_edge(strip_origin_y);
        }
    }

    fn pack_and_cull_if_necessary(&mut self) {
        let (object_index, paint_metadata) =
            match (self.path_info.id, self.path_info.paint_metadata) {
                (PathId::Draw(index), Some(paint_metadata)) => {
                    debug_assert!(index <= u16::MAX as u32);
                    (index as u16, paint_metadata)
                }
                _ => return,
            };

        for (tile_index, tile) in self.built_object.tiles.data.iter().enumerate() {
            let tile_coords = self
                .built_object
                .local_tile_index_to_coords(tile_index as u32);

            if tile.is_solid() {
                // Blank tiles are always skipped.
                if tile.backdrop == 0 {
                    continue;
                }

                // If this is a solid tile, poke it into the Z-buffer and stop here.
                if paint_metadata.is_opaque {
                    self.builder.z_buffer.update(tile_coords, object_index);
                    continue;
                }
            }

            self.built_object.alpha_tiles.push(AlphaTile {
                upper_left: AlphaTileVertex::new(tile_coords,
                                                 tile.alpha_tile_index as u16,
                                                 Vector2I::default(),
                                                 object_index,
                                                 tile.backdrop as i16,
                                                 paint_metadata),
                upper_right: AlphaTileVertex::new(tile_coords,
                                                  tile.alpha_tile_index as u16,
                                                  Vector2I::new(1, 0),
                                                  object_index,
                                                  tile.backdrop as i16,
                                                  paint_metadata),
                lower_left: AlphaTileVertex::new(tile_coords,
                                                 tile.alpha_tile_index as u16,
                                                 Vector2I::new(0, 1),
                                                 object_index,
                                                 tile.backdrop as i16,
                                                 paint_metadata),
                lower_right: AlphaTileVertex::new(tile_coords,
                                                  tile.alpha_tile_index as u16,
                                                  Vector2I::splat(1),
                                                  object_index,
                                                  tile.backdrop as i16,
                                                  paint_metadata),
            });
        }
    }

    fn process_old_active_edges(&mut self, tile_y: i32) {
        let mut current_tile_x = self.built_object.tile_rect().min_x();
        let mut current_subtile_x = 0.0;
        let mut current_winding = 0;

        debug_assert!(self.old_active_edges.is_empty());
        mem::swap(&mut self.old_active_edges, &mut self.active_edges.array);

        // FIXME(pcwalton): Yuck.
        let mut last_segment_x = -9999.0;

        let tile_top = (i32::from(tile_y) * TILE_HEIGHT as i32) as f32;

        debug!("---------- tile y {}({}) ----------", tile_y, tile_top);
        debug!("old active edges: {:#?}", self.old_active_edges);

        for mut active_edge in self.old_active_edges.drain(..) {
            // Determine x-intercept and winding.
            let segment_x = active_edge.crossing.x();
            let edge_winding =
                if active_edge.segment.baseline.from_y() < active_edge.segment.baseline.to_y() {
                    1
                } else {
                    -1
                };

            debug!(
                "tile Y {}({}): segment_x={} edge_winding={} current_tile_x={} \
                 current_subtile_x={} current_winding={}",
                tile_y,
                tile_top,
                segment_x,
                edge_winding,
                current_tile_x,
                current_subtile_x,
                current_winding
            );
            debug!(
                "... segment={:#?} crossing={:?}",
                active_edge.segment, active_edge.crossing
            );

            // FIXME(pcwalton): Remove this debug code!
            debug_assert!(segment_x >= last_segment_x);
            last_segment_x = segment_x;

            // Do initial subtile fill, if necessary.
            let segment_tile_x = f32::floor(segment_x) as i32 / TILE_WIDTH as i32;
            if current_tile_x < segment_tile_x && current_subtile_x > 0.0 {
                let current_x =
                    (i32::from(current_tile_x) * TILE_WIDTH as i32) as f32 + current_subtile_x;
                let tile_right_x = ((i32::from(current_tile_x) + 1) * TILE_WIDTH as i32) as f32;
                let current_tile_coords = Vector2I::new(current_tile_x, tile_y);
                self.built_object.add_active_fill(
                    self.builder,
                    current_x,
                    tile_right_x,
                    current_winding,
                    current_tile_coords,
                );
                current_tile_x += 1;
                current_subtile_x = 0.0;
            }

            // Move over to the correct tile, filling in as we go.
            while current_tile_x < segment_tile_x {
                debug!(
                    "... emitting backdrop {} @ tile {}",
                    current_winding, current_tile_x
                );
                let current_tile_coords = Vector2I::new(current_tile_x, tile_y);
                if let Some(tile_index) = self
                    .built_object
                    .tile_coords_to_local_index(current_tile_coords)
                {
                    // FIXME(pcwalton): Handle winding overflow.
                    self.built_object.tiles.data[tile_index as usize].backdrop =
                        current_winding as i8;
                }

                current_tile_x += 1;
                current_subtile_x = 0.0;
            }

            // Do final subtile fill, if necessary.
            debug_assert_eq!(current_tile_x, segment_tile_x);
            let segment_subtile_x =
                segment_x - (i32::from(current_tile_x) * TILE_WIDTH as i32) as f32;
            if segment_subtile_x > current_subtile_x {
                let current_x =
                    (i32::from(current_tile_x) * TILE_WIDTH as i32) as f32 + current_subtile_x;
                let current_tile_coords = Vector2I::new(current_tile_x, tile_y);
                self.built_object.add_active_fill(
                    self.builder,
                    current_x,
                    segment_x,
                    current_winding,
                    current_tile_coords,
                );
                current_subtile_x = segment_subtile_x;
            }

            // Update winding.
            current_winding += edge_winding;

            // Process the edge.
            debug!("about to process existing active edge {:#?}", active_edge);
            debug_assert!(f32::abs(active_edge.crossing.y() - tile_top) < 0.1);
            active_edge.process(self.builder, &mut self.built_object, tile_y);
            if !active_edge.segment.is_none() {
                self.active_edges.push(active_edge);
            }
        }
    }

    fn add_new_active_edge(&mut self, tile_y: i32) {
        let outline = &self.path_info.outline;
        let point_index = self.point_queue.pop().unwrap().point_index;

        let contour = &outline.contours()[point_index.contour() as usize];

        // TODO(pcwalton): Could use a bitset of processed edges…
        let prev_endpoint_index = contour.prev_endpoint_index_of(point_index.point());
        let next_endpoint_index = contour.next_endpoint_index_of(point_index.point());

        debug!(
            "adding new active edge, tile_y={} point_index={} prev={} next={} pos={:?} \
             prevpos={:?} nextpos={:?}",
            tile_y,
            point_index.point(),
            prev_endpoint_index,
            next_endpoint_index,
            contour.position_of(point_index.point()),
            contour.position_of(prev_endpoint_index),
            contour.position_of(next_endpoint_index)
        );

        if contour.point_is_logically_above(point_index.point(), prev_endpoint_index) {
            debug!("... adding prev endpoint");

            process_active_segment(
                contour,
                prev_endpoint_index,
                &mut self.active_edges,
                self.builder,
                &mut self.built_object,
                tile_y,
            );

            self.point_queue.push(QueuedEndpoint {
                point_index: PointIndex::new(point_index.contour(), prev_endpoint_index),
                y: contour.position_of(prev_endpoint_index).y(),
            });

            debug!("... done adding prev endpoint");
        }

        if contour.point_is_logically_above(point_index.point(), next_endpoint_index) {
            debug!(
                "... adding next endpoint {} -> {}",
                point_index.point(),
                next_endpoint_index
            );

            process_active_segment(
                contour,
                point_index.point(),
                &mut self.active_edges,
                self.builder,
                &mut self.built_object,
                tile_y,
            );

            self.point_queue.push(QueuedEndpoint {
                point_index: PointIndex::new(point_index.contour(), next_endpoint_index),
                y: contour.position_of(next_endpoint_index).y(),
            });

            debug!("... done adding next endpoint");
        }
    }

    fn init_point_queue(&mut self) {
        // Find MIN points.
        self.point_queue.clear();
        for (contour_index, contour) in self.path_info.outline.contours().iter().enumerate() {
            let contour_index = contour_index as u32;
            let mut cur_endpoint_index = 0;
            let mut prev_endpoint_index = contour.prev_endpoint_index_of(cur_endpoint_index);
            let mut next_endpoint_index = contour.next_endpoint_index_of(cur_endpoint_index);
            loop {
                if contour.point_is_logically_above(cur_endpoint_index, prev_endpoint_index)
                    && contour.point_is_logically_above(cur_endpoint_index, next_endpoint_index)
                {
                    self.point_queue.push(QueuedEndpoint {
                        point_index: PointIndex::new(contour_index, cur_endpoint_index),
                        y: contour.position_of(cur_endpoint_index).y(),
                    });
                }

                if cur_endpoint_index >= next_endpoint_index {
                    break;
                }

                prev_endpoint_index = cur_endpoint_index;
                cur_endpoint_index = next_endpoint_index;
                next_endpoint_index = contour.next_endpoint_index_of(cur_endpoint_index);
            }
        }
    }
}

pub fn round_rect_out_to_tile_bounds(rect: RectF) -> RectI {
    rect.scale_xy(Vector2F::new(
        1.0 / TILE_WIDTH as f32,
        1.0 / TILE_HEIGHT as f32,
    ))
    .round_out()
    .to_i32()
}

fn process_active_segment(
    contour: &Contour,
    from_endpoint_index: u32,
    active_edges: &mut SortedVector<ActiveEdge>,
    builder: &SceneBuilder,
    built_object: &mut BuiltObject,
    tile_y: i32,
) {
    let mut active_edge = ActiveEdge::from_segment(&contour.segment_after(from_endpoint_index));
    debug!("... process_active_segment({:#?})", active_edge);
    active_edge.process(builder, built_object, tile_y);
    if !active_edge.segment.is_none() {
        debug!("... ... pushing resulting active edge: {:#?}", active_edge);
        active_edges.push(active_edge);
    }
}

// Queued endpoints

#[derive(PartialEq)]
struct QueuedEndpoint {
    point_index: PointIndex,
    y: f32,
}

impl Eq for QueuedEndpoint {}

impl PartialOrd<QueuedEndpoint> for QueuedEndpoint {
    fn partial_cmp(&self, other: &QueuedEndpoint) -> Option<Ordering> {
        // NB: Reversed!
        (other.y, other.point_index).partial_cmp(&(self.y, self.point_index))
    }
}

// Active edges

#[derive(Clone, PartialEq, Debug)]
struct ActiveEdge {
    segment: Segment,
    // TODO(pcwalton): Shrink `crossing` down to just one f32?
    crossing: Vector2F,
}

impl ActiveEdge {
    fn from_segment(segment: &Segment) -> ActiveEdge {
        let crossing = if segment.baseline.from_y() < segment.baseline.to_y() {
            segment.baseline.from()
        } else {
            segment.baseline.to()
        };
        ActiveEdge::from_segment_and_crossing(segment, crossing)
    }

    fn from_segment_and_crossing(segment: &Segment, crossing: Vector2F) -> ActiveEdge {
        ActiveEdge { segment: *segment, crossing }
    }

    fn process(&mut self, builder: &SceneBuilder, built_object: &mut BuiltObject, tile_y: i32) {
        let tile_bottom = ((i32::from(tile_y) + 1) * TILE_HEIGHT as i32) as f32;
        debug!(
            "process_active_edge({:#?}, tile_y={}({}))",
            self, tile_y, tile_bottom
        );

        let mut segment = self.segment;
        let winding = segment.baseline.y_winding();

        if segment.is_line() {
            let line_segment = segment.as_line_segment();
            self.segment =
                match self.process_line_segment(line_segment, builder, built_object, tile_y) {
                    Some(lower_part) => Segment::line(lower_part),
                    None => Segment::none(),
                };
            return;
        }

        // TODO(pcwalton): Don't degree elevate!
        if !segment.is_cubic() {
            segment = segment.to_cubic();
        }

        // If necessary, draw initial line.
        if self.crossing.y() < segment.baseline.min_y() {
            let first_line_segment =
                LineSegment2F::new(self.crossing, segment.baseline.upper_point()).orient(winding);
            if self
                .process_line_segment(first_line_segment, builder, built_object, tile_y)
                .is_some()
            {
                return;
            }
        }

        let mut oriented_segment = segment.orient(winding);
        loop {
            let mut split_t = 1.0;
            let mut before_segment = oriented_segment;
            let mut after_segment = None;

            while !before_segment
                .as_cubic_segment()
                .is_flat(FLATTENING_TOLERANCE)
            {
                let next_t = 0.5 * split_t;
                let (before, after) = oriented_segment.as_cubic_segment().split(next_t);
                before_segment = before;
                after_segment = Some(after);
                split_t = next_t;
            }

            debug!(
                "... tile_y={} winding={} segment={:?} t={} before_segment={:?}
                    after_segment={:?}",
                tile_y, winding, segment, split_t, before_segment, after_segment
            );

            let line = before_segment.baseline.orient(winding);
            match self.process_line_segment(line, builder, built_object, tile_y) {
                Some(lower_part) if split_t == 1.0 => {
                    self.segment = Segment::line(lower_part);
                    return;
                }
                None if split_t == 1.0 => {
                    self.segment = Segment::none();
                    return;
                }
                Some(_) => {
                    self.segment = after_segment.unwrap().orient(winding);
                    return;
                }
                None => oriented_segment = after_segment.unwrap(),
            }
        }
    }

    fn process_line_segment(
        &mut self,
        line_segment: LineSegment2F,
        builder: &SceneBuilder,
        built_object: &mut BuiltObject,
        tile_y: i32,
    ) -> Option<LineSegment2F> {
        let tile_bottom = ((i32::from(tile_y) + 1) * TILE_HEIGHT as i32) as f32;
        debug!(
            "process_line_segment({:?}, tile_y={}) tile_bottom={}",
            line_segment, tile_y, tile_bottom
        );

        if line_segment.max_y() <= tile_bottom {
            built_object.generate_fill_primitives_for_line(builder, line_segment, tile_y);
            return None;
        }

        let (upper_part, lower_part) = line_segment.split_at_y(tile_bottom);
        built_object.generate_fill_primitives_for_line(builder, upper_part, tile_y);
        self.crossing = lower_part.upper_point();
        Some(lower_part)
    }
}

impl PartialOrd<ActiveEdge> for ActiveEdge {
    fn partial_cmp(&self, other: &ActiveEdge) -> Option<Ordering> {
        self.crossing.x().partial_cmp(&other.crossing.x())
    }
}

impl AlphaTileVertex {
    #[inline]
    fn new(tile_origin: Vector2I,
           tile_index: u16,
           tile_offset: Vector2I,
           object_index: u16,
           backdrop: i16,
           paint_metadata: &PaintMetadata)
           -> AlphaTileVertex {
        let tile_position = tile_origin + tile_offset;
        let color_uv = paint_metadata.calculate_tex_coords(tile_position).scale(65535.0).to_i32();

        let mask_u = tile_index as i32 % MASK_TILES_ACROSS as i32;
        let mask_v = tile_index as i32 / MASK_TILES_ACROSS as i32;
        let mask_scale = 65535.0 / MASK_TILES_ACROSS as f32;
        let mask_uv = Vector2I::new(mask_u, mask_v) + tile_offset;
        let mask_uv = mask_uv.to_f32().scale(mask_scale).to_i32();

        AlphaTileVertex {
            tile_x: tile_position.x() as i16,
            tile_y: tile_position.y() as i16,
            color_u: color_uv.x() as u16,
            color_v: color_uv.y() as u16,
            mask_u: mask_uv.x() as u16,
            mask_v: mask_uv.y() as u16,
            object_index,
            backdrop,
        }
    }

    #[inline]
    pub fn tile_position(&self) -> Vector2I {
        Vector2I::new(self.tile_x as i32, self.tile_y as i32)
    }
}

impl Default for TileObjectPrimitive {
    #[inline]
    fn default() -> TileObjectPrimitive {
        TileObjectPrimitive { backdrop: 0, alpha_tile_index: !0 }
    }
}

impl TileObjectPrimitive {
    #[inline]
    pub fn is_solid(&self) -> bool { self.alpha_tile_index == !0 }
}
