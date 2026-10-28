"""Local, human-approved Ringo releases. Management never runs the active release."""
import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import sys

from latency import canonical, read_json, sha

HERE = Path(__file__).resolve().parent
RUNTIME = ('ringo.py', 'harness.py', 'backends.py', 'pipelines.py', 'context.py', 'findings.py',
           'workflow.py', 'spec_runner.py', 'evaluate.py', 'improve.py', 'releases.py', 'latency.py')
# Files that joined the runtime after releases already existed. New bundles always include them; a bundle staged
# before one joined still verifies without it, since that bundle has no feature that runs it.
LATER_RUNTIME = {'spec_runner.py'}
IDENT = re.compile(r'[0-9a-f]{64}')
LATENCY_WORKLOADS = {'startup', 'review', 'review-trace', 'review-large', 'design', 'work'}


def file_hash(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def metadata_json(path):
    """Reject static symlinks and special files before a potentially blocking read.

    Bundles/state are owner-managed, not an isolation boundary against concurrent
    mutation by another process with the same account.
    """
    if not stat.S_ISREG(Path(path).lstat().st_mode):
        raise ValueError('release metadata must be a regular file')
    return read_json(path)


def inventory(source):
    source = Path(source).resolve()
    if any((source / d).is_symlink() for d in ('plans', 'prompts')):
        raise ValueError('source directories cannot be symlinks')
    paths = [source / p for p in RUNTIME]
    paths += sorted((source / 'prompts').glob('*.md')) + sorted((source / 'plans').glob('*.json'))
    if not all(p.is_file() and not p.is_symlink() for p in paths):
        raise ValueError('source needs every runtime file, with no symlinks')
    return {p.relative_to(source).as_posix(): file_hash(p) for p in paths}


def config_hash(plan):
    return sha({k: v for k, v in plan.items() if k != 'prompts_dir'})


def default_plan(bundle):
    import pipelines
    path = Path(bundle) / 'plans/review.json'
    raw = read_json(path)
    if not isinstance(raw, dict) or raw.get('kind') != 'review':
        raise ValueError('bundle needs a default review plan')
    base = (path.parent / raw['prompts_dir']).resolve() if isinstance(raw.get('prompts_dir'), str) \
        else (Path(bundle) / 'prompts').resolve()
    if not base.is_relative_to(Path(bundle).resolve()):
        raise ValueError('bundled prompts must stay inside their bundle')
    return pipelines.validate(raw, base)


def identity(runtime_root=None):
    """Truthful CLI provenance; checkout content hash includes uncommitted runtime edits."""
    pinned = getattr(sys, '_ringo_release', None)
    runtime_root = Path(runtime_root).resolve() if runtime_root else HERE
    if pinned and pinned['root'] == str(runtime_root):
        return dict(pinned)
    return {'mode': 'checkout', 'id': 'checkout:' + sha(inventory(runtime_root)), 'root': str(runtime_root),
            'quality_claim': None}


def state_root(path=None):
    root = Path(path or os.environ.get('RINGO_RELEASE_HOME', Path.home() / '.local/share/ringo'))
    if root.is_symlink():
        raise ValueError('release state cannot be a symlink')
    root = root.resolve()
    if root == HERE or HERE in root.parents:
        raise ValueError('release state must be outside the source repository')
    return root


def private_root(path=None):
    root = state_root(path)
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    root.chmod(0o700)
    for name in ('bundles', 'approvals'):
        target = root / name
        if target.is_symlink():
            raise ValueError('release state directories cannot be symlinks')
        target.mkdir(mode=0o700, exist_ok=True)
    return root


@contextlib.contextmanager
def locked(root):
    import backends
    lock = root / '.lock'
    if lock.is_symlink():
        raise ValueError('release lock cannot be a symlink')
    with lock.open('a') as f, backends.file_lock(f):
        yield


def fsync_dir(path):
    """Make a rename inside the folder path durable. Windows cannot open a folder for os.fsync; NTFS records the
    rename in its metadata journal instead."""
    if os.name == 'nt':
        return
    directory = os.open(path, os.O_RDONLY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def atomic_json(path, value):
    import tempfile
    fd, name = tempfile.mkstemp(prefix='.pending-', dir=path.parent)
    try:
        with os.fdopen(fd, 'wb') as f:
            f.write(canonical(value) + b'\n')
            f.flush()
            os.fsync(f.fileno())
        os.replace(name, path)
        fsync_dir(path.parent)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def pointer(root):
    path = root / 'active.json'
    if path.is_symlink():
        raise ValueError('active pointer cannot be a symlink')
    if not path.exists():
        return {'version': 1, 'active': None, 'previous': None}
    value = metadata_json(path)
    if not isinstance(value, dict) or set(value) != {'version', 'active', 'previous'} \
            or type(value['version']) is not int or value['version'] != 1:
        raise ValueError('invalid active pointer')
    for key in ('active', 'previous'):
        entry = value[key]
        if entry is not None and (not isinstance(entry, dict) or set(entry) != {'release', 'approval'}
                or any(not isinstance(v, str) or not IDENT.fullmatch(v) for v in entry.values())):
            raise ValueError('invalid release/approval pointer identity')
    return value


def quality(store, digest, plan, runtime_root=None, *, verify_bytecode=False):
    """Read existing protected-evaluator evidence. Never score, decide or adopt here."""
    import evaluate
    import pipelines
    spec = evaluate.checked(store, digest)
    decisions = [r for r in evaluate.ledger(store) if r['kind'] == 'decision' and r['digest'] == digest]
    if spec['split'] != 'test' or len(decisions) != 1 or decisions[0].get('result') != 'pass' \
            or decisions[0].get('void'):
        raise ValueError('RRSI candidate requires an existing non-void sealed-test pass')
    if not spec.get('comparators'):
        raise ValueError('RRSI candidate requires comparative sealed-test evidence')
    candidate = pipelines.load_plan(spec['arms'][spec['candidate']]['plan'], 'review')
    if config_hash(candidate) != config_hash(plan):
        raise ValueError('evaluated candidate is not the bundled effective review plan')
    # checked() binds these current evaluator sources to the frozen digest. A
    # passing plan cannot transfer its claim to different staged review code.
    evaluated = {name: file_hash(evaluate.HERE / name) for name in evaluate.CODE}
    runtime_root = Path(runtime_root) if runtime_root is not None else HERE
    if any(file_hash(runtime_root / name) != expected for name, expected in evaluated.items()):
        raise ValueError('staged review implementation differs from the sealed-test evidence')
    if verify_bytecode:
        # Full bundle hashes detect changes after staging. At activation also
        # reject a self-consistent manifest whose executable pyc was substituted
        # while retaining evaluated sources and their source-hash header.
        import importlib.util
        import py_compile
        import tempfile
        with tempfile.TemporaryDirectory(prefix='ringo-proof-') as folder:
            for name in evaluate.CODE:
                if not name.endswith('.py'):
                    continue
                expected = Path(folder) / (Path(name).name + 'c')
                py_compile.compile(str(runtime_root / name), cfile=str(expected),
                    dfile='ringo-release/' + name, doraise=True,
                    invalidation_mode=py_compile.PycInvalidationMode.CHECKED_HASH)
                cached = Path(importlib.util.cache_from_source(str(runtime_root / name)))
                if not cached.is_relative_to(runtime_root) or file_hash(cached) != file_hash(expected):
                    raise ValueError('staged review bytecode differs from the evaluated sources')
    arm = spec['arms'][spec['candidate']]
    return {'kind': 'rrsi', 'store': str(Path(store).resolve()), 'digest': digest,
            'backend': arm['backend'], 'checker_backend': arm.get('checker_backend'),
            'plan_sha256': config_hash(plan), 'decision_sha256': sha(decisions[0]),
            'implementation_sha256': sha(evaluated), 'objective': spec['objective'],
            'comparators': list(spec['comparators'])}


def stage(source, root, candidate_plan=None, evaluation_store=None, digest=None):
    import pipelines
    import py_compile
    import tempfile
    source, root = Path(source).resolve(), private_root(root)
    if candidate_plan and (not evaluation_store or not digest):
        raise ValueError('candidate staging needs --evaluation-store and --digest')
    if not candidate_plan and (evaluation_store or digest):
        raise ValueError('quality evidence requires a candidate plan')
    files = inventory(source)
    plan = pipelines.load_plan(candidate_plan, 'review') if candidate_plan else default_plan(source)
    evidence = quality(evaluation_store, digest, plan, source) if candidate_plan else {'kind': 'deployment', 'claim': None}
    with locked(root), tempfile.TemporaryDirectory(prefix='.stage-', dir=root) as folder:
        tmp = Path(folder)
        for name, expected in files.items():
            data = (source / name).read_bytes()
            if hashlib.sha256(data).hexdigest() != expected:
                raise ValueError('source changed while staging')
            target = tmp / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(data)
        if candidate_plan:
            for p in plan['passes']:
                target = tmp / 'candidate-prompts' / p['prompt']
                target.parent.mkdir(exist_ok=True)
                target.write_text(p['text'] + '\n', encoding='utf-8')
                files[target.relative_to(tmp).as_posix()] = file_hash(target)
            raw = {k: v for k, v in plan.items() if k != 'prompts_dir'}
            raw['passes'] = [{k: v for k, v in p.items() if k != 'text'} for p in raw['passes']]
            raw['prompts_dir'] = '../candidate-prompts'
            (tmp / 'plans/review.json').write_bytes(canonical(raw) + b'\n')
            files['plans/review.json'] = file_hash(tmp / 'plans/review.json')
        for path in (tmp / 'plans').glob('*.json'):
            raw = read_json(path)
            if not isinstance(raw, dict):
                raise ValueError('bundled plans must be JSON objects')
            base = (path.parent / raw['prompts_dir']).resolve() if isinstance(raw.get('prompts_dir'), str) \
                else tmp / 'prompts'
            if not base.is_relative_to(tmp.resolve()):
                raise ValueError('bundled plans cannot load external prompt directories')
            pipelines.validate(raw, base)
        # Precompile at staging time. Readonly releases must not reparse every module
        # on every invocation. Hash-checked bytecode is also covered by the inventory.
        for name in RUNTIME:
            cached = Path(py_compile.compile(str(tmp / name), dfile='ringo-release/' + name,
                doraise=True, invalidation_mode=py_compile.PycInvalidationMode.CHECKED_HASH))
            files[cached.relative_to(tmp).as_posix()] = file_hash(cached)
        manifest = {'version': 1, 'files': files, 'quality': evidence,
                    'python_cache_tag': sys.implementation.cache_tag,
                    'default_review_sha256': config_hash(default_plan(tmp))}
        rid = sha(manifest)
        target = root / 'bundles' / rid
        if target.exists():
            verify(root, rid)
            return rid
        (tmp / 'release.json').write_bytes(canonical(manifest) + b'\n')
        # The bundle is owner-readonly against accidents, not a same-user security boundary.
        for path in tmp.rglob('*'):
            if path.is_file():
                with path.open('r+b' if os.name == 'nt' else 'rb') as f:  # Windows flushes only a writable handle
                    os.fsync(f.fileno())
            path.chmod(0o555 if path.is_dir() else 0o444)
        os.rename(tmp, target)
        target.chmod(0o555)
        fsync_dir(target.parent)
        # TemporaryDirectory's cleanup tolerates the moved-away path.
        return rid


def verify(root, rid):
    if not isinstance(rid, str) or not IDENT.fullmatch(rid):
        raise ValueError('invalid release id')
    bundle = root / 'bundles' / rid
    if (root / 'bundles').is_symlink() or bundle.is_symlink() or not bundle.is_dir() or (bundle / 'release.json').is_symlink():
        raise ValueError('missing bundle or symlink')
    manifest = metadata_json(bundle / 'release.json')
    if not isinstance(manifest, dict) or set(manifest) != {'version', 'files', 'quality', 'default_review_sha256', 'python_cache_tag'} \
            or type(manifest['version']) is not int or manifest['version'] != 1 or sha(manifest) != rid \
            or not isinstance(manifest['files'], dict) or not isinstance(manifest['quality'], dict) \
            or not isinstance(manifest['default_review_sha256'], str) \
            or not IDENT.fullmatch(manifest['default_review_sha256']) \
            or not (set(RUNTIME) - LATER_RUNTIME).issubset(manifest['files']):
        raise ValueError('release manifest integrity failure')
    # Validate every manifest path before opening inventory members. Keep paths
    # canonical so one physical file cannot satisfy two differently spelled keys.
    for name, expected in manifest['files'].items():
        if not isinstance(name, str):
            raise ValueError('invalid bundle manifest path')
        parts = PurePosixPath(name)
        if parts.is_absolute() or '..' in parts.parts or parts.as_posix() != name \
                or name in ('', '.', 'release.json') or not isinstance(expected, str) \
                or not IDENT.fullmatch(expected):
            raise ValueError('invalid bundle manifest path or hash')
    # DirEntry reuses directory-entry type information instead of asking pathlib
    # for fresh symlink/file metadata for every path. Hash contents in the same
    # walk: there is still one full SHA-256 read per expected file, every call.
    remaining = set(manifest['files'])
    pending, saw_manifest = [(os.fspath(bundle), '')], False
    while pending:
        directory, prefix = pending.pop()
        with os.scandir(directory) as entries:
            for entry in entries:
                if entry.is_symlink():
                    raise ValueError('bundle contains a symlink')
                name = prefix + entry.name
                if entry.is_dir(follow_symlinks=False):
                    pending.append((entry.path, name + '/'))
                elif entry.is_file(follow_symlinks=False):
                    if name == 'release.json':
                        saw_manifest = True
                        continue
                    if name not in remaining:
                        raise ValueError('bundle file inventory changed')
                    if file_hash(entry.path) != manifest['files'][name]:
                        raise ValueError('bundle content integrity failure')
                    remaining.remove(name)
                else:
                    raise ValueError('bundle contains an unsupported file type')
    if remaining or not saw_manifest:
        raise ValueError('bundle file inventory changed')
    return bundle, manifest


def approved(root, entry):
    path = root / 'approvals' / (entry['approval'] + '.json')
    if path.is_symlink():
        raise ValueError('approval cannot be a symlink')
    record = metadata_json(path)
    if not isinstance(record, dict) or sha(record) != entry['approval'] or record.get('release') != entry['release']:
        raise ValueError('approval integrity failure')
    return record


def activate(root, rid, report, approved_by, expected_current, uncertain_reason=None):
    import latency
    import pipelines
    if not isinstance(approved_by, str) or not approved_by.strip():
        raise ValueError('human approval identity is required')
    root = private_root(root)
    with locked(root):
        state = pointer(root)
        current = state['active']['release'] if state['active'] else 'none'
        if current != expected_current:
            raise ValueError('active release changed; reassess instead of replacing an unexpected version')
        bundle, manifest = verify(root, rid)
        if not isinstance(report, dict) or set(report) != {'version', 'reports'} or report['version'] != 1 \
                or not isinstance(report['reports'], list) or not report['reports']:
            raise ValueError('activation needs a version 1 suite of latency reports')
        workloads, baselines, ambiguous = set(), set(), False
        for item in report['reports']:
            latency.verify_report(item)
            if item['measurement']['candidate'] != rid:
                raise ValueError('latency evidence targets another candidate')
            baselines.add(item['measurement']['baseline'])
            workload = item['measurement']['protocol']['workload']
            if workload in workloads:
                raise ValueError('duplicate latency workload')
            workloads.add(workload)
            if item['measurement']['protocol']['candidate_config'] != manifest['default_review_sha256']:
                raise ValueError('latency candidate configuration does not match the bundled default review')
            if item['status'] == 'slower':
                raise ValueError('demonstrated slowdown blocks activation; change the approach')
            ambiguous |= item['status'] == 'uncertain'
        if len(baselines) != 1 or (state['active'] and baselines != {current}):
            raise ValueError('latency baseline is not the currently active release (or differs across workloads)')
        if state['active']:
            _, base_manifest = verify(root, current)
            if any(item['measurement']['protocol']['baseline_config'] != base_manifest['default_review_sha256']
                   for item in report['reports']):
                raise ValueError('latency baseline configuration is not the active default review')
        if not LATENCY_WORKLOADS.issubset(workloads):
            raise ValueError('latency suite lacks required startup/review/tracing/large/design/work coverage')
        if ambiguous and (not isinstance(uncertain_reason, str) or not uncertain_reason.strip()):
            raise ValueError('uncertain latency needs a human decision and explicit rationale')
        evidence = manifest['quality']
        if evidence.get('kind') == 'rrsi':
            refreshed = quality(evidence['store'], evidence['digest'],
                                pipelines.load_plan(bundle / 'plans/review.json', 'review'), bundle,
                                verify_bytecode=True)
            if refreshed != evidence:
                raise ValueError('quality evidence changed')
        elif evidence != {'kind': 'deployment', 'claim': None}:
            raise ValueError('unknown quality evidence; no automatic adoption')
        record = {'version': 1, 'release': rid, 'approved_by': approved_by,
                  'latency': report, 'uncertain_reason': uncertain_reason,
                  'previous': state['active'], 'quality': evidence}
        approval_id = sha(record)
        atomic_json(root / 'approvals' / (approval_id + '.json'), record)
        atomic_json(root / 'active.json', {'version': 1, 'active': {'release': rid, 'approval': approval_id},
                                          'previous': state['active']})
        return pointer(root)


def rollback(root, expected_current):
    root = private_root(root)
    with locked(root):
        state = pointer(root)
        if not state['active'] or state['active']['release'] != expected_current or not state['previous']:
            raise ValueError('no matching active/previous release to roll back')
        verify(root, state['previous']['release'])
        approved(root, state['previous'])
        value = {'version': 1, 'active': state['previous'], 'previous': state['active']}
        atomic_json(root / 'active.json', value)
        return value


def dispatch(argv):
    """Called before application imports. Select once, then execute that bundle in-process."""
    if getattr(sys, '_ringo_release', None):
        if sys._ringo_release['root'] != str(HERE):
            raise ValueError('a different runtime is already pinned in this process')
        return None
    root = state_root()
    state = pointer(root)
    if not state['active']:
        return None
    entry = state['active']
    bundle, manifest = verify(root, entry['release'])
    approved(root, entry)
    return execute(bundle, {'mode': 'release', 'id': entry['release'], 'root': str(bundle),
                           'approval': entry['approval'], 'quality_claim':
                               manifest['quality'] if manifest['quality']['kind'] == 'rrsi' else None}, argv)


def execute(bundle, metadata, argv):
    """Stable bootstrap imports the selected runtime directly, avoiding a second CLI load.

    releases/latency are stable management/bootstrap helpers, not application modules.
    Record their identity separately rather than claiming they came from the bundle.
    """
    modules = {Path(p).stem for p in RUNTIME} - {'ringo', 'releases', 'latency'}
    if any(name in sys.modules for name in modules):
        raise ValueError('runtime modules were imported before release selection; start a fresh CLI process')
    sys._ringo_release = {**metadata, 'checkout_root': str(HERE), 'bootstrap_sha256': sha({name: file_hash(HERE / name)
        for name in ('ringo.py', 'releases.py', 'latency.py')})}
    sys.path[:] = [str(bundle), *[p for p in sys.path if Path(p or os.getcwd()).resolve() != HERE]]
    sys.dont_write_bytecode = True
    sys.argv = [str(bundle / 'ringo.py'), *argv]
    if argv in ([], ['--help'], ['-h']):
        parser = argparse.ArgumentParser(prog='ringo.py', description='Pinned Ringo runtime: review, design, work.')
        for name in ('review', 'design', 'work'):
            parser.add_argument_group(name)
        parser.print_help()
        return 0 if argv else 2
    if argv[:1] == ['work']:
        import workflow
        return workflow.main(argv[1:])
    import harness
    return harness.main(argv, prog='ringo.py')


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--state', type=Path)
    commands = parser.add_subparsers(dest='command', required=True)
    staging = commands.add_parser('stage')
    staging.add_argument('--source', type=Path, default=HERE)
    staging.add_argument('--candidate-plan', type=Path)
    staging.add_argument('--evaluation-store', type=Path)
    staging.add_argument('--digest')
    activation = commands.add_parser('activate')
    activation.add_argument('--release', required=True)
    activation.add_argument('--latency-report', type=Path, required=True)
    activation.add_argument('--approved-by', required=True)
    activation.add_argument('--expected-current', required=True)
    activation.add_argument('--accept-uncertain', metavar='HUMAN_RATIONALE')
    back = commands.add_parser('rollback')
    back.add_argument('--expected-current', required=True)
    commands.add_parser('status')
    preview = commands.add_parser('preview', help='explicitly test an unapproved bundle; never activates it')
    preview.add_argument('--release', required=True)
    preview.add_argument('args', nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    try:
        root = state_root(args.state)
        if args.command == 'stage':
            result = {'release': stage(args.source, root, args.candidate_plan, args.evaluation_store, args.digest)}
        elif args.command == 'activate':
            result = activate(root, args.release, read_json(args.latency_report), args.approved_by,
                              args.expected_current, args.accept_uncertain)
        elif args.command == 'rollback':
            result = rollback(root, args.expected_current)
        elif args.command == 'preview':
            bundle, _ = verify(root, args.release)
            rest = args.args[1:] if args.args[:1] == ['--'] else args.args
            return execute(bundle, {'mode': 'preview', 'id': args.release, 'root': str(bundle),
                                    'approval': None, 'quality_claim': None}, rest)
        else:
            result = pointer(root)
            for entry in (result['active'], result['previous']):
                if entry:
                    verify(root, entry['release'])
                    approved(root, entry)
        print(json.dumps(result, indent=2))
        return 0
    except (ValueError, OSError, TypeError, KeyError) as exc:
        print(f'release: {exc}', file=sys.stderr)
        return 2


if __name__ == '__main__':
    sys.exit(main())
