#!/usr/bin/env python3
"""Exercise the real panel in disposable Niri, Xvfb and systemd user units."""

import argparse
import json
import os
from pathlib import Path
import re
import select
import subprocess
import tempfile
import time

REPO = Path(__file__).resolve().parent.parent
PANEL_WIDTH = 28
TOP_HEIGHT = 24


def wait_for(check):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(0.05)
    raise AssertionError("timed out waiting for desktop geometry")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=REPO / "target/debug/sy-desktop-shell")
    parser.add_argument("--scale", type=int, choices=(1, 2), default=2)
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix="sy-panel-layout-"))
    print(f"Logs: {root}", flush=True)
    env = os.environ.copy()
    for key in ("WAYLAND_DISPLAY", "WAYLAND_SOCKET", "NIRI_SOCKET", "NOTIFY_SOCKET",
                "WATCHDOG_USEC", "WATCHDOG_PID"):
        env.pop(key, None)
    for suffix in ("RUNTIME_DIR", "CONFIG_HOME", "STATE_HOME", "DATA_HOME", "CACHE_HOME"):
        path = root / suffix.lower()
        path.mkdir(mode=0o700)
        env[f"XDG_{suffix}"] = str(path)
    env["GDK_BACKEND"] = "wayland"
    children, units = [], []
    prefix = f"sy-panel-layout-{os.getpid()}"
    waybar_unit, panel_unit = f"{prefix}-waybar", f"{prefix}-stack"

    def spawn(command, **kwargs):
        with (root / f"{Path(command[0]).name}.log").open("a") as log:
            process = subprocess.Popen(command, env=env, stdout=log, stderr=log, **kwargs)
        children.append(process)
        return process

    def msg(*command):
        return json.loads(subprocess.check_output(
            ["niri", "msg", "--json", *command], env=env, timeout=5))

    def action(name, window_id):
        subprocess.run(["niri", "msg", "action", name, "--id", str(window_id)],
                       env=env, check=True, timeout=5, stdout=subprocess.DEVNULL)

    def start_unit(name, command, properties):
        units.append(name)
        options = ["systemd-run", "--user", "--quiet", "--collect", "--no-block", f"--unit={name}"]
        bus = os.environ.get("DBUS_SESSION_BUS_ADDRESS", f"unix:path=/run/user/{os.getuid()}/bus")
        options.append(f"--setenv=DBUS_SESSION_BUS_ADDRESS={bus}")
        for key, value in env.items():
            if key.startswith("XDG_") or key in ("DISPLAY", "WAYLAND_DISPLAY", "NIRI_SOCKET", "GDK_BACKEND"):
                options.append(f"--setenv={key}={value}")
        properties += [f"StandardOutput=append:{root / (name + '.log')}", "StandardError=inherit"]
        subprocess.run(options + [f"--property={p}" for p in properties] + command,
                       check=True, timeout=20)

    def panel_present():
        return any(layer["namespace"] == "sy-stack" for layer in msg("layers"))

    try:
        read_fd, write_fd = os.pipe()
        try:
            spawn(["Xvfb", "-displayfd", str(write_fd), "-screen", "0", "1920x1200x24",
                   "-nolisten", "tcp"], pass_fds=(write_fd,))
            os.close(write_fd)
            if not select.select([read_fd], [], [], 15)[0]:
                raise AssertionError("Xvfb did not start")
            display = os.read(read_fd, 100).decode().strip()
            assert display, "Xvfb failed to allocate a display"
            env["DISPLAY"] = f":{display}"
        finally:
            os.close(read_fd)
        template = (REPO / "configs/niri/config.kdl").read_text()
        struts = re.search(r"struts\s*\{([^}]+)\}", template).group(1)
        config = root / "niri.kdl"
        config.write_text(f"layout {{ gaps 0; struts {{ {struts} }}; }}\n"
                          "hotkey-overlay { skip-at-startup; }\n"
                          f'output "winit" {{ scale {args.scale}; }}\n')
        spawn(["niri", "-c", str(config)])
        runtime = Path(env["XDG_RUNTIME_DIR"])
        # systemctl uses the user manager's private socket under XDG_RUNTIME_DIR.
        (runtime / "systemd").symlink_to(Path(os.environ["XDG_RUNTIME_DIR"]) / "systemd")
        socket = wait_for(lambda: next(runtime.glob("niri.*.sock"), None))
        env["NIRI_SOCKET"] = str(socket)
        env["WAYLAND_DISPLAY"] = socket.name.split(".")[1]
        width = next(iter(msg("outputs").values()))["logical"]["width"]
        css = root / "waybar.css"
        css.write_text("* { font-size: 10px; min-height: 0; }")
        config = root / "waybar.json"
        config.write_text(json.dumps({"name": "top", "layer": "top", "position": "top",
                                      "height": TOP_HEIGHT, "modules-left": ["clock"]}))
        waybar_source = (REPO / "configs/systemd/user/sy-waybar.service").read_text()
        post = next(line.split("=", 1)[1] for line in waybar_source.splitlines()
                    if line.startswith("ExecStartPost="))
        post = post.replace("sy-stack-bar.service", panel_unit + ".service")
        panel_source = (REPO / "configs/systemd/user/sy-stack-bar.service").read_text()
        pre = next(line.split("=", 1)[1] for line in panel_source.splitlines()
                   if line.startswith("ExecStartPre="))
        start_unit(panel_unit, [str(args.binary.resolve()), "stack", "bar"],
                   [f"ExecStartPre={pre}", f"After={waybar_unit}.service", "TimeoutStartSec=15s", "Type=notify"])
        start_unit(waybar_unit, ["/usr/bin/waybar", "-c", str(config), "-s", str(css)],
                   [f"ExecStartPost={post}"])
        wait_for(panel_present)
        wait_for(lambda: subprocess.run(["systemctl", "--user", "is-active", "--quiet", panel_unit]).returncode == 0)
        spawn(["foot", "--app-id=sy-panel-test", "sleep", "600"])
        window = wait_for(lambda: next((w for w in msg("windows")
                                       if w["app_id"] == "sy-panel-test"), None))
        window_id = window["id"]

        def window_layout():
            return next(w for w in msg("windows") if w["id"] == window_id)["layout"]

        def window_width():
            return window_layout()["window_size"][0]

        action("focus-window", window_id)
        subprocess.run(["niri", "msg", "action", "maximize-column"], env=env,
                       check=True, timeout=5, stdout=subprocess.DEVNULL)
        # Foot may round a regular tiled window down to whole terminal cells.
        wait_for(lambda: (width - PANEL_WIDTH) / 2 < window_layout()["tile_size"][0]
                 <= width - PANEL_WIDTH)
        action("maximize-window-to-edges", window_id)
        wait_for(lambda: window_width() == width - PANEL_WIDTH)
        print(f"Maximized: {width - PANEL_WIDTH}/{width} pixels", flush=True)
        for expected in (width, width - PANEL_WIDTH):
            subprocess.run([str(args.binary.resolve()), "stack", "toggle"],
                           env=env, check=True, timeout=5, stdout=subprocess.DEVNULL)
            wait_for(lambda: window_width() == expected)
        print("Hide/show: reservation released and restored", flush=True)
        action("fullscreen-window", window_id)
        wait_for(lambda: window_width() == width)
        action("fullscreen-window", window_id)
        wait_for(lambda: window_width() == width - PANEL_WIDTH)
        def panel_pid():
            return subprocess.check_output(
                ["systemctl", "--user", "show", "-p", "MainPID", "--value", panel_unit],
                text=True, timeout=5).strip()

        for _ in range(2):
            previous_pid = panel_pid()
            log = root / f"{waybar_unit}.log"
            log_offset = log.stat().st_size
            subprocess.run(["systemctl", "--user", "restart", waybar_unit], check=True, timeout=20)
            wait_for(lambda: panel_pid() not in ("0", previous_pid))
            wait_for(panel_present)
            wait_for(lambda: window_width() == width - PANEL_WIDTH)
            wait_for(lambda: re.findall(r"Bar configured \(width: (\d+), height: (\d+)\)",
                                       log.read_text()[log_offset:])[-1:]
                     == [(str(width), str(TOP_HEIGHT))])
        print("Fullscreen and repeated Waybar restart: passed", flush=True)
    finally:
        for unit in reversed(units):
            subprocess.run(["systemctl", "--user", "stop", unit], check=False, timeout=20,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for child in reversed(children):
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()


if __name__ == "__main__":
    main()
