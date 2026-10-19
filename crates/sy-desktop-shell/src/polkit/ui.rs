use super::{
    PamEvent,
    backend::{self, Control, Event},
    guidance,
};
use anyhow::{Context, Result};
use iced::{
    Element, Length, Subscription, Task, Theme,
    widget::{button, column, container, row, text, text_input},
    window,
};
use std::{
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

const PHONE_GUIDANCE: &str = "Approve the request on your phone. If it is unavailable, wait for your security key or password prompt.";
struct Dialog {
    events: Arc<Mutex<mpsc::Receiver<Event>>>,
    window: Option<window::Id>,
    cookie: String,
    action: String,
    message: String,
    user: String,
    instruction: String,
    detail: String,
    prompt: Option<(String, bool)>,
    response: String,
    control: Option<tokio::sync::mpsc::UnboundedSender<Control>>,
    preview: bool,
}
#[derive(Clone)]
enum Msg {
    Tick,
    Input(String),
    Submit,
    Cancel,
    Close(window::Id),
    Opened,
}
impl std::fmt::Debug for Msg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Tick => "Tick",
            Self::Input(_) => "Input(redacted)",
            Self::Submit => "Submit",
            Self::Cancel => "Cancel",
            Self::Close(_) => "Close",
            Self::Opened => "Opened",
        })
    }
}

pub(super) fn run(preview: bool) -> Result<()> {
    let (events, receiver) = mpsc::channel();
    let runtime = tokio::runtime::Runtime::new()?;
    let connection = if preview {
        None
    } else {
        Some(runtime.block_on(backend::register(events))?)
    };
    let receiver = Arc::new(Mutex::new(receiver));
    let palette = sy_theme::theme::load().unwrap_or_default();
    let theme = Theme::custom(
        "sy",
        iced::theme::Palette {
            background: palette.bg,
            text: palette.fg,
            primary: palette.accent,
            success: palette.green,
            danger: palette.red,
            warning: palette.orange,
        },
    );
    iced::daemon(
        move || {
            let mut dialog = Dialog {
                events: receiver.clone(),
                window: None,
                cookie: String::new(),
                action: "org.freedesktop.policykit.exec".into(),
                message: "Authentication is required to run an administrative action".into(),
                user: std::env::var("USER").unwrap_or_default(),
                instruction: PHONE_GUIDANCE.into(),
                detail: String::new(),
                prompt: None,
                response: String::new(),
                control: None,
                preview,
            };
            let task = if preview { dialog.open() } else { Task::none() };
            (dialog, task)
        },
        update,
        view,
    )
    .title(|_: &Dialog, _| "sy · Authorize".to_string())
    .theme(move |_: &Dialog, _| theme.clone())
    .subscription(|_: &Dialog| {
        Subscription::batch([
            iced::time::every(Duration::from_millis(30)).map(|_| Msg::Tick),
            window::close_requests().map(Msg::Close),
        ])
    })
    .run()
    .context("Run authentication dialog")?;
    drop(connection);
    drop(runtime);
    Ok(())
}

impl Dialog {
    fn open(&mut self) -> Task<Msg> {
        let (id, task) = window::open(window::Settings {
            size: iced::Size::new(500.0, 390.0),
            position: window::Position::Centered,
            resizable: false,
            exit_on_close_request: false,
            platform_specific: window::settings::PlatformSpecific {
                application_id: "sy-polkit".into(),
                ..Default::default()
            },
            ..Default::default()
        });
        self.window = Some(id);
        task.map(|_| Msg::Opened)
    }
    fn cancel(&mut self) -> Task<Msg> {
        self.response.clear();
        if let Some(control) = self.control.take() {
            let _ = control.send(Control::Cancel);
        }
        if self.preview {
            return iced::exit();
        }
        self.window
            .take()
            .map(window::close)
            .unwrap_or_else(Task::none)
    }
}

fn update(state: &mut Dialog, message: Msg) -> Task<Msg> {
    match message {
        Msg::Tick => {
            let events = state
                .events
                .lock()
                .map(|rx| rx.try_iter().collect::<Vec<_>>())
                .unwrap_or_default();
            let mut tasks = Vec::new();
            for event in events {
                match event {
                    Event::Started {
                        cookie,
                        action,
                        message,
                        user,
                        control,
                    } => {
                        state.cookie = cookie;
                        state.action = action;
                        state.message = message;
                        state.user = user;
                        state.instruction = PHONE_GUIDANCE.into();
                        state.detail.clear();
                        state.prompt = None;
                        state.response.clear();
                        state.control = Some(control);
                        if state.window.is_none() {
                            tasks.push(state.open());
                        }
                    }
                    Event::Pam { cookie, event } if cookie == state.cookie => {
                        state.instruction = guidance(&event).into();
                        match event {
                            PamEvent::Prompt { text, echo } => {
                                state.prompt = Some((text, echo));
                                tasks.push(iced::widget::operation::focus(
                                    "authentication-response",
                                ));
                            }
                            PamEvent::Info(text) | PamEvent::Error(text) => state.detail = text,
                            PamEvent::Complete(_) => {}
                        }
                    }
                    Event::Finished { cookie, error } if cookie == state.cookie => {
                        state.response.clear();
                        state.prompt = None;
                        state.control = None;
                        if let Some(error) = error {
                            state.instruction = "Authentication did not complete.".into();
                            state.detail = error;
                        } else if let Some(window) = state.window.take() {
                            tasks.push(window::close(window));
                        }
                    }
                    _ => {}
                }
            }
            return Task::batch(tasks);
        }
        Msg::Input(value) => state.response = value,
        Msg::Submit if state.prompt.is_some() => {
            if let Some(control) = &state.control {
                let _ = control.send(Control::Response(std::mem::take(&mut state.response)));
                state.prompt = None;
                state.instruction = "Checking your response…".into();
            }
        }
        Msg::Cancel => return state.cancel(),
        Msg::Close(id) if state.window == Some(id) => return state.cancel(),
        _ => {}
    }
    Task::none()
}

fn view(state: &Dialog, _: window::Id) -> Element<'_, Msg> {
    let mut body = column![
        text("Authorization required").size(25),
        text(&state.message).size(17),
        text(format!("Account: {}", state.user)).size(14),
        text(&state.action).size(12),
        text(&state.instruction).size(17),
        text(&state.detail).size(14)
    ]
    .spacing(14);
    if let Some((prompt, echo)) = &state.prompt {
        body = body.push(
            text_input(prompt, &state.response)
                .id("authentication-response")
                .secure(!*echo)
                .on_input(Msg::Input)
                .on_submit(Msg::Submit),
        );
    }
    let submit = if state.prompt.is_some() {
        button("Authenticate").on_press(Msg::Submit)
    } else {
        button("Authenticate")
    };
    body = body.push(row![button("Cancel").on_press(Msg::Cancel), submit].spacing(12));
    container(body)
        .padding(26)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
