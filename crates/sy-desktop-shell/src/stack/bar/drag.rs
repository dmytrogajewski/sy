//! Native drag sources on the panel's own Wayland connection.

use wayland_client::protocol::{
    wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_pointer, wl_registry,
    wl_seat, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};

pub(super) struct NativeDrag {
    connection: Connection,
    queue: EventQueue<DragState>,
    _registry: wl_registry::WlRegistry,
    state: DragState,
}

#[derive(Default)]
struct DragState {
    manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    device: Option<wl_data_device::WlDataDevice>,
    surface: Option<wl_surface::WlSurface>,
    serial: u32,
    position: (f64, f64),
    gesture: Gesture,
    payload: Option<Payload>,
    source: Option<wl_data_source::WlDataSource>,
}

impl NativeDrag {
    pub(super) fn new(connection: Connection) -> anyhow::Result<Self> {
        let mut queue = connection.new_event_queue();
        let registry = connection.display().get_registry(&queue.handle(), ());
        let mut state = DragState::default();
        queue.roundtrip(&mut state)?;
        if let (Some(manager), Some(seat)) = (&state.manager, &state.seat) {
            state.device = Some(manager.get_data_device(seat, &queue.handle(), ()));
        }
        queue.roundtrip(&mut state)?;
        anyhow::ensure!(
            state.device.is_some() && state.pointer.is_some(),
            "Wayland data device or pointer is unavailable"
        );
        Ok(Self {
            connection,
            queue,
            _registry: registry,
            state,
        })
    }
    pub(super) fn prepare(&mut self, payload: Payload) {
        self.state.gesture.dragged = false;
        self.state.payload = Some(payload);
    }
    pub(super) fn cancel_press(&mut self) {
        self.state.release_press();
    }
    pub(super) fn dragged(&self) -> bool {
        self.state.gesture.dragged
    }
    pub(super) fn active(&self) -> bool {
        self.state.source.is_some()
    }
    pub(super) fn pump(&mut self) -> anyhow::Result<()> {
        self.queue.dispatch_pending(&mut self.state)?;
        // Drain releases before starting: clip decoding may span several input frames.
        if self.state.gesture.motion(self.state.position) && !self.state.gesture.dragged {
            self.state.start(&self.queue.handle());
        }
        self.connection.flush()?;
        Ok(())
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for DragState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_data_device_manager" => {
                    state.manager = Some(registry.bind(name, version.min(3), qh, ()))
                }
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(5), qh, ()))
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for DragState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            } else if !caps.contains(wl_seat::Capability::Pointer) {
                if let Some(pointer) = state.pointer.take() {
                    pointer.release();
                }
                state.gesture.release();
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for DragState {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                state.surface = Some(surface);
                state.position = (surface_x, surface_y);
            }
            wl_pointer::Event::Button {
                serial,
                button: 0x110,
                state: WEnum::Value(button_state),
                ..
            } => {
                if button_state == wl_pointer::ButtonState::Pressed {
                    state.serial = serial;
                    state.gesture.press(state.position);
                } else {
                    state.release_press();
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.position = (surface_x, surface_y);
            }
            _ => {}
        }
    }
}

impl DragState {
    fn release_press(&mut self) {
        self.payload = None;
        self.gesture.release();
    }
    fn start(&mut self, qh: &QueueHandle<Self>) {
        let (Some(manager), Some(device), Some(surface), Some(payload)) = (
            &self.manager,
            &self.device,
            &self.surface,
            self.payload.as_ref(),
        ) else {
            return;
        };
        let source = manager.create_data_source(qh, payload.clone());
        for mime in payload.mime_types() {
            source.offer((*mime).into());
        }
        if source.version() >= 3 {
            source.set_actions(wl_data_device_manager::DndAction::Copy);
        }
        device.start_drag(Some(&source), surface, None, self.serial);
        self.source = Some(source);
        self.payload = None;
        self.gesture.dragged = true;
    }
}

impl Dispatch<wl_data_source::WlDataSource, Payload> for DragState {
    fn event(
        state: &mut Self,
        source: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        payload: &Payload,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd }
                if payload.mime_types().contains(&mime_type.as_str()) =>
            {
                let bytes = payload.bytes.clone();
                std::thread::spawn(move || {
                    use std::io::Write;
                    if let Err(error) = std::fs::File::from(fd).write_all(&bytes) {
                        tracing::debug!(%error, "drag receiver closed transfer");
                    }
                });
            }
            wl_data_source::Event::Cancelled | wl_data_source::Event::DndFinished => {
                source.destroy();
                if state.source.as_ref() == Some(source) {
                    state.source = None;
                    if state.gesture.dragged {
                        state.gesture.release();
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for DragState {
    fn event(
        _: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_device::Event::DataOffer { id } = event {
            id.destroy();
        }
    }
    wayland_client::event_created_child!(DragState, wl_data_device::WlDataDevice, [0 => (wl_data_offer::WlDataOffer, ())]);
}
wayland_client::delegate_noop!(DragState: ignore wl_data_device_manager::WlDataDeviceManager);
wayland_client::delegate_noop!(DragState: ignore wl_data_offer::WlDataOffer);

#[cfg(test)]
mod protocol_tests;

const DRAG_THRESHOLD: f64 = 6.0;
#[derive(Default)]
struct Gesture {
    origin: Option<(f64, f64)>,
    dragged: bool,
}
impl Gesture {
    fn press(&mut self, point: (f64, f64)) {
        self.origin = Some(point);
        self.dragged = false;
    }
    fn release(&mut self) {
        self.origin = None;
    }
    fn motion(&mut self, point: (f64, f64)) -> bool {
        self.origin
            .is_some_and(|p| (p.0 - point.0).hypot(p.1 - point.1) >= DRAG_THRESHOLD)
    }
}

#[derive(Debug, Clone)]
pub(super) struct Payload {
    mime: &'static str,
    bytes: Vec<u8>,
}
impl Payload {
    pub(super) fn text(bytes: Vec<u8>) -> anyhow::Result<Self> {
        std::str::from_utf8(&bytes)?;
        Ok(Self {
            mime: "text/plain;charset=utf-8",
            bytes,
        })
    }
    fn mime_types(&self) -> &[&str] {
        if self.mime == "text/uri-list" {
            &["text/uri-list"]
        } else {
            &["text/plain;charset=utf-8", "text/plain"]
        }
    }
    pub(super) fn file(path: &std::path::Path) -> anyhow::Result<Self> {
        let uri = url::Url::from_file_path(path)
            .map_err(|()| anyhow::anyhow!("drag file path must be absolute"))?;
        Ok(Self {
            mime: "text/uri-list",
            bytes: format!("{uri}\r\n").into_bytes(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releasing_a_click_discards_its_payload_before_the_next_press() {
        let mut state = DragState {
            payload: Some(Payload::text(b"old clip".to_vec()).unwrap()),
            ..Default::default()
        };
        state.release_press();
        assert!(state.payload.is_none());
    }

    #[test]
    fn releasing_or_cancelling_a_press_prevents_a_late_drag() {
        let mut gesture = Gesture::default();
        gesture.press((0.0, 0.0));
        gesture.release();
        assert!(!gesture.motion((100.0, 0.0)));
    }

    #[test]
    fn dragging_requires_six_logical_pixels_of_motion() {
        let mut gesture = Gesture::default();
        gesture.press((10.0, 20.0));
        assert!(!gesture.motion((15.0, 20.0)));
        assert!(gesture.motion((16.0, 20.0)));
    }

    #[test]
    fn text_offers_original_utf8_in_both_text_mime_types() {
        let payload = Payload::text("hello 世界".as_bytes().to_vec()).unwrap();
        assert_eq!(
            payload.mime_types(),
            &["text/plain;charset=utf-8", "text/plain"]
        );
    }

    #[test]
    fn file_offer_encodes_spaces_unicode_and_uri_metacharacters() {
        let offer = Payload::file(std::path::Path::new("/tmp/a b/é#%.png")).unwrap();
        assert_eq!(offer.bytes, b"file:///tmp/a%20b/%C3%A9%23%25.png\r\n");
    }
}
