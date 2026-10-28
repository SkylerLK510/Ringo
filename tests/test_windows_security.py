"""Fail-closed Windows launch/privacy contracts, with native ACL checks on Windows CI."""
import ctypes
import os
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

import backends
import evaluate
import harness
import releases
import workflow


class BatchLaunchTest(unittest.TestCase):
    def test_batch_programs_are_refused_before_any_launch_for_every_argument(self):
        for program in ('backend.cmd', r'C:\Tools\BACKEND.BAT', 'backend.CmD', 'backend.cmd. ', '"backend.cmd" '):
            for arg in ('safe', '', 'a&whoami', 'a|whoami', '<in', '>out', '^x', '%PATH%', '!x!',
                        'a"&whoami', 'double"quote', 'a\nb', 'a\rb'):
                with self.subTest(program=program, arg=arg), \
                        mock.patch.object(backends, 'os', wraps=os) as win, \
                        mock.patch.object(backends, '_win32') as api, \
                        mock.patch.object(backends.subprocess, 'Popen') as launch:
                    win.name = 'nt'
                    with self.assertRaisesRegex(ValueError, 'call the underlying executable') as caught:
                        backends.spawn([program, arg])
                    self.assertIn(repr(program), str(caught.exception))
                    self.assertIn(repr(arg), str(caught.exception))
                    api.assert_not_called()
                    launch.assert_not_called()

    def test_executable_override_and_argumentless_batch_are_also_refused(self):
        with mock.patch.object(backends, 'os', wraps=os) as win:
            win.name = 'nt'
            for argv, kw in ((['backend.bat'], {}), (['safe.exe', 'x'], {'executable': 'bad.cmd'})):
                with self.assertRaises(ValueError):
                    backends.spawn(argv, **kw)
            backends._check_windows_program(['safe.exe', 'a&|<>^%!"\n'])  # no shell for an executable

    def test_catalog_probe_cannot_bypass_the_guard(self):
        with tempfile.TemporaryDirectory() as d, mock.patch.object(backends, 'os', wraps=os) as win, \
                mock.patch.object(backends.subprocess, 'run') as launch:
            win.name = 'nt'
            with self.assertRaisesRegex(ValueError, 'call the underlying executable'):
                backends._lean_codex(['codex.cmd', 'exec', '-'], Path(d), {})
            launch.assert_not_called()

    def test_check_argv_uses_the_guard(self):
        with tempfile.TemporaryDirectory() as d, mock.patch.object(backends, 'os', wraps=os) as win, \
                mock.patch.object(backends.subprocess, 'Popen') as launch:
            win.name = 'nt'
            with self.assertRaisesRegex(ValueError, 'call the underlying executable'):
                workflow.checks({'checks': [{'name': 'batch', 'argv': ['check.cmd', 'a&whoami'],
                                            'timeout_s': 1}]}, Path(d), Path(d), lambda: 10)
            launch.assert_not_called()

    @unittest.skipIf(os.name == 'nt', 'POSIX launch contract')
    def test_posix_launch_is_unchanged_even_for_batch_named_programs(self):
        with mock.patch.object(backends.subprocess, 'Popen') as launch:
            argv = ['backend.cmd', 'a&whoami', '"\n%']
            backends.spawn(argv, cwd='fixture')
            launch.assert_called_once_with(argv, start_new_session=True, cwd='fixture')


class PrivateDirectoryTest(unittest.TestCase):
    @unittest.skipIf(os.name == 'nt', 'POSIX mkdir contract')
    def test_posix_mkdir_options_and_modes_are_unchanged(self):
        with mock.patch.object(Path, 'mkdir') as mkdir, mock.patch.object(backends, '_win_security') as api:
            backends.private_directory(Path('private'), create=True, mode=0o700, parents=True, exist_ok=True)
            mkdir.assert_called_once_with(mode=0o700, parents=True, exist_ok=True)
            api.assert_not_called()

    def test_set_and_readback_failures_propagate(self):
        sid = b'user-sid'
        c = types.SimpleNamespace(create_string_buffer=ctypes.create_string_buffer,
                                  WinError=lambda code: OSError(code, 'ACL failure'), get_last_error=lambda: 5)
        adv = mock.Mock()
        adv.SetNamedSecurityInfoW.return_value = 0
        with mock.patch.object(backends, 'os', wraps=os) as win, \
                mock.patch.object(backends, '_win_security', return_value=(c, None, None, adv)), \
                mock.patch.object(backends, '_windows_user_sid', return_value=sid), \
                mock.patch.object(backends, '_windows_acl') as read:
            win.name = 'nt'
            expected = (0x1000, [(0, 3, 0x1F01FF, sid)])
            read.return_value = expected
            backends.private_directory('private')
            call = adv.SetNamedSecurityInfoW.call_args.args
            self.assertEqual((call[0], call[1], call[2]), ('private', 1, 4 | 0x80000000))
            for bad in ((0, expected[1]), (0x1000, []), (0x1000, expected[1] * 2),
                        (0x1000, [(0, 19, 0x1F01FF, sid)]), (0x1000, [(0, 3, 1, sid)]),
                        (0x1000, [(0, 3, 0x1F01FF, b'other')])):
                with self.subTest(acl=bad), self.assertRaises(OSError):
                    read.return_value = bad
                    backends.private_directory('private')
            read.side_effect = OSError('cannot read ACL')
            with self.assertRaises(OSError):
                backends.private_directory('private')
            read.reset_mock()
            adv.SetNamedSecurityInfoW.return_value = 5
            with self.assertRaises(OSError):
                backends.private_directory('private')
            read.assert_not_called()
            adv.SetNamedSecurityInfoW.reset_mock()
            for fn in (adv.InitializeAcl, adv.AddAccessAllowedAceEx):
                fn.return_value = False
                with self.assertRaises(OSError):
                    backends.private_directory('private')
                adv.SetNamedSecurityInfoW.assert_not_called()
                fn.return_value = True

    def test_private_call_files_and_credentials_are_not_written_if_protection_fails(self):
        with tempfile.TemporaryDirectory() as d, \
                mock.patch.object(backends, 'private_directory', side_effect=OSError('ACL refused')), \
                mock.patch.object(backends, '_spool') as spool, \
                mock.patch.object(backends, '_owner_auth') as owner:
            with self.assertRaisesRegex(OSError, 'ACL refused'):
                backends.run_isolated(['never'], 'secret', 1)
            spool.assert_not_called()
            with self.assertRaisesRegex(OSError, 'ACL refused'):
                backends._codex_home(Path(d), {})
            owner.assert_not_called()
            self.assertEqual(list(Path(d).rglob('auth.json')), [])

    def test_failed_incident_protection_is_never_cached_for_a_later_write(self):
        run = harness.Run()
        with tempfile.TemporaryDirectory() as d, \
                mock.patch.object(harness.tempfile, 'mkdtemp', return_value=d), \
                mock.patch.object(backends, 'private_directory', side_effect=OSError('ACL refused')):
            for _ in range(2):
                with self.assertRaisesRegex(OSError, 'ACL refused'):
                    run.incident_folder()
                self.assertIsNone(run.incident_dir)


@unittest.skipUnless(os.name == 'nt', 'native Windows DACLs')
class NativeWindowsPrivacyTest(unittest.TestCase):
    def assert_private(self, path, *, directory=True):
        control, aces = backends._windows_acl(path)
        self.assertEqual(aces, [(0, 3 if directory else 16, 0x1F01FF, backends._windows_user_sid())])
        if directory:
            self.assertTrue(control & 0x1000, 'SE_DACL_PROTECTED must be set')

    def test_directory_has_exactly_one_user_ace_and_files_inherit_it(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d) / 'private'
            root.mkdir()
            backends.private_directory(root)
            self.assert_private(root)
            (root / 'secret').write_text('private')
            self.assert_private(root / 'secret', directory=False)
            child = root / 'child'
            child.mkdir()
            backends.private_directory(child)
            self.assert_private(child)
            backends.private_directory(root)  # re-securing a root is safe and still verified
            self.assert_private(root)

    def test_created_parents_are_protected_without_changing_existing_ancestors(self):
        with tempfile.TemporaryDirectory() as d:
            before = backends._windows_acl(d)
            leaf = Path(d) / 'new-parent' / 'private'
            backends.private_directory(leaf, create=True, parents=True)
            self.assert_private(leaf.parent)
            self.assert_private(leaf)
            self.assertEqual(backends._windows_acl(d), before)

    def test_store_trace_incidents_release_state_and_codex_homes_use_the_helper(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            self.assert_private(evaluate.private_store(root / 'store'))
            run = harness.Run(root / 'trace')
            for path in (run.trace, run.trace / 'steps', run.incident_folder()):
                self.assert_private(path)
            self.assert_private(run.trace / 'state.json', directory=False)
            state = releases.private_root(root / 'state')
            for path in (state, state / 'bundles', state / 'approvals'):
                self.assert_private(path)
            with mock.patch.object(backends, '_owner_auth', return_value=root / 'absent'):
                backends._codex_home(root, {})
            for path in (root / 'codex', root / 'home'):
                self.assert_private(path)

    def test_refreshed_auth_does_not_inherit_the_owner_parents_acl(self):
        with tempfile.TemporaryDirectory() as d:
            owner, copy = Path(d) / 'auth.json', Path(d) / 'copy.json'
            old, new = b'{"token":"old"}', b'{"token":"new"}'
            owner.write_bytes(old)
            copy.write_bytes(new)
            backends._sync_codex_auth(owner, old, copy)
            self.assertEqual(owner.read_bytes(), new)
            self.assert_private(owner, directory=False)

    def test_setting_acl_failure_stops_before_sensitive_writes(self):
        with tempfile.TemporaryDirectory() as d:
            _, _, _, adv = backends._win_security()
            with mock.patch.object(adv, 'SetNamedSecurityInfoW', return_value=5), \
                    mock.patch.object(backends, '_spool') as spool:
                with self.assertRaises(OSError):
                    backends.run_isolated(['never'], 'secret', 1)
                spool.assert_not_called()
