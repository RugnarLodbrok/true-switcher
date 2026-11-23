import json
import os
import pwd
import shlex
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import List

ROOT = Path(__file__).resolve().parent.parent
BINARY = Path('/usr/local/bin/true-switcher')
ICON_NAME = 'icon.svg'
ICON = Path('/usr/local/share/true-switcher/icons') / ICON_NAME
ERROR_ICON_NAME = 'icon_error.svg'
ERROR_ICON = ICON.parent / ERROR_ICON_NAME
SYSTEM_UNIT = Path('/etc/systemd/system/true-switcher.service')
USER_UNIT = Path.home() / '.config/systemd/user/true-switcher-session.service'
USER_CONFIG = Path.home() / '.config/true-switcher/config.toml'
CONFIG_TEMPLATE = Path(__file__).resolve().with_name('config.example.toml')
POLKIT = Path('/etc/polkit-1/rules.d/49-true-switcher.rules')
SYSTEM_NAME = 'true-switcher.service'
USER_NAME = 'true-switcher-session.service'
SYSTEM_PATH = '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'
QDBUS_NAMES = ('qdbus-qt6', 'qdbus6', 'qdbus')


def _run(args: List[str], check: bool = True) -> subprocess.CompletedProcess:
    executable = args[1] if len(args) > 1 and args[0] == 'sudo' else args[0]
    if executable == 'systemctl':
        print('cmd: ' + shlex.join(args))
    # else:
        # print('OTHER CMD:' + shlex.join(args))
    try:
        return subprocess.run(args, check=check, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    except OSError as error:
        if check:
            raise
        return subprocess.CompletedProcess(args, 127, '', str(error))


def _systemctl(*args: str, user: bool = False, check: bool = True) -> subprocess.CompletedProcess:
    return _run(['systemctl', *(['--user'] if user else ['--system']), *args], check)


def _path_exists(path: Path, is_privileged: bool) -> bool:
    if not is_privileged:
        return os.path.lexists(path)
    script = 'if [ -e "$1" ] || [ -L "$1" ]; then exit 0; else exit 3; fi'
    result = _run(['sudo', 'sh', '-c', script, 'sh', str(path)], check=False)
    if result.returncode == 0:
        return True
    if result.returncode == 3:
        return False
    raise RuntimeError(f'Cannot check {path}: {result.stderr.strip()}')


def _check_invocation() -> None:
    if os.geteuid() == 0 or 'SUDO_USER' in os.environ:
        if os.environ.get('TRUE_SWITCHER_INSTALL_ALLOW_SUDO') == '1':
            raise RuntimeError('NOT IMPLEMENTED')
        raise RuntimeError('Run make without sudo; the installer invokes sudo when needed.')


def _policy_available() -> bool:
    if not shutil.which('restorecon') or not shutil.which('matchpathcon'):
        print('Cannot check SELinux policy: restorecon or matchpathcon is unavailable.')
        return False
    return _run(['matchpathcon', str(BINARY)], check=False).returncode == 0


def _find_qdbus() -> str:
    for name in QDBUS_NAMES:
        executable = shutil.which(name, path=SYSTEM_PATH)
        if executable:
            return executable
    raise RuntimeError(
        f'No qdbus executable found; tried {", ".join(QDBUS_NAMES)} in system service PATH={SYSTEM_PATH}'
    )


def _preflight() -> bool:
    binary = ROOT / 'target/release/true-switcher'
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise RuntimeError('Build the executable target/release/true-switcher first.')
    if not (ROOT / 'icon' / ICON_NAME).is_file():
        raise RuntimeError('Missing tray icon: ' + str(ROOT / 'icon' / ICON_NAME))
    if not (ROOT / 'icon' / ERROR_ICON_NAME).is_file():
        raise RuntimeError('Missing tray icon: ' + str(ROOT / 'icon' / ERROR_ICON_NAME))
    print(f'qdbus executable: {_find_qdbus()}')
    if os.environ.get('XDG_SESSION_TYPE') != 'wayland' or 'KDE' not in os.environ.get('XDG_CURRENT_DESKTOP', '').split(':'):
        raise RuntimeError('An active KDE Wayland session is required.')
    _systemctl('show-environment', user=True)
    _systemctl('show-environment')
    for target in ('graphical-session.target', 'plasma-workspace.target'):
        _systemctl('is-active', '--quiet', target, user=True)
    for path, is_privileged in ((BINARY, True), (ICON, True), (ERROR_ICON, True), (SYSTEM_UNIT, True), (USER_UNIT, False), (POLKIT, True)):
        if _path_exists(path, is_privileged):
            raise RuntimeError('Installation path already exists: ' + str(path))
    for name, user in ((SYSTEM_NAME, False), (USER_NAME, True)):
        for verb in ('is-active', 'is-enabled', 'is-failed'):
            result = _systemctl(verb, name, user=user, check=False)
            if result.returncode == 0:
                raise RuntimeError(f'{name} has existing {verb} state; resolve it before installation.')
            if result.returncode not in (1, 3, 4):
                raise RuntimeError(f'Cannot determine {name} state: {result.stderr.strip()}')
    return _policy_available()


def _ensure_user_config() -> None:
    if os.path.lexists(USER_CONFIG):
        return
    template = CONFIG_TEMPLATE.read_bytes()
    USER_CONFIG.parent.mkdir(parents=True, exist_ok=True)
    try:
        with USER_CONFIG.open('xb') as config:
            config.write(template)
    except FileExistsError:
        return
    print(f'created user config {USER_CONFIG}')


def _contents() -> tuple:
    user = pwd.getpwuid(os.getuid()).pw_name
    if any(c in user for c in '\n\r%'):
        raise RuntimeError('Unsupported username for systemd unit.')
    system = f'''[Unit]
Description=True Switcher

[Service]
Type=simple
User={user}
SupplementaryGroups=input
Environment=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{os.getuid()}/bus
Environment=XDG_RUNTIME_DIR=/run/user/{os.getuid()}
Environment=PATH={SYSTEM_PATH}
ExecStart={BINARY}
SuccessExitStatus=75
RestartForceExitStatus=75
'''
    session = f'''[Unit]
Description=True Switcher graphical session
Requisite=graphical-session.target
After=graphical-session.target
PartOf=graphical-session.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=systemctl --system start {SYSTEM_NAME}
ExecStopPost=systemctl --system stop {SYSTEM_NAME}

[Install]
WantedBy=xdg-desktop-autostart.target
'''
    rule = f'''polkit.addRule(function(action, subject) {{
    if (action.id == "org.freedesktop.systemd1.manage-units" &&
        subject.user == {json.dumps(user, ensure_ascii=True)} &&
        action.lookup("unit") == "{SYSTEM_NAME}" &&
        (action.lookup("verb") == "start" || action.lookup("verb") == "stop" ||
         action.lookup("verb") == "restart")) {{
        return polkit.Result.YES;
    }}
}});
'''
    return system, session, rule


def _install_file(source: Path, destination: Path, privileged: bool) -> None:
    args = ['install', '-D', '-m', '0755' if destination == BINARY else '0644']
    if privileged:
        args += ['-o', 'root', '-g', 'root']
    _run((['sudo'] if privileged else []) + args + [str(source), str(destination)])
    print(f'created [{"root" if privileged else "user"}] {destination}')


def _instructions(policy: bool, skip: bool) -> None:
    if policy and skip:
        print('''The installer skipped SELinux labeling. Apply the SELinux label defined by policy for this path and verify it:

sudo restorecon -v /usr/local/bin/true-switcher
matchpathcon -V /usr/local/bin/true-switcher

If either command fails or reports a label mismatch, resolve the issue before restarting True Switcher.''')
    if policy:
        print('''[!] You have SELinux on your system. Now you have to do these manual checks to verify that is is not blocking TrueSwitcher:
    1. `getenforce` — If Disabled, stop here: SELinux checks and denials are absent, installation is fine.
    2. `matchpathcon -V /usr/local/bin/true-switcher` — expected exit status 0 and a matching label.
    3. `systemctl --user is-active true-switcher-session.service` — expected active.
    4. `systemctl is-active true-switcher.service` — expected active.
    5. `sudo ausearch -m AVC,USER_AVC -ts recent` — check for denials related to this startup. A denial in Permissive would block the operation in Enforcing.''')


def _install() -> None:
    policy = _preflight()
    _ensure_user_config()
    skip = os.environ.get('TRUE_SWITCHER_INSTALL_SELINUX_SKIP') == '1'
    created = []
    enable_attempted = False
    start_attempted = False
    try:
        with tempfile.TemporaryDirectory() as temp:
            system, session, rule = _contents()
            sources = []
            for name, content in (('system', system), ('session', session), ('rule', rule)):
                path = Path(temp) / name
                path.write_text(content, encoding='utf-8')
                sources.append(path)
            for source, dest, privileged in ((ROOT / 'target/release/true-switcher', BINARY, True), (ROOT / 'icon' / ICON_NAME, ICON, True), (ROOT / 'icon' / ERROR_ICON_NAME, ERROR_ICON, True), (sources[0], SYSTEM_UNIT, True), (sources[1], USER_UNIT, False), (sources[2], POLKIT, True)):
                created.append((dest, privileged))
                _install_file(source, dest, privileged)
            if policy and not skip:
                try:
                    _run(['sudo', 'restorecon', str(BINARY)])
                    _run(['matchpathcon', '-V', str(BINARY)])
                except subprocess.CalledProcessError as error:
                    raise RuntimeError('SELinux labeling failed. Retry with TRUE_SWITCHER_INSTALL_SELINUX_SKIP=1. ' + error.stderr) from error
            _run(['systemd-analyze', 'verify', str(SYSTEM_UNIT)])
            _run(['systemd-analyze', '--user', 'verify', str(USER_UNIT)])
            _run(['sudo', 'systemctl', 'daemon-reload'])
            _systemctl('daemon-reload', user=True)
            enable_attempted = True
            _systemctl('enable', USER_NAME, user=True)
            _systemctl('is-enabled', USER_NAME, user=True)
            start_attempted = True
            _systemctl('start', USER_NAME, user=True)
            _systemctl('is-active', USER_NAME, user=True)
            _systemctl('is-active', SYSTEM_NAME)
    except Exception:
        if start_attempted:
            for command in (['systemctl', '--user', 'stop', USER_NAME], ['sudo', 'systemctl', 'stop', SYSTEM_NAME]):
                result = _run(command, check=False)
                if result.returncode:
                    print(f'Rollback stop failed: {result.stderr}', file=sys.stderr)
        if enable_attempted:
            _systemctl('disable', USER_NAME, user=True, check=False)
        for path, privileged in reversed(created):
            result = _run((['sudo'] if privileged else []) + ['rm', '-f', str(path)], check=False)
            if result.returncode:
                print(f'Rollback failed for {path}: {result.stderr}', file=sys.stderr)
        for cmd in (['sudo', 'systemctl', 'daemon-reload'], ['systemctl', '--user', 'daemon-reload']):
            result = _run(cmd, check=False)
            if result.returncode:
                print(f'Rollback reload failed: {result.stderr}', file=sys.stderr)
        raise
    print('[SUCCESS] True Switcher started and autostart enabled.')
    _instructions(policy, skip)


def _uninstall() -> None:
    failures = []
    commands = [(['systemctl', '--user', 'daemon-reload'], 'user daemon reload'),
                (['sudo', 'systemctl', 'daemon-reload'], 'system daemon reload'),
                (['systemctl', '--user', 'reset-failed', USER_NAME], 'user reset-failed'),
                (['sudo', 'systemctl', 'reset-failed', SYSTEM_NAME], 'system reset-failed'),
                (['systemctl', '--user', 'disable', '--now', USER_NAME], 'user service'),
                (['sudo', 'systemctl', 'stop', SYSTEM_NAME], 'system service')]
    for command, label in commands:
        result = _run(command, check=False)
        if result.returncode:
            # Missing units are already removed; other failures must be reported.
            if 'not loaded' not in result.stderr and 'not found' not in result.stderr and 'does not exist' not in result.stderr:
                failures.append(f'{label}: {result.stderr.strip()}')
    for path, is_privileged in ((USER_UNIT, False), (SYSTEM_UNIT, True), (POLKIT, True), (ERROR_ICON, True), (ICON, True), (BINARY, True)):
        try:
            is_present = _path_exists(path, is_privileged)
        except RuntimeError as error:
            failures.append(str(error))
            continue
        if not is_present:
            print(f'Already absent: {path}')
            continue
        command = (['sudo'] if is_privileged else []) + ['rm', '-f', '--', str(path)]
        result = _run(command, check=False)
        if result.returncode:
            failures.append(f'{path}: {result.stderr.strip()}')
        else:
            print(f'Removed: {path}')
    for command in (['sudo', 'systemctl', 'daemon-reload'], ['systemctl', '--user', 'daemon-reload']):
        result = _run(command, check=False)
        if result.returncode:
            failures.append('daemon reload: ' + result.stderr.strip())
    print('[SUCCESS] uninstall done')
    print('user settings are retained (~/.config/true-switcher)')
    if failures:
        raise RuntimeError('\n'.join(failures))


def _main() -> None:
    _check_invocation()
    if sys.argv[1:] == ['install']:
        _install()
    elif sys.argv[1:] == ['uninstall']:
        _uninstall()
    else:
        raise RuntimeError('Usage: installer.sh install|uninstall')


if __name__ == '__main__':
    try:
        _main()
    except (RuntimeError, OSError, subprocess.CalledProcessError) as error:
        print(f'Error: {error}', file=sys.stderr)
        sys.exit(1)
