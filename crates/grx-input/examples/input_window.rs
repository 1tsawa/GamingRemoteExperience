//! Ouvre une fenetre (winit) et affiche les `InputEvent` produits par le clavier/la souris,
//! avec leur encodage binaire et la verification aller-retour encode/decode.
//!
//!   cargo run -p grx-input --example input_window            (fenetre interactive)
//!   cargo run -p grx-input --example input_window -- --auto-close-ms 1500   (ferme seule ; smoke test)
//!
//! Ne rien injecter ici : c'est uniquement le cote CAPTURE (client).

use std::time::{Duration, Instant};

use grx_input::client::{normalize_abs, MouseAccumulator};
use grx_input::keymap::{key_from_extended_u32, scancode_name};
use grx_input::{InputEvent, MouseButton};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::window::{Window, WindowId};

struct App {
    window: Option<Window>,
    size: PhysicalSize<u32>,
    acc: MouseAccumulator,
    last_abs: Option<InputEvent>,
    deadline: Option<Instant>,
    count: u64,
}

impl App {
    fn emit(&mut self, ev: InputEvent) {
        let bytes = ev.encode();
        // verification aller-retour a chaque evenement
        assert_eq!(InputEvent::decode(&bytes), Ok(ev), "roundtrip KO");
        self.count += 1;
        let extra = match ev {
            InputEvent::Key { scancode, extended, .. } => format!("  [{}]", scancode_name(scancode, extended)),
            _ => String::new(),
        };
        println!("{ev:?}{extra}  -> {} octets {:02X?}", bytes.len(), bytes);
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_none() {
            let attrs = Window::default_attributes()
                .with_title("grx-input : evenements (Echap ne ferme pas, fermez la fenetre)")
                .with_inner_size(PhysicalSize::new(640, 400));
            let w = el.create_window(attrs).expect("creation de la fenetre");
            self.size = w.inner_size();
            self.window = Some(w);
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(s) => self.size = s,
            WindowEvent::KeyboardInput { event, .. } => {
                let physical: PhysicalKey = event.physical_key;
                if let Some(sc) = physical.to_scancode() {
                    if let Some(ev) = key_from_extended_u32(sc, event.state == ElementState::Pressed) {
                        self.emit(ev);
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(ev) = normalize_abs(position.x as i32, position.y as i32, self.size.width, self.size.height) {
                    if self.last_abs != Some(ev) {
                        self.last_abs = Some(ev);
                        self.emit(ev);
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                use winit::event::MouseButton as B;
                let button = match button {
                    B::Left => Some(MouseButton::Left),
                    B::Right => Some(MouseButton::Right),
                    B::Middle => Some(MouseButton::Middle),
                    B::Back => Some(MouseButton::X1),
                    B::Forward => Some(MouseButton::X2),
                    B::Other(_) => None,
                };
                if let Some(button) = button {
                    self.emit(InputEvent::MouseButton { button, pressed: state == ElementState::Pressed });
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (x, y) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * 120.0, y * 120.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32),
                };
                let c = |v: f32| v.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                self.emit(InputEvent::Wheel { horizontal: c(x), vertical: c(y) });
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        // deltas bruts (Raw Input sous Windows), sans acceleration : mode "jeu"
        if let DeviceEvent::MouseMotion { delta } = event {
            self.acc.add(delta.0.round() as i32, delta.1.round() as i32);
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        for ev in self.acc.drain() {
            self.emit(ev);
        }
        if let Some(d) = self.deadline {
            if Instant::now() >= d {
                self.deadline = None;
                println!("auto-close : {} evenements produits", self.count);
                el.exit();
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let auto_close = args
        .iter()
        .position(|a| a == "--auto-close-ms")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis);

    let event_loop = EventLoop::new().expect("boucle d'evenements");
    event_loop.set_control_flow(match auto_close {
        Some(_) => ControlFlow::Poll,
        None => ControlFlow::Wait,
    });
    let mut app = App {
        window: None,
        size: PhysicalSize::new(1, 1),
        acc: MouseAccumulator::new(),
        last_abs: None,
        deadline: auto_close.map(|d| Instant::now() + d),
        count: 0,
    };
    event_loop.run_app(&mut app).expect("run_app");
}
