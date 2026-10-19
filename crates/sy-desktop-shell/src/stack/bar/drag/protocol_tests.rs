//! A real Wayland socket pair verifies the source requests and FD transfer.

use super::*;
use std::{io::Read, os::unix::net::UnixStream, sync::mpsc, time::Duration};
use wayland_server::protocol::{
    wl_compositor as compositor, wl_data_device as device, wl_data_device_manager as manager,
    wl_data_offer as offer, wl_data_source as source, wl_pointer as pointer, wl_seat as seat,
    wl_surface as surface,
};
use wayland_server::{
    Client, DataInit, Dispatch as ServerDispatch, Display, DisplayHandle, GlobalDispatch, New,
};

const BUTTON_SERIAL: u32 = 42;
const ENTER_SERIAL: u32 = 41;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Finish,
    Cancel,
    QuickRelease,
}

struct Server {
    pointer: Option<pointer::WlPointer>,
    origin: Option<surface::WlSurface>,
    offered: Vec<String>,
    actions: Option<manager::DndAction>,
    result: mpsc::Sender<(u32, bool, Vec<String>, Option<manager::DndAction>)>,
    writer: Option<UnixStream>,
    requested_mime: String,
    outcome: Outcome,
}

macro_rules! global {
    ($ty:ty, $body:expr) => {
        impl GlobalDispatch<$ty, ()> for Server {
            fn bind(
                state: &mut Self,
                _: &DisplayHandle,
                _: &Client,
                resource: New<$ty>,
                _: &(),
                init: &mut DataInit<'_, Self>,
            ) {
                let resource = init.init(resource, ());
                ($body)(state, resource);
            }
        }
    };
}
global!(
    compositor::WlCompositor,
    |_: &mut Server, _: compositor::WlCompositor| {}
);
global!(
    manager::WlDataDeviceManager,
    |_: &mut Server, _: manager::WlDataDeviceManager| {}
);
global!(seat::WlSeat, |_: &mut Server, seat: seat::WlSeat| seat
    .capabilities(seat::Capability::Pointer));

impl ServerDispatch<compositor::WlCompositor, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &compositor::WlCompositor,
        request: compositor::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let compositor::Request::CreateSurface { id } = request {
            state.origin = Some(init.init(id, ()));
        }
    }
}
impl ServerDispatch<seat::WlSeat, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &seat::WlSeat,
        request: seat::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let seat::Request::GetPointer { id } = request {
            state.pointer = Some(init.init(id, ()));
        }
    }
}
impl ServerDispatch<surface::WlSurface, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &surface::WlSurface,
        request: surface::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let surface::Request::Commit = request {
            let pointer = state.pointer.as_ref().unwrap();
            pointer.enter(ENTER_SERIAL, state.origin.as_ref().unwrap(), 10.0, 20.0);
            pointer.button(BUTTON_SERIAL, 0, 0x110, pointer::ButtonState::Pressed);
            pointer.motion(1, 17.0, 20.0);
            if state.outcome == Outcome::QuickRelease {
                pointer.button(BUTTON_SERIAL + 1, 2, 0x110, pointer::ButtonState::Released);
            }
            pointer.frame();
        }
    }
}
impl ServerDispatch<manager::WlDataDeviceManager, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &manager::WlDataDeviceManager,
        request: manager::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            manager::Request::GetDataDevice { id, .. } => {
                init.init(id, ());
            }
            manager::Request::CreateDataSource { id } => {
                init.init(id, ());
            }
            _ => {}
        }
    }
}
impl ServerDispatch<source::WlDataSource, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &source::WlDataSource,
        request: source::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            source::Request::Offer { mime_type } => state.offered.push(mime_type),
            source::Request::SetActions { dnd_actions } => {
                state.actions = dnd_actions.into_result().ok()
            }
            _ => {}
        }
    }
}
impl ServerDispatch<device::WlDataDevice, ()> for Server {
    fn request(
        state: &mut Self,
        client: &Client,
        device: &device::WlDataDevice,
        request: device::Request,
        _: &(),
        dh: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let device::Request::StartDrag {
            source: Some(source),
            origin,
            serial,
            ..
        } = request
        {
            state
                .result
                .send((
                    serial,
                    Some(&origin) == state.origin.as_ref(),
                    state.offered.clone(),
                    state.actions,
                ))
                .unwrap();
            let writer = state.writer.take().unwrap();
            if state.outcome == Outcome::Cancel {
                let offer = client
                    .create_resource::<offer::WlDataOffer, (), Self>(dh, 3, ())
                    .unwrap();
                device.data_offer(&offer);
                offer.offer("text/plain".into());
                device.enter(43, &origin, 10.0, 20.0, Some(&offer));
                device.leave();
                source.cancelled();
            } else {
                use std::os::fd::AsFd;
                source.send(state.requested_mime.clone(), writer.as_fd());
                source.dnd_finished();
            }
        }
    }
}
impl ServerDispatch<offer::WlDataOffer, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &offer::WlDataOffer,
        _: offer::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
impl ServerDispatch<pointer::WlPointer, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &pointer::WlPointer,
        _: pointer::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

struct SurfaceClient;
wayland_client::delegate_noop!(SurfaceClient: ignore wayland_client::protocol::wl_compositor::WlCompositor);
wayland_client::delegate_noop!(SurfaceClient: ignore wayland_client::protocol::wl_surface::WlSurface);

#[test]
fn native_drag_uses_button_grab_origin_copy_offer_and_transfers_file_uri() {
    let payload = Payload::file(std::path::Path::new("/tmp/screenshot.png")).unwrap();
    assert_eq!(
        transfer(payload, "text/uri-list", Outcome::Finish),
        "file:///tmp/screenshot.png\r\n"
    );
}

fn transfer(payload: Payload, requested_mime: &str, outcome: Outcome) -> String {
    let offered = payload
        .mime_types()
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    let requested_mime = requested_mime.to_owned();
    let (client_socket, server_socket) = UnixStream::pair().unwrap();
    let (writer, mut reader) = UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let (tx, rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel();
    let mut display = Display::<Server>::new().unwrap();
    let mut handle = display.handle();
    handle.create_global::<Server, compositor::WlCompositor, _>(6, ());
    handle.create_global::<Server, seat::WlSeat, _>(5, ());
    handle.create_global::<Server, manager::WlDataDeviceManager, _>(3, ());
    handle
        .insert_client(server_socket, std::sync::Arc::new(()))
        .unwrap();
    let server = std::thread::spawn(move || {
        let mut state = Server {
            pointer: None,
            origin: None,
            offered: vec![],
            actions: None,
            result: tx,
            writer: Some(writer),
            requested_mime,
            outcome,
        };
        while matches!(stop_rx.try_recv(), Err(mpsc::TryRecvError::Empty)) {
            display.dispatch_clients(&mut state).unwrap();
            display.flush_clients().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let connection = wayland_client::Connection::from_socket(client_socket).unwrap();
    let mut native = NativeDrag::new(connection.clone()).unwrap();
    let mut queue = connection.new_event_queue::<SurfaceClient>();
    let compositor: wayland_client::protocol::wl_compositor::WlCompositor =
        native._registry.bind(1, 6, &queue.handle(), ());
    let surface = compositor.create_surface(&queue.handle(), ());
    native.prepare(payload);
    surface.commit();
    for _ in 0..6 {
        queue.roundtrip(&mut SurfaceClient).unwrap();
        native.pump().unwrap();
    }
    if outcome == Outcome::QuickRelease {
        assert!(
            !native.dragged(),
            "released input must not start a late drag"
        );
        assert!(rx.try_recv().is_err());
        stop_tx.send(()).unwrap();
        server.join().unwrap();
        return String::new();
    }
    let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        request,
        (BUTTON_SERIAL, true, offered, Some(manager::DndAction::Copy))
    );
    let mut received = String::new();
    reader.read_to_string(&mut received).unwrap();
    assert!(native.dragged());
    assert!(!native.active());
    native.prepare(Payload::text(b"next click".to_vec()).unwrap());
    assert!(
        !native.dragged(),
        "a completed drag must not suppress the next click"
    );
    stop_tx.send(()).unwrap();
    server.join().unwrap();
    received
}

#[test]
fn native_text_drag_serves_original_content() {
    assert_eq!(
        transfer(
            Payload::text("hello 世界".as_bytes().to_vec()).unwrap(),
            "text/plain;charset=utf-8",
            Outcome::Finish
        ),
        "hello 世界"
    );
}

#[test]
fn cancelled_native_drag_releases_source_without_sending_data() {
    assert_eq!(
        transfer(
            Payload::text(b"private".to_vec()).unwrap(),
            "text/plain",
            Outcome::Cancel
        ),
        ""
    );
}

#[test]
fn releasing_before_the_ui_tick_does_not_start_a_late_drag() {
    transfer(
        Payload::text(b"quick drag".to_vec()).unwrap(),
        "text/plain",
        Outcome::QuickRelease,
    );
}
