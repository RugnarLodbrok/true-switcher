import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import call, patch

import installer


class QdbusTests(unittest.TestCase):
    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)

    def _executable(self, directory: Path, name: str) -> Path:
        executable = directory / name
        executable.write_text('#!/bin/sh\nexit 0\n', encoding='utf-8')
        executable.chmod(0o755)
        return executable

    def _find(self, path: str) -> str:
        with patch.object(installer, 'SYSTEM_PATH', path):
            return installer._find_qdbus()

    def test_accepts_each_supported_name(self) -> None:
        for name in installer.QDBUS_NAMES:
            with self.subTest(name=name):
                executable = self._executable(self.root, name)
                self.assertEqual(self._find(str(self.root)), str(executable))
                executable.unlink()

    def test_prefers_qt6_names_across_path_directories(self) -> None:
        first = self.root / 'first'
        second = self.root / 'second'
        first.mkdir()
        second.mkdir()
        self._executable(first, 'qdbus')
        qt6 = self._executable(second, 'qdbus6')
        path = os.pathsep.join((str(first), str(second)))
        self.assertEqual(self._find(path), str(qt6))
        preferred = self._executable(second, 'qdbus-qt6')
        self.assertEqual(self._find(path), str(preferred))

    def test_skips_non_executable_files_and_directories(self) -> None:
        (self.root / 'qdbus-qt6').write_text('not executable', encoding='utf-8')
        (self.root / 'qdbus6').mkdir()
        executable = self._executable(self.root, 'qdbus')
        self.assertEqual(self._find(str(self.root)), str(executable))

    def test_reports_missing_commands_and_system_path(self) -> None:
        with self.assertRaises(RuntimeError) as raised:
            self._find(str(self.root))
        message = str(raised.exception)
        for name in installer.QDBUS_NAMES:
            self.assertIn(name, message)
        self.assertIn(str(self.root), message)

    def test_rejects_command_available_only_in_user_path(self) -> None:
        user_directory = self.root / 'user'
        service_directory = self.root / 'service'
        user_directory.mkdir()
        service_directory.mkdir()
        self._executable(user_directory, 'qdbus-qt6')
        with patch.dict(os.environ, {'PATH': str(user_directory)}):
            with self.assertRaises(RuntimeError):
                self._find(str(service_directory))


class UserConfigTests(unittest.TestCase):
    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.path = Path(directory.name) / 'config' / 'config.toml'
        self.missing_template = Path(directory.name) / 'missing.toml'

    def test_copies_the_template_verbatim(self) -> None:
        with patch.object(installer, 'USER_CONFIG', self.path), patch('builtins.print'):
            installer._ensure_user_config()
        self.assertEqual(self.path.read_bytes(), installer.CONFIG_TEMPLATE.read_bytes())

    def test_keeps_existing_config_without_reading_the_template(self) -> None:
        original = b'switch_key = "KEY_F12"\n'
        self.path.parent.mkdir()
        self.path.write_bytes(original)
        with patch.object(installer, 'USER_CONFIG', self.path), \
                patch.object(installer, 'CONFIG_TEMPLATE', self.missing_template):
            installer._ensure_user_config()
        self.assertEqual(self.path.read_bytes(), original)

    def test_missing_template_aborts_install_before_creating_config(self) -> None:
        with patch.object(installer, 'USER_CONFIG', self.path), \
                patch.object(installer, 'CONFIG_TEMPLATE', self.missing_template), \
                patch.object(installer, '_preflight', return_value=False), \
                patch.object(installer, '_install_file') as install_file:
            with self.assertRaises(FileNotFoundError) as raised:
                installer._install()
        self.assertEqual(raised.exception.filename, str(self.missing_template))
        self.assertFalse(self.path.exists())
        install_file.assert_not_called()


class TrayInstallationTests(unittest.TestCase):
    def test_only_explicit_restart_exit_status_forces_restart(self) -> None:
        system, *_ = installer._contents()
        self.assertIn('\nSuccessExitStatus=75\n', system)
        self.assertIn('\nRestartForceExitStatus=75\n', system)
        self.assertNotIn('\nRestart=always\n', system)
        self.assertNotIn('\nRestart=on-success\n', system)

    def test_installs_icon_with_service_files(self) -> None:
        with patch.object(installer, '_preflight', return_value=False), \
                patch.object(installer, '_ensure_user_config'), \
                patch.object(installer, '_install_file') as install_file, \
                patch.object(installer, '_run'), \
                patch.object(installer, '_systemctl'), \
                patch.object(installer, '_instructions'), \
                patch('builtins.print'):
            installer._install()
        install_file.assert_any_call(installer.ROOT / 'icon' / installer.ICON_NAME, installer.ICON, True)
        install_file.assert_any_call(installer.ROOT / 'icon' / installer.ERROR_ICON_NAME, installer.ERROR_ICON, True)

    def test_starts_session_after_enabling_autostart(self) -> None:
        with patch.object(installer, '_preflight', return_value=False), \
                patch.object(installer, '_ensure_user_config'), \
                patch.object(installer, '_install_file'), \
                patch.object(installer, '_run'), \
                patch.object(installer, '_systemctl') as systemctl, \
                patch.object(installer, '_instructions'), \
                patch('builtins.print'):
            installer._install()
        self.assertEqual(systemctl.call_args_list[-5:], [
            call('enable', installer.USER_NAME, user=True),
            call('is-enabled', installer.USER_NAME, user=True),
            call('start', installer.USER_NAME, user=True),
            call('is-active', installer.USER_NAME, user=True),
            call('is-active', installer.SYSTEM_NAME),
        ])

    def test_failed_start_stops_services_and_rolls_back(self) -> None:
        completed = subprocess.CompletedProcess([], 0, '', '')

        def systemctl(*args: str, **kwargs: object) -> subprocess.CompletedProcess:
            if args == ('start', installer.USER_NAME):
                raise subprocess.CalledProcessError(1, args)
            return completed

        with patch.object(installer, '_preflight', return_value=False), \
                patch.object(installer, '_ensure_user_config'), \
                patch.object(installer, '_install_file'), \
                patch.object(installer, '_run', return_value=completed) as run, \
                patch.object(installer, '_systemctl', side_effect=systemctl) as control, \
                patch('builtins.print'):
            with self.assertRaises(subprocess.CalledProcessError):
                installer._install()
        run.assert_any_call(['systemctl', '--user', 'stop', installer.USER_NAME], check=False)
        run.assert_any_call(['sudo', 'systemctl', 'stop', installer.SYSTEM_NAME], check=False)
        control.assert_any_call('disable', installer.USER_NAME, user=True, check=False)
        run.assert_any_call(['sudo', 'rm', '-f', str(installer.BINARY)], check=False)

    def test_removes_icon_without_removing_user_config(self) -> None:
        completed = subprocess.CompletedProcess([], 0, '', '')
        with patch.object(installer, '_path_exists', return_value=True), \
                patch.object(installer, '_run', return_value=completed) as run, \
                patch('builtins.print'):
            installer._uninstall()
        run.assert_any_call(['sudo', 'rm', '-f', '--', str(installer.ICON)], check=False)
        for call in run.call_args_list:
            self.assertNotIn(str(installer.USER_CONFIG), call.args[0])

    def test_rolls_back_icon_if_later_installation_fails(self) -> None:
        def install_file(source: Path, destination: Path, privileged: bool) -> None:
            if destination == installer.SYSTEM_UNIT:
                raise OSError('Synthetic installation failure')

        completed = subprocess.CompletedProcess([], 0, '', '')
        with patch.object(installer, '_preflight', return_value=False), \
                patch.object(installer, '_ensure_user_config'), \
                patch.object(installer, '_install_file', side_effect=install_file), \
                patch.object(installer, '_run', return_value=completed) as run, \
                patch('builtins.print'):
            with self.assertRaises(OSError):
                installer._install()
        run.assert_any_call(['sudo', 'rm', '-f', str(installer.ICON)], check=False)


if __name__ == '__main__':
    unittest.main()
