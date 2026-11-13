"""Verify private historical checks in the exact, network-disabled grading image.

This performs no inference and certifies neither a provider broker nor fresh sealed data.
Fixtures are reconstructed from trusted commits; their code runs only inside DockerSandbox.
"""
import argparse
import json
from pathlib import Path
import shlex
import time

from turnrun import (build_fixture, inventory_digest, load_inventory, outside_repo,
                     remove_fixture, sha256_file, side_commit)
from turnrun_isolation import DockerSandbox


PROFILE = """import json,platform,shutil
print(json.dumps({'python': platform.python_version(), 'git': shutil.which('git'),
                  'cargo': shutil.which('cargo'), 'rustc': shutil.which('rustc')}))
"""


def verdict(base, fix):
    # A timeout, output overflow or grader protocol failure is never a discriminating bug.
    if any(row.get('error') or row.get('timeout') or row.get('truncated') for row in (base, fix)):
        return 'BROKEN'
    codes = (base['exit'], fix['exit'])
    if codes == (3, 3):
        return 'unavailable here'
    return 'discriminates' if codes == (1, 0) else 'BROKEN'


def verify(*, tasks, source, image, work, ids=None, runtime=None, seconds=300,
           workspace_bytes=512*1024*1024):
    tasks = outside_repo(tasks, 'the private inventory')
    work = outside_repo(work, 'the certification evidence')
    if work.exists():
        raise ValueError('certification evidence directory must not exist')
    if type(seconds) is not int or not 1 <= seconds <= 300:
        raise ValueError('checker wall limit must be from 1 through 300 seconds')
    items = load_inventory(tasks, source)
    inventory = inventory_digest(tasks, items)
    if ids is not None:
        if not ids or len(set(ids)) != len(ids) or set(ids) - {t['id'] for t in items}:
            raise ValueError('task selection must be nonempty, known and unique')
        items = [t for t in items if t['id'] in ids]
    if runtime is not None:
        if runtime not in ('python', 'rust'):
            raise ValueError('unknown task runtime')
        items = [t for t in items if t['runtime'] == runtime]
    if not items:
        raise ValueError('no tasks selected')
    pins = {name: sha256_file(Path(__file__).parent / name) for name in
            ('turnrun_certify.py', 'turnrun_isolation.py', 'turnrun.py')}
    work.mkdir(parents=True)
    report = {'version': 1, 'image': image, 'inventory_sha256': inventory,
              'sources': pins, 'seconds': seconds, 'workspace_bytes': workspace_bytes,
              'selected': [t['id'] for t in items], 'rows': [], 'provider_calls': 0,
              'all_selected_discriminate': False, 'live_certified': False,
              'scope': 'Historical checks only; no broker, scenario sensitivity or sealed split certification.'}

    def save():
        temporary = work / 'report.tmp'
        temporary.write_text(json.dumps(report, sort_keys=True, indent=2)+'\n', encoding='utf-8')
        temporary.replace(work / 'report.json')

    save()
    try:
        with DockerSandbox(image, lifetime=seconds+60, workspace_bytes=workspace_bytes) as probe:
            profile = probe.execute('python3 -I -c ' + shlex.quote(PROFILE), 30)
        if profile['exit'] or profile['timeout'] or profile['truncated']:
            raise ValueError('cannot inspect the grading runtime')
        report['profile'] = json.loads(profile['output'])
        for task in items:
            check = tasks / task['check']
            check_hash = sha256_file(check)
            row = {'task': task['id'], 'runtime': task['runtime'], 'check_sha256': check_hash}
            report['rows'].append(row)
            required = ('git', 'cargo', 'rustc') if task['runtime'] == 'rust' else ('git',)
            if any(not report['profile'][tool] for tool in required):
                row.update(verdict='unavailable here', error='required runtime tools are missing')
                save()
                continue
            for side in ('base', 'fix'):
                folder = work / f'{task["id"]}-{side}'
                started = time.monotonic()
                try:
                    fixture = build_fixture(source, side_commit(task, side), folder)
                    # Never import or execute submitted sources in this controller.
                    with DockerSandbox(image, check=check, lifetime=seconds+60,
                                       workspace_bytes=workspace_bytes) as checker:
                        if checker.check_sha256 != check_hash:
                            raise ValueError('protected check changed during verification')
                        observation = checker.grade(fixture['dir'], seconds)
                    log = work / f'{task["id"]}-{side}.txt'
                    log.write_text(observation.pop('output'), encoding='utf-8')
                    row[side] = {**observation, 'log': log.name, 'log_sha256': sha256_file(log),
                                 'tree': fixture['setup_tree']}
                except Exception as exc:
                    row[side] = {'error': str(exc)}
                finally:
                    row[side]['seconds'] = round(time.monotonic()-started, 3)
                    if folder.exists():
                        remove_fixture(folder)
                save()
            row['verdict'] = verdict(row['base'], row['fix'])
            save()
        if inventory_digest(tasks, load_inventory(tasks, source)) != inventory or any(
                sha256_file(Path(__file__).parent / name) != value for name, value in pins.items()):
            raise ValueError('certification inputs changed during verification')
        report['all_selected_discriminate'] = all(r['verdict'] == 'discriminates' for r in report['rows'])
    except Exception as exc:
        report['error'] = str(exc)
    save()
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--tasks', required=True)
    parser.add_argument('--source', default=str(Path(__file__).parent))
    parser.add_argument('--image', required=True)
    parser.add_argument('--work', required=True)
    parser.add_argument('--ids', help='comma-separated private task IDs')
    parser.add_argument('--runtime', choices=('python', 'rust'))
    parser.add_argument('--seconds', type=int, default=300)
    parser.add_argument('--workspace-bytes', type=int, default=512*1024*1024)
    args = vars(parser.parse_args())
    if args['ids'] is not None:
        args['ids'] = args['ids'].split(',')
    result = verify(**args)
    print(json.dumps(result, sort_keys=True))
    return 0 if result['all_selected_discriminate'] else 2


if __name__ == '__main__':
    raise SystemExit(main())
