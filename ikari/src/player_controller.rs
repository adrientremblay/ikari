use crate::collisions::*;
use crate::math::*;
use crate::physics::*;
use crate::renderer::*;
use crate::time::*;
use crate::transform::*;

use rapier3d_f64::prelude::*;

use glam::f32::{Quat, Vec3};
use glam::EulerRot;
use winit::event::MouseButton;
use winit::keyboard::Key;
use winit::keyboard::NamedKey;
use winit::window::CursorGrabMode;
use winit::{
    dpi::PhysicalPosition,
    event::{DeviceEvent, ElementState, MouseScrollDelta, WindowEvent},
    window::Window,
};

#[derive(Clone, Debug)]
pub struct PlayerController {
    unprocessed_delta: Option<(f64, f64)>,
    window_focused: bool,
    is_window_focused_and_clicked: bool,
    is_enabled: bool,
    cached_cursor_grab: bool,

    is_forward_pressed: bool,
    is_backward_pressed: bool,
    is_left_pressed: bool,
    is_right_pressed: bool,
    is_jump_pressed: bool,
    is_up_pressed: bool,
    is_down_pressed: bool,

    pub mouse_button_pressed: bool,

    pub view_direction: ControlledViewDirection,
    pub speed: f32,
    pub rigid_body_handle: RigidBodyHandle,

    pub last_jump_time: Option<Instant>,
}

#[derive(Copy, Clone, Debug)]
pub struct ControlledViewDirection {
    pub horizontal: f32,
    pub vertical: f32,
}

impl ControlledViewDirection {
    pub fn to_quat(self) -> Quat {
        Quat::from_euler(EulerRot::XYZ, 0.0, self.horizontal, 0.0)
            * Quat::from_euler(EulerRot::XYZ, self.vertical, 0.0, 0.0)
    }

    pub fn to_vector(self) -> Vec3 {
        let horizontal_scale = self.vertical.cos();
        Vec3::new(
            (self.horizontal + std::f32::consts::PI).sin() * horizontal_scale,
            self.vertical.sin(),
            (self.horizontal + std::f32::consts::PI).cos() * horizontal_scale,
        )
        .normalize()
    }
}

impl PlayerController {
    pub fn new(
        physics_state: &mut PhysicsState,
        speed: f32,
        position: Vec3,
        view_direction: ControlledViewDirection,
        collider: Collider,
    ) -> Self {
        let rigid_body = RigidBodyBuilder::dynamic()
            .translation(vector![
                position.x as f64,
                position.y as f64,
                position.z as f64
            ])
            .lock_rotations()
            .build();
        let rigid_body_handle = physics_state.rigid_body_set.insert(rigid_body);
        physics_state.collider_set.insert_with_parent(
            collider,
            rigid_body_handle,
            &mut physics_state.rigid_body_set,
        );

        Self {
            unprocessed_delta: None,
            window_focused: false,
            is_window_focused_and_clicked: false,
            is_enabled: true,
            cached_cursor_grab: false,

            mouse_button_pressed: false,

            is_forward_pressed: false,
            is_backward_pressed: false,
            is_left_pressed: false,
            is_right_pressed: false,
            is_jump_pressed: false,
            is_up_pressed: false,
            is_down_pressed: false,

            view_direction,
            speed,
            rigid_body_handle,
            last_jump_time: None,
        }
    }

    pub fn set_is_controlling_game(&mut self, value: bool) {
        self.is_enabled = value;

        if !value {
            self.is_forward_pressed = false;
            self.is_left_pressed = false;
            self.is_backward_pressed = false;
            self.is_right_pressed = false;
            self.is_jump_pressed = false;
            self.is_down_pressed = false;
            self.is_up_pressed = false;
            self.mouse_button_pressed = false;
        }
    }

    pub fn set_is_gravity_enabled(&self, physics_state: &mut PhysicsState, is_enabled: bool) {
        let rigid_body = physics_state
            .rigid_body_set
            .get_mut(self.rigid_body_handle)
            .unwrap();
        rigid_body.set_gravity_scale(if is_enabled { 1.0 } else { 0.0 }, true);
    }

    fn is_controlling_game(&self) -> bool {
        self.is_window_focused_and_clicked && self.is_enabled
    }

    fn increment_speed(&mut self, increase: bool) {
        let direction = if increase { 1.0 } else { -1.0 };
        let amount = 1.0;
        self.speed = (self.speed + (direction * amount)).clamp(0.5, 300.0);
    }

    pub fn process_device_event(&mut self, event: &DeviceEvent) {
        if !self.is_controlling_game() {
            return;
        }
        match event {
            DeviceEvent::MouseMotion { delta: (d_x, d_y) } if self.window_focused => {
                self.unprocessed_delta = match self.unprocessed_delta {
                    Some((x, y)) => Some((x + d_x, y + d_y)),
                    None => Some((*d_x, *d_y)),
                };
            }
            DeviceEvent::MouseWheel { delta } if self.window_focused => {
                let scroll_amount = match delta {
                    MouseScrollDelta::LineDelta(_, y) => *y,
                    MouseScrollDelta::PixelDelta(PhysicalPosition { y, .. }) => *y as f32,
                };
                self.increment_speed(scroll_amount > 0.0);
            }
            _ => {}
        };
    }

    pub fn process_window_event(&mut self, event: &WindowEvent, _window: &Window) {
        match event {
            WindowEvent::KeyboardInput { event, .. } => {
                let key = event.logical_key.as_ref();
                if event.state == ElementState::Pressed && key == Key::Named(NamedKey::ArrowUp) {
                    self.increment_speed(true);
                }

                if event.state == ElementState::Pressed && key == Key::Named(NamedKey::ArrowUp) {
                    self.increment_speed(false);
                }

                if self.is_enabled {
                    let is_pressed = event.state == ElementState::Pressed;
                    match key {
                        Key::Character(character) => match character.to_lowercase().as_str() {
                            "w" => {
                                self.is_forward_pressed = is_pressed;
                            }
                            "a" => {
                                self.is_left_pressed = is_pressed;
                            }
                            "s" => {
                                self.is_backward_pressed = is_pressed;
                            }
                            "d" => {
                                self.is_right_pressed = is_pressed;
                            }
                            "q" => {
                                self.is_down_pressed = is_pressed;
                            }
                            "e" => {
                                self.is_up_pressed = is_pressed;
                            }
                            _ => {}
                        },
                        Key::Named(NamedKey::Space) => {
                            self.is_jump_pressed = is_pressed;
                        }
                        Key::Named(NamedKey::Control) => {
                            self.is_down_pressed = is_pressed;
                        }
                        _ => {}
                    }
                }
            }
            WindowEvent::Focused(focused) => {
                #[cfg(not(target_arch = "wasm32"))]
                if *focused {
                    crate::thread::sleep(std::time::Duration::from_millis(100));
                }

                self.window_focused = *focused;
                if !self.window_focused {
                    self.is_window_focused_and_clicked = false;
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                let is_pressed = *state == ElementState::Pressed;
                self.mouse_button_pressed = self.is_enabled && is_pressed;

                if self.window_focused && is_pressed {
                    self.is_window_focused_and_clicked = true;
                }
            }
            _ => {}
        };
    }

    pub fn update_cursor_grab(&mut self, grab: bool, window: &Window) {
        let grab = self.is_window_focused_and_clicked && grab;

        if grab == self.cached_cursor_grab {
            return;
        }

        self.cached_cursor_grab = grab;

        let new_grab_mode = if !grab {
            CursorGrabMode::None
        } else if cfg!(target_arch = "wasm32") || cfg!(target_os = "macos") {
            CursorGrabMode::Locked
        } else {
            CursorGrabMode::Confined
        };

        if let Err(err) = window.set_cursor_grab(new_grab_mode) {
            log::error!(
                "Couldn't {:?} cursor: {:?}",
                if grab { "grab" } else { "release" },
                err
            )
        }

        window.set_cursor_visible(!grab);
    }

    pub fn update(&mut self, physics_state: &mut PhysicsState) {
        if let Some((d_x, d_y)) = self.unprocessed_delta {
            let mouse_sensitivity = 0.002;

            self.view_direction.horizontal += -d_x as f32 * mouse_sensitivity;
            self.view_direction.vertical = (self.view_direction.vertical
                + (-d_y as f32 * mouse_sensitivity))
                .clamp(deg_to_rad(-89.5), deg_to_rad(89.5));
        }
        self.unprocessed_delta = None;

        let forward_direction = self.view_direction.to_vector();
        let up_direction = Vec3::new(0.0, 1.0, 0.0);
        let right_direction = forward_direction.cross(up_direction);

        let rigid_body = physics_state
            .rigid_body_set
            .get_mut(self.rigid_body_handle)
            .unwrap();

        let gravity_is_enabled = rigid_body.gravity_scale() * physics_state.gravity.norm() > 0.001;

        let new_linear_velocity = {
            let mut res: Option<Vec3> = None;

            let mut add_movement = |movement: Vec3| {
                res = match res {
                    Some(res) => Some(res + movement),
                    None => Some(movement),
                }
            };

            if self.is_forward_pressed {
                add_movement(forward_direction);
            } else if self.is_backward_pressed {
                add_movement(-forward_direction);
            }

            if self.is_right_pressed {
                add_movement(right_direction);
            } else if self.is_left_pressed {
                add_movement(-right_direction);
            }

            if self.is_up_pressed {
                add_movement(up_direction);
            } else if self.is_down_pressed {
                add_movement(-up_direction);
            } else if !gravity_is_enabled && self.is_jump_pressed {
                add_movement(up_direction);
            }

            res.map(|res| res.normalize() * self.speed)
                .unwrap_or(Vec3::new(0.0, 0.0, 0.0))
        };

        let current_linear_velocity = rigid_body.linvel();
        rigid_body.set_linvel(
            vector![
                new_linear_velocity.x as f64,
                if gravity_is_enabled {
                    current_linear_velocity.y
                } else {
                    new_linear_velocity.y as f64
                },
                new_linear_velocity.z as f64
            ],
            true,
        );

        let can_jump = || {
            if !gravity_is_enabled || self.is_up_pressed {
                return false;
            }

            let jump_cooldown_seconds = 1.25;
            match self.last_jump_time {
                Some(last_jump_time) => {
                    Instant::now().duration_since(last_jump_time).as_secs_f32()
                        > jump_cooldown_seconds
                }
                None => true,
            }
        };
        if self.is_jump_pressed && can_jump() {
            rigid_body.apply_impulse(vector![0.0, 3.0, 0.0], true);
            self.last_jump_time = Some(Instant::now());
        }
    }

    pub fn transform(&self, physics_state: &PhysicsState) -> crate::transform::Transform {
        TransformBuilder::new()
            .position(self.position(physics_state))
            .rotation(self.view_direction.to_quat())
            .build()
    }

    pub fn position(&self, physics_state: &PhysicsState) -> Vec3 {
        let position = physics_state
            .rigid_body_set
            .get(self.rigid_body_handle)
            .unwrap()
            .translation();
        Vec3::new(position.x as f32, position.y as f32, position.z as f32)
    }

    pub fn view_forward_vector(&self) -> Vec3 {
        self.view_direction.to_vector()
    }

    pub fn view_frustum_with_position(
        &self,
        aspect_ratio: f32,
        camera_position: Vec3,
    ) -> CameraFrustumDescriptor {
        let camera_forward = self.view_direction.to_vector();

        CameraFrustumDescriptor {
            focal_point: camera_position,
            forward_vector: camera_forward,
            aspect_ratio,
            near_plane_distance: NEAR_PLANE_DISTANCE,
            far_plane_distance: FAR_PLANE_DISTANCE,
            fov_y_rad: deg_to_rad(FOV_Y_DEG),
        }
    }
}
