%global debug_package %{nil}

Name:           sy
Version:        0.1.0
Release:        16%{?dist}
Summary:        Desktop environment and agent tools for Fedora
License:        MIT
URL:            https://github.com/dmytrogajewski/sy
Source0:        %{name}-%{version}.tar.gz
BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
BuildRequires:  gcc-c++
BuildRequires:  cmake
BuildRequires:  pkgconfig
BuildRequires:  pkgconfig(alsa)
BuildRequires:  pkgconfig(openssl)
BuildRequires:  pkgconfig(libudev)
BuildRequires:  pkgconfig(xkbcommon)
%description
Small sy command launcher. Install feature RPMs for desktop and AI commands.

%package desktop
Summary:        Declarative niri desktop configuration for sy
Requires:       sy = %{version}-%{release}
Requires:       sy-desktop-shell = %{version}-%{release}
Requires:       fuzzel
Requires:       foot
Requires:       niri
Requires:       swaylock
Requires:       waybar
Requires:       python3

%description desktop
Desktop executable, configuration, themes, and user units. Install feature RPMs
separately for power, agents, AI workloads, and file management.

%package system-power
Summary:        Optional session power menu for sy
Requires:       sy = %{version}-%{release}
Requires:       fuzzel
Requires:       swaylock

%description system-power
Standalone lock, logout, suspend, reboot, and power-off commands.

%package system-agent
Summary:        Optional agent runner and policy controls for sy
Requires:       sy = %{version}-%{release}
Requires:       fuzzel
Requires:       foot

%description system-agent
Standalone ACP agent daemon, session CLI, sandbox policy, and user unit.

%package system-auth
Summary:        Optional phone-as-key authentication controls for sy
Requires:       sy = %{version}-%{release}

%description system-auth
Standalone phone-as-key CLI and SELinux policy sources. The external syauth
daemon and PAM module are installed separately.

%package system-aiplane
Summary:        Optional AI workloads and inference CLI for sy
Requires:       sy = %{version}-%{release}

%description system-aiplane
Standalone AI workload CLI with CPU and optional accelerator execution.

%package system-aiplane-ryzenai
Summary:        AMD Ryzen AI host integration for sy
Requires:       sy-system-aiplane = %{version}-%{release}
Requires:       dracut
Requires:       python3

%description system-aiplane-ryzenai
NPU boot integration and post-upgrade verification for a separately installed
AMD Ryzen AI runtime. The proprietary runtime and model files are external.

%package system-knowledge
Summary:        Optional semantic knowledge plane for sy
Requires:       sy = %{version}-%{release}

%description system-knowledge
Standalone knowledge daemon, search CLI, local Qdrant integration, and user units.

%package system-monitor
Summary:        Optional system telemetry dashboard for sy
Requires:       sy = %{version}-%{release}

%description system-monitor
Standalone system metrics collector, monitor CLI, MCP server, and user unit.

%package desktop-shell
Summary:        Optional desktop shell applets for sy
Requires:       sy = %{version}-%{release}
Requires:       fuzzel
Requires:       foot
Requires:       waybar

%description desktop-shell
Standalone desktop bar, applets, and shell CLI.

%package filemanager
Summary:        Optional file manager and plugin host for sy
Requires:       sy = %{version}-%{release}

%description filemanager
Native file manager, IPC daemon, plugin host, Markdown previewer, and user units.

%prep
%autosetup

%build
cargo build --release --locked --workspace --bins
cp target/release/sy target/release/sy-all

%check
SY_FEATURE_BIN_DIR=/nonexistent target/release/sy-launcher --help | grep -q 'Optional packages'
SY_FEATURE_BIN_DIR=target/release target/release/sy-launcher --help | grep -q 'apply'
SY_FEATURE_BIN_DIR=target/release target/release/sy-launcher --help | grep -q '^Usage: sy '
SY_FEATURE_BIN_DIR=target/release target/release/sy-launcher power reboot --dry-run --json | grep -q '"dry_run":true'
target/release/sy-system-power reboot --dry-run --json | grep -q '"dry_run":true'
SY_FEATURE_BIN_DIR=target/release target/release/sy-launcher agt --help | grep -q 'long-lived daemon'
SY_ROOT="$PWD" SY_FEATURE_BIN_DIR=target/release target/release/sy-launcher policy lint --json | grep -q '"summary"'
SY_FEATURE_BIN_DIR=target/release target/release/sy-launcher aiplane list --json | grep -q 'embed'
target/release/sy-system-aiplane aiplane daemon --help | grep -q 'standalone'
SY_FEATURE_BIN_DIR=target/release target/release/sy-launcher knowledge --help | grep -q 'daemon'
SY_FEATURE_BIN_DIR=target/release target/release/sy-launcher mon --help | grep -q 'collect'
SY_FEATURE_BIN_DIR=target/release target/release/sy-launcher stack --help | grep -q 'bar'
SY_FEATURE_BIN_DIR=target/release target/release/sy-launcher syauth --help | grep -q 'install-pam'
SY_FEATURE_BIN_DIR=target/release target/release/sy-launcher file --help | grep -q 'doctor'

%install
install -D -m 0755 target/release/sy-launcher %{buildroot}%{_bindir}/sy
strip --strip-unneeded %{buildroot}%{_bindir}/sy
install -D -m 0644 configs/systemd/user/sy.target %{buildroot}%{_userunitdir}/sy.target
install -D -m 0755 target/release/sy-all %{buildroot}%{_libexecdir}/sy/sy-all
strip --strip-unneeded %{buildroot}%{_libexecdir}/sy/sy-all
install -D -m 0755 target/release/sy-system-power %{buildroot}%{_libexecdir}/sy/sy-system-power
strip --strip-unneeded %{buildroot}%{_libexecdir}/sy/sy-system-power
install -D -m 0755 target/release/sy-system-agent %{buildroot}%{_libexecdir}/sy/sy-system-agent
strip --strip-unneeded %{buildroot}%{_libexecdir}/sy/sy-system-agent
install -D -m 0755 target/release/sy-system-auth %{buildroot}%{_libexecdir}/sy/sy-system-auth
strip --strip-unneeded %{buildroot}%{_libexecdir}/sy/sy-system-auth
install -D -m 0755 target/release/sy-system-aiplane %{buildroot}%{_libexecdir}/sy/sy-system-aiplane
strip --strip-unneeded %{buildroot}%{_libexecdir}/sy/sy-system-aiplane
install -D -m 0755 target/release/sy-system-knowledge %{buildroot}%{_libexecdir}/sy/sy-system-knowledge
strip --strip-unneeded %{buildroot}%{_libexecdir}/sy/sy-system-knowledge
install -D -m 0755 target/release/sy-system-monitor %{buildroot}%{_libexecdir}/sy/sy-system-monitor
strip --strip-unneeded %{buildroot}%{_libexecdir}/sy/sy-system-monitor
install -D -m 0755 target/release/sy-desktop-shell %{buildroot}%{_libexecdir}/sy/sy-desktop-shell
strip --strip-unneeded %{buildroot}%{_libexecdir}/sy/sy-desktop-shell
install -D -m 0755 target/release/sy-filemanager %{buildroot}%{_libexecdir}/sy/sy-filemanager
strip --strip-unneeded %{buildroot}%{_libexecdir}/sy/sy-filemanager
install -D -m 0755 target/release/sy-plugin-md %{buildroot}%{_libexecdir}/sy/sy-plugin-md
strip --strip-unneeded %{buildroot}%{_libexecdir}/sy/sy-plugin-md
install -D -m 0644 configs/systemd/user/sy-agentd.service %{buildroot}%{_userunitdir}/sy-agentd.service
sed -i 's|%h/.local/bin/sy |/usr/bin/sy |g' %{buildroot}%{_userunitdir}/sy-agentd.service
install -D -m 0644 configs/systemd/user/sy-aiplane.service %{buildroot}%{_userunitdir}/sy-aiplane.service
sed -i 's|%h/.local/bin/sy |/usr/bin/sy |g' %{buildroot}%{_userunitdir}/sy-aiplane.service
install -D -m 0644 configs/systemd/user/sy-knowledge.service %{buildroot}%{_userunitdir}/sy-knowledge.service
install -D -m 0644 configs/systemd/user/sy-knowledge.socket %{buildroot}%{_userunitdir}/sy-knowledge.socket
install -D -m 0644 configs/systemd/user/sy-qdrant.service %{buildroot}%{_userunitdir}/sy-qdrant.service
sed -i 's|%h/.local/bin/sy |/usr/bin/sy |g' %{buildroot}%{_userunitdir}/sy-knowledge.service %{buildroot}%{_userunitdir}/sy-qdrant.service
install -D -m 0644 configs/systemd/user/sy-mon-collect.service %{buildroot}%{_userunitdir}/sy-mon-collect.service
install -D -m 0644 configs/systemd/user/sy-stack-bar.service %{buildroot}%{_userunitdir}/sy-stack-bar.service
install -D -m 0644 configs/systemd/user/sy-waybar.service %{buildroot}%{_userunitdir}/sy-waybar.service
sed -i 's|%h/.local/bin/sy |/usr/bin/sy |g' %{buildroot}%{_userunitdir}/sy-mon-collect.service %{buildroot}%{_userunitdir}/sy-stack-bar.service
install -D -m 0644 configs/systemd/user/sy-file.service %{buildroot}%{_userunitdir}/sy-file.service
install -D -m 0644 configs/systemd/user/sy-file.socket %{buildroot}%{_userunitdir}/sy-file.socket
sed -i 's|%h/.local/bin/sy |/usr/bin/sy |g' %{buildroot}%{_userunitdir}/sy-file.service
install -D -m 0644 packaging/sy.1 %{buildroot}%{_mandir}/man1/sy.1
mkdir -p %{buildroot}%{_datadir}/sy
cp -a configs themes %{buildroot}%{_datadir}/sy/
install -m 0644 packaging/default-sy.toml %{buildroot}%{_datadir}/sy/sy.toml
find %{buildroot}%{_datadir}/sy/configs/systemd/user -type f -name '*.service' \
  -exec sed -i 's|%h/.local/bin/sy |/usr/bin/sy |g' {} +
sed -i 's|~/.local/bin/sy-plugin-md|/usr/libexec/sy/sy-plugin-md|g' \
  %{buildroot}%{_datadir}/sy/configs/sy/plugins/sy-plugin-md/plugin.toml
find %{buildroot}%{_datadir}/sy -type f \( -name .gitkeep -o -name .keep \) -delete
mkdir -p %{buildroot}%{_datadir}/sy/scripts
install -m 0755 scripts/check-npu-upgrade.sh scripts/install-system-npu.sh \
  scripts/install-system-syauth-selinux.sh %{buildroot}%{_datadir}/sy/scripts/

%files
%license LICENSE
%doc README.md
%{_bindir}/sy
%{_userunitdir}/sy.target
%{_mandir}/man1/sy.1*

%files desktop
%doc README.md
%{_libexecdir}/sy/sy-all
%{_datadir}/sy
%exclude %{_datadir}/sy/configs/policy
%exclude %{_datadir}/sy/configs/selinux/syauth
%exclude %{_datadir}/sy/configs/systemd/user/sy-agentd.service
%exclude %{_datadir}/sy/configs/systemd/user/sy-aiplane.service
%exclude %{_datadir}/sy/configs/sy/aiplane
%exclude %{_datadir}/sy/configs/sy-knowledge
%exclude %{_datadir}/sy/configs/systemd/user/sy-knowledge.service
%exclude %{_datadir}/sy/configs/systemd/user/sy-knowledge.socket
%exclude %{_datadir}/sy/configs/systemd/user/sy-qdrant.service
%exclude %{_datadir}/sy/configs/systemd/user/sy-mon-collect.service
%exclude %{_datadir}/sy/configs/systemd/user/sy-stack-bar.service
%exclude %{_datadir}/sy/configs/systemd/user/sy-waybar.service
%exclude %{_datadir}/sy/configs/systemd/user/sy.target.wants/sy-mon-collect.service
%exclude %{_datadir}/sy/configs/waybar/modules/sy-mon.json
%exclude %{_datadir}/sy/configs/dracut
%exclude %{_datadir}/sy/configs/profile.d/ryzenai.sh
%exclude %{_datadir}/sy/configs/systemd/system/sy-amdxdna-load.service
%exclude %{_datadir}/sy/configs/systemd/system/sy-npu-perf.service
%exclude %{_datadir}/sy/scripts/check-npu-upgrade.sh
%exclude %{_datadir}/sy/scripts/install-system-npu.sh
%exclude %{_datadir}/sy/scripts/install-system-syauth-selinux.sh
%exclude %{_datadir}/sy/configs/sy/plugins
%exclude %{_datadir}/sy/configs/sy/file.toml
%exclude %{_datadir}/sy/configs/sy/file-keymap.toml
%exclude %{_datadir}/sy/configs/sy/file-theme.toml
%exclude %{_datadir}/sy/configs/systemd/user/sy-file.service
%exclude %{_datadir}/sy/configs/systemd/user/sy-file.socket

%files system-power
%{_libexecdir}/sy/sy-system-power

%files system-agent
%{_libexecdir}/sy/sy-system-agent
%{_userunitdir}/sy-agentd.service
%{_datadir}/sy/configs/policy
%{_datadir}/sy/configs/systemd/user/sy-agentd.service

%files system-auth
%{_libexecdir}/sy/sy-system-auth
%{_datadir}/sy/configs/selinux/syauth
%{_datadir}/sy/scripts/install-system-syauth-selinux.sh

%files system-aiplane
%{_libexecdir}/sy/sy-system-aiplane
%{_userunitdir}/sy-aiplane.service
%{_datadir}/sy/configs/sy/aiplane
%{_datadir}/sy/configs/systemd/user/sy-aiplane.service

%files system-knowledge
%{_libexecdir}/sy/sy-system-knowledge
%{_userunitdir}/sy-knowledge.service
%{_userunitdir}/sy-knowledge.socket
%{_userunitdir}/sy-qdrant.service
%{_datadir}/sy/configs/sy-knowledge
%{_datadir}/sy/configs/systemd/user/sy-knowledge.service
%{_datadir}/sy/configs/systemd/user/sy-knowledge.socket
%{_datadir}/sy/configs/systemd/user/sy-qdrant.service

%files system-monitor
%{_libexecdir}/sy/sy-system-monitor
%{_userunitdir}/sy-mon-collect.service
%{_datadir}/sy/configs/systemd/user/sy-mon-collect.service
%{_datadir}/sy/configs/systemd/user/sy.target.wants/sy-mon-collect.service
%{_datadir}/sy/configs/waybar/modules/sy-mon.json

%files desktop-shell
%{_libexecdir}/sy/sy-desktop-shell
%{_userunitdir}/sy-stack-bar.service
%{_userunitdir}/sy-waybar.service
%{_datadir}/sy/configs/systemd/user/sy-stack-bar.service
%{_datadir}/sy/configs/systemd/user/sy-waybar.service

%files system-aiplane-ryzenai
%{_datadir}/sy/configs/dracut
%{_datadir}/sy/configs/profile.d/ryzenai.sh
%{_datadir}/sy/configs/systemd/system/sy-amdxdna-load.service
%{_datadir}/sy/configs/systemd/system/sy-npu-perf.service
%{_datadir}/sy/scripts/check-npu-upgrade.sh
%{_datadir}/sy/scripts/install-system-npu.sh

%files filemanager
%{_libexecdir}/sy/sy-filemanager
%{_libexecdir}/sy/sy-plugin-md
%{_userunitdir}/sy-file.service
%{_userunitdir}/sy-file.socket
%{_datadir}/sy/configs/sy/plugins
%{_datadir}/sy/configs/sy/file.toml
%{_datadir}/sy/configs/sy/file-keymap.toml
%{_datadir}/sy/configs/sy/file-theme.toml
%{_datadir}/sy/configs/systemd/user/sy-file.service
%{_datadir}/sy/configs/systemd/user/sy-file.socket

%changelog
* Sat Oct 17 2026 sy maintainers <maintainers@example.invalid> - 0.1.0-14
- Extract knowledge, monitor, and desktop shell executables into optional packages
- Preserve installed feature selections during local RPM reinstall
- Record host and AMD/XRT versions in the NPU upgrade smoke check

* Sat Oct 17 2026 sy maintainers <maintainers@example.invalid> - 0.1.0-13
- Give the Markdown previewer enough address space for cold README rendering
- Download Fedora build dependencies concurrently on slow mirrors

* Sat Oct 17 2026 sy maintainers <maintainers@example.invalid> - 0.1.0-12
- Preserve daemon logging when sy aiplane daemon runs from a checkout

* Sat Oct 17 2026 sy maintainers <maintainers@example.invalid> - 0.1.0-11
- Add a standalone aiplane daemon and user service with its own IPC socket

* Sat Oct 17 2026 sy maintainers <maintainers@example.invalid> - 0.1.0-10
- Use the packaged Foot executable in GPU and NPU Waybar actions

* Sat Oct 17 2026 sy maintainers <maintainers@example.invalid> - 0.1.0-9
- Reconcile removed feature configs and units during desktop apply

* Sat Oct 17 2026 sy maintainers <maintainers@example.invalid> - 0.1.0-8
- Extract phone-as-key CLI and SELinux integration into sy-system-auth

* Sat Oct 17 2026 sy maintainers <maintainers@example.invalid> - 0.1.0-7
- Declare terminal and Python dependencies used by desktop and NPU checks

* Sat Oct 17 2026 sy maintainers <maintainers@example.invalid> - 0.1.0-6
- Make desktop features independently installable

* Sat Oct 17 2026 sy maintainers <maintainers@example.invalid> - 0.1.0-5
- Extract the file manager and plugin host with shared theme support

* Sat Oct 17 2026 sy maintainers <maintainers@example.invalid> - 0.1.0-4
- Extract AI workloads and CLI; package Ryzen AI integration separately

* Sat Oct 17 2026 sy maintainers <maintainers@example.invalid> - 0.1.0-3
- Extract the agent runner, policy configuration, and user service

* Sat Oct 17 2026 sy maintainers <maintainers@example.invalid> - 0.1.0-2
- Split base launcher and power executable from the desktop bundle
