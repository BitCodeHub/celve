//! The desktop app's [`SageHost`]: Celve's shared Sage agent ([`celve_sage`]), waking the window
//! when it has news.

use std::path::PathBuf;
use std::sync::Arc;

use gridcraft_ui_egui::sage::{SageEvent, SageHost, SagePicture};

pub use celve_sage::{new_token, run_bridge};

pub struct Sage(celve_sage::Sage);

impl Sage {
    pub fn new(port: u16, token: String, workdir: PathBuf, ctx: egui::Context) -> Self {
        Sage(celve_sage::Sage::new(port, token, workdir, Arc::new(move || ctx.request_repaint())))
    }
}

/// The pane's event for one agent event.
pub fn event(e: celve_sage::Event) -> SageEvent {
    match e {
        celve_sage::Event::Text(t) => SageEvent::Text(t),
        celve_sage::Event::Step(s) => SageEvent::Step(s),
        celve_sage::Event::Retract(n) => SageEvent::Retract(n),
        celve_sage::Event::Done => SageEvent::Done,
        celve_sage::Event::Error(m) => SageEvent::Error(m),
    }
}

impl SageHost for Sage {
    fn availability(&self) -> Result<(), String> {
        self.0.availability()
    }
    fn warm_up(&mut self) {
        self.0.warm_up();
    }
    fn send(&mut self, text: &str, pictures: &[SagePicture], context: &str) {
        let pictures: Vec<celve_sage::Picture> =
            pictures.iter().map(|p| celve_sage::Picture { name: p.name.clone(), mime: p.mime.clone(), bytes: p.bytes.clone() }).collect();
        self.0.send(text, &pictures, context);
    }
    fn stop(&mut self) {
        self.0.stop();
    }
    fn reset(&mut self) {
        self.0.reset();
    }
    fn poll(&mut self) -> Vec<SageEvent> {
        self.0.poll().into_iter().map(event).collect()
    }
}
