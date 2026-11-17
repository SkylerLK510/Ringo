"""Build a pinned Codex worker-only image from publisher artifacts, without inference."""
import argparse
import copy
import json
from pathlib import Path
import re
import shutil
import subprocess
import tarfile

from turnrun import outside_repo, sha256_file
from turnrun_isolation import IMAGE
from turnrun_codex import MODEL


def build(*, archive, archive_sha256, catalog, catalog_sha256, base, work, platform):
    if platform not in ('aarch64-unknown-linux-musl', 'x86_64-unknown-linux-musl'):
        raise ValueError('select a pinned Linux Codex platform')
    for path, checksum in ((archive, archive_sha256), (catalog, catalog_sha256)):
        if not re.fullmatch('[0-9a-f]{64}', checksum) or sha256_file(path) != checksum:
            raise ValueError('publisher artifact checksum mismatch')
    if not IMAGE.fullmatch(base):
        raise ValueError('pin an immutable base image')
    if base.startswith('sha256:'):
        metadata = json.loads(subprocess.check_output(['docker', 'image', 'inspect', base]))[0]
        digests = metadata.get('RepoDigests') or []
        if not digests or not IMAGE.fullmatch(digests[0]):
            raise ValueError('the base image needs a repository digest for BuildKit')
        base = digests[0]
    work = outside_repo(work, 'Codex native image build context')
    work.mkdir(parents=True, exist_ok=False, mode=0o700)
    with tarfile.open(archive) as tar:
        entry = tar.getmember('codex-'+platform)
        if not entry.isfile() or entry.size > 512*1024*1024:
            raise ValueError('publisher archive lacks the selected ordinary binary')
        (work/'codex').write_bytes(tar.extractfile(entry).read())
    (work/'codex').chmod(0o755)
    models = json.loads(Path(catalog).read_text())['models']
    selected = [m for m in models if m['slug'] == MODEL]
    if len(selected) != 1:
        raise ValueError('publisher catalog does not contain the selected model')
    selected = copy.deepcopy(selected[0])
    # Keep publisher model instructions and capabilities, reducing exposed tools
    # to the trusted dynamic worker. The offline native replay checks the actual
    # outgoing tool schema, rather than inferring it from configuration flags.
    selected.update(shell_type='disabled', apply_patch_tool_type=None,
                    experimental_supported_tools=[], supports_search_tool=False, tool_mode='direct')
    (work/'models.json').write_text(json.dumps({'models': [selected]}))
    sources = ('turnrun_codex_bridge.py', 'turnrun_native_bridge.py')
    for name in sources:
        shutil.copyfile(Path(__file__).with_name(name), work/name)
    (work/'.dockerignore').write_text('*\n!Dockerfile\n!codex\n!models.json\n'+''.join('!'+s+'\n' for s in sources))
    (work/'Dockerfile').write_text(f'FROM {base}\nCOPY codex /opt/codex\nCOPY models.json /opt/ringo/models.json\n'
        + ''.join(f'COPY {s} /opt/ringo/{s}\n' for s in sources))
    subprocess.run(['docker', 'build', '--network', 'none', '--platform',
        'linux/arm64' if platform.startswith('aarch64') else 'linux/amd64',
        '--iidfile', str(work/'image-id.txt'), str(work)], check=True, timeout=300)
    result = {'image': (work/'image-id.txt').read_text().strip(), 'base': base,
        'binary_sha256': sha256_file(work/'codex'), 'archive_sha256': archive_sha256,
        'publisher_catalog_sha256': catalog_sha256, 'catalog_sha256': sha256_file(work/'models.json'),
        'sources': {s: sha256_file(work/s) for s in sources}, 'model': MODEL,
        'reasoning_effort': 'low', 'provider_calls': 0, 'live_certified': False}
    if not IMAGE.fullmatch(result['image']):
        raise ValueError('Docker did not return an immutable image ID')
    (work/'build.json').write_text(json.dumps(result, indent=2)+'\n')
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('archive', 'archive-sha256', 'catalog', 'catalog-sha256', 'base', 'work', 'platform'):
        parser.add_argument('--'+name, required=True)
    print(json.dumps(build(**vars(parser.parse_args()))))
