use super::{PamEvent, parse_line};
use anyhow::{Context, Result, bail};
use std::{
    collections::HashMap,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex, mpsc},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::mpsc as channel,
};
use zbus::{Connection, message::Header, zvariant::OwnedValue};

const OBJECT_PATH: &str = "/org/sy/PolicyKit1/AuthenticationAgent";
const AUTHORITY: &str = "org.freedesktop.PolicyKit1";
const HELPER: &str = "/usr/lib/polkit-1/polkit-agent-helper-1";
pub(super) enum Control {
    Response(String),
    Cancel,
}
impl std::fmt::Debug for Control {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Response(_) => "Response(redacted)",
            Self::Cancel => "Cancel",
        })
    }
}
pub(super) enum Event {
    Started {
        cookie: String,
        action: String,
        message: String,
        user: String,
        control: channel::UnboundedSender<Control>,
    },
    Pam {
        cookie: String,
        event: PamEvent,
    },
    Finished {
        cookie: String,
        error: Option<String>,
    },
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.PolicyKit1.Error")]
enum AgentError {
    Failed(String),
    Cancelled(String),
}

type Active = Arc<Mutex<HashMap<String, channel::UnboundedSender<Control>>>>;
#[derive(Clone)]
struct Agent {
    runtime: tokio::runtime::Handle,
    connection: Connection,
    events: mpsc::Sender<Event>,
    active: Active,
    helper_path: std::path::PathBuf,
}
struct RequestGuard {
    cookie: String,
    active: Active,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(&self.cookie);
        }
    }
}

impl Agent {
    async fn verify_authority(&self, header: &Header<'_>) -> Result<(), AgentError> {
        let proxy = zbus::fdo::DBusProxy::new(&self.connection)
            .await
            .map_err(|e| AgentError::Failed(e.to_string()))?;
        let owner = proxy
            .get_name_owner(
                AUTHORITY
                    .try_into()
                    .map_err(|e: zbus::names::Error| AgentError::Failed(e.to_string()))?,
            )
            .await
            .map_err(|e| AgentError::Failed(e.to_string()))?;
        if header.sender().map(|s| s.as_str()) != Some(owner.as_str()) {
            return Err(AgentError::Failed(
                "Only the PolicyKit authority may request authentication".into(),
            ));
        }
        Ok(())
    }
}

#[zbus::interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl Agent {
    #[expect(
        clippy::too_many_arguments,
        reason = "PolicyKit specifies six input arguments plus the authenticated D-Bus header"
    )]
    async fn begin_authentication(
        &self,
        action_id: &str,
        message: &str,
        _icon_name: &str,
        _details: HashMap<String, String>,
        cookie: &str,
        identities: Vec<(String, HashMap<String, OwnedValue>)>,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), AgentError> {
        self.verify_authority(&header).await?;
        let agent = self.clone();
        let action_id = action_id.to_owned();
        let message = message.to_owned();
        let cookie = cookie.to_owned();
        self.runtime
            .spawn(async move {
                agent
                    .authenticate(&action_id, &message, &cookie, identities)
                    .await
            })
            .await
            .map_err(|error| AgentError::Failed(error.to_string()))?
    }

    async fn cancel_authentication(
        &self,
        cookie: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), AgentError> {
        self.verify_authority(&header).await?;
        if let Ok(active) = self.active.lock()
            && let Some(control) = active.get(cookie)
        {
            let _ = control.send(Control::Cancel);
        }
        Ok(())
    }
}

impl Agent {
    async fn authenticate(
        &self,
        action_id: &str,
        message: &str,
        cookie: &str,
        identities: Vec<(String, HashMap<String, OwnedValue>)>,
    ) -> Result<(), AgentError> {
        let mut users = identities
            .iter()
            .filter(|(kind, _)| kind == "unix-user")
            .filter_map(|(_, fields)| fields.get("uid").and_then(|v| u32::try_from(v).ok()))
            .collect::<Vec<_>>();
        let current_uid = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines().find_map(|l| {
                    l.strip_prefix("Uid:")
                        .and_then(|u| u.split_whitespace().next()?.parse::<u32>().ok())
                })
            });
        users.sort_by_key(|uid| Some(*uid) != current_uid);
        let uid = users
            .first()
            .ok_or_else(|| AgentError::Failed("No supported user identity was offered".into()))?;
        let lookup = Command::new("getent")
            .args(["passwd", &uid.to_string()])
            .output()
            .await
            .map_err(|e| AgentError::Failed(e.to_string()))?;
        let user = String::from_utf8_lossy(&lookup.stdout)
            .split(':')
            .next()
            .unwrap_or("")
            .trim()
            .to_owned();
        if !lookup.status.success() || user.is_empty() || user.contains(['\n', '\r']) {
            return Err(AgentError::Failed(
                "Cannot resolve authentication account".into(),
            ));
        }
        let (control, commands) = channel::unbounded_channel();
        {
            let mut active = self
                .active
                .lock()
                .map_err(|_| AgentError::Failed("Authentication state unavailable".into()))?;
            if !active.is_empty() {
                return Err(AgentError::Failed(
                    "Another authentication request is already open".into(),
                ));
            }
            active.insert(cookie.into(), control.clone());
        }
        let _guard = RequestGuard {
            cookie: cookie.into(),
            active: self.active.clone(),
        };
        self.events
            .send(Event::Started {
                cookie: cookie.into(),
                action: action_id.into(),
                message: message.into(),
                user: user.clone(),
                control,
            })
            .map_err(|_| AgentError::Cancelled("Authentication dialog closed".into()))?;
        let result = helper(&self.helper_path, &user, cookie, &self.events, commands).await;
        let error = match &result {
            Ok(true) => None,
            Ok(false) => Some("Authentication failed. Try the action again.".into()),
            Err(error) => Some(error.to_string()),
        };
        let _ = self.events.send(Event::Finished {
            cookie: cookie.into(),
            error,
        });
        match result {
            Ok(true) => Ok(()),
            Ok(false) => Err(AgentError::Failed("Authentication failed".into())),
            Err(error) => Err(AgentError::Cancelled(error.to_string())),
        }
    }
}

/// Register before starting the GUI so an existing agent remains undisturbed on failure.
pub(super) async fn register(events: mpsc::Sender<Event>) -> Result<Connection> {
    let connection = Connection::system().await?;
    let agent = Agent {
        runtime: tokio::runtime::Handle::current(),
        connection: connection.clone(),
        events,
        active: Arc::default(),
        helper_path: HELPER.into(),
    };
    connection.object_server().at(OBJECT_PATH, agent).await?;
    let session_id = std::env::var("XDG_SESSION_ID")
        .context("XDG_SESSION_ID is missing; start the agent inside the graphical session")?;
    let subject = (
        "unix-session",
        HashMap::from([(
            "session-id",
            OwnedValue::from(zbus::zvariant::Str::from(session_id.as_str())),
        )]),
    );
    let locale = std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into());
    let proxy = zbus::Proxy::new(
        &connection,
        AUTHORITY,
        "/org/freedesktop/PolicyKit1/Authority",
        "org.freedesktop.PolicyKit1.Authority",
    )
    .await?;
    proxy
        .call::<_, _, ()>(
            "RegisterAuthenticationAgent",
            &(subject, locale, OBJECT_PATH),
        )
        .await
        .context("Register sy PolicyKit agent (another agent may already be running)")?;
    Ok(connection)
}

pub(super) async fn helper(
    path: &Path,
    user: &str,
    cookie: &str,
    events: &mpsc::Sender<Event>,
    mut controls: channel::UnboundedReceiver<Control>,
) -> Result<bool> {
    let mut child = Command::new(path)
        .arg(user)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("Start PolicyKit PAM helper")?;
    let result = async {
        let mut input = child.stdin.take().context("Helper stdin unavailable")?;
        input.write_all(format!("{cookie}\n").as_bytes()).await?;
        let mut output =
            BufReader::new(child.stdout.take().context("Helper stdout unavailable")?).lines();
        let mut awaiting_response = false;
        loop {
            tokio::select! {
                command = controls.recv() => match command {
                    Some(Control::Response(response)) if awaiting_response => {
                        if response.contains(['\n', '\r', '\0']) { bail!("Invalid authentication response"); }
                        input.write_all(response.as_bytes()).await?;
                        input.write_all(b"\n").await?;
                        awaiting_response = false;
                    }
                    Some(Control::Response(_)) => {},
                    _ => bail!("Authentication cancelled"),
                },
                line = output.next_line() => {
                    let event = parse_line(&line?.context("PolicyKit helper closed without a result")?)?;
                    if let PamEvent::Complete(success) = event { return Ok(success); }
                    if matches!(event, PamEvent::Prompt { .. }) { awaiting_response = true; }
                    events.send(Event::Pam { cookie: cookie.into(), event }).context("Authentication dialog closed")?;
                }
            }
        }
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(120), result)
        .await
        .context("Authentication timed out")
        .and_then(|r| r);
    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Bus(std::process::Child);
    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dbus_agent_rejects_forged_authority_and_authenticates_an_offered_identity() {
        use std::io::BufRead;
        let mut bus = Bus(std::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap());
        let mut address = String::new();
        std::io::BufReader::new(bus.0.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let authority = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .name(AUTHORITY)
            .unwrap()
            .build()
            .await
            .unwrap();
        let connection = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .build()
            .await
            .unwrap();
        let file =
            fixture("read -r cookie\n[ \"$cookie\" = valid-cookie ] || exit 1\necho SUCCESS");
        let (events, receiver) = mpsc::channel();
        connection
            .object_server()
            .at(
                OBJECT_PATH,
                Agent {
                    runtime: tokio::runtime::Handle::current(),
                    connection: connection.clone(),
                    events,
                    active: Arc::default(),
                    helper_path: file.to_path_buf(),
                },
            )
            .await
            .unwrap();
        let uid: u32 = String::from_utf8(
            std::process::Command::new("id")
                .arg("-u")
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .parse()
        .unwrap();
        let body = (
            "test.action",
            "Test authorization",
            "",
            HashMap::<String, String>::new(),
            "valid-cookie",
            vec![("unix-user", HashMap::from([("uid", OwnedValue::from(uid))]))],
        );
        let outsider = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .build()
            .await
            .unwrap();
        let forged = zbus::Proxy::new(
            &outsider,
            connection.unique_name().unwrap().to_string(),
            OBJECT_PATH,
            "org.freedesktop.PolicyKit1.AuthenticationAgent",
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                forged.call::<_, _, ()>("BeginAuthentication", &body)
            )
            .await
            .unwrap()
            .is_err()
        );
        let genuine = zbus::Proxy::new(
            &authority,
            connection.unique_name().unwrap().to_string(),
            OBJECT_PATH,
            "org.freedesktop.PolicyKit1.AuthenticationAgent",
        )
        .await
        .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            genuine.call::<_, _, ()>("BeginAuthentication", &body),
        )
        .await
        .unwrap()
        .unwrap();
        std::fs::write(&file, "#!/bin/sh\nread -r cookie\necho 'PAM_PROMPT_ECHO_OFF Password:'\nread -r response\necho SUCCESS\n").unwrap();
        let waiting_proxy = genuine.clone();
        let pending = tokio::spawn(async move {
            waiting_proxy
                .call::<_, _, ()>("BeginAuthentication", &body)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if receiver
                    .try_iter()
                    .any(|event| matches!(event, Event::Pam { .. }))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            genuine
                .call::<_, _, ()>(
                    "BeginAuthentication",
                    &(
                        "another.action",
                        "Concurrent",
                        "",
                        HashMap::<String, String>::new(),
                        "other-cookie",
                        vec![("unix-user", HashMap::from([("uid", OwnedValue::from(uid))]))]
                    )
                )
                .await
                .is_err()
        );
        genuine
            .call::<_, _, ()>("CancelAuthentication", &("valid-cookie",))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), pending)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
    }

    #[tokio::test]
    async fn informational_message_does_not_discard_the_pending_password_prompt() {
        let file = fixture(
            "read -r cookie\necho 'PAM_PROMPT_ECHO_OFF Password:'\necho 'PAM_TEXT_INFO Waiting'\nread -r response\n[ \"$response\" = correct ] || exit 1\necho SUCCESS",
        );
        let (events, received) = mpsc::channel();
        let (control, commands) = channel::unbounded_channel();
        let responder = tokio::task::spawn_blocking(move || {
            while let Ok(event) = received.recv() {
                if matches!(
                    event,
                    Event::Pam {
                        event: PamEvent::Info(_),
                        ..
                    }
                ) {
                    control.send(Control::Response("correct".into())).unwrap();
                    break;
                }
            }
            control
        });
        assert!(
            helper(&file, "user", "cookie", &events, commands)
                .await
                .unwrap()
        );
        responder.await.unwrap();
    }

    fn fixture(body: &str) -> tempfile::TempPath {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        file.into_temp_path()
    }

    #[tokio::test]
    async fn helper_receives_cookie_over_stdin_and_authenticates() {
        let file =
            fixture("read -r cookie\n[ \"$cookie\" = secret-cookie ] || exit 1\necho SUCCESS");
        let (events, _) = mpsc::channel();
        let (_control, commands) = channel::unbounded_channel();
        assert!(
            helper(&file, "user", "secret-cookie", &events, commands)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn cancellation_terminates_a_waiting_helper() {
        let file = fixture("read -r cookie\nread -r response");
        let (events, _) = mpsc::channel();
        let (control, commands) = channel::unbounded_channel();
        control.send(Control::Cancel).unwrap();
        let result = helper(&file, "user", "cookie", &events, commands).await;
        assert!(result.unwrap_err().to_string().contains("cancelled"));
    }
}
