"""Fail-closed Windows launch/privacy contracts, with native ACL checks on Windows CI."""
import contextlib
import ctypes
from ctypes import wintypes
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

    @contextlib.contextmanager
    def creation_api(self, create=None):
        """Exercise pointer contents and control flow, without pretending mocks enforce a Windows ACL."""
        sid, events = b'user-sid', []
        c = types.SimpleNamespace(Structure=ctypes.Structure, byref=ctypes.byref, sizeof=ctypes.sizeof,
                                  create_string_buffer=ctypes.create_string_buffer,
                                  WinError=lambda code: code, get_last_error=lambda: OSError('create refused'))
        kernel, adv = mock.Mock(), mock.Mock()

        def sid_text(_, out):
            events.append('sid')
            out._obj.value = 'S-1-5-21-123'
            return True

        def descriptor(text, revision, out, size):
            self.assertEqual((text, revision, size), ('D:P(A;OICI;FA;;;S-1-5-21-123)', 1, None))
            events.append('descriptor')
            out._obj.value = 123
            return True

        def mkdir(path, attributes):
            self.assertEqual(attributes._obj.nLength, ctypes.sizeof(attributes._obj))
            self.assertEqual(attributes._obj.lpSecurityDescriptor, 123)
            self.assertFalse(attributes._obj.bInheritHandle)
            events.append(('create', path))
            return create(path) if create else True

        def read(path):
            events.append(('verify', str(path)))
            return 0x1000, [(0, 3, 0x1F01FF, sid)]

        adv.ConvertSidToStringSidW.side_effect = sid_text
        adv.ConvertStringSecurityDescriptorToSecurityDescriptorW.side_effect = descriptor
        adv.SetNamedSecurityInfoW.return_value = 0
        kernel.CreateDirectoryW.side_effect = mkdir
        kernel.LocalFree.side_effect = lambda _: events.append('free')
        with mock.patch.object(backends, 'os', wraps=os) as win, \
                mock.patch.object(backends, '_win_security', return_value=(c, wintypes, kernel, adv)), \
                mock.patch.object(backends, '_windows_user_sid', return_value=sid), \
                mock.patch.object(backends, '_windows_acl', side_effect=read), \
                mock.patch.object(Path, 'mkdir') as plain:
            win.name = 'nt'
            yield kernel, adv, events
            plain.assert_not_called()

    def test_atomic_creation_passes_descriptor_before_verification_and_never_sets_acl_later(self):
        with self.creation_api() as (kernel, adv, events):
            backends.private_directory('private', create=True)
            self.assertEqual(events, ['sid', 'descriptor', ('create', 'private'), ('verify', 'private'),
                                      'free', 'free'])
            adv.SetNamedSecurityInfoW.assert_not_called()

    def test_atomic_parents_are_created_and_verified_before_the_leaf(self):
        parent, leaf = Path('parent'), Path('parent/private')
        attempted = set()

        def create(path):
            if path == str(leaf) and path not in attempted:
                attempted.add(path)
                raise FileNotFoundError()
            return True

        with self.creation_api(create) as (_, adv, events):
            backends.private_directory(leaf, create=True, parents=True)
            self.assertEqual(events[2:-2], [('create', str(leaf)), ('create', str(parent)),
                                           ('verify', str(parent)), ('create', str(leaf)), ('verify', str(leaf))])
            adv.SetNamedSecurityInfoW.assert_not_called()

    def test_an_ancestor_another_process_created_meanwhile_is_left_as_it_is(self):
        parent, leaf = Path('parent'), Path('parent/private')
        attempted = set()

        def create(path):
            if path == str(leaf) and path not in attempted:
                attempted.add(path)
                raise FileNotFoundError()
            if path == str(parent):
                raise FileExistsError()
            return True

        with self.creation_api(create) as (_, adv, events), mock.patch.object(Path, 'is_dir', return_value=True):
            backends.private_directory(leaf, create=True, parents=True)
            adv.SetNamedSecurityInfoW.assert_not_called()  # the existing ancestor is not tightened
            self.assertNotIn(('verify', str(parent)), events)
            self.assertIn(('verify', str(leaf)), events)

    def test_existing_directory_is_tightened_only_with_exist_ok_and_files_are_refused(self):
        def exists(path):
            raise FileExistsError()

        with tempfile.TemporaryDirectory() as d:
            directory, file = Path(d), Path(d) / 'file'
            file.touch()
            for path, exist_ok, allowed in ((directory, False, False), (directory, True, True), (file, True, False)):
                with self.subTest(path=path, exist_ok=exist_ok), self.creation_api(exists) as (_, adv, events):
                    if allowed:
                        backends.private_directory(path, create=True, exist_ok=exist_ok)
                        adv.SetNamedSecurityInfoW.assert_called_once()
                        self.assertIn(('verify', str(path)), events)
                    else:
                        with self.assertRaises(FileExistsError):
                            backends.private_directory(path, create=True, exist_ok=exist_ok)
                        adv.SetNamedSecurityInfoW.assert_not_called()
                    self.assertEqual(events[-2:], ['free', 'free'])

    def test_descriptor_creation_and_mkdir_failures_stop_before_readback_and_free_allocations(self):
        for failure in ('sid', 'descriptor', 'create'):
            with self.subTest(failure=failure), self.creation_api() as (kernel, adv, events):
                if failure == 'sid':
                    adv.ConvertSidToStringSidW.side_effect = None
                    adv.ConvertSidToStringSidW.return_value = False
                elif failure == 'descriptor':
                    adv.ConvertStringSecurityDescriptorToSecurityDescriptorW.side_effect = None
                    adv.ConvertStringSecurityDescriptorToSecurityDescriptorW.return_value = False
                else:
                    kernel.CreateDirectoryW.side_effect = PermissionError('denied')
                with self.assertRaises(OSError):
                    backends.private_directory('private', create=True, parents=True)
                self.assertFalse(any(isinstance(e, tuple) and e[0] == 'verify' for e in events))
                self.assertEqual(kernel.LocalFree.call_count, {'sid': 0, 'descriptor': 1, 'create': 2}[failure])
                adv.SetNamedSecurityInfoW.assert_not_called()

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
                mock.patch.object(backends, 'private_tempdir', side_effect=OSError('ACL refused')), \
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
        with mock.patch.object(backends, 'private_tempdir', side_effect=OSError('ACL refused')):
            for _ in range(2):
                with self.assertRaisesRegex(OSError, 'ACL refused'):
                    run.incident_folder()
                self.assertIsNone(run.incident_dir)

    @unittest.skipIf(os.name == 'nt', 'POSIX tempfile contract')
    def test_posix_tempdirs_delegate_to_the_original_tempfile_functions(self):
        with mock.patch.object(backends.tempfile, 'TemporaryDirectory') as temporary, \
                mock.patch.object(backends.tempfile, 'mkdtemp', return_value='retained') as retained, \
                mock.patch.object(backends, 'private_directory') as protect:
            temporary.return_value.__enter__.return_value = 'temporary'
            with backends.private_tempdir('prefix', dir='parent') as folder:
                self.assertEqual(folder, 'temporary')
            temporary.assert_called_once_with(prefix='prefix', dir='parent')
            with backends.private_tempdir('incident', cleanup=False) as folder:
                self.assertEqual(folder, 'retained')
            retained.assert_called_once_with(prefix='incident', dir=None)
            protect.assert_not_called()

    def test_windows_tempdirs_retry_collisions_without_adopting_or_removing_them(self):
        with tempfile.TemporaryDirectory() as d, mock.patch.object(backends, 'os', wraps=os) as win, \
                mock.patch.object(backends, 'private_directory', side_effect=[FileExistsError(), None]) as create, \
                mock.patch.object(backends.secrets, 'token_hex', side_effect=['collision', 'new']), \
                mock.patch.object(backends.shutil, 'rmtree') as cleanup, \
                mock.patch.object(backends.tempfile, 'mkdtemp') as plain:
            win.name = 'nt'
            with backends.private_tempdir('prefix-', dir=d) as folder:
                self.assertEqual(folder, str(Path(d) / 'prefix-new'))
                cleanup.assert_not_called()
            self.assertEqual(create.call_args_list, [mock.call(Path(d) / 'prefix-collision', create=True),
                                                    mock.call(Path(d) / 'prefix-new', create=True)])
            self.assertEqual(cleanup.call_args.args[0], Path(folder))
            plain.assert_not_called()


@unittest.skipUnless(os.name == 'nt', 'native Windows DACLs')
class NativeWindowsPrivacyTest(unittest.TestCase):
    def assert_private(self, path, *, directory=True, protected=True):
        control, aces = backends._windows_acl(path)
        flags = (3 if protected else 19) if directory else 16
        self.assertEqual(aces, [(0, flags, 0x1F01FF, backends._windows_user_sid())])
        if directory:
            self.assertEqual(bool(control & 0x1000), protected, 'SE_DACL_PROTECTED')

    def test_directory_has_exactly_one_user_ace_and_files_inherit_it(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d) / 'private'
            backends.private_directory(root, create=True)
            self.assert_private(root)
            (root / 'secret').write_text('private')
            self.assert_private(root / 'secret', directory=False)
            child = root / 'child'
            child.mkdir()
            self.assert_private(child, protected=False)
            backends.private_directory(root)  # re-securing a root is safe and still verified
            self.assert_private(root)

    def test_new_directory_is_already_protected_when_create_directory_returns(self):
        with tempfile.TemporaryDirectory() as d:
            _, _, kernel, adv = backends._win_security()
            create, observed = kernel.CreateDirectoryW, []

            def inspect(path, attributes):
                result = create(path, attributes)
                if result:
                    # Read the actual filesystem before private_directory can verify or change anything.
                    self.assert_private(path)
                    observed.append(Path(path))
                return result

            leaf = Path(d) / 'parent' / 'private'
            with mock.patch.object(kernel, 'CreateDirectoryW', side_effect=inspect), \
                    mock.patch.object(adv, 'SetNamedSecurityInfoW', side_effect=AssertionError('late DACL update')):
                backends.private_directory(leaf, create=True, parents=True)
            self.assertEqual(observed, [leaf.parent, leaf])

    def test_created_parents_are_protected_without_changing_existing_ancestors(self):
        with tempfile.TemporaryDirectory() as d:
            before = backends._windows_acl(d)
            leaf = Path(d) / 'new-parent' / 'private'
            backends.private_directory(leaf, create=True, parents=True)
            self.assert_private(leaf.parent)
            self.assert_private(leaf)
            self.assertEqual(backends._windows_acl(d), before)

    def test_existing_directory_is_tightened_and_mkdir_exist_ok_semantics_are_preserved(self):
        with tempfile.TemporaryDirectory() as d:
            root, file = Path(d) / 'existing', Path(d) / 'file'
            root.mkdir()
            file.touch()
            with self.assertRaises(FileExistsError):
                backends.private_directory(root, create=True)
            with self.assertRaises(FileExistsError):
                backends.private_directory(file, create=True, exist_ok=True)
            backends.private_directory(root, create=True, exist_ok=True)
            self.assert_private(root)

    def test_store_trace_incidents_release_state_and_codex_homes_use_the_helper(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            self.assert_private(evaluate.private_store(root / 'store'))
            run = harness.Run(root / 'trace')
            self.assert_private(run.trace)
            for path in (run.trace / 'steps', run.incident_folder()):
                self.assert_private(path, protected=False)
            self.assert_private(run.trace / 'state.json', directory=False)
            state = releases.private_root(root / 'state')
            self.assert_private(state)
            for path in (state / 'bundles', state / 'approvals'):
                self.assert_private(path, protected=False)
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
            with mock.patch.object(adv, 'ConvertStringSecurityDescriptorToSecurityDescriptorW',
                                   side_effect=OSError('descriptor refused')), \
                    mock.patch.object(backends, '_spool') as spool:
                with self.assertRaises(OSError):
                    backends.run_isolated(['never'], 'secret', 1)
                spool.assert_not_called()

    def test_private_tempdir_is_atomic_and_removes_readonly_files_on_failure(self):
        with self.assertRaisesRegex(RuntimeError, 'body failed'):
            with backends.private_tempdir('ringo-test-') as folder:
                self.assert_private(folder)
                secret = Path(folder) / 'secret'
                secret.write_text('private')
                self.assert_private(secret, directory=False)
                secret.chmod(0o444)
                raise RuntimeError('body failed')
        self.assertFalse(Path(folder).exists())
