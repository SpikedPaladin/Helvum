// Copyright 2021 Tom A. Wagner <tom.a.wagner@protonmail.com>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License version 3 as published by
// the Free Software Foundation.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: GPL-3.0-only

use super::{Node, Port};

use gtk::{
    glib::{self, clone},
    graphene,
    graphene::Point,
    gsk,
    prelude::*,
    subclass::prelude::*,
};
use log::{error, warn};

use std::{cmp::Ordering, collections::HashMap};

use crate::NodeType;

const CANVAS_SIZE: f64 = 5000.0;

mod imp {
    use super::*;

    use std::cell::{Cell, RefCell};

    use gtk::{
        gdk::{self, RGBA},
        graphene::Rect,
        gsk::ColorStop,
    };
    use log::warn;
    use once_cell::sync::Lazy;

    pub struct DragState {
        node: glib::WeakRef<Node>,
        /// This stores the offset of the pointer to the origin of the node,
        /// so that we can keep the pointer over the same position when moving the node
        ///
        /// The offset is normalized to the default zoom-level of 1.0.
        offset: Point,
    }

    #[derive(Default)]
    pub struct GraphView {
        /// Stores nodes and their positions.
        pub(super) nodes: RefCell<HashMap<u32, (Node, Point)>>,
        /// Stores the link and whether it is currently active.
        pub(super) links: RefCell<HashMap<u32, (crate::PipewireLink, bool)>>,
        pub hadjustment: RefCell<Option<gtk::Adjustment>>,
        pub vadjustment: RefCell<Option<gtk::Adjustment>>,
        pub zoom_factor: Cell<f64>,
        /// This keeps track of an ongoing node drag operation.
        pub dragged_node: RefCell<Option<DragState>>,
        // Memorized data for an in-progress zoom gesture
        pub zoom_gesture_initial_zoom: Cell<Option<f64>>,
        pub zoom_gesture_anchor: Cell<Option<(f64, f64)>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for GraphView {
        const NAME: &'static str = "GraphView";
        type Type = super::GraphView;
        type ParentType = gtk::Widget;
        type Interfaces = (gtk::Scrollable,);

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("graphview");
        }
    }

    impl ObjectImpl for GraphView {
        fn constructed(&self) {
            self.parent_constructed();

            self.obj().set_overflow(gtk::Overflow::Hidden);

            self.setup_node_dragging();
            self.setup_scroll_zooming();
            self.setup_zoom_gesture();
        }

        fn dispose(&self) {
            self.nodes
                .borrow()
                .values()
                .for_each(|(node, _)| node.unparent())
        }

        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
                vec![
                    glib::ParamSpecOverride::for_interface::<gtk::Scrollable>("hadjustment"),
                    glib::ParamSpecOverride::for_interface::<gtk::Scrollable>("vadjustment"),
                    glib::ParamSpecOverride::for_interface::<gtk::Scrollable>("hscroll-policy"),
                    glib::ParamSpecOverride::for_interface::<gtk::Scrollable>("vscroll-policy"),
                    glib::ParamSpecDouble::builder("zoom-factor")
                        .minimum(0.3)
                        .maximum(4.0)
                        .default_value(1.0)
                        .flags(glib::ParamFlags::CONSTRUCT | glib::ParamFlags::READWRITE)
                        .build(),
                ]
            });

            PROPERTIES.as_ref()
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "hadjustment" => self.hadjustment.borrow().to_value(),
                "vadjustment" => self.vadjustment.borrow().to_value(),
                "hscroll-policy" | "vscroll-policy" => gtk::ScrollablePolicy::Natural.to_value(),
                "zoom-factor" => self.zoom_factor.get().to_value(),
                _ => unimplemented!(),
            }
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            let obj = self.obj();

            match pspec.name() {
                "hadjustment" => {
                    self.set_adjustment(&obj, value.get().ok(), gtk::Orientation::Horizontal)
                }
                "vadjustment" => {
                    self.set_adjustment(&obj, value.get().ok(), gtk::Orientation::Vertical)
                }
                "hscroll-policy" | "vscroll-policy" => {}
                "zoom-factor" => {
                    self.zoom_factor.set(value.get().unwrap());
                    obj.queue_allocate();
                }
                _ => unimplemented!(),
            }
        }
    }

    impl WidgetImpl for GraphView {
        fn size_allocate(&self, _width: i32, _height: i32, baseline: i32) {
            let widget = &*self.obj();

            let zoom_factor = self.zoom_factor.get();

            for (node, point) in self.nodes.borrow().values() {
                let (_, natural_size) = node.preferred_size();

                let transform = self
                    .canvas_space_to_screen_space_transform()
                    .translate(point);

                node.allocate(
                    (natural_size.width() as f64 / zoom_factor).ceil() as i32,
                    (natural_size.height() as f64 / zoom_factor).ceil() as i32,
                    baseline,
                    Some(transform),
                );
            }

            if let Some(ref hadjustment) = *self.hadjustment.borrow() {
                self.set_adjustment_values(widget, hadjustment, gtk::Orientation::Horizontal);
            }
            if let Some(ref vadjustment) = *self.vadjustment.borrow() {
                self.set_adjustment_values(widget, vadjustment, gtk::Orientation::Vertical);
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = &*self.obj();
            let alloc = widget.allocation();

            self.snapshot_background(widget, snapshot);

            // Draw all visible children
            self.nodes
                .borrow()
                .values()
                // Cull nodes from rendering when they are outside the visible canvas area
                .filter(|(node, _)| alloc.intersect(&node.allocation()).is_some())
                .for_each(|(node, _)| widget.snapshot_child(node, snapshot));

            self.snapshot_links(widget, snapshot);
        }
    }

    impl ScrollableImpl for GraphView {}

    impl GraphView {
        /// Returns a [`gsk::Transform`] matrix that can translate from canvas space to screen space.
        ///
        /// Canvas space is non-zoomed, and (0, 0) is fixed at the middle of the graph. \
        /// Screen space is zoomed and adjusted for scrolling, (0, 0) is at the top-left corner of the window.
        ///
        /// This is the inverted form of [`Self::screen_space_to_canvas_space_transform()`].
        fn canvas_space_to_screen_space_transform(&self) -> gsk::Transform {
            let hadj = self.hadjustment.borrow().as_ref().unwrap().value();
            let vadj = self.vadjustment.borrow().as_ref().unwrap().value();
            let zoom_factor = self.zoom_factor.get();

            gsk::Transform::new()
                .translate(&Point::new(-hadj as f32, -vadj as f32))
                .scale(zoom_factor as f32, zoom_factor as f32)
        }

        /// Returns a [`gsk::Transform`] matrix that can translate from screen space to canvas space.
        ///
        /// This is the inverted form of [`Self::canvas_space_to_screen_space_transform()`], see that function for a more detailed explantion.
        fn screen_space_to_canvas_space_transform(&self) -> gsk::Transform {
            self.canvas_space_to_screen_space_transform()
                .invert()
                .unwrap()
        }

        fn setup_node_dragging(&self) {
            let drag_controller = gtk::GestureDrag::new();

            drag_controller.connect_drag_begin(|drag_controller, x, y| {
                let widget = drag_controller
                    .widget()
                    .dynamic_cast::<super::GraphView>()
                    .expect("drag-begin event is not on the GraphView");
                let mut dragged_node = widget.imp().dragged_node.borrow_mut();

                // pick() should at least return the widget itself.
                let target = widget
                    .pick(x, y, gtk::PickFlags::DEFAULT)
                    .expect("drag-begin pick() did not return a widget");
                *dragged_node = if target.ancestor(Port::static_type()).is_some() {
                    // The user targeted a port, so the dragging should be handled by the Port
                    // component instead of here.
                    None
                } else if let Some(target) = target.ancestor(Node::static_type()) {
                    // The user targeted a Node without targeting a specific Port.
                    // Drag the Node around the screen.
                    let node = target.dynamic_cast_ref::<Node>().unwrap();

                    let Some(canvas_node_pos) = widget.node_position(node) else { return };
                    let canvas_cursor_pos = widget
                        .imp()
                        .screen_space_to_canvas_space_transform()
                        .transform_point(&Point::new(x as f32, y as f32));

                    Some(DragState {
                        node: node.clone().downgrade(),
                        offset: Point::new(
                            canvas_cursor_pos.x() - canvas_node_pos.x(),
                            canvas_cursor_pos.y() - canvas_node_pos.y(),
                        ),
                    })
                } else {
                    None
                }
            });
            drag_controller.connect_drag_update(|drag_controller, x, y| {
                let widget = drag_controller
                    .widget()
                    .dynamic_cast::<super::GraphView>()
                    .expect("drag-update event is not on the GraphView");
                let dragged_node = widget.imp().dragged_node.borrow();
                let Some(DragState { node, offset }) = dragged_node.as_ref() else { return };
                let Some(node) = node.upgrade() else { return };

                let (start_x, start_y) = drag_controller
                    .start_point()
                    .expect("Drag has no start point");

                let onscreen_node_origin = Point::new((start_x + x) as f32, (start_y + y) as f32);
                let transform = widget.imp().screen_space_to_canvas_space_transform();
                let canvas_node_origin = transform.transform_point(&onscreen_node_origin);

                widget.move_node(
                    &node,
                    &Point::new(
                        canvas_node_origin.x() - offset.x(),
                        canvas_node_origin.y() - offset.y(),
                    ),
                );
            });
            self.obj().add_controller(drag_controller);
        }

        fn setup_scroll_zooming(&self) {
            // We're only interested in the vertical axis, but for devices like touchpads,
            // not capturing a small accidental horizontal move may cause the scroll to be disrupted if a widget
            // higher up captures it instead.
            let scroll_controller =
                gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);

            scroll_controller.connect_scroll(|eventcontroller, _, delta_y| {
                let event = eventcontroller.current_event().unwrap(); // We are inside the event handler, so it must have an event

                if event
                    .modifier_state()
                    .contains(gdk::ModifierType::CONTROL_MASK)
                {
                    let widget = eventcontroller
                        .widget()
                        .downcast::<super::GraphView>()
                        .unwrap();
                    widget.set_zoom_factor(widget.zoom_factor() + (0.1 * -delta_y), None);

                    gtk::Inhibit(true)
                } else {
                    gtk::Inhibit(false)
                }
            });
            self.obj().add_controller(scroll_controller);
        }

        fn setup_zoom_gesture(&self) {
            let zoom_gesture = gtk::GestureZoom::new();
            zoom_gesture.connect_begin(|gesture, _| {
                let widget = gesture.widget().downcast::<super::GraphView>().unwrap();

                widget
                    .imp()
                    .zoom_gesture_initial_zoom
                    .set(Some(widget.zoom_factor()));
                widget
                    .imp()
                    .zoom_gesture_anchor
                    .set(gesture.bounding_box_center());
            });
            zoom_gesture.connect_scale_changed(move |gesture, delta| {
                let widget = gesture.widget().downcast::<super::GraphView>().unwrap();

                let initial_zoom = widget
                    .imp()
                    .zoom_gesture_initial_zoom
                    .get()
                    .expect("Initial zoom not set during zoom gesture");

                widget.set_zoom_factor(initial_zoom * delta, gesture.bounding_box_center());
            });
            self.obj().add_controller(zoom_gesture);
        }

        fn snapshot_background(&self, widget: &super::GraphView, snapshot: &gtk::Snapshot) {
            // Grid size and line width during neutral zoom (factor 1.0).
            const NORMAL_GRID_SIZE: f32 = 20.0;
            const NORMAL_GRID_LINE_WIDTH: f32 = 1.0;

            let zoom_factor = self.zoom_factor.get();
            let grid_size = NORMAL_GRID_SIZE * zoom_factor as f32;
            let grid_line_width = NORMAL_GRID_LINE_WIDTH * zoom_factor as f32;

            let alloc = widget.allocation();

            // We need to offset the lines between 0 and (excluding) `grid_size` so the grid moves with
            // the rest of the view when scrolling.
            // The offset is rounded so the grid is always aligned to a row of pixels.
            let hadj = self
                .hadjustment
                .borrow()
                .as_ref()
                .map(|hadjustment| hadjustment.value())
                .unwrap_or(0.0);
            let hoffset = (grid_size - (hadj as f32 % grid_size)) % grid_size;
            let vadj = self
                .vadjustment
                .borrow()
                .as_ref()
                .map(|vadjustment| vadjustment.value())
                .unwrap_or(0.0);
            let voffset = (grid_size - (vadj as f32 % grid_size)) % grid_size;

            snapshot.push_repeat(
                &Rect::new(0.0, 0.0, alloc.width() as f32, alloc.height() as f32),
                Some(&Rect::new(0.0, voffset, alloc.width() as f32, grid_size)),
            );
            let grid_color = RGBA::new(0.137, 0.137, 0.137, 1.0);
            snapshot.append_linear_gradient(
                &Rect::new(0.0, voffset, alloc.width() as f32, grid_line_width),
                &Point::new(0.0, 0.0),
                &Point::new(alloc.width() as f32, 0.0),
                &[
                    ColorStop::new(0.0, grid_color),
                    ColorStop::new(1.0, grid_color),
                ],
            );
            snapshot.pop();

            snapshot.push_repeat(
                &Rect::new(0.0, 0.0, alloc.width() as f32, alloc.height() as f32),
                Some(&Rect::new(hoffset, 0.0, grid_size, alloc.height() as f32)),
            );
            snapshot.append_linear_gradient(
                &Rect::new(hoffset, 0.0, grid_line_width, alloc.height() as f32),
                &Point::new(0.0, 0.0),
                &Point::new(0.0, alloc.height() as f32),
                &[
                    ColorStop::new(0.0, grid_color),
                    ColorStop::new(1.0, grid_color),
                ],
            );
            snapshot.pop();
        }

        fn snapshot_links(&self, widget: &super::GraphView, snapshot: &gtk::Snapshot) {
            let alloc = widget.allocation();

            let link_cr = snapshot.append_cairo(&graphene::Rect::new(
                0.0,
                0.0,
                alloc.width() as f32,
                alloc.height() as f32,
            ));

            link_cr.set_line_width(2.0 * self.zoom_factor.get());

            let rgba = widget
                .style_context()
                .lookup_color("graphview-link")
                .unwrap_or(gtk::gdk::RGBA::BLACK);

            link_cr.set_source_rgba(
                rgba.red().into(),
                rgba.green().into(),
                rgba.blue().into(),
                rgba.alpha().into(),
            );

            for (link, active) in self.links.borrow().values() {
                // TODO: Do not draw links when they are outside the view
                if let Some((from_x, from_y, to_x, to_y)) = self.get_link_coordinates(link) {
                    link_cr.move_to(from_x, from_y);

                    // Use dashed line for inactive links, full line otherwise.
                    if *active {
                        link_cr.set_dash(&[], 0.0);
                    } else {
                        link_cr.set_dash(&[10.0, 5.0], 0.0);
                    }

                    // If the output port is farther right than the input port and they have
                    // a similar y coordinate, apply a y offset to the control points
                    // so that the curve sticks out a bit.
                    let y_control_offset = if from_x > to_x {
                        f64::max(0.0, 25.0 - (from_y - to_y).abs())
                    } else {
                        0.0
                    };

                    // Place curve control offset by half the x distance between the two points.
                    // This makes the curve scale well for varying distances between the two ports,
                    // especially when the output port is farther right than the input port.
                    let half_x_dist = f64::abs(from_x - to_x) / 2.0;
                    link_cr.curve_to(
                        from_x + half_x_dist,
                        from_y - y_control_offset,
                        to_x - half_x_dist,
                        to_y - y_control_offset,
                        to_x,
                        to_y,
                    );

                    if let Err(e) = link_cr.stroke() {
                        warn!("Failed to draw graphview links: {}", e);
                    };
                } else {
                    warn!("Could not get allocation of ports of link: {:?}", link);
                }
            }
        }

        /// Get coordinates for the drawn link to start at and to end at.
        ///
        /// # Returns
        /// `Some((from_x, from_y, to_x, to_y))` if all objects the links refers to exist as widgets.
        fn get_link_coordinates(&self, link: &crate::PipewireLink) -> Option<(f64, f64, f64, f64)> {
            let widget = &*self.obj();
            let nodes = self.nodes.borrow();

            let output_port = &nodes.get(&link.node_from)?.0.get_port(link.port_from)?;

            let output_port_padding =
                (output_port.allocated_width() - output_port.width()) as f64 / 2.0;

            let (from_x, from_y) = output_port.translate_coordinates(
                widget,
                output_port.width() as f64 + output_port_padding,
                (output_port.height() / 2) as f64,
            )?;

            let input_port = &nodes.get(&link.node_to)?.0.get_port(link.port_to)?;

            let input_port_padding =
                (input_port.allocated_width() - input_port.width()) as f64 / 2.0;

            let (to_x, to_y) = input_port.translate_coordinates(
                widget,
                -input_port_padding,
                (input_port.height() / 2) as f64,
            )?;

            Some((from_x, from_y, to_x, to_y))
        }

        fn set_adjustment(
            &self,
            obj: &super::GraphView,
            adjustment: Option<&gtk::Adjustment>,
            orientation: gtk::Orientation,
        ) {
            match orientation {
                gtk::Orientation::Horizontal => {
                    *self.hadjustment.borrow_mut() = adjustment.cloned()
                }
                gtk::Orientation::Vertical => *self.vadjustment.borrow_mut() = adjustment.cloned(),
                _ => unimplemented!(),
            }

            if let Some(adjustment) = adjustment {
                adjustment
                    .connect_value_changed(clone!(@weak obj => move |_|  obj.queue_allocate() ));
            }
        }

        fn set_adjustment_values(
            &self,
            obj: &super::GraphView,
            adjustment: &gtk::Adjustment,
            orientation: gtk::Orientation,
        ) {
            let size = match orientation {
                gtk::Orientation::Horizontal => obj.width(),
                gtk::Orientation::Vertical => obj.height(),
                _ => unimplemented!(),
            };
            let zoom_factor = self.zoom_factor.get();

            adjustment.configure(
                adjustment.value(),
                -(CANVAS_SIZE / 2.0) * zoom_factor,
                (CANVAS_SIZE / 2.0) * zoom_factor,
                (f64::from(size) * 0.1) * zoom_factor,
                (f64::from(size) * 0.9) * zoom_factor,
                f64::from(size) * zoom_factor,
            );
        }
    }
}

glib::wrapper! {
    pub struct GraphView(ObjectSubclass<imp::GraphView>)
        @extends gtk::Widget;
}

impl GraphView {
    pub const ZOOM_MIN: f64 = 0.3;
    pub const ZOOM_MAX: f64 = 4.0;

    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn zoom_factor(&self) -> f64 {
        self.property("zoom-factor")
    }

    /// Set the scale factor.
    ///
    /// A factor of 1.0 is equivalent to 100% zoom, 0.5 to 50% zoom etc.
    ///
    /// An optional anchor (in canvas-space coordinates) can be specified, which will be used as the center of the zoom,
    /// so that its position stays fixed.
    /// If no anchor is specified, the middle of the screen is used instead.
    ///
    /// Note that the zoom level is [clamped](`f64::clamp`) to between 30% and 300%.
    /// See [`Self::ZOOM_MIN`] and [`Self::ZOOM_MAX`].
    pub fn set_zoom_factor(&self, zoom_factor: f64, anchor: Option<(f64, f64)>) {
        let zoom_factor = zoom_factor.clamp(Self::ZOOM_MIN, Self::ZOOM_MAX);

        let (anchor_x_screen, anchor_y_screen) = anchor.unwrap_or_else(|| {
            (
                self.allocation().width() as f64 / 2.0,
                self.allocation().height() as f64 / 2.0,
            )
        });

        let old_zoom = self.imp().zoom_factor.get();
        let hadjustment_ref = self.imp().hadjustment.borrow();
        let vadjustment_ref = self.imp().vadjustment.borrow();
        let hadjustment = hadjustment_ref.as_ref().unwrap();
        let vadjustment = vadjustment_ref.as_ref().unwrap();

        let x_total = (anchor_x_screen + hadjustment.value()) / old_zoom;
        let y_total = (anchor_y_screen + vadjustment.value()) / old_zoom;

        let new_hadjustment = x_total * zoom_factor - anchor_x_screen;
        let new_vadjustment = y_total * zoom_factor - anchor_y_screen;

        hadjustment.set_value(new_hadjustment);
        vadjustment.set_value(new_vadjustment);

        self.set_property("zoom-factor", zoom_factor);
    }

    pub fn add_node(&self, id: u32, node: Node, node_type: Option<NodeType>) {
        let imp = self.imp();
        node.set_parent(self);

        // Place widgets in colums of 3, growing down
        let x = if let Some(node_type) = node_type {
            match node_type {
                NodeType::Output => 20.0,
                NodeType::Input => 820.0,
            }
        } else {
            420.0
        };

        let y = imp
            .nodes
            .borrow()
            .values()
            .map(|node| {
                // Map nodes to their locations
                let point = self.node_position(&node.0.clone().upcast()).unwrap();
                (point.x(), point.y())
            })
            .filter(|(x2, _)| {
                // Only look for other nodes that have a similar x coordinate
                (x - x2).abs() < 50.0
            })
            .max_by(|y1, y2| {
                // Get max in column
                y1.partial_cmp(y2).unwrap_or(Ordering::Equal)
            })
            .map_or(20_f32, |(_x, y)| y + 120.0);

        imp.nodes.borrow_mut().insert(id, (node, Point::new(x, y)));
    }

    pub fn remove_node(&self, id: u32) {
        let mut nodes = self.imp().nodes.borrow_mut();
        if let Some((node, _)) = nodes.remove(&id) {
            node.unparent();
        } else {
            warn!("Tried to remove non-existant node (id={}) from graph", id);
        }
    }

    pub fn add_port(&self, node_id: u32, port_id: u32, port: crate::view::port::Port) {
        if let Some((node, _)) = self.imp().nodes.borrow_mut().get_mut(&node_id) {
            node.add_port(port_id, port);
        } else {
            error!(
                "Node with id {} not found when trying to add port with id {} to graph",
                node_id, port_id
            );
        }
    }

    pub fn remove_port(&self, id: u32, node_id: u32) {
        let nodes = self.imp().nodes.borrow();
        if let Some((node, _)) = nodes.get(&node_id) {
            node.remove_port(id);
        }
    }

    pub fn add_link(&self, link_id: u32, link: crate::PipewireLink, active: bool) {
        self.imp()
            .links
            .borrow_mut()
            .insert(link_id, (link, active));
        self.queue_draw();
    }

    pub fn set_link_state(&self, link_id: u32, active: bool) {
        if let Some((_, state)) = self.imp().links.borrow_mut().get_mut(&link_id) {
            *state = active;
            self.queue_draw();
        } else {
            warn!("Link state changed on unknown link (id={})", link_id);
        }
    }

    pub fn remove_link(&self, id: u32) {
        let mut links = self.imp().links.borrow_mut();
        links.remove(&id);

        self.queue_draw();
    }

    /// Get the position of the specified node inside the graphview.
    ///
    /// The returned position is in canvas-space (non-zoomed, (0, 0) fixed in the middle of the canvas).
    pub(super) fn node_position(&self, node: &Node) -> Option<Point> {
        self.imp()
            .nodes
            .borrow()
            .get(&node.pipewire_id())
            .map(|(_, point)| *point)
    }

    pub(super) fn move_node(&self, widget: &Node, point: &Point) {
        let mut nodes = self.imp().nodes.borrow_mut();
        let mut node = nodes
            .get_mut(&widget.pipewire_id())
            .expect("Node is not on the graph");

        // Clamp the new position to within the graph, so a node can't be moved outside it and be lost.
        node.1 = Point::new(
            point.x().clamp(
                -(CANVAS_SIZE / 2.0) as f32,
                (CANVAS_SIZE / 2.0) as f32 - widget.width() as f32,
            ),
            point.y().clamp(
                -(CANVAS_SIZE / 2.0) as f32,
                (CANVAS_SIZE / 2.0) as f32 - widget.height() as f32,
            ),
        );

        self.queue_allocate();
    }
}

impl Default for GraphView {
    fn default() -> Self {
        Self::new()
    }
}
